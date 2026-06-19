//! Test-only halo2 voucher pipeline driver. Mints ECC via the default
//! Giver (a shellnet/dev shortcut — Giver doesn't exist on production)
//! and proves the resulting `RootPN.VoucherGenerated` event via the
//! production `dodex_sdk::halo2::live::prove_voucher_for_event` Stage B.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::contracts::giver::GiverV3;
use ackinacki_kit::contracts::giver::ParamsOfSendCurrencyWithBody;
use ackinacki_kit::contracts::traits::AbiAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessageBody;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::root_pn::ParamsOfGenerateVoucher;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::halo2::live::prove_voucher_for_event;
use dodex_sdk::halo2::sk_commit::compute_sk_u_commit_hex;
use dodex_sdk::halo2::voucher_event;
use dodex_sdk::halo2::Halo2Paths;
use dodex_sdk::proof;
use serde_json::json;

use crate::common::context::ENDPOINT;

const EVENT_WAIT_TIMEOUT_S: u64 = 300;
const BLOCK_ID_WAIT_TIMEOUT_S: u64 = 180;

/// Drive a full live halo2 voucher proof against shellnet using the
/// default Giver as ECC source. `ephemeral_pubkey_hex` is the raw 32-byte
/// hex (no `0x`) committed in the proof; for PN deploy / sendEcc this is
/// the PN's own ephemeral key.
pub async fn make_voucher_proof(
    context: Arc<ClientContext>,
    ephemeral_pubkey_hex: &str,
    voucher_token_type: u32,
    voucher_value: u64,
    is_fee: bool,
) -> proof::Halo2Proof {
    let t_total = std::time::Instant::now();
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let network_url = format!("https://{ENDPOINT}");
    let ephemeral_pubkey_hex = proof::strip_0x(ephemeral_pubkey_hex).to_string();

    // 1. Random sk_u; skUCommit = poseidon([sk_u, 0]).
    let t_sk = std::time::Instant::now();
    let sk_u_hex = proof::random_secret_key();
    let sk_u_commit_hex =
        compute_sk_u_commit_hex(&sk_u_hex).expect("compute skUCommit (poseidon([sk_u, 0]))");
    eprintln!("[halo2-time] skUCommit poseidon: {:.3}s", t_sk.elapsed().as_secs_f64());
    eprintln!(
        "[halo2-live] sk_u={sk_u_hex}, skUCommit=0x{sk_u_commit_hex}, \
         tokenType={voucher_token_type}, value={voucher_value}"
    );

    // 2. Encode `RootPN.generateVoucher` body.
    let t_encode = std::time::Instant::now();
    let voucher_body = encode_generate_voucher_body(
        context.clone(),
        &root_pn,
        &format!("0x{sk_u_commit_hex}"),
        is_fee,
    )
    .await;
    eprintln!("[halo2-time] encode generateVoucher body: {:.3}s", t_encode.elapsed().as_secs_f64());

    // 3. Fire the Giver-funded `generateVoucher` message, then locate OUR
    //    `VoucherGenerated` ext-out by matching `skUCommit` (a 32-byte Poseidon
    //    commitment freshly generated above). The naive "snapshot count → wait for
    //    one more event" approach raced badly under parallel test runs — we'd grab
    //    another concurrent test's voucher event by accident, then fail later at
    //    submit with `ERR_INVALID_ZKPROOF (137)` because the proof was built
    //    against that other voucher's commitment.
    let t_send = std::time::Instant::now();
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
    eprintln!(
        "[halo2-time] Giver.sendCurrencyWithBody (RootPN.generateVoucher): {:.2}s",
        t_send.elapsed().as_secs_f64()
    );

    let t_wait_event = std::time::Instant::now();
    let event = voucher_event::wait_for_voucher_event_by_sk_u_commit(
        context.clone(),
        root_pn.address(),
        &format!("0x{sk_u_commit_hex}"),
        Duration::from_secs(EVENT_WAIT_TIMEOUT_S + BLOCK_ID_WAIT_TIMEOUT_S),
    )
    .await
    .expect("wait_for_voucher_event_by_sk_u_commit");
    eprintln!(
        "[halo2-time] indexer surfaced event + block_id (sk_u_commit match): {:.2}s",
        t_wait_event.elapsed().as_secs_f64()
    );

    // 4. Hand off to the production Stage B prover. Tests pull paths from env
    //    (matching the historical `PARAMS_DIR` / `HALO2_PK_CACHE` /
    //    `HALO2_FIXTURE_DIR` defaults); host integrations should construct
    //    `Halo2Paths` explicitly from app-managed dirs instead.
    let paths = Halo2Paths::from_env();
    let proof_out = prove_voucher_for_event(
        context,
        network_url,
        event,
        sk_u_hex,
        sk_u_commit_hex,
        voucher_value,
        voucher_token_type,
        ephemeral_pubkey_hex,
        None,
        &paths,
    )
    .await
    .expect("prove_voucher_for_event with env-default Halo2Paths");
    eprintln!("[halo2-time] make_voucher_proof TOTAL: {:.2}s", t_total.elapsed().as_secs_f64());
    proof_out
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
