// Fixture types for `tests/fixtures/test_pns.json`. Shared between
// every e2e test that talks to shellnet — both POST and DELETE flows
// take a `TestPn` and feed `owner_secret_key_hex` into the chain
// signer. Format is intentionally narrow: just the fields the tests
// actually need.
//
// SECURITY: `test_pns.json` ships plaintext `owner_secret_key_hex` for
// shellnet-only throwaway PNs. Safe ONLY because the PNs hold test
// NACKL and the network is a public devnet. Never repurpose this for
// stage/prod — see the SECURITY NOTE block in `e2e_order.rs`.

#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TestPnPool {
    pub notes: Vec<TestPn>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TestPn {
    pub address: String,
    pub deposit_identifier_hash: String,
    pub owner_public_key_hex: String,
    pub owner_secret_key_hex: String,
    #[serde(default)]
    pub shell_funded: bool,
    #[serde(default)]
    pub native_funded: bool,
}

impl TestPnPool {
    /// Load `tests/fixtures/test_pns.json` from the workspace root.
    /// Panics with a clear message if the file is missing — none of
    /// the e2e tests can do anything useful without it, so a panic is
    /// the right shape rather than a `Result` no caller would handle.
    pub fn load() -> Self {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let path = format!("{manifest}/../../tests/fixtures/test_pns.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read test_pns.json at {path}: {err}"));
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("parse test_pns.json: {err}"))
    }

    pub fn first(&self) -> &TestPn {
        self.notes.first().expect("test_pns.json: at least one PN")
    }
}
