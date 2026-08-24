// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the TokenContract.* SETTLEMENT projector.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors::project_event;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

mod fixtures;

use fixtures::chain_bodies::HARVESTED_TC_BUYER_BOND_FUNDED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_CONTRACT_DEPLOYED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_CONTRACT_DESTROYED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_DISPUTE_RESOLVED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_ENDPOINT_SET_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_PROBE_ACCEPTED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_PROBE_BURNED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_SELLER_BOND_FUNDED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_STREAM_DISPUTED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_STREAM_FUNDED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_STREAM_OPENED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_STREAM_STOPPED_DECODED;
use fixtures::chain_bodies::HARVESTED_TC_TICKS_CLAIMED_DECODED;
use fixtures::chain_bodies::TC_BUYER_BOND_FUNDED;
use fixtures::chain_bodies::TC_CONTRACT_DEPLOYED;
use fixtures::chain_bodies::TC_CONTRACT_DESTROYED;
use fixtures::chain_bodies::TC_DISPUTE_RESOLVED;
use fixtures::chain_bodies::TC_ENDPOINT_SET;
use fixtures::chain_bodies::TC_PROBE_ACCEPTED;
use fixtures::chain_bodies::TC_PROBE_BURNED;
use fixtures::chain_bodies::TC_SELLER_BOND_FUNDED;
use fixtures::chain_bodies::TC_STREAM_DISPUTED;
use fixtures::chain_bodies::TC_STREAM_FUNDED;
use fixtures::chain_bodies::TC_STREAM_OPENED;
use fixtures::chain_bodies::TC_STREAM_STOPPED;
use fixtures::chain_bodies::TC_TICKS_CLAIMED;

fn ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "TokenContract",
        event_name: event_name.to_string(),
        event_type: format!("TokenContract.{event_name}"),
        value,
    }
}

/// Build an InferenceOrderBook DecodedEvent (the `ev` helper hardcodes TokenContract).
fn iob_ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "InferenceOrderBook",
        event_name: event_name.to_string(),
        event_type: format!("InferenceOrderBook.{event_name}"),
        value,
    }
}

/// Build a `PrivateNote.InferenceDealClosed` event (the `ev` helper hardcodes
/// TokenContract, and this one is emitted by the NOTE).
fn pn_deal_closed(deal: &str) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "PrivateNote",
        event_name: "InferenceDealClosed".to_string(),
        event_type: "PrivateNote.InferenceDealClosed".to_string(),
        value: serde_json::json!({ "deal": deal }),
    }
}

