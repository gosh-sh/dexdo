// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Inference-book getters run against a real account BOC, offline.
//!
//! IX-REC-07. Until this file every getter in the reconciler was mocked: the seam is
//! the `OrderBookGetter` trait, and tests inject a fake so the orchestration (gates,
//! sweep, ERR_NO_LIQUIDITY → NULL) is deterministic. Nothing ran the production
//! implementation against real account bytes.
//!
//! The production implementation is `tvm_runner::run_getter`, and it does three things
//! a mock cannot exercise: it pins the `expire` header (the ABI default is `u32::MAX`,
//! which replay protection rejects on any real account with TVM throw 401), it accepts
//! the reply under either ext-out header tag (`ExtOutMsgInfo` / `ExtOutMsgInfoV2`), and
//! it decodes the output. All three are invisible on synthetic input.
//!
//! Not covered here: `DecoderGetter::call` itself (two lines; no public constructor,
//! and `KIND` is private), and `GraphqlClient::fetch_account_boc` (covered by
//! `account_boc.rs` against a mock gateway). No database and no chain — `run_getter` is
//! a pure function of the account bytes.
//!
//! Every `Ok` assertion below (field-by-field and whole-object) is taken from the
//! getter snapshot recorded in the wave-4 harvest journal at BOC-capture time
//! (`specs/2026-08-13-wave4-harvest.md`, Task 1 Step 4), not from this test's own first
//! run. That is a deliberate exception to the "assert from an independent source, not
//! from the first green run" rule that governs the body-decoding tests in this same
//! wave (`decode_real_bodies.rs`): there the assert is on ABI
//! parsing, which has an independent source (the ABI/contract source and a second
//! decoder). Here the assert is on the STATE of one specific account at one specific
//! moment — there is no independent source for that, only the recorded run. The
//! snapshot's provenance (address, network, date) lives with the BOC constant in
//! `fixtures/chain_bodies.rs`.
//!
//! The book behind [`INFERENCE_BOOK_ACCOUNT_BOC`] is DRY: `getStats` reports
//! `orderCount = 0`. The brief's optional second `getOrder` assert (reading a LIVE
//! order id from the snapshot, to prove the getter can read a filled slot and not only
//! a zero one) does not apply here — there is no live order id in this snapshot to
//! read, and inventing one would not be a transcription. `getOrder` is exercised on the
//! empty tuple only.

mod fixtures;

use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::tvm_runner::run_getter;
use dodex_infrastructure::tvm_runner::tvm_exit_code;
use fixtures::chain_bodies::GET_ORDER_EMPTY_SNAPSHOT;
use fixtures::chain_bodies::INFERENCE_BOOK_ACCOUNT_BOC;
use serde_json::json;
use serde_json::Value;

/// Book contract on a dry book reverts with this; the reconciler treats it as a
/// successful "no reference price yet" rather than a failure
/// (`inference_reconciler.rs:35` and `:359`).
const ERR_NO_LIQUIDITY: i32 = 334;

/// Exactly what `DecoderGetter::call` does (`inference_reconciler.rs:75-81`), minus the
/// wrapper's single "abi missing" arm: take the contract from the decoder, run the
/// getter against the account bytes.
fn getter(name: &str, input: Value) -> anyhow::Result<Value> {
    let decoder = Decoder::new().expect("decoder");
    let contract = decoder.contract("InferenceOrderBook").expect("InferenceOrderBook abi");
    run_getter(contract, INFERENCE_BOOK_ACCOUNT_BOC, name, &input)
}

#[test]
fn get_stats_answers_against_a_real_account_boc() {
    let out = getter("getStats", json!({})).expect("getStats must run against real bytes");
    // Field assertions from the getter snapshot recorded in the harvest log (Task 1
    // wave-4 plan, Step 4): a dry book, one order slot ever allocated (`nextOrderId`
    // = "1") and none currently open.
    assert_eq!(out["nextOrderId"], "1");
    assert_eq!(out["orderCount"], "0");
    assert_eq!(out["executedNotional"], "0");
    assert_eq!(out["executedTicks"], "0");

    // The whole object against the same snapshot. Field-by-field asserts say which
    // field moved; this one says that none did.
    let expected = json!({
        "nextOrderId": "1",
        "orderCount": "0",
        "executedNotional": "0",
        "executedTicks": "0",
    });
    assert_eq!(out, expected, "getStats shape drifted: {out}");
}

