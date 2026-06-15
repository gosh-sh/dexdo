//! Multisig-flow voucher minting: route `RootPN.generateVoucher` through a
//! user-owned multisig wallet's `submitTransaction`, capture the resulting
//! `VoucherGenerated` ext-out by `skUCommit` match, and run the layered
//! halo2 prover. This is the **production / user-facing** counterpart to
//! [`super::giver_voucher`] (which exists as a shellnet/dev shortcut and
//! depends on the Giver — absent on mainnet).
//!
//! Caller pre-conditions: the multisig wallet is already deployed and
//! funded with (a) enough native vmshell to cover the queued message's
//! `value`, and (b) enough of `voucher_token_type` ECC currency to cover
//! `voucher_value`. The caller passes the multisig's address and the
//! custodian keypair allowed to sign `submitTransaction`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::contracts::dex::root_pn::ParamsOfGenerateVoucher;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::multisig::Multisig;
use ackinacki_kit::contracts::multisig::ParamsOfSubmitTransaction;
use ackinacki_kit::contracts::traits::AbiAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessageBody;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use ackinacki_kit::contracts::account::ParamsOfNewContract;
use serde_json::json;

use crate::dex_contract_params;
use crate::services::halo2::live::prove_voucher_for_event;
use crate::services::halo2::live::Halo2Proof;
use crate::services::halo2::paths::Halo2Paths;
use crate::services::halo2::paths::Halo2PathsError;
use crate::services::halo2::sk_commit::compute_sk_u_commit_hex;
use crate::services::halo2::voucher_event;
use crate::services::proof;

/// Native vmshell value attached to the multisig→RootPN internal message.
/// Covers gas for `RootPN.generateVoucher` execution; mirrors the giver
/// flow's hardcoded value (which has been empirically sufficient).
const SUBMIT_NATIVE_VALUE: u128 = 2_000_000_000;

/// How long we wait for the indexer to surface our `VoucherGenerated` event
/// (matched by `skUCommit`) including the source transaction's `block_id`
/// the prover needs. Same budget as the giver flow.
const VOUCHER_EVENT_RESOLVE_TIMEOUT_S: u64 = 480;

/// Mint a voucher via a user-owned multisig and produce its halo2 proof
/// in one shot. The user-facing counterpart to
/// [`super::giver_voucher::mint_voucher_via_giver`] — see that function
/// for the semantics of the recipient / token / fee parameters.
#[allow(clippy::too_many_arguments)]
pub async fn mint_voucher_via_multisig(
    context: Arc<ClientContext>,
    network_url: String,
    multisig_address: &str,
    multisig_owner_keys: KeyPair,
    recipient_ephemeral_pubkey_hex: &str,
    voucher_token_type: u32,
    voucher_value: u64,
    is_fee: bool,
    paths: &Halo2Paths,
) -> Result<Halo2Proof, Halo2PathsError> {
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let recipient_ephemeral_pubkey_hex =
        proof::strip_0x(recipient_ephemeral_pubkey_hex).to_string();

    // 1. Random sk_u; skUCommit = poseidon([sk_u, 0]).
    let sk_u_hex = proof::random_secret_key();
    let sk_u_commit_hex =
        compute_sk_u_commit_hex(&sk_u_hex).expect("compute skUCommit (poseidon([sk_u, 0]))");

    // 2. Encode the `RootPN.generateVoucher` body — same payload the giver
    //    flow uses; the wallet just transports it on the user's behalf.
    let voucher_body = encode_generate_voucher_body(
        context.clone(),
        &root_pn,
        &format!("0x{sk_u_commit_hex}"),
        is_fee,
    )
    .await;

    // 3. Drive the multisig's submitTransaction. With reqConfirms=1 the
    //    queued transaction executes in the same block, dispatching the
    //    encoded body to RootPN.
    let mut ecc = HashMap::new();
    ecc.insert(voucher_token_type, voucher_value);
    // A user-deployed multisig lives under its OWN dApp (dapp_id = its bare
    // account id), unlike the System-dApp DEX contracts above. Build its
    // handle with that dapp_id so >= 1.0.0 gateway lookups resolve it.
    let multisig_account_id = multisig_address.strip_prefix("0:").unwrap_or(multisig_address);
    let multisig = Multisig::new(
        context.clone(),
        ParamsOfNewContract::new(multisig_address, multisig_account_id),
    );
    multisig
        .submit_transaction(
            ParamsOfSubmitTransaction {
                dest: root_pn.address().to_string(),
                value: SUBMIT_NATIVE_VALUE,
                cc: ecc,
                bounce: true,
                flag: 1,
                payload: voucher_body.body,
                dapp_id: "0".to_string(),
            },
            Signer::Keys { keys: multisig_owner_keys },
        )
        .await
        .expect("Multisig.submit_transaction → RootPN.generateVoucher");

    // 4. Locate OUR voucher event by `skUCommit` — collision-free under
    //    any parallel load (32-byte Poseidon commitment freshly generated
    //    above).
    let event = voucher_event::wait_for_voucher_event_by_sk_u_commit(
        context.clone(),
        root_pn.address(),
        &format!("0x{sk_u_commit_hex}"),
        Duration::from_secs(VOUCHER_EVENT_RESOLVE_TIMEOUT_S),
    )
    .await
    .expect("wait_for_voucher_event_by_sk_u_commit (multisig flow)");

    // 5. Hand off to the production Stage B prover.
    prove_voucher_for_event(
        context,
        network_url,
        event,
        sk_u_hex,
        sk_u_commit_hex,
        voucher_value,
        voucher_token_type,
        recipient_ephemeral_pubkey_hex,
        None,
        paths,
    )
    .await
}

async fn encode_generate_voucher_body(
    context: Arc<ClientContext>,
    root_pn: &RootPn,
    sk_u_commit_hex_0x: &str,
    is_fee: bool,
) -> ackinacki_kit::tvm_client::abi::ResultOfEncodeMessageBody {
    let abi = root_pn.abi().clone();
    ackinacki_kit::tvm_client::abi::encode_message_body(
        context,
        ParamsOfEncodeMessageBody {
            abi,
            address: Some(root_pn.address().to_string()),
            call_set: CallSet {
                function_name: "generateVoucher".to_string(),
                header: None,
                input: Some(json!(ParamsOfGenerateVoucher {
                    sk_u_commit: sk_u_commit_hex_0x.to_string(),
                    is_fee,
                })),
            },
            is_internal: true,
            signer: Signer::None,
            processing_try_index: None,
            signature_id: None,
        },
    )
    .await
    .expect("encode RootPN.generateVoucher body")
}