fn node(src: &str, chain_order: &str) -> EventNode {
    EventNode {
        msg_id: format!("m_{chain_order}"),
        msg_chain_order: Some(chain_order.to_string()),
        src: Some(src.to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: Some(serde_json::json!(1_700_000_000)),
    }
}

/// Like `node` but with a custom `created_at` unix timestamp.
fn node_at(src: &str, chain_order: &str, created_at: u64) -> EventNode {
    EventNode {
        msg_id: format!("m_{chain_order}"),
        msg_chain_order: Some(chain_order.to_string()),
        src: Some(src.to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: Some(serde_json::json!(created_at)),
    }
}

async fn project(
    tx: &mut Transaction<'_, Postgres>,
    e: &DecodedEvent,
    n: &EventNode,
) -> ProjectionOutcome {
    project_event(tx, e, n).await.unwrap()
}
/// Row shape shared by every test that reads back the dispute columns
/// (`disputed_at_chain`, `clean_settlement`, `close_kind`, `settled_at_chain`).
/// A bare 4-tuple of these types trips clippy's `type_complexity`; named once
/// here so both `stream_disputed_sets_disputed_at_and_not_clean` and its
/// real-body counterpart below can reuse it.
type DisputedRow = (
    Option<chrono::DateTime<chrono::Utc>>,
    Option<bool>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// Decode a real `TokenContract.*` body and assert it matches the `decoded`
/// snapshot captured in the harvest journal at capture time — the check every
/// real-body test in this file performs before handing the event to the
/// projector, so a field-layout regression is caught here rather than only
/// showing up as an unexplained read-model mismatch below.
fn decode_and_verify_snapshot(
    body_base64: &str,
    dst: &str,
    expected_event_type: &str,
    snapshot: &str,
) -> DecodedEvent {
    let decoded = Decoder::new()
        .expect("decoder")
        .decode_event_body(body_base64, Some(dst))
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known to the airegistry ABIs");
    assert_eq!(decoded.event_type, expected_event_type);
    let expected: serde_json::Value =
        serde_json::from_str(snapshot).expect("snapshot from the harvest log");
    assert_eq!(decoded.value, expected, "payload drifted from the captured snapshot");
    decoded
}

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

#[tokio::test]
async fn stream_funded_then_opened_records_buyer_deposit_price() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_fund_open";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(
            &mut tx,
            &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
            &node(tc, "co-1")
        )
        .await,
        ProjectionOutcome::Applied
    );
    assert_eq!(
        project(
            &mut tx,
            &ev("StreamOpened", serde_json::json!({"buyer":"0:buyer1","pricePerTick":"10"})),
            &node(tc, "co-2")
        )
        .await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    let (buyer, deposit, ppt): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, deposit::text, price_per_tick::text from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(buyer.as_deref(), Some("0:buyer1"));
    assert_eq!(deposit.as_deref(), Some("5000"));
    assert_eq!(ppt.as_deref(), Some("10"));
}

#[tokio::test]
async fn tick_finalized_counts_each_tick_once_under_replay() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_ticks";
    sqlx::query("delete from inference_ticks where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let tick1 = ev("TickFinalized", serde_json::json!({"finalizedOwed":"10","deposit":"4990"}));
    let tick2 = ev("TickFinalized", serde_json::json!({"finalizedOwed":"10","deposit":"4980"}));
    project(&mut tx, &tick1, &node(tc, "co-1")).await;
    project(&mut tx, &tick2, &node(tc, "co-2")).await;
    // Replay tick1 (same chain_order) — must NOT double-count.
    project(&mut tx, &tick1, &node(tc, "co-1")).await;
    tx.commit().await.unwrap();

    let ticks: i32 = sqlx::query_scalar(
        "select finalized_ticks from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ticks, 2, "each distinct TickFinalized event counted once");
    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_ticks where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 2);
}

// Between weekly boundaries `TickFinalized` says nothing, so a deal can run for
// days with no sign it is moving. `TicksClaimed` is that sign: the seller's
// cumulative claim against what has actually been trusted. Both figures are
// cumulative on chain (`claimTokens` requires the new value to be >= the stored
// one), so the projector keeps the high-water mark and an out-of-order replay
// cannot walk them backwards.
#[tokio::test]
async fn ticks_claimed_tracks_progress_and_never_goes_backwards() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_claimed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("TicksClaimed", serde_json::json!({"trusted":"5","claimed":"12"})),
        &node(tc, "co-1"),
    )
    .await;
    project(
        &mut tx,
        &ev("TicksClaimed", serde_json::json!({"trusted":"12","claimed":"20"})),
        &node(tc, "co-2"),
    )
    .await;
    // A late replay of the earlier claim must not undo the later one.
    project(
        &mut tx,
        &ev("TicksClaimed", serde_json::json!({"trusted":"5","claimed":"12"})),
        &node(tc, "co-1"),
    )
    .await;
    tx.commit().await.unwrap();

    let (trusted, claimed): (String, String) = sqlx::query_as(
        "select trusted_ticks::text, claimed_ticks::text from inference_deals \
         where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((trusted.as_str(), claimed.as_str()), ("12", "20"));
}

#[tokio::test]
async fn stream_stopped_marks_clean_settlement() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_stop";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "StreamStopped",
            serde_json::json!({"buyer":"0:b","toSeller":"40","refundToBuyer":"60"}),
        ),
        &node(tc, "co-9"),
    )
    .await;
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("STOPPED"));
    assert_eq!(clean, Some(true));
    assert!(settled.is_some());
}

#[tokio::test]
async fn dispute_resolved_marks_close_kind_and_not_clean() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_dispute_resolved";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(
        &mut tx,
        &ev(
            "DisputeResolved",
            serde_json::json!({"toSeller":"40","refundToBuyer":"60","released":true}),
        ),
        &node(tc, "co-dr-1"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("DISPUTE_RESOLVED"));
    assert_eq!(clean, Some(false));
    assert!(settled.is_some(), "settled_at_chain must be set on DisputeResolved");
}

