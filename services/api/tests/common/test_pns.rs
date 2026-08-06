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
    /// The label the zerostate generator's `DEX_TEST_NOTES_SPEC` gave this
    /// note's group. Absent from a pool of identical notes, and `null` on
    /// rows the generator wrote without a spec.
    #[serde(default)]
    profile: Option<String>,
}

/// The label marking a note the trader-path tests own. Groups carrying any
/// other label belong to the sdk e2e harness, which reads the same file
/// (`sdk/tests/integration/common/allocator.rs`), or to another profile here.
pub const API_PROFILE: &str = "PN-API";

/// The label marking a note the inference tests own.
///
/// These need a SHELL note, not a NACKL one, and the difference is not a
/// matter of topping one up. An inference buy is paid out of the note's own
/// ledger — `require(_balance[CURRENCIES_ID_SHELL] >= escrow)` in
/// `PrivateNote.placeInferenceBuy` — and the only thing that ever writes that
/// ledger from nothing is the constructor, `_balance[tokenType] = value`. So
/// the deposit currency at deploy time decides, for good, whether a note can
/// trade inference at all. Physical ECC SHELL sitting on the account does not
/// count: the escrow used to leave the account and stopped doing so.
pub const INFERENCE_PROFILE: &str = "PN-INF";

/// Which rows of the pool a suite may use, given each row's declared profile.
///
/// A pool of identical notes (`DEX_TEST_NOTES_CNT`) declares nothing, and the
/// two suites divide it by position: the sdk harness confines itself to the
/// last few entries and this suite indexes the whole file. Once the pool is
/// baked from a spec, position means nothing — groups differ in token type,
/// balance and ECC seeding — so ownership follows the label instead.
///
/// Only [`API_PROFILE`] falls back to "all rows" on an unlabelled pool. Such a
/// pool is the historical single-currency NACKL one, which is what the trader
/// path wants and is precisely what an inference test must never be handed
/// silently — it would deploy, place, and fail on chain with `ERR_LOW_VALUE`
/// several minutes later, naming nothing.
pub fn owned_indices(profiles: &[Option<&str>], want: &str) -> Vec<usize> {
    if profiles.iter().all(Option::is_none) {
        return if want == API_PROFILE { (0..profiles.len()).collect() } else { Vec::new() };
    }
    profiles.iter().enumerate().filter(|(_, p)| **p == Some(want)).map(|(i, _)| i).collect()
}

/// [`owned_indices`] for the trader-path profile.
pub fn api_owned_indices(profiles: &[Option<&str>]) -> Vec<usize> {
    owned_indices(profiles, API_PROFILE)
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
        Self::load_profile(API_PROFILE)
    }

    /// The pool of SHELL notes the inference tests own. See
    /// [`INFERENCE_PROFILE`] for why these cannot be the same notes.
    pub fn load_inference() -> Self {
        Self::load_profile(INFERENCE_PROFILE)
    }

    fn load_profile(want: &str) -> Self {
        let path = std::env::var("E2E_SEED_NOTES").unwrap_or_else(|_| {
            format!("{}/../../tests/fixtures/seed_notes.json", env!("CARGO_MANIFEST_DIR"))
        });
        Self::load_path_profile(std::path::Path::new(&path), want)
    }

    /// [`TestPnPool::load`] against an explicit path.
    pub fn load_path(path: &std::path::Path) -> Self {
        Self::load_path_profile(path, API_PROFILE)
    }

    /// [`TestPnPool::load`] against an explicit path, holding only the notes
    /// `want` owns (see [`owned_indices`]) in file order. Split out so the
    /// ownership rule is testable without the env var every e2e test depends
    /// on.
    pub fn load_path_profile(path: &std::path::Path, want: &str) -> Self {
        let path = path.display();
        let raw = std::fs::read_to_string(path.to_string())
            .unwrap_or_else(|err| panic!("read e2e seed notes at {path}: {err}"));
        let rows: Vec<SeedNote> = serde_json::from_str(&raw)
            .unwrap_or_else(|err| panic!("parse e2e seed notes {path}: {err}"));

        let profiles: Vec<Option<&str>> = rows.iter().map(|r| r.profile.as_deref()).collect();
        let owned = owned_indices(&profiles, want);
        if owned.is_empty() {
            // Returning an empty pool would surface much later as a modulo by
            // zero inside whichever test indexed it first, naming nothing.
            let mut census: Vec<&str> =
                profiles.iter().map(|p| p.unwrap_or("<unprofiled>")).collect();
            census.sort_unstable();
            census.dedup();
            panic!(
                "e2e seed notes {path} hold no `{want}` note — this suite owns none of the {} \
                 row(s), which carry: {}. A pool that declares no profile at all is the \
                 historical single-currency NACKL one and cannot serve `{INFERENCE_PROFILE}`: \
                 mint those notes with `--token-type shell --profile {INFERENCE_PROFILE}`",
                rows.len(),
                census.join(", ")
            );
        }

        let notes = owned
            .into_iter()
            .map(|i| &rows[i])
            .map(|n| TestPn {
                address: n.pn_address.clone(),
                deposit_identifier_hash: hex_to_dec(&n.pn_dih_hex),
                owner_public_key_hex: n.pn_pubkey_hex.clone(),
                owner_secret_key_hex: n.pn_seckey_hex.clone(),
                // The seeder mints these funded; the seed_notes format
                // does not carry funding flags, so assume funded.
                shell_funded: true,
                native_funded: true,
            })
            .collect();
        Self { notes }
    }

    /// The single deployer-PN every e2e test shares. The suite is
    /// single-threaded (see tests/fixtures/README.md), so one note never
    /// contends on its `_busy` lock.
    pub fn first(&self) -> &TestPn {
        self.notes.first().expect("e2e seed notes: at least one PN")
    }
}

/// Hex uint256 (optionally `0x`-prefixed) → decimal string.
fn hex_to_dec(s: &str) -> String {
    let h = s.strip_prefix("0x").unwrap_or(s);
    BigUint::parse_bytes(h.as_bytes(), 16)
        .unwrap_or_else(|| panic!("pn_dih_hex is not valid hex: {s}"))
        .to_str_radix(10)
}
