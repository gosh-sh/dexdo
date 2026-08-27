// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the TokenContract.* SETTLEMENT projector.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors::project_event;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

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

/// Build a PrivateNote DecodedEvent. The note is the SENDER of these — the deal
/// they name is in the payload, not in `src`.
fn pn_ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "PrivateNote",
        event_name: event_name.to_string(),
        event_type: format!("PrivateNote.{event_name}"),
        value,
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

/// Like `node` but with no `created_at` at all — the shape `persist_page`
/// stores when the gateway omits the field or sends something unparseable.
fn node_without_timestamp(src: &str, chain_order: &str) -> EventNode {
    EventNode { created_at: None, ..node(src, chain_order) }
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
async fn stream_reclaimed_marks_close_kind_and_not_clean() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_reclaimed";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let outcome = project(
        &mut tx,
        &ev("StreamReclaimed", serde_json::json!({"buyer":"0:b","refundToBuyer":"100"})),
        &node(tc, "co-sr-1"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (kind, clean, settled): (Option<String>, Option<bool>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select close_kind, clean_settlement, settled_at_chain from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(kind.as_deref(), Some("RECLAIMED"));
    assert_eq!(clean, Some(false));
    assert!(settled.is_some(), "settled_at_chain must be set on StreamReclaimed");
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

    type DisputedRow = (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<bool>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
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
                    "deadline": "0"
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
                    "deadline": "0"
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

/// Run a full first cycle — funded, opened, one finalized tick, stopped — and
/// leave it committed. Shared by the two deal-reuse tests below.
/// The per-cycle columns a cycle reset has to clear, in the order the query
/// below selects them. Named because the tuple is nine wide and `sqlx` needs the
/// annotation to pick a decoder.
type CycleColumns = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<bool>,
    Option<chrono::DateTime<chrono::Utc>>,
    i32,
);

async fn commit_first_cycle(pool: &PgPool, tc: &str) {
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
        &node(tc, "co-1"),
    )
    .await;
    project(
        &mut tx,
        &ev("StreamOpened", serde_json::json!({"buyer":"0:buyer1","pricePerTick":"10"})),
        &node(tc, "co-2"),
    )
    .await;
    project(
        &mut tx,
        &ev("TickFinalized", serde_json::json!({"finalizedOwed":"10","deposit":"4990"})),
        &node(tc, "co-3"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "StreamStopped",
            serde_json::json!({"buyer":"0:buyer1","toSeller":"10","refundToBuyer":"4990"}),
        ),
        &node(tc, "co-4"),
    )
    .await;
    tx.commit().await.unwrap();
}

/// A deal address serves more than one match since contracts 4.0.36:
/// `cleanupUnopened` returns the `TokenContract` to the book instead of
/// destroying it. The row is keyed by that address, so a second funding must
/// start from a clean slate rather than inherit the first match's buyer,
/// settlement and tick log.
#[tokio::test]
async fn a_second_funding_starts_a_new_cycle() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_cycle_reset";
    commit_first_cycle(&pool, tc).await;

    let (kind, ticks): (Option<String>, i32) = sqlx::query_as(
        "select close_kind, finalized_ticks from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("STOPPED"), "precondition: cycle one closed");
    assert_eq!(ticks, 1, "precondition: cycle one recorded its tick");

    // Second match on the same address: a different buyer, a different deposit.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer2","deposit":"7000"})),
        &node(tc, "co-5"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit, ppt, opened, settled, kind, clean, disputed, ticks): CycleColumns =
        sqlx::query_as(
            "select buyer_note, deposit::text, price_per_tick::text, opened_at_chain, \
         settled_at_chain, close_kind, clean_settlement, disputed_at_chain, finalized_ticks \
         from inference_deals where token_contract_address=$1",
        )
        .bind(tc)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        buyer.as_deref(),
        Some("0:buyer2"),
        "the new match's buyer must replace the old one"
    );
    assert_eq!(
        deposit.as_deref(),
        Some("7000"),
        "the new match's deposit must replace the old one"
    );
    assert_eq!(ppt, None, "price_per_tick belongs to a cycle and is not known yet");
    assert_eq!(opened, None, "opened_at_chain must not carry over");
    assert_eq!(settled, None, "a live deal must not report a settlement");
    assert_eq!(kind, None, "close_kind must not carry over");
    assert_eq!(clean, None, "clean_settlement must not carry over");
    assert_eq!(disputed, None, "disputed_at_chain must not carry over");
    assert_eq!(ticks, 0, "the tick counter restarts with the cycle");

    let tick_rows: i64 =
        sqlx::query_scalar("select count(*) from inference_ticks where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tick_rows, 0, "the tick log must be cleared with the counter it backs");
}

/// A funding whose edge carried no usable `created_at` still counts as a cycle.
///
/// `funded_at_chain` comes from the edge timestamp, which the gateway may omit
/// or send unparseable — `persist_page` stores NULL and warns rather than
/// dropping the row, so the funding itself lands with a buyer, a deposit and a
/// watermark but no timestamp. Keying the reset on that timestamp made the gap
/// permanent: every later cycle boundary skipped the row and the `coalesce` in
/// `apply_stream_funded` went on serving cycle one's buyer and deposit under
/// cycle two's match.
#[tokio::test]
async fn a_cycle_funded_without_a_chain_timestamp_still_resets() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_cycle_no_timestamp";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    // Cycle one, funded off an edge the gateway gave no timestamp for.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
        &node_without_timestamp(tc, "co-1"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit, funded): (
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "select buyer_note, deposit::text, funded_at_chain from inference_deals \
             where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer.as_deref(), Some("0:buyer1"), "precondition: cycle one is funded");
    assert_eq!(deposit.as_deref(), Some("5000"), "precondition: cycle one has its deposit");
    assert_eq!(funded, None, "precondition: the timestamp is the half that is missing");

    // Cycle two, on the same address, by a different buyer.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer2","deposit":"7000"})),
        &node(tc, "co-2"),
    )
    .await;
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
        Some("0:buyer2"),
        "a cycle with no funded_at_chain must still be ended by the next funding"
    );
    assert_eq!(
        deposit.as_deref(),
        Some("7000"),
        "cycle one's deposit must not survive into cycle two's match"
    );
}