#[tokio::test]
async fn stream_disputed_sets_disputed_at_and_not_clean() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_disputed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(
        &mut tx,
        &ev("StreamDisputed", serde_json::json!({"buyer":"0:b","at":"1700000000"})),
        &node(tc, "co-sd-1"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (disputed, clean, kind, settled): DisputedRow = sqlx::query_as(
        "select disputed_at_chain, clean_settlement, close_kind, settled_at_chain \
         from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(disputed.is_some(), "disputed_at_chain must be set on StreamDisputed");
    assert_eq!(clean, Some(false), "clean_settlement must be false on StreamDisputed");
    assert!(kind.is_none(), "close_kind must remain NULL (StreamDisputed is not a terminal close)");
    assert!(settled.is_none(), "settled_at_chain must remain NULL (StreamDisputed is not a close)");
}

#[tokio::test]
async fn contract_destroyed_sets_close_kind_without_clean_settlement() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_destroyed_nocl";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(
        &mut tx,
        &ev("ContractDestroyed", serde_json::json!({"self": tc})),
        &node(tc, "co-cd-1"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, settled, clean): (Option<String>, Option<chrono::DateTime<chrono::Utc>>, Option<bool>) = sqlx::query_as(
        "select close_kind, settled_at_chain, clean_settlement from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("DESTROYED"));
    assert!(settled.is_some(), "settled_at_chain must be set on ContractDestroyed");
    // Deliberate tri-state: DESTROYED without a prior close does NOT write clean_settlement.
    assert!(clean.is_none(), "clean_settlement must remain NULL when destroy without prior close");
}

#[tokio::test]
async fn first_close_wins_over_later_close() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_first_close_wins";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    // First close: StreamStopped (clean) at t=1_700_000_001.
    // Second close: DisputeResolved (not clean) at t=1_700_000_002.
    // coalesce semantics mean the first write wins for all three columns.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "StreamStopped",
            serde_json::json!({"buyer":"0:b","toSeller":"40","refundToBuyer":"60"}),
        ),
        &node_at(tc, "co-fcw-1", 1_700_000_001),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "DisputeResolved",
            serde_json::json!({"toSeller":"40","refundToBuyer":"60","released":true}),
        ),
        &node_at(tc, "co-fcw-2", 1_700_000_002),
    )
    .await;
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    // First close wins via coalesce.
    assert_eq!(kind.as_deref(), Some("STOPPED"), "first close_kind must be kept");
    assert_eq!(clean, Some(true), "first clean_settlement must be kept");
    // settled_at_chain must match the first event's timestamp (1_700_000_001).
    let expected_ts = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_001, 0).unwrap();
    assert_eq!(settled, Some(expected_ts), "settled_at_chain must be the first close's timestamp");
}

