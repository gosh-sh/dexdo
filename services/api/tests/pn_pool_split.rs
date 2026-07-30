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
use common::test_pns::TestPnPool;
use common::test_pns::API_PROFILE;

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

#[test]
fn an_unprofiled_pool_belongs_to_this_suite_whole() {
    // The `DEX_TEST_NOTES_CNT` pool CI bakes today. Every note is identical,
    // the sdk harness confines itself to the tail by index, and this suite
    // indexes the whole file — which is exactly what it must keep doing.
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