/// The reset is gated on `last_chain_order`, because reprojection replays rows.
/// An older funding arriving after a newer cycle has begun must be inert — an
/// ungated reset would wipe the live cycle every time the projection loop
/// replayed the first one.
#[tokio::test]
async fn a_replayed_earlier_funding_does_not_reset_a_later_cycle() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_cycle_replay";
    commit_first_cycle(&pool, tc).await;

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer2","deposit":"7000"})),
        &node(tc, "co-5"),
    )
    .await;
    project(
        &mut tx,
        &ev("StreamOpened", serde_json::json!({"buyer":"0:buyer2","pricePerTick":"20"})),
        &node(tc, "co-6"),
    )
    .await;
    tx.commit().await.unwrap();

    // Replay of cycle one's funding, at its original chain order.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
        &node(tc, "co-1"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit, ppt): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, deposit::text, price_per_tick::text from inference_deals \
         where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer.as_deref(), Some("0:buyer2"), "a replay must not restore the old buyer");
    assert_eq!(deposit.as_deref(), Some("7000"), "a replay must not restore the old deposit");
    assert_eq!(ppt.as_deref(), Some("20"), "a replay must not clear the live cycle");
}

/// The only signal a buyer no-show leaves on chain.
///
/// `cleanupUnopened` emits nothing since contracts 4.0.36 — it no longer dies,
/// so neither `TokenContract.ContractDestroyed` nor
/// `PrivateNote.InferenceDealClosed` fires. What it does do is clear
/// `_offerPosted`, which lets the seller re-list; and since `postFromNote`
/// refuses to post while `_offerPosted || _funded`, an ask reaching the book for
/// a deal this table shows as funded proves the funding was undone.
#[tokio::test]
async fn a_fresh_sell_offer_ends_a_funded_deals_cycle() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_reoffer_reset";
    let ob = "0:ob_reoffer_reset";
    commit_first_cycle(&pool, tc).await;
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // The seller re-lists after the no-show. Chain order is newer than anything
    // in cycle one, which is what separates this from a late-delivered offer
    // belonging to the cycle that is already recorded.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &iob_ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "7",
                "isBuy": false,
                "price": "100",
                "ticks": "10",
                "note": "0:seller",
                "tokenContract": tc,
                "deadline": "0"
            }),
        ),
        &node(ob, "co-9"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit, kind, ticks): (Option<String>, Option<String>, Option<String>, i32) =
        sqlx::query_as(
            "select buyer_note, deposit::text, close_kind, finalized_ticks \
             from inference_deals where token_contract_address=$1",
        )
        .bind(tc)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(buyer, None, "the dead match's buyer must not outlive it");
    assert_eq!(deposit, None, "the dead match's deposit must not outlive it");
    assert_eq!(kind, None, "a re-listed deal is not a settled one");
    assert_eq!(ticks, 0, "the tick counter restarts with the cycle");

    let tick_rows: i64 =
        sqlx::query_scalar("select count(*) from inference_ticks where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tick_rows, 0, "the tick log must be cleared with the counter it backs");
}

