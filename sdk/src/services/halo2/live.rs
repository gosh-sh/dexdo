//! Production-grade halo2 voucher prover.
//!
//! `prove_voucher_for_event` takes a `VoucherGenerated` ext-out event
//! that has already been emitted on `RootPN` (typically by a multifactor
//! wallet's `submitTransaction`) and produces the layered halo2 proof
//! that `RootPN.deployPrivateNote` / `RootPN.sendEccShellToPrivateNote`
//! consume. The caller owns voucher minting (multifactor on production,
//! Giver on tests — see `tests/integration/common/voucher.rs`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::ClientContext;
use halo2_proover::ProofOutput;

use crate::services::halo2::cache::FilesystemCache;
use crate::services::halo2::proof as halo2_proof;
use crate::services::halo2::proof::ExportWitnessParams;
use crate::services::halo2::voucher_event;

/// Layer numbers we attempt before giving up, **0-indexed** to match
/// `dark_dex_halo2_private_witness_export_lib`'s `layer_number` (boundary
/// = `W^(layer+1)`). In the halo2 spec diagram buckets are 1-indexed
/// `L_N`, so code `layer=N` corresponds to diagram **`L_(N+1)`**.
///
/// With W=128 (current shellnet + mainnet) the bucket sizes are:
///   - `layer=0` (= diagram **L_1**, `W^1` = 128 blocks ≈ 2 min) — primary
///     target. Matches the acki-nacki Python orchestrator (`tests/dex/README.md`).
///   - `layer=1` (= diagram **L_2**, `W^2` = 16384 blocks) — fallback if L_1
///     boundary already passed by the time we start.
///   - `layer=2` (= diagram **L_3**, `W^3` ≈ 2.1M blocks) — practically
///     unreachable on fresh chain; left as a ceiling.
///
/// Override with `HALO2_ATTEMPT_LAYERS=…` if needed.
const DEFAULT_ATTEMPT_LAYERS: &[u32] = &[0, 1, 2];

fn attempt_layers() -> Vec<u32> {
    match std::env::var("HALO2_ATTEMPT_LAYERS") {
        Ok(s) => s.split(',').filter_map(|p| p.trim().parse::<u32>().ok()).collect::<Vec<_>>(),
        Err(_) => DEFAULT_ATTEMPT_LAYERS.to_vec(),
    }
}

/// Per-layer wait timeout (s). At `layer=4` (= diagram L_5, `W^6` blocks)
/// shellnet may take ~21 minutes; this caps our patience above that.
const LAYER_WAIT_TIMEOUT_S: u64 = 1800;

/// Fully-resolved halo2 proof + the public inputs the contract expects.
#[derive(Debug, Clone)]
#[allow(dead_code)] // sk_u_hex / sk_u_commit_hex / ephemeral_pubkey_hex kept for diagnostics
pub struct Halo2Proof {
    pub proof: String,
    pub deposit_identifier_hash_hex: String,
    pub final_layer_historical_hash_root_hex: String,
    pub voucher_nominal_fr_hex: String,
    pub token_type_fr_hex: String,
    pub ephemeral_pubkey_hex: String,
    pub voucher_value: u64,
    pub voucher_token_type: u32,
    pub layer_number: u8,
    pub sk_u_hex: String,
    pub sk_u_commit_hex: String,
}

