// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Cross-checks that `tvm_hash::stake_hash` produces the same key the
// chain itself uses for `PrivateNote._stakes`. Independent of every
// other off-chain path: this loads a real captured PN account BOC,
// invokes the `_stakes` getter through `tvm_runner::run_getter`, and
// asserts the off-chain hash appears among the chain-built map keys.
//
// The fixture lives at `tests/fixtures/pn_with_stake.json` and has
// shape:
//
//   {
//     "boc_base64":       "<base64 of PN account BOC>",
//     "event_id":         "<uint256 literal — decimal, or 0x-prefixed hex>",
//     "oracle_list_hash": "<uint256 literal — decimal, or 0x-prefixed hex>",
//     "token_type":       <u32 integer>
//   }
//
// Capture procedure: deploy a PrivateNote on shellnet/testnet, register
// a stake with the listed `(event_id, oracle_list_hash, token_type)`
// triple, fetch the PN account through GraphQL
// (`accounts(...) { boc }` already returns base64), and store the four
// fields in the JSON above. PN state is public on-chain — the fixture
// carries no secret.
//
// When the fixture is absent the test prints an explanatory note and
// passes, so workspace builds on machines that have not captured a PN
// stay green. CI without the fixture provides no cross-check coverage.

use std::path::PathBuf;

use num_bigint::BigUint;
use serde::Deserialize;
use serde_json::json;
use tvm_abi::Contract;

const PN_ABI: &str = include_str!("../../../contracts/dex/PrivateNote.abi.json");

#[derive(Deserialize)]
struct Fixture {
    boc_base64: String,
    event_id: String,
    oracle_list_hash: String,
    token_type: u32,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pn_with_stake.json")
}

fn parse_uint256_literal(s: &str, name: &str) -> BigUint {
    let parsed = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        BigUint::parse_bytes(hex.as_bytes(), 16)
    } else {
        BigUint::parse_bytes(s.as_bytes(), 10)
    };
    parsed.unwrap_or_else(|| {
        panic!("{name} must be a uint256 literal (decimal, or 0x-prefixed hex); got: {s}")
    })
}

fn load_fixture() -> Option<Fixture> {
    let path = fixture_path();
    if !path.exists() {
        eprintln!(
            "stake_hash_onchain_xcheck: fixture not present at {} — skipping.\n  \
             Capture a PrivateNote BOC with one known stake and write \
             {{boc_base64, event_id, oracle_list_hash, token_type}} as JSON to that path. \
             See file header for details.",
            path.display(),
        );
        return None;
    }
    let bytes = std::fs::read(&path).expect("read fixture file");
    Some(serde_json::from_slice(&bytes).expect("parse fixture JSON"))
}

#[test]
fn stake_hash_matches_on_chain_stakes_key() {
    let Some(fix) = load_fixture() else { return };

    let event_id = parse_uint256_literal(&fix.event_id, "event_id");
    let oracle_list_hash = parse_uint256_literal(&fix.oracle_list_hash, "oracle_list_hash");

    let expected_key =
        dodex_infrastructure::tvm_hash::stake_hash(&event_id, &oracle_list_hash, fix.token_type)
            .expect("off-chain stake_hash compute");

    let contract = Contract::load(std::io::Cursor::new(PN_ABI)).expect("load PrivateNote ABI");
    let reply = dodex_infrastructure::tvm_runner::run_getter(
        &contract,
        &fix.boc_base64,
        "_stakes",
        &json!({}),
    )
    .expect("run_getter _stakes");

    let map = reply
        .get("_stakes")
        .and_then(serde_json::Value::as_object)
        .expect("`_stakes` object in run_getter reply");

    assert!(
        !map.is_empty(),
        "fixture PN has an empty `_stakes` map — the cross-check needs at least one registered stake"
    );

    assert!(
        map.contains_key(&expected_key),
        "off-chain stake_hash(event_id={event_id}, oracle_list_hash={oracle_list_hash}, token_type={tt}) \
         = {expected_key} is not among on-chain `_stakes` keys: {keys:?}. \
         Either the fixture triple does not match what was registered on chain, \
         or tvm_abi packing has drifted vs Solidity abi.encode on the chain.",
        tt = fix.token_type,
        keys = map.keys().collect::<Vec<_>>(),
    );
}
