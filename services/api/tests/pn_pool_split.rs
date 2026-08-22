// Which rows of the shared `dex_test_notes.keys.json` this suite may use.
//
// The file is shared with the sdk e2e harness, which rents from it under its
// own bookkeeping. Nothing on either side reads the other's state, so the
// split has to hold by construction — and every e2e test here picks its note
// by a fixed index into the pool, which only means anything while the pool is
// a run of identical notes.
//
// These tests live in their own binary rather than next to the helper: 29 test
// binaries declare `mod common;`, so a `#[test]` inside `common/test_pns.rs`
// would be compiled and run 29 times over.

mod common;

use common::test_pns::api_owned_indices;
use common::test_pns::owned_indices;
use common::test_pns::TestPnPool;
use common::test_pns::API_PROFILE;
use common::test_pns::INFERENCE_PROFILE;

/// One seed row, with an optional profile — the shape the zerostate
/// generator writes.
fn row(i: usize, profile: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "pn_address": format!("0:{i:064x}"),
        "pn_pubkey_hex": format!("{:064x}", i + 1),
        "pn_seckey_hex": format!("{:064x}", i + 100),
        "pn_dih_hex": format!("0x{i:x}"),
        "tokenType": 1, "value": 1_000_000_000_000u64,
    });
    if let Some(p) = profile {
        v["profile"] = serde_json::json!(p);
    }
    v
}

fn write_pool(name: &str, profiles: &[Option<&str>]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dodex-pn-pool-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rows: Vec<_> = profiles.iter().enumerate().map(|(i, p)| row(i, *p)).collect();
    let path = dir.join("dex_test_notes.keys.json");
    std::fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
    path
}

/// Recovers the index `row` encoded into an address.
fn index_of(address: &str) -> usize {
    usize::from_str_radix(address.trim_start_matches("0:"), 16).unwrap()
}

/// The spec the e2e pipeline bakes the stand's note pool from.
const STAND_NOTES_SPEC: &str = include_str!("../../../tests/e2e/dex_test_notes.spec.json");

/// The e2e tests here index their note as `notes[k % len]` with `k` running
/// 0..=18, so nineteen notes give each test its own.
///
/// Several of those indices are paired rather than one per test: the inference
/// tests that involve two parties take a seller and a buyer each, because a
/// note that is both sides of its own deal cannot say anything about the two
/// being told apart. A deal also binds its parties to a `TokenContract` for
/// good, so those pairs are not lent out to anything else.
///
/// Fewer is not broken — the suite runs single-threaded, and notes carrying
/// the same label are interchangeable, so a shorter pool just means more tests
/// share one — but it changes which tests share, and each shared note then
/// pays for more market deploys out of the same balance. For the two-party
/// tests a short pool is worse than that: `k % len` would fold their two
/// indices onto one note and turn each back into the self-trade it exists to
/// stop being, which is why they assert the two addresses differ rather than
/// trusting the arithmetic.
const API_NOTES_FOR_ONE_EACH: u64 = 19;

#[test]
fn the_stand_spec_gives_every_test_here_a_note_of_its_own() {
    let groups: Vec<serde_json::Value> = serde_json::from_str(STAND_NOTES_SPEC).unwrap();
    let declared = groups
        .iter()
        .find(|g| g["profile"] == API_PROFILE)
        .map(|g| g["count"].as_u64().unwrap())
        .unwrap_or(0);
    assert!(
        declared >= API_NOTES_FOR_ONE_EACH,
        "the stand spec declares {declared} `{API_PROFILE}` note(s); \
         {API_NOTES_FOR_ONE_EACH} keep one per test"
    );
}

/// The inference binaries index their own note the same way, out of `PN-INF`
/// rather than `PN-API`: 6 (`e2e_inference_match`), 9 (`e2e_inference`),
/// 12 (`e2e_inference_orders`), 13 (`e2e_inference_clob`),
/// 18 (`e2e_inference_funding`), 19 and 20 (`e2e_inference_stream` — seller and
/// buyer), 21 (`e2e_inference_expiry_sweep`), 22 (`e2e_inference_range_link`);
/// 23 is spare. Twenty-four notes give each of those indices a row of its own.
///
/// Wave 5 CREATED this group — before it the spec declared no `PN-INF` at all,
/// so `TestPnPool::load_inference()` panicked on the woodpecker stand and every
/// inference binary there died on the pool rather than on anything it tests.
///
/// Unlike the `PN-API` case above, a short pool here is not merely a change of
/// who shares with whom. `e2e_inference_stream` exists to prove that a deal's
/// two sides are told apart, and `k % len` folds 19 and 20 onto one note as
/// soon as `len` divides their difference — turning the scenario back into the
/// self-trade it was written to stop being. It asserts the two addresses differ
/// rather than trusting this constant, but the constant is what keeps the
/// assertion from being the thing that fails.
const INFERENCE_NOTES_FOR_ONE_EACH: u64 = 24;

#[test]
fn the_stand_spec_gives_every_inference_test_a_note_of_its_own() {
    let groups: Vec<serde_json::Value> = serde_json::from_str(STAND_NOTES_SPEC).unwrap();
    let declared = groups
        .iter()
        .find(|g| g["profile"] == INFERENCE_PROFILE)
        .map(|g| g["count"].as_u64().unwrap())
        .unwrap_or(0);
    assert!(
        declared >= INFERENCE_NOTES_FOR_ONE_EACH,
        "the stand spec declares {declared} `{INFERENCE_PROFILE}` note(s); \
         {INFERENCE_NOTES_FOR_ONE_EACH} keep one per test"
    );
}

