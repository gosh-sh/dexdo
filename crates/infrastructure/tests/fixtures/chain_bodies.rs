// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Real event bodies captured from chain, one constant per event type.
//!
//! These are the bytes an indexer actually receives. Every assertion built on them
//! confirms the payload's shape by observation rather than by intention — which is
//! the whole point of the replay wave, and the reason a synthetic body must never
//! be added to this file.
//!
//! Each constant carries where it came from. A shellnet redeploy retires the message
//! id, and that is fine: the base64 is self-contained and the id is a historical
//! reference, not a dependency.
//!
//! Every body below is `rank 1` from the wave-4 harvest journal
//! (`specs/2026-08-13-wave4-harvest.md`, Task 1 of this wave): the sole or longest
//! candidate of its event type inside the fresh capture window
//! (`created_at_chain >= 2026-08-08`), with `InferenceOrderCancelRejected` captured
//! exactly on the boundary date and counted as fresh. Length is a proxy for a
//! multi-cell payload — the descent that carries the prefix offset into a
//! continuation cell is the thing a single-cell body leaves untested. All 59
//! candidates in the journal, this body included, passed decode validation against
//! the `decoded` snapshot production recorded by the decoder at capture time
//! (journal Step 7); nothing here was hand-fixed to pass.
//!
//! `TokenContract.*` constants (Task 5 of this wave) follow further down in this
//! file, in their own section: same `rank 1` / decode-validated provenance
//! discipline, with one documented exception (`TC_DISPUTE_RESOLVED`, taken from
//! before the fresh-window boundary — see its doc comment for why that is
//! still sound).

/// `InferenceOrderBook.InferenceOrderPlaced`, captured from event message
/// `1269194757f243fb77f116d629f86a7431902c5330db592a528fd1be7548dc0a` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003e8`.
///
/// Inference event ids are unique across the loaded ABIs (`decoder.rs`'s
/// `all_inference_events_resolve_uniquely_by_id` proves it), so decoding this body
/// needs no `dst` — `decode_event_body(INFERENCE_ORDER_PLACED, None)` resolves it
/// unambiguously. The `dst` above is recorded for provenance only; routing by `dst`
/// matters for `TokenContract.ContractDeployed`, which collides on event id with
/// `RootModel.ContractDeployed` — a different fixture, owned by a different task.
///
/// Chosen as the longest body of its type in the source: length is the proxy for a
/// multi-cell payload, and a multi-cell body is the one that exercises carrying the
/// prefix offset into a continuation cell — the descent a single-cell body leaves
/// untested.
pub const INFERENCE_ORDER_PLACED: &str = "te6ccgEBAwEAmgABiWDHFLoAAAAAAAAAAAAAAAAAAAIWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAFloLwAAAAAAAAAAAAAAAAAAAAACQAEBQ4Ac5gwN5iwntKJmSzeCwvw5YbuXPp1Njnr4tAFKZN5lXVACAFWAEuHiAOHYgk3WaMpg5GUKq+sYYOxSwlQqbvvsyqrpFMjAAAAADU/C54AQ";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_PLACED`] at capture
/// time (harvest journal, Task 1 Step 5) — the whole-payload comparison target used by
/// `decode_real_bodies.rs` alongside the per-field asserts.
pub const HARVESTED_ORDER_PLACED_DECODED: &str = r#"{"note":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea","flags":"0","isBuy":false,"price":"0x00000000000000000000000000000000000000000000000000000000b2d05e00","ticks":"4","orderId":"534","deadline":"1786648380","tokenContract":"0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"}"#;

/// `InferenceOrderBook.InferenceFilled`, captured from event message
/// `adad773107dc8aff53d52a54ea3b4cbbba6b27b54d9ce7dd9ab9ec7dcdb81ed6` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003eb`.
///
/// The longest body in the whole wave-4 harvest (280 base64 chars) — the strongest
/// exercise of the multi-cell descent available among the 59 harvested candidates.
pub const INFERENCE_FILLED: &str = "te6ccgEBBAEAxQABqEDU3wcAAAAAAAAAAAAAAAAAAAIUAAAAAAAAAAAAAAAAAAACFQAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAstBeAAEBQ4AKNy/qEBMEZZMyP4l90vavDVDoo+1k8DtmFrhzsaCk0TACAUOAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOwAwBDgBzmDA3mLCe0omZLN4LC/Dlhu5c+nU2Oevi0AUpk3mVdUA==";

