// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! The decoder against real chain bodies.
//!
//! `crates/infrastructure/src/decoder.rs` already proves the decoder on one real
//! `OrderBook.*` body (plus a real non-ABI body proving the unknown-id path). Nothing proved it on an inference body, because until this
//! wave the repository held none: `capture.rs` reused the same prediction-side
//! `OrderPlaced` fixture. So the inference payload layout was asserted by intention.
//!
//! No database and no chain: `decode_event_body` is a pure function of the bytes.
//!
//! One test per `InferenceOrderBook.*` type harvested in wave 4 (see
//! `specs/2026-08-13-wave4-harvest.md`, Task 1). The mandatory `InferenceOrderPlaced`
//! body gets per-field asserts plus a whole-payload compare; every other type gets
//! `event_type` plus the whole-payload compare, which is enough to say "no field
//! moved" without re-deriving each field name from the ABI by hand.
//! `TokenContract.*` bodies are a different task's fixtures and are not decoded here.

mod fixtures;

use dodex_infrastructure::decoder::Decoder;
use fixtures::chain_bodies::HARVESTED_INFERENCE_EXECUTED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_FILLED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_ORDER_BOOK_DEPLOYED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_ORDER_CANCELLED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_ORDER_CANCEL_REJECTED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_ORDER_EXPIRED_DECODED;
use fixtures::chain_bodies::HARVESTED_INFERENCE_REFUNDED_DECODED;
use fixtures::chain_bodies::HARVESTED_ORDER_PLACED_DECODED;
use fixtures::chain_bodies::INFERENCE_EXECUTED;
use fixtures::chain_bodies::INFERENCE_FILLED;
use fixtures::chain_bodies::INFERENCE_ORDER_BOOK_DEPLOYED;
use fixtures::chain_bodies::INFERENCE_ORDER_CANCELLED;
use fixtures::chain_bodies::INFERENCE_ORDER_CANCEL_REJECTED;
use fixtures::chain_bodies::INFERENCE_ORDER_EXPIRED;
use fixtures::chain_bodies::INFERENCE_ORDER_PLACED;
use fixtures::chain_bodies::INFERENCE_REFUNDED;

#[test]
fn decodes_a_real_inference_order_placed() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_ORDER_PLACED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderPlaced");
    // Field assertions come from the `decoded` snapshot recorded in the harvest log
    // (Task 1, Step 5) — the JSON production stored at capture time. NOT from this
    // test's own first run: taking them from the output under test would assert that
    // the decoder agrees with itself, and a regression that keeps the body decodable
    // and the event_type right while shifting a field would stay green.
    assert_eq!(
        decoded.value["note"],
        "0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea"
    );
    assert_eq!(decoded.value["flags"], "0");
    assert_eq!(decoded.value["isBuy"], false);
    assert_eq!(
        decoded.value["price"],
        "0x00000000000000000000000000000000000000000000000000000000b2d05e00"
    );
    assert_eq!(decoded.value["ticks"], "4");
    assert_eq!(decoded.value["orderId"], "534");
    assert_eq!(decoded.value["deadline"], "1786648380");
    assert_eq!(
        decoded.value["tokenContract"],
        "0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"
    );

    // The whole payload against what production stored. Field-by-field asserts say
    // which field moved; this one says that none did.
    let expected: serde_json::Value = serde_json::from_str(HARVESTED_ORDER_PLACED_DECODED)
        .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_filled() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_FILLED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceFilled");
    let expected: serde_json::Value = serde_json::from_str(HARVESTED_INFERENCE_FILLED_DECODED)
        .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_executed() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_EXECUTED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceExecuted");
    let expected: serde_json::Value = serde_json::from_str(HARVESTED_INFERENCE_EXECUTED_DECODED)
        .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_order_book_deployed() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_ORDER_BOOK_DEPLOYED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderBookDeployed");
    let expected: serde_json::Value =
        serde_json::from_str(HARVESTED_INFERENCE_ORDER_BOOK_DEPLOYED_DECODED)
            .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_order_cancelled() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_ORDER_CANCELLED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderCancelled");
    let expected: serde_json::Value =
        serde_json::from_str(HARVESTED_INFERENCE_ORDER_CANCELLED_DECODED)
            .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_order_cancel_rejected() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_ORDER_CANCEL_REJECTED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderCancelRejected");
    let expected: serde_json::Value =
        serde_json::from_str(HARVESTED_INFERENCE_ORDER_CANCEL_REJECTED_DECODED)
            .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_order_expired() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_ORDER_EXPIRED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderExpired");
    let expected: serde_json::Value =
        serde_json::from_str(HARVESTED_INFERENCE_ORDER_EXPIRED_DECODED)
            .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}

#[test]
fn decodes_a_real_inference_refunded() {
    let decoder = Decoder::new().expect("decoder");
    let decoded = decoder
        .decode_event_body(INFERENCE_REFUNDED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");

    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceRefunded");
    let expected: serde_json::Value = serde_json::from_str(HARVESTED_INFERENCE_REFUNDED_DECODED)
        .expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
}