/// The offer that PRODUCED a match must not undo it.
///
/// In the ordinary flow the SELL offer rests before the funding it leads to, so
/// its chain order is older — and a late delivery of it says nothing about the
/// deal having been wound down. Only an offer newer than everything already
/// folded into the row can end a cycle.
#[tokio::test]
async fn the_offer_that_produced_the_match_does_not_end_its_cycle() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_reoffer_inorder";
    let ob = "0:ob_reoffer_inorder";
    commit_first_cycle(&pool, tc).await;
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // "co-0" predates the cycle-one funding at "co-1": this is the ask that was
    // matched, arriving late.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &iob_ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "8",
                "isBuy": false,
                "price": "100",
                "ticks": "10",
                "note": "0:seller",
                "tokenContract": tc,
                "deadline": "0"
            }),
        ),
        &node(ob, "co-0"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, kind): (Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, close_kind from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer.as_deref(), Some("0:buyer1"), "a late in-cycle offer must change nothing");
    assert_eq!(kind.as_deref(), Some("STOPPED"), "a late in-cycle offer must not reopen a close");
}

/// `cleanupUnopened` hands a no-show's deposit back and keeps the deal. Since
/// the 4.0.36 contract update it tells both notes, and each note says so on
/// chain — so the cycle ends here rather than waiting for a re-listing that may
/// never come.
#[tokio::test]
async fn a_notes_deal_closed_ends_an_unsettled_funding_cycle() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_closed_unsettled";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    // Funded and never opened: exactly the state `cleanupUnopened` runs on.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
        &node(tc, "co-1"),
    )
    .await;
    tx.commit().await.unwrap();

    // The note reports the close. `src` is the NOTE; the deal is in the payload.
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &pn_ev("InferenceDealClosed", serde_json::json!({"deal": tc})),
        &node("0:buyer1", "co-2"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit): (Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, deposit::text from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer, None, "the wound-down match's buyer must not survive it");
    assert_eq!(deposit, None, "nor its deposit — the deposit went back to the buyer");
}

/// The seller's note reports the same close, and finds the work done. One
/// close produces two of these events, and the second must be inert rather than
/// reach for a cycle that is already over.
#[tokio::test]
async fn the_second_note_reporting_the_same_close_changes_nothing() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_closed_twice";
    sqlx::query("delete from inference_deals where token_contract_address=$1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev("StreamFunded", serde_json::json!({"buyer":"0:buyer1","deposit":"5000"})),
        &node(tc, "co-1"),
    )
    .await;
    project(
        &mut tx,
        &pn_ev("InferenceDealClosed", serde_json::json!({"deal": tc})),
        &node("0:buyer1", "co-2"),
    )
    .await;
    project(
        &mut tx,
        &pn_ev("InferenceDealClosed", serde_json::json!({"deal": tc})),
        &node("0:seller1", "co-3"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, order): (Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, last_chain_order from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer, None);
    assert_eq!(
        order.as_deref(),
        Some("co-2"),
        "the second report ends nothing, so it must not advance the watermark either"
    );
}

/// `_die` emits this too, on a deal that settled and was then destroyed. That
/// row carries the settlement, and erasing it would turn a closed deal into a
/// blank one — the read model would report nothing ever happened.
#[tokio::test]
async fn a_deal_closed_after_settling_keeps_its_record() {
    let Some(pool) = setup().await else { return };
    let tc = "0:tc_closed_settled";
    commit_first_cycle(&pool, tc).await;

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &pn_ev("InferenceDealClosed", serde_json::json!({"deal": tc})),
        &node("0:buyer1", "co-9"),
    )
    .await;
    tx.commit().await.unwrap();

    let (buyer, deposit, kind): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select buyer_note, deposit::text, close_kind from inference_deals \
         where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(buyer.as_deref(), Some("0:buyer1"), "a settled deal keeps its buyer");
    assert_eq!(deposit.as_deref(), Some("5000"), "and its deposit");
    assert_eq!(kind.as_deref(), Some("STOPPED"), "and the close that was recorded for it");
}