#[test]
fn the_stand_spec_mints_inference_notes_in_shell() {
    // The count alone is not the whole precondition. `_balance[CURRENCIES_ID_SHELL]`
    // is written only by the constructor, so a `PN-INF` group baked as NACKL
    // would pass the guard above and still fail every inference test on chain
    // with ERR_LOW_VALUE — the exact trap
    // `an_unprofiled_pool_holds_no_inference_note` guards on the loader side.
    let groups: Vec<serde_json::Value> = serde_json::from_str(STAND_NOTES_SPEC).unwrap();
    let group = groups
        .iter()
        .find(|g| g["profile"] == INFERENCE_PROFILE)
        .expect("the spec must declare a PN-INF group");
    assert_eq!(
        group["tokenType"].as_u64(),
        Some(2),
        "PN-INF must be minted as SHELL (tokenType 2), not {:?}",
        group["tokenType"]
    );
}

#[test]
fn an_unprofiled_pool_belongs_to_this_suite_whole() {
    // A pool baked by note count rather than from a spec: every note is
    // identical, the sdk harness confines itself to the tail by index, and
    // this suite indexes the whole file — which is exactly what it must keep
    // doing for any seed file that predates profiles.
    let profiles = vec![None; 10];
    assert_eq!(api_owned_indices(&profiles), (0..10).collect::<Vec<_>>());
}

#[test]
fn a_profiled_pool_belongs_to_this_suite_only_where_it_says_so() {
    let profiles = vec![Some("PN-DEP"), Some(API_PROFILE), Some("PN-TRD"), Some(API_PROFILE)];
    assert_eq!(api_owned_indices(&profiles), vec![1, 3]);
}

#[test]
fn loading_a_profiled_pool_hands_out_only_this_suites_notes() {
    // The fixed index every e2e test carries is an index into *this suite's*
    // notes, not into the file. Without that, a spec-baked pool would hand a
    // trader test a note baked for the sdk harness — same file, different
    // token type and ECC seeding.
    let path = write_pool(
        "profiled",
        &[Some("PN-DEP"), Some(API_PROFILE), Some("PN-TRD"), Some(API_PROFILE)],
    );
    let pool = TestPnPool::load_path(&path);
    assert_eq!(pool.notes.len(), 2);
    assert_eq!(index_of(&pool.notes[0].address), 1);
    assert_eq!(index_of(&pool.notes[1].address), 3);
}

#[test]
fn loading_an_unprofiled_pool_is_unchanged() {
    let path = write_pool("unprofiled", &[None, None, None]);
    let pool = TestPnPool::load_path(&path);
    assert_eq!(pool.notes.len(), 3);
    assert_eq!(index_of(&pool.first().address), 0);
}

#[test]
fn a_profiled_pool_with_no_note_for_this_suite_fails_loudly() {
    // Silently loading zero notes would surface as a panic on `notes[k % 0]`
    // — a modulo-by-zero far from the cause. The pool is the cause and the
    // message has to say so.
    let path = write_pool("foreign", &[Some("PN-DEP"), Some("PN-TRD")]);
    let err = std::panic::catch_unwind(|| TestPnPool::load_path(&path)).unwrap_err();
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains(API_PROFILE), "the message must name the label looked for: {msg}");
    assert!(msg.contains("PN-DEP"), "and what the pool holds instead: {msg}");
}

#[test]
fn an_unprofiled_pool_holds_no_inference_note() {
    // The trap this guards. An unprofiled pool is the historical
    // single-currency NACKL one, and the trader path is right to take it
    // whole — but handing the same rows to an inference test would look like
    // a working fixture and fail on chain minutes later with ERR_LOW_VALUE,
    // because `_balance[CURRENCIES_ID_SHELL]` is zero on a NACKL note and
    // nothing but the constructor ever fills it.
    let profiles = vec![None; 10];
    assert_eq!(owned_indices(&profiles, API_PROFILE), (0..10).collect::<Vec<_>>());
    assert!(
        owned_indices(&profiles, INFERENCE_PROFILE).is_empty(),
        "an unprofiled pool must not pass for a SHELL pool"
    );
}

#[test]
fn the_two_profiles_divide_a_mixed_pool() {
    // The shape CI writes: one NACKL note and one SHELL note concatenated
    // into a single file, told apart only by their label.
    let profiles = vec![Some(API_PROFILE), Some(INFERENCE_PROFILE)];
    assert_eq!(owned_indices(&profiles, API_PROFILE), vec![0]);
    assert_eq!(owned_indices(&profiles, INFERENCE_PROFILE), vec![1]);
}

#[test]
fn loading_a_mixed_pool_gives_each_suite_its_own_note() {
    let path = write_pool("mixed", &[Some(API_PROFILE), Some(INFERENCE_PROFILE)]);

    let api = TestPnPool::load_path(&path);
    assert_eq!(api.notes.len(), 1);
    assert_eq!(index_of(&api.first().address), 0);

    let inf = TestPnPool::load_path_profile(&path, INFERENCE_PROFILE);
    assert_eq!(inf.notes.len(), 1);
    assert_eq!(index_of(&inf.first().address), 1);
}

#[test]
fn asking_for_inference_notes_a_pool_lacks_says_how_to_mint_them() {
    // A stale local fixture is the likely way to meet this, and the fix is
    // not guessable from "no notes found".
    let path = write_pool("api-only", &[Some(API_PROFILE)]);
    let err = std::panic::catch_unwind(|| TestPnPool::load_path_profile(&path, INFERENCE_PROFILE))
        .unwrap_err();
    let msg = err
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(msg.contains(INFERENCE_PROFILE), "the message must name the label: {msg}");
    assert!(msg.contains("--token-type shell"), "and how to produce it: {msg}");
}