/// The `decoded` snapshot production recorded for [`INFERENCE_FILLED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_FILLED_DECODED: &str = r#"{"ticks":"8","makerId":"532","takerId":"533","sellerTC":"0:51b97f508098232c9991fc4bee97b5786a87451f6b2781db30b5c39d8d052689","buyerNote":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","sellerNote":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea","clearingPrice":"0x00000000000000000000000000000000000000000000000000000000b2d05e00"}"#;

/// `InferenceOrderBook.InferenceExecuted`, captured from event message
/// `bc15baa85fe19129f34c615982c0da71c66dc475594ba443041af6796f497b4d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003ec`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_EXECUTED: &str = "te6ccgEBAQEARgAAiDrthbkAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAALLQXgAAAAAAAAAAAAAAAAW6RjYA";

/// The `decoded` snapshot production recorded for [`INFERENCE_EXECUTED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_EXECUTED_DECODED: &str = r#"{"cost":"24600000000","ticks":"8","clearingPrice":"0x00000000000000000000000000000000000000000000000000000000b2d05e00"}"#;

/// `InferenceOrderBook.InferenceOrderBookDeployed`, captured from event message
/// `e1cccb7ddf6314f97645b851c05588d42ab98e0eb5d1b3848607884155c35c61` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f0`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_BOOK_DEPLOYED: &str = "te6ccgEBAgEAegABiyAMXJyAEbwN6jH/wYQuEaioSLkzwR5jVeLjF2h4QprAb+JSLeGo+fL+lZsvt7p39iQit9WIIK03J92kaPEDTopdHOnkCNABAF5xd2VuLS1xd2VuMy0tMzJiLWlzc3VlMjY0LWZhaWxjbG9zZWQtMTc4NjYzNjE5NA==";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_BOOK_DEPLOYED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_BOOK_DEPLOYED_DECODED: &str = r#"{"note":"0:8de06f518ffe0c21708d454245c99e08f31aaf1718bb43c214d6037f12916f0d","modelHash":"0x47cf97f4acd97dbdd3bfb12115beac410569b93eed2347881a7452e8e74f2046","modelName":"qwen--qwen3--32b-issue264-failclosed-1786636194"}"#;

/// `InferenceOrderBook.InferenceOrderCancelled`, captured from event message
/// `bafa1344a5fb8d7ee0c2cb8dd6d760a8d9578ec2b018c940e49a7348e2201778` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003e9`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_CANCELLED: &str = "te6ccgEBAQEASAAAi32/6dQAAAAAAAAAAAAAAAAAAAIQAAAAAAAAAAAAAAAAAAAAAIAUYbHKpTOwM6xtPrpWbMqpwLOWFRu6nEsTfO3J/p02MfA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_CANCELLED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_CANCELLED_DECODED: &str = r#"{"note":"0:a30d8e55299d819d6369f5d2b366554e059cb0a8ddd4e2589be76e4ff4e9b18f","orderId":"528","refunded":"0"}"#;

/// `InferenceOrderBook.InferenceOrderCancelRejected`, captured from event message
/// `a761aaf992289802a963bd8ce8e830eee159ccedbdc20494921b9cf56599f08d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f1`.
///
/// Captured 2026-08-08, exactly on the harvest window's boundary date and counted as
/// fresh; the sole rank-1 candidate of its type. Passed decode validation against its
/// captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_CANCEL_REJECTED: &str =
    "te6ccgEBAQEAOQAAbXwo6fcAAAAAAAAAAAAAAAAAAAAGAYACxDrV+QqgA5a/I+ef3w7HEju+IBgke+I3JQITOflKTNA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_CANCEL_REJECTED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_CANCEL_REJECTED_DECODED: &str = r#"{"note":"0:1621d6afc855001cb5f91f3cfef8763891ddf100c123df11b9281099cfca5266","reason":"1","orderId":"6"}"#;

