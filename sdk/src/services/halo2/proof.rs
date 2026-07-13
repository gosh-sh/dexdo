//! Thin wrappers around the halo2 prover and the dark-DEX witness-export
//! library. They keep the kit error vocabulary consistent and isolate
//! consumers from the underlying crate APIs.

use std::path::Path;

use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::KitResult;
use dark_dex_halo2_private_witness_export_lib as witness_lib;
use halo2_proover::ProofOutput;
use halo2_proover::Prover;

use crate::services::halo2::cache::ProverCacheStorage;

const MODULE: KitModule = KitModule::External("dex.root_pn");

/// `HISTORY_PROOF_WINDOW_SIZE` for the connected network. Both shellnet
/// (after the bridge/DEX restart) and mainnet use 128.
pub const SHELLNET_HISTORY_PROOF_WINDOW_SIZE: u64 = 128;
pub const MAINNET_HISTORY_PROOF_WINDOW_SIZE: u64 = 128;

/// Canonical Hermez-anchored KZG SRS (K=19, ~64 MB) served by GOSH. The
/// prover's verifying key is derived from this exact SRS, and the on-chain
/// `RootPN` / `USDCBridge` verifier is anchored to the same Hermez Perpetual
/// Powers of Tau ceremony — a different SRS yields proofs that fail
/// verification. The prover downloads it once and caches it next to the keygen
/// artefacts (`<cache>/hermez_kzg_srs_k19.bin`).
const HERMEZ_KZG_SRS_URL: &str = "https://binaries.gosh.sh/dexdo/hermez_kzg_bn254_19.srs";

/// Inputs for the witness-export step. Matches `witness_lib::ExportParams`
/// but uses `KitResult` for ergonomics.
#[derive(Debug, Clone)]
pub struct ExportWitnessParams {
    /// Network endpoint (e.g. `"localhost"` or `"http://127.0.0.1:80"`).
    pub network: String,
    /// Block ID (hash) of the block carrying the `VoucherGenerated` event.
    /// Mutually exclusive with `block_height`.
    pub block_id: Option<String>,
    /// Block height — used when `block_id` is unknown.
    pub block_height: Option<u64>,
    /// `VoucherGenerated` ext-out message BOC (base64).
    pub event_boc: String,
    /// Voucher secret `sk_u` in hex (32 bytes).
    pub sk_u: String,
    /// Ephemeral public key (raw 32 bytes hex) committed in the voucher.
    pub ephemeral_pubkey: String,
    /// Output JSON path. The library writes the fixture there and also
    /// returns its content as a string.
    pub output: String,
    /// Strict number of historical layers to collect. The export errors out
    /// if the chain hasn't yet reached the boundary block for that layer.
    pub max_layers: Option<u32>,
}

impl From<&ExportWitnessParams> for witness_lib::ExportParams {
    fn from(p: &ExportWitnessParams) -> Self {
        witness_lib::ExportParams {
            network: p.network.clone(),
            block_height: p.block_height,
            block_id: p.block_id.clone(),
            event_boc: p.event_boc.clone(),
            sk_u: p.sk_u.clone(),
            ephemeral_pubkey: p.ephemeral_pubkey.clone(),
            output: p.output.clone(),
            max_layers: p.max_layers,
        }
    }
}

/// Run the dark-DEX witness exporter against a live node. Returns the
/// fixture JSON (also persisted at `params.output`).
pub async fn export_witness(params: &ExportWitnessParams) -> KitResult<String> {
    witness_lib::make_private_witness_and_public_data(&params.into()).await.map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::QueryEvents,
            format!("dark-DEX witness export failed: {e}"),
        )
    })
}

/// Generate a halo2 proof from a fixture JSON, using `cache` for SRS + PK/VK
/// persistence. The first call downloads the Hermez KZG SRS (~64 MB) and runs
/// keygen; subsequent calls reuse both from `cache`. Phase timings are written
/// to stderr for diagnostics.
pub fn generate_proof(
    cache: &impl ProverCacheStorage,
    fixture_json: &str,
) -> KitResult<ProofOutput> {
    let cache_dir = cache.pk_dir();
    let cached = cache_dir.as_ref().map(|d| d.join("pk_cache.bin").exists()).unwrap_or(false);

    let t_init = std::time::Instant::now();
    let mut prover = Prover::new_with_srs_from_url(
        Some(HERMEZ_KZG_SRS_URL),
        cache_dir.as_deref().map(Path::new),
    )
    .map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::QueryEvents,
            format!("halo2 prover initialisation failed: {e}"),
        )
    })?;
    eprintln!(
        "[halo2-time] Prover::new_with_srs_from_url ({}): {:.2}s",
        if cached { "cached" } else { "fresh" },
        t_init.elapsed().as_secs_f64()
    );

    let t_proof = std::time::Instant::now();
    let out = prover.generate_proof(fixture_json).map_err(|e| {
        KitError::new(
            MODULE,
            KitErrorCode::QueryEvents,
            format!("halo2 proof generation failed: {e}"),
        )
    })?;
    eprintln!(
        "[halo2-time] generate_proof ({}): {:.2}s ({} bytes)",
        if cached { "warm" } else { "keygen + prove" },
        t_proof.elapsed().as_secs_f64(),
        out.proof.len() / 2
    );
    Ok(out)
}

/// Layer `L` (1-based) target height: first block at or after which
/// `dark_dex_halo2_private_witness_export_lib` will accept `max_layers = L`.
/// Compute the chain height at which the L_(layer+1) history-proof bucket
/// containing `event_block_height` becomes finalised (the witness exporter +
/// halo2 prover can run only past this point).
///
/// `layer` is **0-indexed** to match
/// `dark_dex_halo2_private_witness_export_lib`'s `layer_number` convention
/// (`HISTORY_PROOF_WINDOW_SIZE.pow(layer_number + 1)`). The diagram on the
/// halo2 spec uses **1-indexed** L_N notation, so:
///   * code `layer=0` ↔ diagram **L_1** — bucket of `W^1` blocks
///   * code `layer=1` ↔ diagram **L_2** — bucket of `W^2` blocks (~20 s on
///     shellnet)
///   * code `layer=2` ↔ diagram **L_3** — bucket of `W^3` blocks (~160 s)
///   * code `layer=3` ↔ diagram **L_4** — bucket of `W^4` blocks (~21 min)
/// Keep that off-by-one in mind when reading attempt-plan logs / comments.
///
/// `window_size` is `HISTORY_PROOF_WINDOW_SIZE` for the connected network
/// (128 on shellnet/mainnet).
///
/// Mirrors the acki-nacki Python pipeline's `+ W` safety margin: the
/// witness lib's internal readiness check is `block_height.div_ceil(W^(L+1))
/// * W^(L+1) <= last`, but the dense-chain leaf for the boundary bucket is
/// only materialised on the chain a few blocks past the boundary itself.
/// On events landing exactly on a boundary (e.g. bh % W == 0), waiting
/// just for `latest >= boundary` makes witness export fail with
/// `block at height boundary+k not found`. The `+ W` cushion gives the
/// node time to seal the leaf-bearing block.
pub fn target_height_for_layer(event_block_height: u64, layer: u32, window_size: u64) -> u64 {
    let boundary = window_size.pow(layer + 1);
    event_block_height.div_ceil(boundary) * boundary + window_size
}
