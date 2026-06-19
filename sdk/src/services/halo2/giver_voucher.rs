//! Giver-flow voucher minting: fire `RootPN.generateVoucher` via the default
//! Giver, capture the resulting `VoucherGenerated` ext-out by `skUCommit`
//! match, and run the layered halo2 prover. This is a **shellnet/dev
//! shortcut** — the Giver doesn't exist on production, where vouchers come
//! from the multifactor wallet's `submitTransaction` instead.
//!
//! Used by: integration tests (`tests/integration/common/voucher.rs`) and
//! offline tooling such as the `mint_pn_pool` binary that pre-deploys a
//! pool of PrivateNotes for the orderbook / parallel-test scenarios.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::contracts::giver::v3::GiverV3;
use ackinacki_kit::contracts::giver::v3::ParamsOfSendCurrencyWithBody;
use ackinacki_kit::contracts::traits::AbiAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessageBody;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::root_pn::ParamsOfGenerateVoucher;
use dodex_contracts::dex::root_pn::RootPn;
use serde_json::json;

use crate::dex_contract_params;
use crate::services::halo2::live::prove_voucher_for_event;
use crate::services::halo2::live::Halo2Proof;
use crate::services::halo2::paths::Halo2Paths;
use crate::services::halo2::paths::Halo2PathsError;
use crate::services::halo2::sk_commit::compute_sk_u_commit_hex;
use crate::services::halo2::voucher_event;
use crate::services::proof;

/// How long we wait for the indexer to surface our `VoucherGenerated` event
/// (matched by `skUCommit`) including the source transaction's `block_id`
/// the prover needs.
const VOUCHER_EVENT_RESOLVE_TIMEOUT_S: u64 = 480;

/// Mint a voucher via Giver and produce its halo2 proof in one shot.
///
/// Caller passes the **recipient ephemeral pubkey** the proof will commit
/// to (for `RootPN.deployPrivateNote` this is the PN's own keypair public
/// half; for `RootPN.sendEccShellToPrivateNote` it's the PN owner's
/// pubkey). `voucher_value` is the raw amount with token decimals already
/// applied; `voucher_token_type` is the ECC currency id (1=NACKL,
/// 2=SHELL); `is_fee=true` makes RootPN rewrite the emitted token type to
/// `CURRENCIES_ID_SHELL_FEE` (=300) — required by `sendEccShellToPrivateNote`.
///
/// Returns the fully-resolved `Halo2Proof` ready to feed into a `Dex`
/// facade method (or kit `RootPn` wrappers).
pub async fn mint_voucher_via_giver(
    context: Arc<ClientContext>,
    network_url: String,
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

    // 2. Encode the `RootPN.generateVoucher` body.
    let voucher_body = encode_generate_voucher_body(
        context.clone(),
        &root_pn,
        &format!("0x{sk_u_commit_hex}"),
        is_fee,
    )
    .await;

    // 3. Fire the Giver-funded ECC + body message that triggers
    //    `RootPN.generateVoucher`.
    let mut ecc = HashMap::new();
    ecc.insert(voucher_token_type, voucher_value);
    let giver = GiverV3::new_default(context.clone());
    giver
        .send_currency_with_body(
            ParamsOfSendCurrencyWithBody {
                dest: root_pn.address().to_string(),
                value: 2_000_000_000,
                ecc,
                flag: 1,
                body: voucher_body.body,
            },
            Signer::None,
        )
        .await
        .expect("Giver.send_currency_with_body → RootPN.generateVoucher");

    // 4. Locate OUR voucher event by `skUCommit` (32-byte Poseidon commitment
    //    freshly generated above) — collision-free under any parallel load.
    let event = voucher_event::wait_for_voucher_event_by_sk_u_commit(
        context.clone(),
        root_pn.address(),
        &format!("0x{sk_u_commit_hex}"),
        Duration::from_secs(VOUCHER_EVENT_RESOLVE_TIMEOUT_S),
    )
    .await
    .expect("wait_for_voucher_event_by_sk_u_commit (giver flow)");

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