/// `InferenceOrderBook.InferenceOrderExpired`, captured from event message
/// `83ecb128592fff0feb27a66069bdad70c88ebc68965694c5300aed9ae154ad1f` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003f2`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_ORDER_EXPIRED: &str = "te6ccgEBAgEAXQABax2o6lQAAAAAAAAAAAAAAAAAAAACwAjeBvUY/+DCFwjUVCRcmeCPMarxcYu0PCFNYDfxKRbw2AEAQ4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_ORDER_EXPIRED`] at capture
/// time (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_ORDER_EXPIRED_DECODED: &str = r#"{"note":"0:8de06f518ffe0c21708d454245c99e08f31aaf1718bb43c214d6037f12916f0d","isBuy":true,"orderId":"2","tokenContract":"0:0000000000000000000000000000000000000000000000000000000000000000"}"#;

/// `InferenceOrderBook.InferenceRefunded`, captured from event message
/// `d6b8faf2ce003a53b6835e582092b0489e3ce36d916bb13e6de72300b468e119` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000003ea`.
///
/// The sole rank-1 candidate of its type inside the fresh capture window; passed
/// decode validation against its captured `decoded` snapshot (journal Step 7).
pub const INFERENCE_REFUNDED: &str = "te6ccgEBAQEASAAAizv8L8MAAAAAAAAAAAAAAAAAAAIRgBUAeKJUqULkEVwAYQ2+MI36Jt/ZHP02YXFWcN22U8nDoAAAAAAAAAAAAAAAAAAAABA=";

/// The `decoded` snapshot production recorded for [`INFERENCE_REFUNDED`] at capture time
/// (harvest journal, Task 1 Step 5).
pub const HARVESTED_INFERENCE_REFUNDED_DECODED: &str = r#"{"note":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","amount":"0","orderId":"529"}"#;

// -----------------------------------------------------------------------------
// `TokenContract.*` — owned by Task 5 of the same wave. Same provenance rules as
// the `InferenceOrderBook.*` constants above: `rank 1` of the wave-4 harvest
// journal (`specs/2026-08-13-wave4-harvest.md`), passed decode validation
// against the `decoded` snapshot recorded at capture time (journal Step 7).
//
// Every type present in the journal is here — the controller-scope ruling for
// Task 5 takes ALL `TokenContract.*` types the journal actually harvested, not
// just the brief's eleven-body minimum: the stream trio, the whole IX-TC-11
// skeleton group, `ContractDestroyed`, `ProbeBurned` (IX-TC-16), plus
// `TicksClaimed`, `StreamDisputed` and `DisputeResolved` (IX-TC-15, both dispute
// branches — `DisputeResolved` taken through the brief's pre-boundary exception,
// see its doc comment below).
//
// `ShellWithdrawn` is named in the brief's skeleton group (IX-TC-11) but is
// NOT among these constants: it is absent from the wave-4 harvest journal and
// from `/tmp/harvest.jsonl` (13 distinct `TokenContract.*` types were
// harvested, `ShellWithdrawn` is not one of them) even though the census in
// `specs/2026-08-13-wave4-replay-research.md:180` counts it as a live type
// (328 occurrences). No post-upgrade body means no fixture — the same
// "exclude from the minimum, narrow the matrix row" rule the brief spells out
// for a skeleton type that turns out not to be among the post-upgrade
// candidates. Recorded for Task 7.
//
// `dst` matters here in a way it does not for `InferenceOrderBook.*`:
// `TokenContract.ContractDeployed` and `RootModel.ContractDeployed` have
// byte-identical signatures and therefore ONE shared body id; only the external
// `dst` separates them (`decoder.rs`'s route table: 732 routes to the deal,
// 703 to the root model). Decoding without `dst` returns `AmbiguousCollision`,
// since both loaded ABIs claim that id. The `dst`
// recorded below is the gateway-encoded string exactly as `raw_events` and the
// decoder's route table use it (`:` + event id as 64 lowercase hex digits,
// `config::event_type_dst`) — passing it uniformly to every constant below is
// harmless for the non-colliding types (no route matches their dst, so they
// fall through to the ordinary unique-id lookup) and required for
// `ContractDeployed`.