#[test]
fn get_params_answers_against_a_real_account_boc() {
    let out = getter("getParams", json!({})).expect("getParams must run against real bytes");
    // model_hash and platform_fee_bps are the two fields the reconciler reads out of
    // getParams (IX-REC-01) — asserting both, not just that the call succeeded.
    assert_eq!(
        out["modelHash"],
        "0xb99e54823e2a846e45b861a5fee75fd98ca1817ad8dc9e68e8b4b32056d89bfc"
    );
    assert_eq!(out["platformFeeBps"], "250");

    let expected = json!({
        "modelHash": "0xb99e54823e2a846e45b861a5fee75fd98ca1817ad8dc9e68e8b4b32056d89bfc",
        "platformFeeBps": "250",
    });
    assert_eq!(out, expected, "getParams shape drifted: {out}");
}

#[test]
fn get_model_name_answers_against_a_real_account_boc() {
    let out = getter("getModelName", json!({})).expect("getModelName must run against real bytes");
    // Feeds identity (IX-REC-02): forgotten in the first edition of this plan.
    assert_eq!(out["value0"], "adv--identity-l1--1786588200945294283");

    let expected = json!({ "value0": "adv--identity-l1--1786588200945294283" });
    assert_eq!(out, expected, "getModelName shape drifted: {out}");
}

#[test]
fn get_version_answers_against_a_real_account_boc() {
    let out = getter("getVersion", json!({})).expect("getVersion must run against real bytes");
    // Feeds supersede (IX-REC-03): forgotten in the first edition of this plan.
    assert_eq!(out["value0"], "4.0.35");
    assert_eq!(out["value1"], "InferenceOrderBook");

    let expected = json!({ "value0": "4.0.35", "value1": "InferenceOrderBook" });
    assert_eq!(out, expected, "getVersion shape drifted: {out}");
}

#[test]
fn get_queue_size_answers_against_a_real_account_boc() {
    let out = getter("getQueueSize", json!({})).expect("getQueueSize must run against real bytes");
    assert_eq!(out["value0"], "0");

    let expected = json!({ "value0": "0" });
    assert_eq!(out, expected, "getQueueSize shape drifted: {out}");
}

#[test]
fn get_order_returns_a_zero_tuple_for_an_id_the_book_does_not_hold() {
    // Not a revert: `_orders[id]` on a missing key yields the default-constructed
    // Order, so the getter answers with zeros. A test expecting a revert here would
    // go red on a correct contract — and, worse, invite narrowing IX-REC-07 for the
    // wrong reason.
    let out = getter("getOrder", json!({ "id": "0" })).expect("getOrder must run");
    // The WHOLE object, not two fields of it. `getOrder` returns nine
    // (note, tokenContract, price, amount, escrow, deadline, flags, isBuy, ts), and
    // IX-REC-07 is a claim about the output's SHAPE — a missing or mis-decoded field
    // is exactly what it exists to catch. Checking `amount` and `note` alone would
    // leave seven of the nine unproven while the row reads as closed.
    let expected: Value =
        serde_json::from_str(GET_ORDER_EMPTY_SNAPSHOT).expect("snapshot from the harvest log");
    assert_eq!(out, expected, "empty-order shape drifted: {out}");
}

#[test]
fn weekly_median_price_reverts_typed_on_a_dry_book() {
    // A typed revert is the success path here — IX-REC-04. Asserting `is_err()`
    // alone would pass for a broken getter too, which is the whole distinction
    // `real_getter_failure_surfaces_as_err_not_silent_null` exists to keep.
    match getter("getWeeklyMedianPrice", json!({})) {
        Ok(v) => panic!("a dry book must revert, got {v}"),
        Err(e) => assert_eq!(tvm_exit_code(&e), Some(ERR_NO_LIQUIDITY), "wrong revert: {e:?}"),
    }
}