#[tokio::test]
async fn token_contract_event_seeds_skeleton_then_filled_enriches() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_seed_filled_test";
    let ob = "0:ob_seed_filled_test";

    // Clean slate: cascade-delete inference_deals (also removes inference_ticks),
    // and delete inference_orders / inference_markets for the orderbook. The Filled
    // step below also mints a global-PK inference_trades row, so clear that too.
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Step 1: TokenContract.StreamFunded arrives BEFORE the Filled event.
    // This seeds the skeleton and records buyer/deposit from the TC side.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(
            &mut tx,
            &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer","deposit":"5000"})),
            &node(tc, "co-sf-1"),
        )
        .await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    // Step 2: Seed SELL and BUY legs via InferenceOrderBook.InferenceOrderPlaced.
    let mut tx = pool.begin().await.unwrap();
    // SELL leg (is_buy=false, note=tc is seller's note address, orderId=1).
    assert_eq!(
        project(
            &mut tx,
            &iob_ev(
                "InferenceOrderPlaced",
                serde_json::json!({
                    "orderId": "1",
                    "isBuy": false,
                    "price": "100",
                    "ticks": "10",
                    "note": "0:seller",
                    "tokenContract": tc,
                    "deadline": "0",
                    "flags": "0"
                }),
            ),
            &node(ob, "co-op-sell"),
        )
        .await,
        ProjectionOutcome::Applied
    );
    // BUY leg (is_buy=true, orderId=2).
    assert_eq!(
        project(
            &mut tx,
            &iob_ev(
                "InferenceOrderPlaced",
                serde_json::json!({
                    "orderId": "2",
                    "isBuy": true,
                    "price": "100",
                    "ticks": "10",
                    "note": "0:buyer",
                    "tokenContract": tc,
                    "deadline": "0",
                    "flags": "0"
                }),
            ),
            &node(ob, "co-op-buy"),
        )
        .await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    // Step 3: InferenceOrderBook.InferenceFilled cross-links sellerTC=tc with ob.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(
            &mut tx,
            &iob_ev(
                "InferenceFilled",
                serde_json::json!({
                    "makerId": "1",
                    "takerId": "2",
                    "ticks": "10",
                    "clearingPrice": "100",
                    "sellerTC": tc,
                    "sellerNote": "0:seller",
                    "buyerNote": "0:buyer"
                }),
            ),
            &node(ob, "co-filled-1"),
        )
        .await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    // Assert: single inference_deals row for tc has BOTH sides merged.
    let (buyer_note, deposit, orderbook_address, seller_note): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "select buyer_note, deposit::text, orderbook_address, seller_note \
         from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    // TC side: buyer_note and deposit come from StreamFunded.
    assert_eq!(buyer_note.as_deref(), Some("0:buyer"), "buyer_note from StreamFunded");
    assert_eq!(deposit.as_deref(), Some("5000"), "deposit from StreamFunded");
    // Filled side: orderbook_address and seller_note come from the Filled cross-link.
    assert_eq!(orderbook_address.as_deref(), Some(ob), "orderbook_address from Filled");
    assert_eq!(seller_note.as_deref(), Some("0:seller"), "seller_note resolved from SELL leg");
}