/// `TokenContract.ContractDeployed`, captured from event message
/// `c89e376926ab4faf271c0cf15e1bc0ba5b4107ee740bcfe2cf59540a91de0b06` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002dc`.
///
/// `dst` is present and non-empty in the journal specifically because this is
/// the collision case: without it, `decode_event_body` cannot tell this apart
/// from `RootModel.ContractDeployed` (byte-identical body) and reports
/// `AmbiguousCollision` instead of decoding.
pub const TC_CONTRACT_DEPLOYED: &str =
    "te6ccgEBAQEAKAAAS1AxZlCAEuHiAOHYgk3WaMpg5GUKq+sYYOxSwlQqbvvsyqrpFMjQ";

/// The `decoded` snapshot production recorded for [`TC_CONTRACT_DEPLOYED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_CONTRACT_DEPLOYED_DECODED: &str =
    r#"{"self":"0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"}"#;

/// `TokenContract.ContractDestroyed`, captured from event message
/// `da8817bfa0395886b38873296fbd8e7f45ab13feff3b945214861b320ec45dc0` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002c5`.
///
/// Not part of the IX-TC-11 skeleton group but the stand produces it (1619 rows
/// in the wave-4 census) and its body is cheap — taken per the controller-scope
/// ruling.
pub const TC_CONTRACT_DESTROYED: &str =
    "te6ccgEBAQEAKAAAS1ZjMryACjcv6hATBGWTMj+JfdL2rw1Q6KPtZPA7Zha4c7GgpNEw";

/// The `decoded` snapshot production recorded for [`TC_CONTRACT_DESTROYED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_CONTRACT_DESTROYED_DECODED: &str =
    r#"{"self":"0:51b97f508098232c9991fc4bee97b5786a87451f6b2781db30b5c39d8d052689"}"#;

/// `TokenContract.BuyerBondFunded`, captured from event message
/// `ba8de2d5e3307c95dc1df08ed505c0d0fc521718d6e27f144a6cc1f7c4bf2cd2` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002dd`.
///
/// IX-TC-11 skeleton group: `BuyerBondFunded` is the two-sided counterpart of
/// `SellerBondFunded` (v4.0.35 made the bond two-sided) and carries no
/// deal-level state the SETTLEMENT read-model needs.
pub const TC_BUYER_BOND_FUNDED: &str = "te6ccgEBAQEAFgAAKFbDw2UAAAAAAAAAAAAAAAFloLwA";

/// The `decoded` snapshot production recorded for [`TC_BUYER_BOND_FUNDED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_BUYER_BOND_FUNDED_DECODED: &str = r#"{"amount":"6000000000"}"#;

/// `TokenContract.SellerBondFunded`, captured from event message
/// `3970dc6d04a58e51135ae8fcb0ee987c9b23827d2f94cf52334d8e165b29502f` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d7`.
///
/// IX-TC-11 skeleton group.
pub const TC_SELLER_BOND_FUNDED: &str = "te6ccgEBAQEAFgAAKHyekmMAAAAAAAAAAAAAAAFloLwA";

/// The `decoded` snapshot production recorded for [`TC_SELLER_BOND_FUNDED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_SELLER_BOND_FUNDED_DECODED: &str = r#"{"amount":"6000000000"}"#;

/// `TokenContract.StreamOpened`, captured from event message
/// `08d7d8b2e3ba1b61264017cb767dd7e528951a22d7b7ff1bba76a06b3a29f552` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d1`.
///
/// Named explicitly by the IX-TC-14 line: one of the stream trio.
pub const TC_STREAM_OPENED: &str =
    "te6ccgEBAQEAOAAAa1klfyOAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOgAAAAAAAAAAAAAAAWWgvAEA==";

/// The `decoded` snapshot production recorded for [`TC_STREAM_OPENED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_STREAM_OPENED_DECODED: &str = r#"{"buyer":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","pricePerTick":"3000000000"}"#;

/// `TokenContract.StreamFunded`, captured from event message
/// `b3d46bbe52aed7d333634f6243085c9f59995ed385fd4742719f2d84c3d4987a` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d0`.
///
/// Named explicitly by the IX-TC-14 line: one of the stream trio.
pub const TC_STREAM_FUNDED: &str =
    "te6ccgEBAQEAOAAAa36h2BaAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOgAAAAAAAAAAAAAAC3SMbAEA==";

