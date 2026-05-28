//! Loader for the `mint_ob_pool` fixture (`ob_pool.json`).
//!
//! Every test that needs a live OrderBook calls [`load`] to take one or
//! more pre-warmed markets from the pool. The fixture is built ahead of
//! time by `cargo run --release --bin mint_ob_pool` — the seeder waits
//! through the bidding window + spawns the OrderBook so tests don't have
//! to.
//!
//! Each market has a finite trading window (`stake_end_unix .. result_start_unix`).
//! Loading checks `now < result_start - SAFETY_MARGIN` and returns a
//! clear error if the pool has aged out — re-seed with `mint_ob_pool`.
//!
//! Path lookup (first match wins):
//!   1. `OB_POOL_PATH` env var
//!   2. `ob_pool.json` in the workspace root (`CARGO_MANIFEST_DIR`)
//!
//! The test that picked a market is responsible for not stepping on
//! orders left over by another test in the same suite. Default usage:
//! one market per `#[tokio::test]`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use ackinacki_kit::tvm_client::crypto::KeyPair;
use serde::Deserialize;

/// Stop using a market this many seconds before its `result_start_unix`.
/// Inside the safety window the OrderBook is technically still alive but
/// every test op is racing against shutdown — better to surface a clean
/// "re-seed" error than to chase flaky failures.
const SAFETY_MARGIN_SECS: u64 = 5 * 60;

#[allow(dead_code)] // fixture loader — fields are read by tests as needed
#[derive(Debug, Clone, Deserialize)]
pub struct ObPool {
    pub endpoint: String,
    pub created_at_unix: u64,
    pub last_lifetime_secs: u64,
    pub deployer_nominal: String,
    pub deployer_raw_value: u64,
    pub token_type: u32,
    pub markets: Vec<ObMarket>,
}

#[allow(dead_code)] // fixture loader — fields/methods are used by tests as needed
#[derive(Debug, Clone, Deserialize)]
pub struct ObMarket {
    pub oracle_address: String,
    pub oracle_name: String,
    pub oracle_pubkey_hex: String,
    pub oracle_secret_hex: String,
    pub oracle_list_address: String,
    pub event_id: String,
    pub outcome_names: HashMap<u32, String>,

    pub deployer_pn_address: String,
    pub deployer_pn_pubkey_hex: String,
    pub deployer_pn_secret_hex: String,
    pub deployer_deposit_identifier_hash: String,

    pub pmp_address: String,
    pub order_book_address: String,
    pub oracle_list_hash: String,
    pub token_type: u32,

    pub stake_start_unix: u64,
    pub stake_end_unix: u64,
    pub result_start_unix: u64,
    pub result_end_unix: u64,
    pub freeze_unix: u64,

    pub deployer_split_collateral: u128,
    pub lifetime_secs: u64,
}

#[allow(dead_code)] // helper methods consumed by tests as they're written
impl ObMarket {
    /// `KeyPair` for signing oracle-side messages
    /// (`submitSetTimings`/`submitResolve`/`submitCancelEvent`).
    pub fn oracle_keys(&self) -> KeyPair {
        KeyPair { public: self.oracle_pubkey_hex.clone(), secret: self.oracle_secret_hex.clone() }
    }

    /// `KeyPair` for signing as the deployer-PN (owner of the PMP and
    /// holder of pre-split outcome tokens).
    pub fn deployer_keys(&self) -> KeyPair {
        KeyPair {
            public: self.deployer_pn_pubkey_hex.clone(),
            secret: self.deployer_pn_secret_hex.clone(),
        }
    }