#[tokio::test]
async fn probe_burned_is_terminal_close() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_probe_burned";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let ev_pb = ev(
        "ProbeBurned",
        serde_json::json!({"buyer":"0:b","burnedProbe":"10","burnedBond":"5","refundToBuyer":"85"}),
    );
    assert_eq!(project(&mut tx, &ev_pb, &node(tc, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, settled, clean): (Option<String>, Option<chrono::DateTime<chrono::Utc>>, Option<bool>) =
        sqlx::query_as("select close_kind, settled_at_chain, clean_settlement from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("PROBE_BURNED"));
    assert!(settled.is_some(), "probe-burn is terminal: settled_at_chain must be set");
    assert!(clean.is_none(), "probe-burn is not a clean settlement: clean_settlement stays NULL");
}

#[tokio::test]
async fn a_tick_finalized_with_only_its_own_fields_still_inserts_the_row() {
    // A GUARD, green today. It catches lenient reading being turned strict:
    // strictness here would mean ABI drift halting the whole settlement rather
    // than losing a single column — a payload missing a field it does not own must
    // still project. Two ways it goes red: a strict DTO makes `project` return
    // `Err` and the `Applied` assert fails, and a projector that stops inserting
    // leaves the count at 0.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_dto_lenient";
    for table in ["inference_ticks", "inference_deals"] {
        sqlx::query(&format!("delete from {table} where token_contract_address=$1"))
            .bind(tc)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    let e = ev("TickFinalized", serde_json::json!({"finalizedOwed":"5","deposit":"10"}));
    assert_eq!(project(&mut tx, &e, &node(tc, "co-dtolen-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let n: i64 =
        sqlx::query_scalar("select count(*) from inference_ticks where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn unknown_token_contract_event_returns_unknown() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_unknown_event";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    // Call project_event directly so we can inspect the returned ProjectionOutcome
    // without unwrap-discarding it (the `project` helper unwraps but returns the outcome).
    let mut tx = pool.begin().await.unwrap();
    let outcome =
        project_event(&mut tx, &ev("Bogus", serde_json::json!({})), &node(tc, "co-unk-1"))
            .await
            .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(outcome, ProjectionOutcome::Unknown, "unrecognised event name must return Unknown");

    // The deal skeleton must still have been seeded (seed runs before dispatch).
    let count: i64 =
        sqlx::query_scalar("select count(*) from inference_deals where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "skeleton row must exist even for an Unknown event");
}

// -----------------------------------------------------------------------------
// Real bodies (Task 5, wave 4). Every test above builds its `DecodedEvent`
// through `ev(...)`, so the field layout was asserted by intention: the test
// and the projector agreed with each other and neither consulted the chain.
// Here the event comes out of the decoder applied to real bytes captured from
// shellnet (`fixtures/chain_bodies.rs`, harvest journal
// `specs/2026-08-13-wave4-harvest.md`), and every test also checks the decoded
// payload against the `decoded` snapshot recorded at capture time — without
// that check a regression in the field layout could pass while the projector
// still writes *something*.
//
// One test per `TokenContract.*` type the wave-4 harvest actually produced a
// post-upgrade body for. `ShellWithdrawn` is the exception in both directions: it
// has no fixture (see the provenance comment on the `TokenContract.*` section of
// `chain_bodies.rs`), so its skeleton-only property is pinned by a SYNTHETIC test
// instead — `a_synthetic_shell_withdrawn_event_is_skeleton_only`, which says so in
// its own comment and is to be replaced by a real body at the first run that
// withdraws shell.

#[tokio::test]
async fn a_real_contract_deployed_body_seeds_skeleton_via_dst_route() {
    // TokenContract.ContractDeployed collides on event id with
    // RootModel.ContractDeployed (decoder.rs's route table, ids 732 vs 703);
    // `dst` is what tells them apart, and the journal's `dst` is exactly the
    // route table's key format (`config::event_type_dst`). Passing it here
    // exercises the same disambiguation path a live row goes through.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_contract_deployed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_CONTRACT_DEPLOYED,
        ":00000000000000000000000000000000000000000000000000000000000002dc",
        "TokenContract.ContractDeployed",
        HARVESTED_TC_CONTRACT_DEPLOYED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-cd-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let count: i64 =
        sqlx::query_scalar("select count(*) from inference_deals where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "skeleton row must exist; ContractDeployed carries no further state");
}

#[tokio::test]
async fn a_real_contract_destroyed_body_sets_close_kind_without_clean_settlement() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_contract_destroyed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_CONTRACT_DESTROYED,
        ":00000000000000000000000000000000000000000000000000000000000002c5",
        "TokenContract.ContractDestroyed",
        HARVESTED_TC_CONTRACT_DESTROYED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-cdd-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, settled, clean): (Option<String>, Option<chrono::DateTime<chrono::Utc>>, Option<bool>) = sqlx::query_as(
        "select close_kind, settled_at_chain, clean_settlement from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("DESTROYED"));
    assert!(settled.is_some(), "settled_at_chain must be set on ContractDestroyed");
    assert!(clean.is_none(), "clean_settlement must remain NULL when destroy without prior close");
}

#[tokio::test]
async fn a_real_buyer_bond_funded_body_is_skeleton_only() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_buyer_bond_funded";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_BUYER_BOND_FUNDED,
        ":00000000000000000000000000000000000000000000000000000000000002dd",
        "TokenContract.BuyerBondFunded",
        HARVESTED_TC_BUYER_BOND_FUNDED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-bbf-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (deposit, close_kind): (Option<String>, Option<String>) = sqlx::query_as(
        "select deposit::text, close_kind from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deposit.is_none(), "BuyerBondFunded carries no deal-level state the read model needs");
    assert!(close_kind.is_none());
}

#[tokio::test]
async fn a_real_seller_bond_funded_body_is_skeleton_only() {
    // The arm ignores its payload, so the two `is_none` assertions below cannot fail
    // whatever that payload says — they restate the schema, not a behaviour. What
    // carries this test is the `Applied` assertion: drop the arm and the outcome is
    // `Unknown`, which marks the row processed and loses it for good, and the
    // `fetch_one` that follows finds no skeleton row to read. Both failures are real
    // and both are named here so the next reader does not mistake the negatives for
    // the whole test. (Wave 7 first annotated `a_tick_finalized_…` instead — same
    // verdict, wrong test; this is the one the review meant.)
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_seller_bond_funded";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_SELLER_BOND_FUNDED,
        ":00000000000000000000000000000000000000000000000000000000000002d7",
        "TokenContract.SellerBondFunded",
        HARVESTED_TC_SELLER_BOND_FUNDED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-sbf-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (deposit, close_kind): (Option<String>, Option<String>) = sqlx::query_as(
        "select deposit::text, close_kind from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deposit.is_none(), "SellerBondFunded carries no deal-level state the read model needs");
    assert!(close_kind.is_none());
}

#[tokio::test]
async fn a_synthetic_shell_withdrawn_event_is_skeleton_only() {
    // IX-TC-11's last uncovered type. SYNTHETIC by necessity: the wave-4 harvest
    // captured no post-upgrade `ShellWithdrawn` body (no run withdrew shell), so
    // unlike the five real-body neighbours this payload is hand-built after
    // `ShellWithdrawnData` (recipient, amount). Replace it with a
    // `decode_and_verify_snapshot` fixture at the first stand run that performs
    // a withdrawal. Until then the claim is the projector's, not the decoder's:
    // the arm seeds the deal skeleton and writes no further state.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_synth_shell_withdrawn";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded =
        ev("ShellWithdrawn", serde_json::json!({"recipient":"0:withdraw_to","amount":"12345"}));

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-synth-sw-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (deposit, close_kind): (Option<String>, Option<String>) = sqlx::query_as(
        "select deposit::text, close_kind from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deposit.is_none(), "ShellWithdrawn carries no deal-level state the read model needs");
    assert!(close_kind.is_none());
}

#[tokio::test]
async fn a_real_stream_opened_body_projects_price_per_tick() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_stream_opened";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_STREAM_OPENED,
        ":00000000000000000000000000000000000000000000000000000000000002d1",
        "TokenContract.StreamOpened",
        HARVESTED_TC_STREAM_OPENED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-so-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (buyer, ppt): (Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, price_per_tick::text from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(
        buyer.as_deref(),
        Some("0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d")
    );
    assert_eq!(ppt.as_deref(), Some("3000000000"));
}

#[tokio::test]
async fn a_real_stream_funded_body_projects_buyer_deposit() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_stream_funded";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_STREAM_FUNDED,
        ":00000000000000000000000000000000000000000000000000000000000002d0",
        "TokenContract.StreamFunded",
        HARVESTED_TC_STREAM_FUNDED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-sf-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (buyer, deposit): (Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, deposit::text from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        buyer.as_deref(),
        Some("0:a803c512a54a17208ae003086df1846fd136fec8e7e9b30b8ab386edb29e4e1d")
    );
    assert_eq!(deposit.as_deref(), Some("24600000000"));
}

#[tokio::test]
async fn a_real_stream_stopped_body_marks_clean_settlement() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_stream_stopped";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_STREAM_STOPPED,
        ":00000000000000000000000000000000000000000000000000000000000002d3",
        "TokenContract.StreamStopped",
        HARVESTED_TC_STREAM_STOPPED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-ss-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("STOPPED"));
    assert_eq!(clean, Some(true));
    assert!(settled.is_some());
}

#[tokio::test]
async fn a_real_stream_disputed_body_sets_disputed_at_and_not_clean() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_stream_disputed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_STREAM_DISPUTED,
        ":00000000000000000000000000000000000000000000000000000000000002d4",
        "TokenContract.StreamDisputed",
        HARVESTED_TC_STREAM_DISPUTED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-sd-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (disputed, clean, kind, settled): DisputedRow = sqlx::query_as(
        "select disputed_at_chain, clean_settlement, close_kind, settled_at_chain \
         from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(disputed.is_some(), "disputed_at_chain must be set on StreamDisputed");
    assert_eq!(clean, Some(false), "clean_settlement must be false on StreamDisputed");
    assert!(kind.is_none(), "close_kind must remain NULL (StreamDisputed is not a terminal close)");
    assert!(settled.is_none(), "settled_at_chain must remain NULL (StreamDisputed is not a close)");
}

#[tokio::test]
async fn a_real_dispute_resolved_body_marks_close_kind_and_not_clean() {
    // IX-TC-15 outcome 1: the pre-boundary exception body (captured 2026-08-05,
    // before the wave-4 fresh window) — see its doc comment in
    // `fixtures/chain_bodies.rs` for why taking it is still sound.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_dispute_resolved";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_DISPUTE_RESOLVED,
        ":00000000000000000000000000000000000000000000000000000000000002d5",
        "TokenContract.DisputeResolved",
        HARVESTED_TC_DISPUTE_RESOLVED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-dr-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("DISPUTE_RESOLVED"));
    assert_eq!(clean, Some(false));
    assert!(settled.is_some(), "settled_at_chain must be set on DisputeResolved");
}

#[tokio::test]
async fn a_real_probe_accepted_body_is_skeleton_only() {
    // ProbeAccepted reads as a probe-branch event but belongs to the IX-TC-11
    // skeleton group by name; skipping it would leave that group's line looking
    // closed while one of its named arms stayed synthetic.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_probe_accepted";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_PROBE_ACCEPTED,
        ":00000000000000000000000000000000000000000000000000000000000002d8",
        "TokenContract.ProbeAccepted",
        HARVESTED_TC_PROBE_ACCEPTED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-pa-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (close_kind, settled): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, settled_at_chain from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(close_kind.is_none(), "ProbeAccepted carries no deal-level state the read model needs");
    assert!(settled.is_none());
}

#[tokio::test]
async fn a_real_probe_burned_body_is_terminal_close() {
    // IX-TC-16: closes the row the matrix had marked unreachable before wave 5.
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_probe_burned";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_PROBE_BURNED,
        ":00000000000000000000000000000000000000000000000000000000000002d9",
        "TokenContract.ProbeBurned",
        HARVESTED_TC_PROBE_BURNED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-pb-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, settled, clean): (Option<String>, Option<chrono::DateTime<chrono::Utc>>, Option<bool>) =
        sqlx::query_as("select close_kind, settled_at_chain, clean_settlement from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("PROBE_BURNED"));
    assert!(settled.is_some(), "probe-burn is terminal: settled_at_chain must be set");
    assert!(clean.is_none(), "probe-burn is not a clean settlement: clean_settlement stays NULL");
}

#[tokio::test]
async fn a_real_ticks_claimed_body_projects_progress() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_ticks_claimed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_TICKS_CLAIMED,
        ":00000000000000000000000000000000000000000000000000000000000002da",
        "TokenContract.TicksClaimed",
        HARVESTED_TC_TICKS_CLAIMED_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-tc-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (trusted, claimed): (String, String) = sqlx::query_as(
        "select trusted_ticks::text, claimed_ticks::text from inference_deals \
         where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((trusted.as_str(), claimed.as_str()), ("1000000", "2000000"));
}

#[tokio::test]
async fn a_real_endpoint_set_body_is_skeleton_only() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_real_endpoint_set";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let decoded = decode_and_verify_snapshot(
        TC_ENDPOINT_SET,
        ":00000000000000000000000000000000000000000000000000000000000002db",
        "TokenContract.EndpointSet",
        HARVESTED_TC_ENDPOINT_SET_DECODED,
    );

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(&mut tx, &decoded, &node(tc, "co-real-es-1")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (close_kind, settled): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, settled_at_chain from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(close_kind.is_none(), "EndpointSet carries no deal-level state the read model needs");
    assert!(settled.is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// PrivateNote.InferenceDealClosed — the only LIVE settlement signal since ingest
// stopped capturing TokenContract routes. Every test below pins a property that
// distinguishes it from the TokenContract close handlers above.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn deal_closed_is_keyed_by_the_payload_deal_not_by_the_emitting_note() {
    // THE WHOLE POINT OF THIS TEST. Every other handler in the settlement module
    // keys on `node.src`, because there the emitter IS the deal. Here the emitter
    // is the note. A handler that kept the `src` habit would stamp the settlement
    // onto a row named after a party and leave the deal unsettled — and both rows
    // exist here, so that mistake fails rather than passes by absence.
    let Some(pool) = setup().await else { return };
    let deal = "0:tc_dealclosed_key";
    let note = "0:pn_dealclosed_key";
    for a in [deal, note] {
        sqlx::query("delete from inference_deals where token_contract_address=$1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("insert into inference_deals (token_contract_address) values ($1), ($2)")
        .bind(deal)
        .bind(note)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &pn_deal_closed(deal), &node(note, "co-dc-1")).await;
    tx.commit().await.unwrap();

    let settled_deal: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select settled_at_chain from inference_deals where token_contract_address=$1",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    let settled_note: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select settled_at_chain from inference_deals where token_contract_address=$1",
    )
    .bind(note)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(settled_deal.is_some(), "the deal named in the payload must be settled");
    assert!(settled_note.is_none(), "the emitting note must not be settled");
}

#[tokio::test]
async fn deal_closed_says_that_it_closed_and_never_how() {
    // `close_kind` and `clean_settlement` must stay NULL. The event carries one
    // field and the branch is not among the things it can be asked. Inferring it
    // from surrounding payments would be indistinguishable from a fact downstream,
    // so the absence is pinned here rather than left to a reviewer to notice.
    let Some(pool) = setup().await else { return };
    let deal = "0:tc_dealclosed_how";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(deal)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &pn_deal_closed(deal), &node("0:pn_any", "co-dc-2")).await;
    tx.commit().await.unwrap();

    let (kind, clean, settled): (
        Option<String>,
        Option<bool>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals \
             where token_contract_address=$1",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(settled.is_some(), "the fact of closing is the one thing it does carry");
    assert_eq!(kind, None, "close_kind is not derivable from this event");
    assert_eq!(clean, None, "clean_settlement is not derivable from this event");
}

#[tokio::test]
async fn both_notes_report_the_same_close_and_the_first_one_wins() {
    // `_die` calls `onDealClosed` on the buyer's note AND the seller's, so this
    // event arrives twice for one deal, at two different chain moments. The second
    // must not move `settled_at_chain` — the same property that makes it idempotent
    // under replay.
    let Some(pool) = setup().await else { return };
    let deal = "0:tc_dealclosed_twice";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(deal)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &pn_deal_closed(deal), &node_at("0:pn_buyer", "co-dc-3a", 1_700_000_000))
        .await;
    project(&mut tx, &pn_deal_closed(deal), &node_at("0:pn_seller", "co-dc-3b", 1_700_009_999))
        .await;
    tx.commit().await.unwrap();

    let (settled, order): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) = sqlx::query_as(
        "select settled_at_chain, last_chain_order from inference_deals where token_contract_address=$1")
        .bind(deal).fetch_one(&pool).await.unwrap();
    assert_eq!(
        settled.map(|t| t.timestamp()),
        Some(1_700_000_000),
        "the second note must not overwrite the moment the first one recorded"
    );
    assert_eq!(order.as_deref(), Some("co-dc-3b"), "last_chain_order still advances");
    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_deals where token_contract_address=$1")
            .bind(deal)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1, "two reports are one deal");
}

#[tokio::test]
async fn a_close_for_a_deal_never_filled_records_a_skeleton_rather_than_deferring() {
    // The deal row normally exists — `InferenceFilled` precedes this on chain. It
    // can legitimately be absent when the fill happened before this indexer's
    // captured history, and there `Deferred` would be wrong twice: the parent is
    // unreachable rather than late, and this type is not in the dead-letter
    // allow-list, so the row would stay pending forever. The fact it carries is
    // kept; what it cannot fill stays NULL.
    let Some(pool) = setup().await else { return };
    let deal = "0:tc_dealclosed_orphan";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(deal)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome =
        project_event(&mut tx, &pn_deal_closed(deal), &node("0:pn_any", "co-dc-4")).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outcome, ProjectionOutcome::Applied, "must not defer on a missing parent");

    let (settled, seller, buyer): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "select settled_at_chain, seller_note, buyer_note from inference_deals \
             where token_contract_address=$1",
    )
    .bind(deal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(settled.is_some());
    assert_eq!(seller, None, "the parties are unknown and must read as unknown");
    assert_eq!(buyer, None);
}

#[tokio::test]
async fn a_zero_deal_address_creates_no_row() {
    // Unreachable on the live path — the note emits only inside
    // `if (_liveDeals.exists(msg.sender))` — but a replayed or hand-built row can
    // carry anything, and a zero-keyed row would be a permanent phantom in a table
    // keyed by address.
    let Some(pool) = setup().await else { return };
    let zero = "0:0000000000000000000000000000000000000000000000000000000000000000";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(zero)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome =
        project_event(&mut tx, &pn_deal_closed(zero), &node("0:pn_any", "co-dc-5")).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        outcome,
        ProjectionOutcome::Applied,
        "a nonsense payload is not a retryable failure"
    );

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_deals where token_contract_address=$1")
            .bind(zero)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0);
}