/// The `decoded` snapshot production recorded for [`TC_STREAM_FUNDED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_STREAM_FUNDED_DECODED: &str = r#"{"buyer":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","deposit":"24600000000"}"#;

/// `TokenContract.StreamStopped`, captured from event message
/// `c32f88ecb844757d053a31a1247cd0bb059c44f3bb2cada92b348ef42cb94c07` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d3`.
///
/// Named explicitly by the IX-TC-14 line: one of the stream trio.
pub const TC_STREAM_STOPPED: &str = "te6ccgEBAQEASAAAi2Esa7uAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOgAAAAAAAAAAAAAABZanjwAAAAAAAAAAAAAAAAtiqskBA=";

/// The `decoded` snapshot production recorded for [`TC_STREAM_STOPPED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_STREAM_STOPPED_DECODED: &str = r#"{"buyer":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","toSeller":"12001200000","refundToBuyer":"24450000000"}"#;

/// `TokenContract.StreamDisputed`, captured from event message
/// `2e653abf67649d37c9c7889dcb512690c5e1e7988be4750f10d379d757f06f0d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d4`.
///
/// IX-TC-15 (dispute branches): the only `StreamDisputed` instance in the whole
/// wave-4 harvest window (rare event, no rank 2/3 exist). Its captured date
/// (2026-08-12) is inside the fresh window, no exception needed.
pub const TC_STREAM_DISPUTED: &str =
    "te6ccgEBAQEAMAAAWztujzGABrPAEYaYTbDpAa1Rjmv3BfDy92vy3hqAB4H5oDLpa9YgAAAADU+FgvA=";

/// The `decoded` snapshot production recorded for [`TC_STREAM_DISPUTED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_STREAM_DISPUTED_DECODED: &str = r#"{"at":"1786522647","buyer":"0:359e008c34c26d87480d6a8c735fb82f8797bb5f96f0d4003c0fcd01974b5eb1"}"#;

/// `TokenContract.DisputeResolved`, captured from event message
/// `2368f7f90c8ed7590e1644de29113084208d3fdd0e9ededffe4955308dfc42ec` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d5`.
///
/// IX-TC-15 (dispute branches), taken through the brief's pre-boundary
/// exception: its only instance in the reviewed period is dated 2026-08-05,
/// before the wave-4 fresh-window boundary (`created_at_chain >= 2026-08-08`),
/// so the main harvest query does not select it. The exception applies because
/// this body decodes under its own `event_type` with today's ABI AND today's
/// parse matches the `decoded` snapshot recorded at capture time — agreement
/// that is exactly what the boundary exists to establish: the field layout has
/// not moved since. Fetched by a separate network query (harvest journal Task
/// 1 Step 3, "the pre-boundary body"); the decision to spend it on this fixture
/// is this task's (IX-TC-15 outcome 1: both dispute-branch bodies taken).
pub const TC_DISPUTE_RESOLVED: &str =
    "te6ccgEBAQEAJwAASWX8tNkAAAAAAAAAAAAAAAFloLwAAAAAAAAAAAAAAAAAu8EvgMA=";

/// The `decoded` snapshot production recorded for [`TC_DISPUTE_RESOLVED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_DISPUTE_RESOLVED_DECODED: &str =
    r#"{"released":true,"toSeller":"6000000000","refundToBuyer":"3150000000"}"#;

/// `TokenContract.ProbeAccepted`, captured from event message
/// `271fbd2e2e84552d23551ec48abcdf9ba97a038a03b1680bcb41402e3f7a52c4` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d8`.
///
/// IX-TC-11 skeleton group: intuitively reads as a probe-branch event, but the
/// matrix line names it in the skeleton group by name — skipping it would leave
/// that line looking closed while one of its named arms stayed synthetic.
pub const TC_PROBE_ACCEPTED: &str = "te6ccgEBAQEASAAAi2iz7MKAFQB4olSpQuQRXABhDb4wjfom39kc/TZhcVZw3bZTycOgAAAAAAAAAAAAAAAWWgvAAAAAAAAAAAAAAAAAAAAAABA=";

