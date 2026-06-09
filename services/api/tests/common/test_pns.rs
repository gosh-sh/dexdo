// Loads the e2e Private-Note pool from a seed_notes-format JSON file —
// the same format the api seeder reads (`NoteEntry`). Shared by every
// e2e test that talks to shellnet: they feed `owner_secret_key_hex` into
// the chain signer and `deposit_identifier_hash` into read-model lookups.
//
// SECURITY: the file carries plaintext secret keys for shellnet-only
// throwaway PNs. Safe ONLY because the PNs hold test NACKL on a public
// devnet. The file is git-ignored and provided out of band (CI fetches it
// from S3 to the path below); never commit it. See tests/fixtures/README.md.

#![allow(dead_code)]

use num_bigint::BigUint;
use serde::Deserialize;

/// On-disk row: the seed_notes format the api seeder reads. `tokenType`
/// and `value` are present in the file but not needed here, so they are
/// left undeserialized.
#[derive(Debug, Deserialize)]
struct SeedNote {
    pn_address: String,
    pn_pubkey_hex: String,
    pn_seckey_hex: String,
    pn_dih_hex: String,
}

#[derive(Debug)]
pub struct TestPnPool {
    pub notes: Vec<TestPn>,
}

/// The shape the e2e helpers consume. Field names are kept stable so
/// `e2e_setup` / `deploy_market` / `cleanup` need no change when the
/// on-disk format does — `load` maps and normalises the file into it.
#[derive(Debug, Clone)]
pub struct TestPn {
    pub address: String,
    /// Decimal uint256 — the form read-model queries and the chain APIs
    /// expect. Converted from the file's hex `pn_dih_hex`.
    pub deposit_identifier_hash: String,
    pub owner_public_key_hex: String,
    pub owner_secret_key_hex: String,
    pub shell_funded: bool,
    pub native_funded: bool,
}

impl TestPnPool {
    /// Load the e2e notes file: `E2E_SEED_NOTES` if set, otherwise
    /// `tests/fixtures/seed_notes.json` under the workspace root — where
    /// CI drops the file it fetches from S3. Panics with a clear message
    /// if it is missing or malformed; no e2e test can run without it.
    pub fn load() -> Self {
        let path = std::env::var("E2E_SEED_NOTES").unwrap_or_else(|_| {
            format!("{}/../../tests/fixtures/seed_notes.json", env!("CARGO_MANIFEST_DIR"))
        });
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read e2e seed notes at {path}: {err}"));
        let rows: Vec<SeedNote> = serde_json::from_str(&raw)
            .unwrap_or_else(|err| panic!("parse e2e seed notes {path}: {err}"));
        let notes = rows
            .into_iter()
            .map(|n| TestPn {
                address: n.pn_address,
                deposit_identifier_hash: hex_to_dec(&n.pn_dih_hex),
                owner_public_key_hex: n.pn_pubkey_hex,
                owner_secret_key_hex: n.pn_seckey_hex,
                // The seeder mints these funded; the seed_notes format
                // does not carry funding flags, so assume funded.
                shell_funded: true,
                native_funded: true,
            })
            .collect();
        Self { notes }
    }

    pub fn first(&self) -> &TestPn {
        self.notes.first().expect("e2e seed notes: at least one PN")
    }

    /// Return PN at slot `idx` (zero-based). Each e2e test claims a
    /// distinct slot so a parallel run does not contend on the same PN's
    /// `_busy` lock — every chain op serialises through it.
    pub fn slot(&self, idx: usize) -> &TestPn {
        self.notes.get(idx).unwrap_or_else(|| {
            panic!(
                "e2e seed notes: PN slot {idx} requested, only {} available — top up the pool",
                self.notes.len()
            )
        })
    }
}

/// Hex uint256 (optionally `0x`-prefixed) → decimal string.
fn hex_to_dec(s: &str) -> String {
    let h = s.strip_prefix("0x").unwrap_or(s);
    BigUint::parse_bytes(h.as_bytes(), 16)
        .unwrap_or_else(|| panic!("pn_dih_hex is not valid hex: {s}"))
        .to_str_radix(10)
}
