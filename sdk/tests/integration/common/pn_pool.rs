//! Loader for the `mint_pn_pool` fixture (`pn_pool.json`).
//!
//! Each note in the pool is a fully-funded `PrivateNote` (NACKL deposit +
//! SHELL gas + native vmshell), ready to be claimed by a test as a
//! "trader" against a market from `ob_pool.json`.
//!
//! Path lookup (first match wins):
//!   1. `PN_POOL_PATH` env var
//!   2. `pn_pool.json` in `dodex_sdk/` or workspace root
//!
//! Tests are responsible for taking distinct notes if they run in the
//! same process: pass an explicit start index or call
//! [`PnPool::take_pair`] / [`PnPool::take_n`] sequentially without
//! re-loading.

use std::path::Path;
use std::path::PathBuf;

use ackinacki_kit::tvm_client::crypto::KeyPair;
use serde::Deserialize;

#[allow(dead_code)] // fixture loader
#[derive(Debug, Clone, Deserialize)]
pub struct PnPool {
    pub endpoint: String,
    pub created_at_unix: u64,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value_per_pn: u64,
    pub ecc_shell_deposit_per_pn: u64,
    pub notes: Vec<PnNote>,
}

#[allow(dead_code)] // fixture loader
#[derive(Debug, Clone, Deserialize)]
pub struct PnNote {
    pub address: String,
    pub deposit_identifier_hash: String,
    pub owner_public_key_hex: String,
    pub owner_secret_key_hex: String,
    pub deployed_at_unix: u64,
    pub shell_funded: bool,
    pub native_funded: bool,
}

impl PnNote {
    pub fn keys(&self) -> KeyPair {
        KeyPair {
            public: self.owner_public_key_hex.clone(),
            secret: self.owner_secret_key_hex.clone(),
        }
    }
}

impl PnPool {
    /// Take exactly `n` fully-funded notes starting from index `start`.
    /// Panics if the pool doesn't have enough — that's a fixture-sizing
    /// problem, not a runtime error.
    pub fn take_n(&self, start: usize, n: usize) -> Vec<PnNote> {
        let funded: Vec<&PnNote> =
            self.notes.iter().filter(|nt| nt.shell_funded && nt.native_funded).collect();
        assert!(
            funded.len() >= start + n,
            "pn pool has {} fully-funded notes, need {} starting from index {}",
            funded.len(),
            n,
            start,
        );
        funded[start..start + n].iter().map(|n| (*n).clone()).collect()
    }

    /// Convenience: take exactly two distinct funded notes (trader A & B).
    pub fn take_pair(&self, start: usize) -> (PnNote, PnNote) {
        let mut v = self.take_n(start, 2);
        let b = v.pop().expect("take_n returned 2");
        let a = v.pop().expect("take_n returned 2");
        (a, b)
    }
}

fn pool_path() -> PathBuf {
    if let Ok(env) = std::env::var("PN_POOL_PATH") {
        return PathBuf::from(env);
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR set during cargo test");
    let manifest = Path::new(&manifest);
    // Try dodex_sdk/ first (where mint_pn_pool lands JSON when run from
    // dodex_sdk/), then workspace root.
    for base in [manifest, &manifest.join("..")] {
        let p = base.join("pn_pool.json");
        if p.exists() {
            return p;
        }
    }
    manifest.join("pn_pool.json") // surface clear "not found" error
}

pub fn load() -> PnPool {
    let path = pool_path();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "pn pool not found at {} ({e}). Generate one first:\n  \
             cargo run --release --bin mint_pn_pool -- --count 10",
            path.display(),
        )
    });
    let pool: PnPool = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("parse {} as PnPool: {e}", path.display()));
    assert!(
        !pool.notes.is_empty(),
        "pn pool at {} has no notes — re-run mint_pn_pool",
        path.display(),
    );
    pool
}