/// The `decoded` snapshot production recorded for [`TC_PROBE_ACCEPTED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_PROBE_ACCEPTED_DECODED: &str = r#"{"buyer":"0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d","toSeller":"3000000000","bondReturned":"0"}"#;

/// `TokenContract.ProbeBurned`, captured from event message
/// `1647c58cf2fef63365915286f706b87b4cb0b8db0c7a28827cd1275ad9cba73d` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002d9`.
///
/// IX-TC-16: closes the row the matrix had marked unreachable before wave 5 —
/// the census shows 997 post-upgrade occurrences.
pub const TC_PROBE_BURNED: &str = "te6ccgEBAQEAWAAAqxKovvyAFA47IsmNodpDgPmGhOwmqUvwU1EMK3/rsd3vTsai+HkgAAAAAAAAAAAAAAAWWgvAAAAAAAAAAAAAAAAAFloLwAAAAAAAAAAAAAAAAsWq9RAQ";

/// The `decoded` snapshot production recorded for [`TC_PROBE_BURNED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_PROBE_BURNED_DECODED: &str = r#"{"buyer":"0:a071d9164c6d0ed21c07cc342761354a5f829a88615bff5d8eef7a763517c3c9","burnedBond":"3000000000","burnedProbe":"3000000000","refundToBuyer":"95250000000"}"#;

/// `TokenContract.TicksClaimed`, captured from event message
/// `17e760ca04dddb61039761c6e71f8194910c702859092083ca8e43d3b8905a54` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002da`.
///
/// Taken because a body was available and it proves something: the seller's
/// cumulative-claim high-water-mark path, distinct from the tick-finalization event.
pub const TC_TICKS_CLAIMED: &str =
    "te6ccgEBAQEAJgAASBSW5p8AAAAAAAAAAAAAAAAAD0JAAAAAAAAAAAAAAAAAAB6EgA==";

/// The `decoded` snapshot production recorded for [`TC_TICKS_CLAIMED`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_TICKS_CLAIMED_DECODED: &str = r#"{"claimed":"2000000","trusted":"1000000"}"#;

/// `TokenContract.EndpointSet`, captured from event message
/// `9a606d07236b4852297f5621e03b0855f966e2d58becc6209fa3fc6758e3a68e` on shellnet,
/// `dst_address` `:00000000000000000000000000000000000000000000000000000000000002db`.
///
/// IX-TC-11 skeleton group. The longest body in the whole wave-4 harvest in
/// absolute terms (484 base64 chars) — not chosen for that reason (length only
/// compares within one event type), but it does mean this fixture exercises the
/// multi-cell descent particularly well for a `bytes`-carrying payload.
pub const TC_ENDPOINT_SET: &str = "te6ccgECBAEAAV8AAQgqyrY7AQH+VdgwgxlmnqTihNGMkqYGtVanCw5E331ZJFJNmWeIPCtD4369zz9/bv2889i/U//F4ehOqtB5socM/YWMzjJgbdknnip/u2yYmTgFYWJTCR382kstK/6fTLzwSqK9eJb325lkdOpe2V7NefgRWNkOPdjuADkgkV1zWBqvec5RsAIB/p7preIzRGKn5T94uOMGICvd1ISm/KUj4mrB+gHxc/naqnrHieeDXFZVjHPpfLA34ROGoCSlzGXWqdiba0dtiPTmr1g5XSXz87O5gvcs8L+91FD7r162JOCKvfddmXFCjKfHJU7OAAlg9yrPV0azZhsiGc2fcKtpco4LxnHf604DAKSVjWLw3ckDgtVdlNof+ZNpc/XSXJl5P7r7D8Xa+ncuYOfxn6s30UvWR0dUADLxZh7k0GDFzOz0/rEPSlhnYOHV7q+9V5mysshvvtvQ0G6pyQ/W";