    /// Seconds remaining until `result_start_unix` (after which the
    /// OrderBook starts accepting `setResultStart` and orders get
    /// rejected). 0 if already past.
    pub fn seconds_until_result_start(&self) -> u64 {
        let now = now_unix();
        self.result_start_unix.saturating_sub(now)
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn pool_path() -> PathBuf {
    if let Ok(env) = std::env::var("OB_POOL_PATH") {
        return PathBuf::from(env);
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR set during cargo test");
    let manifest = Path::new(&manifest);
    // Try dodex_sdk/ first (where the seeder lands by default when run
    // from dodex_sdk/), then workspace root (when run from workspace root).
    for candidate in [manifest.join("ob_pool.json"), manifest.join("../ob_pool.json")] {
        if candidate.exists() {
            return candidate;
        }
    }
    manifest.join("ob_pool.json") // surface clear "not found" error
}

/// Load the OB pool from disk, validate freshness and basic invariants.
/// Panics with a readable message on any failure — these are setup bugs,
/// not runtime errors a test could recover from.
pub fn load() -> ObPool {
    let path = pool_path();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "ob_pool.json not found at {} ({e}). Generate one first:\n  \
             cargo run --release --bin mint_ob_pool -- --count 2",
            path.display(),
        )
    });
    let pool: ObPool = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!("parse {} as ObPool: {e}", path.display())
    });

    assert!(
        !pool.markets.is_empty(),
        "ob_pool.json at {} has no markets — re-run mint_ob_pool",
        path.display(),
    );

    let now = now_unix();
    let mut live = 0;
    for (i, m) in pool.markets.iter().enumerate() {
        let cutoff = m.result_start_unix.saturating_sub(SAFETY_MARGIN_SECS);
        if now < cutoff {
            live += 1;
        } else {
            eprintln!(
                "[ob-pool] market[{i}] (pmp={}) is stale: \
                 now={now} ≥ result_start - {SAFETY_MARGIN_SECS}s = {cutoff}",
                m.pmp_address,
            );
        }
    }
    assert!(
        live > 0,
        "all {} markets in {} have aged out (now={now}); re-run mint_ob_pool to refresh",
        pool.markets.len(),
        path.display(),
    );

    pool
}

/// Take the first non-stale market from the pool. Convenience for tests
/// that need exactly one market.
pub fn first_live_market() -> ObMarket {
    nth_live_market(0)
}

/// Take the `idx`-th non-stale market from the pool. Used by the suite
/// to distribute tests across multiple markets so books stay isolated.
/// Falls back to the last live market if `idx` is out of range — better
/// than panicking, but tests that need strict isolation should ensure
/// `mint_ob_pool --count >= idx + 1`.
pub fn nth_live_market(idx: usize) -> ObMarket {
    let pool = load();
    let now = now_unix();
    let live: Vec<ObMarket> = pool
        .markets
        .into_iter()
        .filter(|m| now < m.result_start_unix.saturating_sub(SAFETY_MARGIN_SECS))
        .collect();
    assert!(
        !live.is_empty(),
        "ob_pool::load() ensured at least one live market exists",
    );
    let pick = idx.min(live.len() - 1);
    if pick != idx {
        eprintln!(
            "[ob-pool] requested market[{idx}] but only {} live; using market[{pick}] instead",
            live.len(),
        );
    }
    live.into_iter().nth(pick).expect("bounded above")
}

/// Return ALL markets past `result_start` and still inside their
/// resolve window (`now < result_end`). The caller decides which one
/// to use — important for the resolve+claim test which has to skip
/// markets that have ALREADY been resolved on chain (a state that
/// only `getDetails` can reveal; the JSON has no resolution flag).
pub fn post_result_start_markets() -> Vec<ObMarket> {
    let pool = load_unfiltered();
    let now = now_unix();
    pool.markets
        .into_iter()
        .filter(|m| now >= m.result_start_unix && now < m.result_end_unix)
        .collect()
}

/// Like [`load`] but doesn't assert any market is live. Used by tests
/// that explicitly want stale markets (e.g. resolve+claim).
pub fn load_unfiltered() -> ObPool {
    let path = pool_path();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "ob_pool.json not found at {} ({e}). Generate one first:\n  \
             cargo run --release --bin mint_ob_pool -- --count 2",
            path.display(),
        )
    });
    let pool: ObPool = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!("parse {} as ObPool: {e}", path.display())
    });
    assert!(
        !pool.markets.is_empty(),
        "ob_pool.json at {} has no markets — re-run mint_ob_pool",
        path.display(),
    );
    pool
}