/// Production-grade Stage B: prove halo2 voucher against a `VoucherGenerated`
/// event that was emitted by something **other** than `Giver` — typically a
/// `Multifactor::submitTransaction`-driven `RootPN.generateVoucher`. Caller
/// is responsible for picking `sk_u` + computing `skUCommit`, sending the
/// multifactor message with that `skUCommit` in the payload, and resolving
/// the resulting `VoucherExtoutMessage` (via
/// `voucher_event::wait_for_voucher_event_by_sk_u_commit`, which matches
/// the event by its `skUCommit` payload — bulletproof under concurrent
/// voucher mints from independent callers).
///
/// `paths` controls where the SRS / prover cache / witness fixtures live —
/// hosts (mobile / packaged builds) must point these at writable
/// app-managed locations; `Halo2Paths::from_env()` is the test/CLI default.
///
/// Panics on halo2 / chain failures (layer wait timeout, witness export
/// failure, prover failure across the whole attempt plan); returns
/// `Halo2PathsError` if `paths` is misconfigured (missing SRS, unwritable
/// cache/fixture dir).
pub async fn prove_voucher_for_event(
    context: Arc<ClientContext>,
    network_url: String,
    event: voucher_event::VoucherExtoutMessage,
    sk_u_hex: String,
    sk_u_commit_hex: String,
    voucher_value: u64,
    voucher_token_type: u32,
    ephemeral_pubkey_hex: String,
    history_proof_window_size: Option<u64>,
    paths: &super::paths::Halo2Paths,
) -> Result<Halo2Proof, super::paths::Halo2PathsError> {
    paths.validate()?;
    paths.install_env();

    let t_total = std::time::Instant::now();
    let history_proof_window_size =
        history_proof_window_size.unwrap_or(halo2_proof::SHELLNET_HISTORY_PROOF_WINDOW_SIZE);

    let t_height_query = std::time::Instant::now();
    let block_height = voucher_event::get_block_height_by_id(
        context.clone(),
        event.block_id.as_deref().expect("event block_id resolved before prove"),
    )
    .await
    .expect("block height query")
    .expect("block exists for event block_id");
    eprintln!(
        "[halo2-time] resolve event block_height via GraphQL: {:.2}s",
        t_height_query.elapsed().as_secs_f64()
    );
    eprintln!(
        "[halo2-live] event captured: block_id={}, height={block_height}",
        event.block_id.as_deref().unwrap_or("<unknown>")
    );

    let cache = FilesystemCache::new(paths.prover_cache_dir.clone());
    let plan = attempt_layers();

    for &layer in &plan {
        let target =
            halo2_proof::target_height_for_layer(block_height, layer, history_proof_window_size);

        let latest = voucher_event::get_latest_block_height(context.clone()).await.ok();
        let already_done = latest.map(|h| h >= target).unwrap_or(false);
        eprintln!(
            "[halo2-live] L{layer}: target={target} (W={history_proof_window_size}, \
             +{} blocks from event), latest={:?}, wait={}",
            target.saturating_sub(block_height),
            latest,
            if already_done { "skipped" } else { "needed" }
        );

        let t_wait_chain = std::time::Instant::now();
        if !already_done {
            if let Err(e) = voucher_event::wait_for_block_height(
                context.clone(),
                target,
                Duration::from_secs(LAYER_WAIT_TIMEOUT_S),
            )
            .await
            {
                eprintln!("[halo2-live] L{layer} wait failed: {e}");
                continue;
            }
        }
        eprintln!(
            "[halo2-time] L{layer} chain height ≥ target: {:.2}s",
            t_wait_chain.elapsed().as_secs_f64()
        );

        if !already_done {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        let fixture_path = fixture_path_for(&paths.fixture_dir, block_height, layer);
        let export = ExportWitnessParams {
            network: network_url.clone(),
            block_id: event.block_id.clone(),
            block_height: None,
            event_boc: event.boc.clone(),
            sk_u: sk_u_hex.clone(),
            ephemeral_pubkey: ephemeral_pubkey_hex.clone(),
            output: fixture_path.to_string_lossy().into_owned(),
            max_layers: Some(layer),
        };

        let t_witness = std::time::Instant::now();
        let fixture_json = match halo2_proof::export_witness(&export).await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[halo2-live] L{layer} witness export failed: {e}");
                continue;
            }
        };
        eprintln!(
            "[halo2-time] L{layer} witness export: {:.2}s",
            t_witness.elapsed().as_secs_f64()
        );

        let t_prover = std::time::Instant::now();
        match halo2_proof::generate_proof(&cache, &fixture_json) {
            Ok(out) => {
                eprintln!(
                    "[halo2-time] L{layer} prover wall-clock total: {:.2}s",
                    t_prover.elapsed().as_secs_f64()
                );
                eprintln!(
                    "[halo2-time] prove_voucher_for_event TOTAL: {:.2}s",
                    t_total.elapsed().as_secs_f64()
                );
                return Ok(into_halo2_proof(
                    out,
                    sk_u_hex,
                    sk_u_commit_hex,
                    voucher_value,
                    voucher_token_type,
                    layer,
                    ephemeral_pubkey_hex,
                ));
            }
            Err(e) => {
                eprintln!("[halo2-live] L{layer} prover failed: {e}");
                continue;
            }
        }
    }

    panic!("prove_voucher_for_event: halo2 proof failed for all attempted layers {plan:?}");
}

fn into_halo2_proof(
    out: ProofOutput,
    sk_u_hex: String,
    sk_u_commit_hex: String,
    voucher_value: u64,
    voucher_token_type: u32,
    layer: u32,
    ephemeral_pubkey_hex: String,
) -> Halo2Proof {
    Halo2Proof {
        proof: out.proof,
        deposit_identifier_hash_hex: format!("0x{}", out.deposit_identifier_hash),
        final_layer_historical_hash_root_hex: format!("0x{}", out.final_layer_historical_hash_root),
        voucher_nominal_fr_hex: format!("0x{}", out.voucher_nominal),
        token_type_fr_hex: format!("0x{}", out.token_type),
        ephemeral_pubkey_hex,
        voucher_value,
        voucher_token_type,
        layer_number: layer.saturating_add(1).try_into().expect("layer fits into u8"),
        sk_u_hex,
        sk_u_commit_hex,
    }
}

fn fixture_path_for(dir: &std::path::Path, block_height: u64, layer: u32) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_nanos();
    std::fs::create_dir_all(dir).expect("create fixture dir");
    dir.join(format!("dex_fixture_live_L{layer}_H{block_height}_T{nanos}.json"))
}