/// The `decoded` snapshot production recorded for [`TC_ENDPOINT_SET`] at
/// capture time (harvest journal, Task 1 Step 5).
pub const HARVESTED_TC_ENDPOINT_SET_DECODED: &str = r#"{"endpointCipher":"55d8308319669ea4e284d18c92a606b556a70b0e44df7d5924524d9967883c2b43e37ebdcf3f7f6efdbcf3d8bf53ffc5e1e84eaad079b2870cfd858cce32606dd9279e2a7fbb6c98993805616253091dfcda4b2d2bfe9f4cbcf04aa2bd7896f7db996474ea5ed95ecd79f81158d90e3dd8ee003920915d73581aaf79ce51b09ee9ade2334462a7e53f78b8e306202bddd484a6fca523e26ac1fa01f173f9daaa7ac789e7835c56558c73e97cb037e11386a024a5cc65d6a9d89b6b476d88f4e6af58395d25f3f3b3b982f72cf0bfbdd450fbaf5eb624e08abdf75d9971428ca7c7254ece000960f72acf5746b3661b2219cd9f70ab69728e0bc671dfeb4e958d62f0ddc90382d55d94da1ff9936973f5d25c99793fbafb0fc5dafa772e60e7f19fab37d14bd64747540032f1661ee4d060c5ccecf4feb10f4a586760e1d5eeafbd5799b2b2c86fbedbd0d06ea9c90fd6"}"#;

// -----------------------------------------------------------------------------
// IX-REC-07 (Task 6 of the same wave) — an account BOC, not an event body. The
// constants above are message bodies decoded by `Decoder::decode_event_body`;
// this one is a whole account's persistent state, replayed through
// `tvm_runner::run_getter` to exercise the production getter runner end to end
// (expire-header pinning, ext-out tag acceptance, output decoding — see
// `getters_against_real_boc.rs`'s module doc for why a mock cannot cover any of
// that). Same journal, different step (Task 1 wave-4 plan, Step 4).

/// Account state of `InferenceOrderBook` at
/// `0:36f374222b9a639690ce34363bd8f498d6be38fd19b04a51bc1dc39fbc7c6528`,
/// shellnet, taken 2026-08-13.
///
/// Address, network and date live HERE and not only in the harvest log: the log is not
/// committed, so without them the repository would hold a BOC no one can trace back to an
/// account. The getter snapshot stays in the log — it is bulky and not needed to identify
/// the fixture.
///
/// A snapshot, not a truth: the live account moves on. That is fine — this proves the
/// SHAPE of what the getters return for real account bytes, and shape does not drift
/// with balance.
///
/// Chosen DRY on purpose (`getWeeklyMedianPrice` reverts `ERR_NO_LIQUIDITY` = 334): the
/// alternative — a book with live weekly-median buckets — carries a live mine, because
/// `run_getter` feeds TVM the CURRENT wall-clock time
/// (`sci.unix_time = now_unixtime()`, `tvm_runner.rs`) and `_weeklyMedian` only counts
/// buckets inside a rolling weekly window. A frozen BOC's buckets eventually age out of
/// that window, so an assertion on a stored median value would go red on a calendar
/// timer, not on a regression. A dry book carries no such mine: it has no buckets to age
/// out, and the typed 334 revert is stable forever. Found on the second address tried
/// (of 2695 candidates) — see the harvest journal's Step 4 for the untried-candidate
/// tally and why an exhaustive sweep was not needed once one dry candidate passed all
/// seven getters.
pub const INFERENCE_BOOK_ACCOUNT_BOC: &str = "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";

/// `getOrder` reply for an id the book does not hold (`id = "0"`, `nextOrderId` on this
/// book starts at `1`, so `0` was never assigned). Transcribed verbatim from the harvest
/// log's getter snapshot (Task 1 wave-4 plan, Step 4) — the zero forms here are exactly
/// what `Detokenizer` renders for a Solidity default-constructed `Order`, and some of
/// them are not the "obvious" zero: `address` fields default to the EMPTY STRING (`""`),
/// not `"0:00...00"`, while the `uint256` `price` field defaults to `"0x00...00"` (64 hex
/// zeros after `0x`). Getting either wrong is exactly the kind of drift IX-REC-07 exists
/// to catch, so this is copied from the recorded run, not reconstructed by hand.
pub const GET_ORDER_EMPTY_SNAPSHOT: &str = r#"{"note":"","tokenContract":"","price":"0x0000000000000000000000000000000000000000000000000000000000000000","amount":"0","escrow":"0","deadline":"0","flags":"0","isBuy":false,"ts":"0"}"#;
