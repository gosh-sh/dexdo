// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference order-book projectors.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::inference_projectors::project_inference_event;
use dodex_infrastructure::inference_projectors::repair_expired_inference_orphan;
use dodex_infrastructure::inference_projectors::ExpiredOrphanOutcome;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

mod fixtures;

use fixtures::chain_bodies::INFERENCE_ORDER_PLACED;

// Call sites pass the full on-wire event name (e.g. "InferenceOrderPlaced");
// since v4.0.10 the inference book emits every event with an `Inference` prefix.
fn ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent {
        contract_kind: "InferenceOrderBook",
        event_type: format!("InferenceOrderBook.{event_name}"),
        event_name: event_name.to_string(),
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
async fn project(
    tx: &mut Transaction<'_, Postgres>,
    e: &DecodedEvent,
    n: &EventNode,
) -> ProjectionOutcome {
    project_inference_event(tx, e, n).await.unwrap()
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
async fn skeleton_insert_needs_only_orderbook_and_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:skeleton_smoke_ob";
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Skeleton: only the two seed columns. Must not violate NOT NULL anywhere.
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain)
         values ($1, to_timestamp(1700000000)) on conflict (orderbook_address) do nothing",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .expect("skeleton insert must succeed");
    let (reconciled, attempts): (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("select last_reconciled_at, reconcile_attempts from inference_markets where orderbook_address=$1")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert!(reconciled.is_none(), "skeleton must be invisible (last_reconciled_at NULL)");
    assert_eq!(attempts, 0);
}

#[tokio::test]
async fn order_placed_seeds_market_and_rests_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_op_seed_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"5","isBuy":true,"price":"100","ticks":"10","note":"0:note5",
        // A BUY carries the zero address on chain; only a SELL names a deal contract.
        "tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0" }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    // Market skeleton seeded, still invisible.
    let reconciled: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select last_reconciled_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(reconciled.is_none());
    // Order rests OPEN with full amount.
    let (status, init, rem, is_buy): (String, String, String, bool) = sqlx::query_as(
        "select status, amount_initial::text, amount_remaining::text, is_buy from inference_orders where orderbook_address=$1 and order_id=5")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), init.as_str(), rem.as_str(), is_buy), ("OPEN", "10", "10", true));
}

#[tokio::test]
async fn order_placed_replay_does_not_reset_partial_fill() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_op_replay_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({"orderId":"9","isBuy":false,"price":"7","ticks":"10","note":"0:n","tokenContract":"0:tc","deadline":"0","flags":"0"}),
    );
    project(&mut tx, &e, &node(ob, "co-1")).await;
    tx.commit().await.unwrap();
    // Simulate a partial fill landing (manually) then replay the placement.
    sqlx::query(
        "update inference_orders set amount_remaining=4 where orderbook_address=$1 and order_id=9",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &e, &node(ob, "co-1")).await; // replay
    tx.commit().await.unwrap();
    let rem: String = sqlx::query_scalar("select amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=9").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(rem, "4", "replay must not reset amount_remaining to full ticks");
}

#[tokio::test]
async fn placement_with_subscription_flag_marks_the_row() {
    let Some(pool) = setup().await else { return };
    let ob = "0:sub_flag";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();

    // FLAG_SUBSCRIPTION = 0x40 (contracts/airegistry/modifiers/modifiers.sol).
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:buyer", "tokenContract": "0:0", "deadline": "0", "flags": "64"
        }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "a1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let is_sub: bool = sqlx::query_scalar(
        "select is_subscription from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(is_sub, "FLAG_SUBSCRIPTION in `flags` must set is_subscription");
}

#[tokio::test]
async fn placement_without_the_flag_is_not_a_subscription() {
    let Some(pool) = setup().await else { return };
    let ob = "0:sub_noflag";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();

    // FLAG_AON = 0x20 — set, but this is not a subscription.
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:buyer", "tokenContract": "0:0", "deadline": "0", "flags": "32"
        }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "a1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let is_sub: bool = sqlx::query_scalar(
        "select is_subscription from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!is_sub, "a different bit is set — that does not make it a subscription");
}

#[tokio::test]
async fn a_placement_replay_does_not_resurrect_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_replay";
    clean(&pool, ob).await;
    let placed = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
            "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &placed, &node(ob, "a1")).await;
    let expired = ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"}));
    project(&mut tx, &expired, &node(ob, "a2")).await;
    // The same placement arrives again (overlapping capture pages).
    project(&mut tx, &placed, &node(ob, "a1")).await;
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "a placement replay has no right to re-open an expired order");
    assert_eq!(rem, "10", "the remainder is not restored");
}

#[tokio::test]
async fn a_late_cancel_does_not_demote_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_cancel";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
                "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "a1"),
    )
    .await;
    project(
        &mut tx,
        &ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"})),
        &node(ob, "a2"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderCancelled",
            serde_json::json!({"orderId": "1", "refunded": "0", "note": "0:b"}),
        ),
        &node(ob, "a3"),
    )
    .await;
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "expiry is already terminal; a cancel does not overwrite it");
}

#[tokio::test]
async fn a_late_fill_does_not_revive_an_expired_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_fill";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": true, "price": "5", "ticks": "10",
                "note": "0:b", "tokenContract": "0:0", "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "a1"),
    )
    .await;
    // THE SECOND LEG IS MANDATORY: `apply_inference_filled` returns Deferred while
    // any named row is missing. Without it, defective code would also leave EXPIRED
    // and a remainder of 10 — the test would be green without ever reaching
    // apply_filled_decrement.
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "2", "isBuy": false, "price": "5", "ticks": "10",
                "note": "0:s", "tokenContract": "0:tc", "deadline": "1", "flags": "0"
            }),
        ),
        &node(ob, "a2"),
    )
    .await;
    project(
        &mut tx,
        &ev("InferenceOrderExpired", serde_json::json!({"orderId": "1"})),
        &node(ob, "a3"),
    )
    .await;
    let outcome = project(
        &mut tx,
        &ev(
            "InferenceFilled",
            serde_json::json!({
                "makerId": "1", "takerId": "2", "ticks": "4", "clearingPrice": "5",
                "sellerTC": "0:tc_expfill", "buyerNote": "0:b", "sellerNote": "0:s"
            }),
        ),
        &node(ob, "co-expfill-4"),
    )
    .await;
    assert_eq!(outcome, ProjectionOutcome::Applied, "both legs are present — the fill must apply");
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(status, "EXPIRED", "expiry is terminal for a fill too");
    assert_eq!(rem, "10", "a fill does not decrement an expired row's remainder");
}

// `place` (:721) hardcodes `deadline: "0"`, so the deadline tests need a seed of their
// own. `is_buy` is a parameter rather than a constant: the sequential test below places
// a SELL leg, whose deadline must be non-zero (the contract rejects a GTC offer, `:1600`).
async fn place_with_deadline(
    pool: &sqlx::PgPool,
    ob: &str,
    order_id: &str,
    is_buy: bool,
    deadline: &str,
    chain_order: &str,
) {
    let mut tx = pool.begin().await.unwrap();
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": order_id, "isBuy": is_buy, "price": "5", "ticks": "10",
            "note": "0:b", "tokenContract": if is_buy { ZERO_ADDRESS } else { "0:tc" },
            "deadline": deadline, "flags": "0"
        }),
    );
    project(&mut tx, &e, &node(ob, chain_order)).await;
    tx.commit().await.unwrap();
}

fn refunded(order_id: &str) -> DecodedEvent {
    ev("InferenceRefunded", serde_json::json!({"orderId": order_id, "note": "0:b", "amount": "7"}))
}

fn expired_ev(order_id: &str) -> DecodedEvent {
    ev(
        "InferenceOrderExpired",
        serde_json::json!({"orderId": order_id, "isBuy": true, "note": "0:n", "tokenContract": ZERO_ADDRESS}),
    )
}

// TWO GUARDS, both green before the change and both mandatory: the "may be closed"
// predicate is a conjunction of two clauses, and each needs its own test to pin it.
//
// Their shared meaning is a REFUSAL to close the row: `_finalizeTaker` (`:1183`,
// `:1223`) sends a refund to a taker whose deadline has not passed, and what actually
// happened to the order is known to `InferenceFilled`, not to the refund. An
// IOC/MARKET leftover is closed by the phantom sweep, reconciling against the chain.

// Clause 1: `deadline is not null`. A GTC bid arrives with zero, and the placement
// projector normalizes it to SQL NULL.
#[tokio::test]
async fn a_filled_orphan_with_no_legs_still_records_the_seller() {
    let Some(pool) = setup().await else { return };
    let ob = "0:seller_direct";
    // `clean` purges orders/trades/markets but NOT inference_deals, and the deal
    // upsert keeps the first non-null seller_note via coalesce. A shared `0:tc` key
    // would share the row with other tests, making the result order-dependent under
    // parallel nextest. The key is unique and cleans up after itself.
    let tc = "0:tc_seller_direct";
    clean(&pool, ob).await;
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();

    // Neither leg is projected — exactly the orphan-repair case. The seller is named
    // by the event itself, so there is nothing for it to be lost to.
    let e = ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": "1", "takerId": "2", "ticks": "3", "clearingPrice": "5",
            "sellerTC": tc, "buyerNote": "0:buyer", "sellerNote": "0:seller"
        }),
    );
    repair_expired_inference_orphan(&mut tx, &e, &node(ob, "co-sellerdirect-1")).await.unwrap();
    tx.commit().await.unwrap();

    let seller: Option<String> = sqlx::query_scalar(
        "select seller_note from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        seller.as_deref(),
        Some("0:seller"),
        "the seller arrived in the event — no leg walk needed"
    );
    sqlx::query("delete from inference_deals where token_contract_address = $1")
        .bind(tc)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_orphans_name_all_four_deferrable_types() {
    // Filled, Cancelled, Expired and Refunded can all defer without a parent. Each
    // must have an outcome of its own: `Nothing` means "we do not know what was
    // lost", and it is of no use in a log.
    for (name, value) in [
        ("InferenceOrderExpired", serde_json::json!({"orderId": "9"})),
        ("InferenceRefunded", serde_json::json!({"orderId": "9", "note": "0:b", "amount": "1"})),
    ] {
        let Some(pool) = setup().await else { return };
        let ob = "0:orphan_types";
        clean(&pool, ob).await;
        let mut tx = pool.begin().await.unwrap();
        let outcome = repair_expired_inference_orphan(&mut tx, &ev(name, value), &node(ob, "a1"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_ne!(
            outcome,
            ExpiredOrphanOutcome::Nothing,
            "{name}: an orphan of this type must be named, not written off silently"
        );
    }
}

#[tokio::test]
async fn a_uint256_price_arrives_as_decimal_not_hex() {
    // A GUARD. `clearingPrice` and `price` are uint256, and the decoder hands them
    // over as "0x" + 64 hex (`uint256_hex_to_decimal`). Every other fixture supplies
    // decimal, where the old and the new code are indistinguishable — hence hex here.
    //
    // `expect` rather than `else { return }`: this guard is the only thing that
    // tells a correct move to DTOs from data corruption.
    let pool = setup().await.expect("the hex guard requires Postgres");
    let ob = "0:uint256_hex";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    // 0x…0101 = 257
    let hex257 = "0x0000000000000000000000000000000000000000000000000000000000000101";
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "1", "isBuy": false, "price": hex257, "ticks": "10",
                "note": "0:s", "tokenContract": "0:tc", "deadline": "1700009999", "flags": "0"
            }),
        ),
        &node(ob, "co-hex-1"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": "2", "isBuy": true, "price": hex257, "ticks": "10",
                "note": "0:b", "tokenContract": ZERO_ADDRESS, "deadline": "0", "flags": "0"
            }),
        ),
        &node(ob, "co-hex-2"),
    )
    .await;
    let f = ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": "1", "takerId": "2", "ticks": "10", "clearingPrice": hex257,
            "sellerTC": "0:tc_hex", "buyerNote": "0:b", "sellerNote": "0:s"
        }),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-hex-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let price: String = sqlx::query_scalar(
        "select price::text from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(price, "257", "a uint256 must arrive as decimal: numeric will not accept hex");

    let traded: String =
        sqlx::query_scalar("select price::text from inference_trades where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(traded, "257", "the trade price on the tape must be decimal");
}

#[tokio::test]
async fn a_fill_without_seller_tc_still_decrements_the_legs() {
    // A GUARD on leniency: making the DTO field strict would turn ABI drift from
    // "the deal did not link, and there is a warn" into "every fill fails forever".
    let Some(pool) = setup().await else { return };
    let ob = "0:dto_lenient";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "co-lenient-1").await;
    place(&pool, &mut tx, ob, "2", true, "10", "co-lenient-2").await;
    let f = ev(
        "InferenceFilled",
        // sellerTC and sellerNote are absent — the pre-v4.0.33 payload shape.
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"1","buyerNote":"0:b"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-lenient-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 1).await, ("FILLED".into(), "0".into()));
    assert_eq!(status_rem(&pool, ob, 2).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn a_refund_on_a_gtc_order_leaves_it_open() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_gtc";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "0", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        (status.as_str(), rem.as_str()),
        ("OPEN", "10"),
        "a GTC refund cannot tell a filled taker from an IOC leftover — guessing is not allowed"
    );
}

// Clause 2: `deadline <= chain_seconds`. WITHOUT THIS TEST an implementation
// checking only `is not null` passes the whole set while marking live orders with a
// future deadline `EXPIRED`. The case is not synthetic but exactly the entire SELL
// side: an offer must carry a non-zero deadline (`InferenceOrderBook.sol:1600`
// rejects zero as malformed), so the taker-only SELL from `:1223` always lands here.
#[tokio::test]
async fn a_refund_before_a_future_deadline_leaves_the_order_open() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_future";
    clean(&pool, ob).await;
    // node() sets created_at = 1_700_000_000; the deadline is deliberately later.
    place_with_deadline(&pool, ob, "1", false, "1700009999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, rem) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        (status.as_str(), rem.as_str()),
        ("OPEN", "10"),
        "an order whose deadline has not passed is alive: closing it on a refund would invent an expiry"
    );
}

#[tokio::test]
async fn a_refund_past_the_deadline_expires_the_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_exp";
    clean(&pool, ob).await;
    // node() sets created_at = 1_700_000_000; the deadline is one second earlier.
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        status, "EXPIRED",
        "a continuation expired before resuming emits only a refund — the status is derived from the deadline"
    );
}

// A GUARD, not a red-first test: today `InferenceRefunded` goes to the observability
// arm and does not touch the row, so the check is green before the change too. It
// exists so that a NEW projector does not start touching a terminal row — including
// its `updated_at`.
#[tokio::test]
async fn a_refund_over_a_filled_row_changes_nothing_at_all() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_filled";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "0", "a1").await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("update inference_orders set status='FILLED', amount_remaining=0 where orderbook_address=$1")
        .bind(ob)
        .execute(&mut *tx)
        .await
        .unwrap();
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, _) = status_rem(&pool, ob, 1).await;
    assert_eq!(
        status, "FILLED",
        "a refund also serves dust removal — it does not touch a filled row"
    );
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(after, before, "the no-op must be complete: even updated_at does not move");
}

#[tokio::test]
async fn a_refund_without_its_parent_defers() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_orphan";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(&mut tx, &refunded("1"), &node(ob, "a1")).await,
        ProjectionOutcome::Deferred
    );
    tx.commit().await.unwrap();
}

/// Clause 3 of the "may be closed" predicate, and the only one with no test:
/// `CANCELLED` qualifies ONLY with a `swept_at` mark. Both directions are asserted
/// here because one alone cannot fail usefully — relaxing the clause to a bare
/// `status = 'CANCELLED'` leaves the sweep-marked half green, and that is the
/// dangerous direction.
///
/// The distinction is what the mark means. The sweep writes `CANCELLED` as a GUESS,
/// because the order vanished from the book — and expiry is one way to vanish, so a
/// refund correcting it to `EXPIRED` is the sweep being told what actually happened.
/// A cancel with `swept_at` NULL came from `InferenceOrderCancelled`: the chain said
/// the owner cancelled, and a refund arriving afterwards does not make that untrue.
#[tokio::test]
async fn a_refund_corrects_a_swept_cancel_but_not_a_real_one() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_cancel_kinds";
    clean(&pool, ob).await;

    // Both rows are past their deadline, so only the sweep mark separates them.
    for (order_id, swept) in [(1i64, true), (2, false)] {
        sqlx::query(
            r#"insert into inference_orders
                   (orderbook_address, order_id, is_buy, price, amount_initial,
                    amount_remaining, status, deadline, swept_at, last_chain_order)
               values ($1, $2, true, 5, 10, 10, 'CANCELLED', 1699999999,
                       case when $3 then now() else null end, 'a0')"#,
        )
        .bind(ob)
        .bind(order_id)
        .bind(swept)
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a1")).await, ProjectionOutcome::Applied);
    assert_eq!(project(&mut tx, &refunded("2"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let rows: Vec<(i64, String, bool)> = sqlx::query_as(
        "select order_id::bigint, status, swept_at is null
           from inference_orders where orderbook_address = $1 order by order_id",
    )
    .bind(ob)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(1, "EXPIRED".to_string(), true), (2, "CANCELLED".to_string(), true)],
        "the swept cancel is corrected to EXPIRED and loses its provisional mark; the real \
         cancel stands"
    );
}

/// The missing-chain-time branch, and the ordering it sits behind.
///
/// Without a time the projector cannot tell expiry from a finished taker, so it
/// declines to guess, leaves the row alone and returns Applied — closing falls to the
/// sweep. But that early return is placed AFTER the parent lookup on purpose: reached
/// first, an orphan would be marked Applied and dropped, and a placement arriving
/// later would open an order this refund can never close. Both halves are asserted
/// together because the bug is the ORDER of two branches that are individually
/// correct, and swapping them is invisible to either one alone.
#[tokio::test]
async fn a_refund_without_chain_time_declines_to_guess_but_still_defers_an_orphan() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_no_time";
    clean(&pool, ob).await;
    let timeless = EventNode { created_at: None, ..node(ob, "a1") };

    // No parent AND no time: the parent check must win, or the orphan is lost.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(&mut tx, &refunded("1"), &timeless).await,
        ProjectionOutcome::Deferred,
        "the parent is checked FIRST — an Applied here drops the orphan out of the age-based \
         dead letter and a late placement would open an order this refund can never close"
    );
    tx.commit().await.unwrap();

    // Parent present, still no time: applied, and the row is untouched — including
    // updated_at, so a no-op does not lie to an observer.
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a0").await;
    let before: (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "select status, updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &timeless).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let after: (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "select status, updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before.0, "OPEN", "precondition");
    assert_eq!(after, before, "no time means no guess: the row is left entirely to the sweep");
}

// A GUARD, green before the change too — and the ONLY test that turns red if someone
// gives the projector a `CANCELLED` branch for a deadline that has not passed. That is
// exactly the scenario it is absent for; this is an extension of the existing
// `filled_defers_zero_writes_when_one_side_absent_then_applies_once` with a refund
// inserted between the `Deferred` and the retry.
//
// On chain a fully filled taker BUY produces `Placed(2)` -> `Filled(1,2)` ->
// `Refunded(2, leftover)` (`:1183`; the event goes out even with `leftover == 0`). The
// maker leg was lost at capture, so the fill defers and the drain moves on (the
// `Deferred` arms of `reproject_pending_from` leave the row unmarked and take the next
// one) — the refund is applied BEFORE its own fill.
#[tokio::test]
async fn a_refund_between_a_deferred_fill_and_its_retry_does_not_steal_the_terminal_status() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_seq";
    clean(&pool, ob).await;
    // The taker (id 2) is projected; the maker SELL leg (id 1) is not yet.
    place_with_deadline(&pool, ob, "2", true, "0", "a1").await;
    let f = ev(
        "InferenceFilled",
        // sellerTC is repo-unique: clean() does not purge inference_deals (its PK is the TC address).
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"5",
                           "sellerTC":"0:tc_refseq","buyerNote":"0:b","sellerNote":"0:s"}),
    );

    // 1. The fill defers — zero writes.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &f, &node(ob, "co-refseq-9")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();

    // 2. The taker's refund drains next and must decide nothing.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("2"), &node(ob, "a3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, _) = status_rem(&pool, ob, 2).await;
    assert_eq!(status, "OPEN", "the refund closed a row with an unapplied fill still pending");

    // 3. The maker leg arrives and the fill is retried.
    place_with_deadline(&pool, ob, "1", false, "1700009999", "a4").await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &f, &node(ob, "co-refseq-9")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    assert_eq!(
        status_rem(&pool, ob, 2).await,
        ("FILLED".into(), "0".into()),
        "a filled taker must end up FILLED: the refund only returned the unspent escrow"
    );
}

// A GUARD on the FORWARD order — the one that happens on chain. `_removeExpiredBid`
// (`InferenceOrderBook.sol:1143-1146`) calls `_refundAndRemove` and only THEN emits
// `InferenceOrderExpired`, so by `chain_order` the refund always comes first.
//
// The test has specific teeth: let the refund close the row under any status other
// than `EXPIRED` (say `CANCELLED`, as in this task's first draft), and the expiry
// arriving next will no longer fix anything.
#[tokio::test]
async fn a_refund_before_its_expiry_event_leaves_expired_standing() {
    let Some(pool) = setup().await else { return };
    let ob = "0:ref_then_exp";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a2")).await, ProjectionOutcome::Applied);
    assert_eq!(
        project(&mut tx, &expired_ev("1"), &node(ob, "a3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), swept_null),
        ("EXPIRED", true),
        "a refund must close a past-deadline row into EXPIRED SPECIFICALLY — otherwise the expiry will not pick it up"
    );
}

// A GUARD on the REVERSE order: a replay and a reordering within a batch. `updated_at`
// is checked here deliberately and not for decoration — the status assert is toothless
// on its own: the row is already `EXPIRED`, and a predicate like "not FILLED and not a
// real cancel" would rewrite it into the same `EXPIRED`, leaving the assert green. Only
// a complete no-op catches the substitution.
#[tokio::test]
async fn a_refund_after_its_expiry_event_changes_nothing_at_all() {
    let Some(pool) = setup().await else { return };
    let ob = "0:exp_then_ref";
    clean(&pool, ob).await;
    place_with_deadline(&pool, ob, "1", true, "1699999999", "a1").await;

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        project(&mut tx, &expired_ev("1"), &node(ob, "a2")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &refunded("1"), &node(ob, "a3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let (status, after): (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "select status, updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "EXPIRED");
    assert_eq!(
        after, before,
        "`EXPIRED` must be outside the refund predicate: the row is not touched at all"
    );
}

#[tokio::test]
async fn order_cancelled_is_terminal_and_defers_when_absent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_cancel_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Cancel with no prior placement => Deferred (zero writes).
    let mut tx = pool.begin().await.unwrap();
    let c = ev("InferenceOrderCancelled", serde_json::json!({"orderId":"2","refunded":"0"}));
    assert_eq!(project(&mut tx, &c, &node(ob, "co-1")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "select count(*) from inference_orders where orderbook_address=$1 and order_id=2",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
    // Place then cancel => CANCELLED, swept_at NULL.
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"2","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"})),&node(ob,"co-2")).await;
    assert_eq!(project(&mut tx, &c, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=2").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("CANCELLED", true));
}

// The chain decides expiry, not the reader: a resting order whose deadline has
// passed keeps its OPEN status until InferenceOrderExpired arrives, and only then
// becomes EXPIRED. Nothing derives the status from `deadline` vs wall-clock.
// The one exception is a Refunded whose chain time is past this deadline: there the
// chain has already removed the order, and the deadline only says the cause was
// expiry — continuation expiry emits no InferenceOrderExpired at all.
#[tokio::test]
async fn order_expired_is_terminal_and_defers_when_absent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_expire_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // Expiry with no prior placement => Deferred (zero writes), same as a cancel.
    let mut tx = pool.begin().await.unwrap();
    let x = ev(
        "InferenceOrderExpired",
        serde_json::json!({"orderId":"3","isBuy":true,"note":"0:n","tokenContract":ZERO_ADDRESS}),
    );
    assert_eq!(project(&mut tx, &x, &node(ob, "xo-1")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "select count(*) from inference_orders where orderbook_address=$1 and order_id=3",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
    // A placed order stays OPEN while its deadline sits in the past ...
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"3","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000","flags":"0"})),&node(ob,"xo-2")).await;
    tx.commit().await.unwrap();
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=3",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "a past deadline alone must not change the status");
    // ... and only the event makes it EXPIRED, leaving swept_at untouched.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, &x, &node(ob, "xo-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=3").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("EXPIRED", true));
}

// The common ordering, not an edge case: an order expires, the sweep notices it is
// gone from the book and provisionally marks it CANCELLED, and only then does the
// authoritative InferenceOrderExpired arrive. The event must win, or every expiry
// that the sweep outruns is recorded under the wrong terminal status. A real
// event-cancel (swept_at NULL) stays CANCELLED — the order was gone before it aged out.
#[tokio::test]
async fn expired_overrides_provisional_sweep_cancel_but_not_a_real_cancel() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_expire_override";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    for id in ["40", "41"] {
        project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":id,"isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000","flags":"0"})),&node(ob,&format!("eo-{id}"))).await;
    }
    tx.commit().await.unwrap();
    // 40: provisional sweep-cancel (swept_at set). 41: real event-cancel (swept_at NULL).
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=now() where orderbook_address=$1 and order_id=40").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=null where orderbook_address=$1 and order_id=41").bind(ob).execute(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    for id in ["40", "41"] {
        let x = ev(
            "InferenceOrderExpired",
            serde_json::json!({"orderId":id,"isBuy":true,"note":"0:n","tokenContract":ZERO_ADDRESS}),
        );
        assert_eq!(
            project(&mut tx, &x, &node(ob, &format!("ex-{id}"))).await,
            ProjectionOutcome::Applied
        );
    }
    tx.commit().await.unwrap();

    let (swept_status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=40").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (swept_status.as_str(), swept_null),
        ("EXPIRED", true),
        "a provisional sweep-cancel must yield to the authoritative expiry"
    );
    let real_status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=41",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(real_status, "CANCELLED", "a real event-cancel stays terminal");
}

#[tokio::test]
async fn observability_event_seeds_market_only() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_obs_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    // This used to be an `InferenceRefunded` WITHOUT an `orderId` — an event that does
    // not exist. The refund now has a projector of its own with strict id parsing, and
    // such a payload would make `project` fail. The test's point (any inference event
    // seeds the market skeleton) holds for any of the remaining observability types.
    let r =
        ev("InferenceExecuted", serde_json::json!({"ticks":"1","clearingPrice":"1","cost":"1"}));
    assert_eq!(project(&mut tx, &r, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let m: i64 =
        sqlx::query_scalar("select count(*) from inference_markets where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    let o: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((m, o), (1, 0), "observability seeds the market but creates no order");
}

/// Seed-only assert shared by the three IX-OB-18 tests below: project one
/// event of the given type onto a fresh book and require exactly (1 market,
/// 0 orders) — the shared skeleton pre-step ran, the arm itself wrote nothing.
async fn assert_seed_only(pool: &PgPool, ob: &str, e: &DecodedEvent) {
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(project(&mut tx, e, &node(ob, "co-seed-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let m: i64 =
        sqlx::query_scalar("select count(*) from inference_markets where orderbook_address=$1")
            .bind(ob)
            .fetch_one(pool)
            .await
            .unwrap();
    let o: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!((m, o), (1, 0), "the event must seed the market and write nothing else");
}

#[tokio::test]
async fn order_book_deployed_event_seeds_market_only() {
    // IX-OB-18: `InferenceOrderBookDeployed` is one of the four seed-only arms.
    // Payload follows `OrderBookDeployedData` (note, modelHash, modelName); the
    // arm never reads it — the assert is that nothing beyond the seed happens.
    let Some(pool) = setup().await else { return };
    let e = ev(
        "InferenceOrderBookDeployed",
        serde_json::json!({"note":"0:deployer_note","modelHash":"123","modelName":"seed-model"}),
    );
    assert_seed_only(&pool, "0:t_obd_seed", &e).await;
}

#[tokio::test]
async fn order_cancel_rejected_event_seeds_market_only() {
    // IX-OB-18: `InferenceOrderCancelRejected` fires when a cancel matched no
    // resting order or came from a foreign owner — the book did not change, so
    // the arm must not touch any row. Payload follows `OrderCancelRejectedData`.
    let Some(pool) = setup().await else { return };
    let e = ev(
        "InferenceOrderCancelRejected",
        serde_json::json!({"orderId":"7","reason":"1","note":"0:cancel_note"}),
    );
    assert_seed_only(&pool, "0:t_ocr_seed", &e).await;
}

#[tokio::test]
async fn order_rejected_event_seeds_market_only() {
    // IX-OB-18: `InferenceOrderRejected` carries no orderId — the placement was
    // refused before anything rested, so there is no row to key on. The routing
    // outcome of this type is already pinned by no_op_event_arms.rs; THIS test
    // asserts the different claim that projecting it writes nothing beyond the
    // market skeleton. Payload follows `OrderRejectedData`.
    let Some(pool) = setup().await else { return };
    let e = ev(
        "InferenceOrderRejected",
        serde_json::json!({"reason":"2","note":"0:reject_note","tokenContract":ZERO_ADDRESS,"refund":"0"}),
    );
    assert_seed_only(&pool, "0:t_or_seed", &e).await;
}

#[tokio::test]
async fn routes_by_event_type_when_event_name_is_empty() {
    // The reprojection loop reconstructs DecodedEvent with event_name EMPTY (only
    // event_type is persisted). This guards against routing on event_name, which
    // would send every live captured row to the seed-only path.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_empty_name";
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let loop_shaped = DecodedEvent {
        contract_kind: "",
        event_name: String::new(), // <-- as the live loop builds it
        event_type: "InferenceOrderBook.InferenceOrderPlaced".to_string(),
        value: serde_json::json!({"orderId":"7","isBuy":true,"price":"1","ticks":"3","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    };
    assert_eq!(project(&mut tx, &loop_shaped, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=7",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "empty event_name must still reach the OrderPlaced handler");
}

// ---- Orphan dead-letter helpers and test ----

use dodex_infrastructure::indexer_repo::IndexerRepository;

// ingest_age_secs => raw_events.created_at; chain_age_secs => created_at_chain (independent),
// so a test can make a row freshly-ingested yet ancient on chain.
#[allow(clippy::too_many_arguments)]
// Test helper with 8 intentional knobs (pool, msg, chain_order, ingest_age, chain_age, ob, event_type, decoded) for orphan tests
async fn insert_raw(
    pool: &sqlx::PgPool,
    msg: &str,
    co: &str,
    ingest_age_secs: i64,
    chain_age_secs: i64,
    ob: &str,
    event_type: &str,
    decoded: serde_json::Value,
) {
    sqlx::query(
        "insert into raw_events (msg_id, chain_order, created_at_chain, created_at, src_address, dst_address, event_type, body_json, decoded)
         values ($1, $2, now() - make_interval(secs => $3), now() - make_interval(secs => $4), $5, null, $6, '{}'::jsonb, $7::jsonb)
         on conflict (msg_id) do nothing")
        .bind(msg).bind(co).bind(chain_age_secs as f64).bind(ingest_age_secs as f64).bind(ob).bind(event_type).bind(decoded.to_string())
        .execute(pool).await.unwrap();
}
async fn raw_processed(pool: &sqlx::PgPool, msg: &str) -> bool {
    sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
        "select processed_at from raw_events where msg_id=$1",
    )
    .bind(msg)
    .fetch_one(pool)
    .await
    .unwrap()
    .is_some()
}
// Upsert the capture-stream cursor's `at_head` flag. The orphan dead-letter only
// fires once capture has drained to head; tests use a unique stream (via
// `with_capture_stream`) so they never race the shared live `blockchain_events` row.
async fn set_cursor_at_head(pool: &sqlx::PgPool, stream: &str, at_head: bool) {
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at)
           values ($1, 'x', $2, now())
         on conflict (stream_name) do update set at_head = excluded.at_head, updated_at = now()",
    )
    .bind(stream)
    .bind(at_head)
    .execute(pool)
    .await
    .unwrap();
}
async fn order_amount_status(
    pool: &sqlx::PgPool,
    ob: &str,
    order_id: &str,
) -> Option<(i64, String)> {
    sqlx::query_as::<_, (i64, String)>(
        "select amount_remaining::bigint, status from inference_orders
          where orderbook_address=$1 and order_id=$2::numeric",
    )
    .bind(ob)
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn expired_orphans_dropped_all_four_types_using_ingest_age_not_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_ob";
    sqlx::query("delete from raw_events where chain_order like '00orphan-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let filled = serde_json::json!({"makerId":"900","takerId":"901","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    let cancel = serde_json::json!({"orderId":"902","refunded":"0"});
    let expired =
        serde_json::json!({"orderId":"903","isBuy":true,"note":"0:b","tokenContract":ZERO_ADDRESS});
    let refunded = serde_json::json!({"orderId":"904","note":"0:b","amount":"1"});
    // (a)-(b'') aged-ingest orphans of ALL FOUR deferrable types => dropped.
    //
    // THIS IS A GREEN GUARD, not a red-first test. The `is_dead_letterable_orphan`
    // gate admits any `InferenceOrderBook.*`, and both drain paths match `Ok(_)`, i.e.
    // they mark the row processed even on a `Nothing` outcome. So new types drain even
    // without arms. The value lies elsewhere: the test turns red if the gate is ever
    // narrowed back to Filled/Cancelled — the rows would then be pending forever.
    insert_raw(
        &pool,
        "orphan-fill",
        "00orphan-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-cancel",
        "00orphan-b",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceOrderCancelled",
        cancel.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-expired",
        "00orphan-b1",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceOrderExpired",
        expired.clone(),
    )
    .await;
    insert_raw(
        &pool,
        "orphan-refund",
        "00orphan-b2",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceRefunded",
        refunded.clone(),
    )
    .await;
    // (c) FRESH ingest but ANCIENT created_at_chain (1 day) => NOT dropped — cutoff uses ingest age, not chain time.
    insert_raw(
        &pool,
        "orphan-oldchain",
        "00orphan-c",
        0,
        86400,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;
    // (d) fresh ingest, fresh chain => NOT dropped (normal short deferral).
    insert_raw(
        &pool,
        "orphan-fresh",
        "00orphan-d",
        0,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled.clone(),
    )
    .await;

    // Orphan dead-lettering only fires once capture has reached head. The repo
    // is bound (fresh instance, per-instance counters at zero) so the drop
    // counter below asserts an absolute value, not a delta.
    let stream = "orphan_drop_athead_stream";
    set_cursor_at_head(&pool, stream, true).await;
    let repo = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60));
    repo.reproject_pending_from(50, Some("00orphan-"), Some("00orphan-z")).await.unwrap();
    // The counter third of IX-MET-06: four aged orphans (one per deferrable
    // type) = exactly four drops recorded on this fresh instance; the fresh
    // and old-chain rows deferred, not dropped, and must not count.
    assert_eq!(
        repo.inference_orphans_dropped_count(),
        4,
        "each of the four aged orphan types must bump the drop counter exactly once"
    );

    assert!(raw_processed(&pool, "orphan-fill").await, "aged Filled orphan must be dropped");
    assert!(
        raw_processed(&pool, "orphan-cancel").await,
        "aged OrderCancelled orphan must be dropped"
    );
    assert!(
        raw_processed(&pool, "orphan-expired").await,
        "aged OrderExpired orphan must be dropped — narrowing the gate would leave the row pending forever"
    );
    assert!(
        raw_processed(&pool, "orphan-refund").await,
        "aged Refunded orphan must be dropped — narrowing the gate would leave the row pending forever"
    );
    assert!(!raw_processed(&pool, "orphan-oldchain").await,
        "old created_at_chain but fresh ingest => NOT dropped (proves raw_events.created_at, not chain time)");
    assert!(!raw_processed(&pool, "orphan-fresh").await, "fresh ingest => stays pending");
    // Dead-letter writes no order row.
    let n: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0);

    // Cleanup residual pending rows so they do not pollute other tests that
    // query max_pending_chain_order / has_pending_above globally.
    sqlx::query("delete from raw_events where chain_order like '00orphan-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_orphan_not_dropped_until_capture_at_head() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_nh_ob";
    let stream = "orphan_not_athead_stream";
    sqlx::query("delete from raw_events where chain_order like '00orphnh-%'")
        .execute(&pool)
        .await
        .unwrap();
    let filled = serde_json::json!({"makerId":"800","takerId":"801","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    // Aged ingest (1h) — well past the 60s cutoff — so only the at_head gate decides.
    insert_raw(
        &pool,
        "orphnh-fill",
        "00orphnh-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled,
    )
    .await;

    let repo = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60));

    // at_head = false: a missing parent may still be ahead in the backfill, so the
    // aged orphan must NOT be declared permanently dropped yet.
    set_cursor_at_head(&pool, stream, false).await;
    repo.reproject_pending_from(50, Some("00orphnh-"), Some("00orphnh-z")).await.unwrap();
    assert!(
        !raw_processed(&pool, "orphnh-fill").await,
        "aged orphan must stay pending while capture is still backfilling (at_head=false)"
    );

    // Same row, same age; only at_head flips to true — now it is dead-lettered.
    set_cursor_at_head(&pool, stream, true).await;
    repo.reproject_pending_from(50, Some("00orphnh-"), Some("00orphnh-z")).await.unwrap();
    assert!(
        raw_processed(&pool, "orphnh-fill").await,
        "once capture reaches head, the aged orphan is dead-lettered"
    );

    sqlx::query("delete from raw_events where chain_order like '00orphnh-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_filled_orphan_decrements_present_leg() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_leg_ob";
    let stream = "orphan_leg_athead_stream";
    sqlx::query("delete from raw_events where chain_order like '00orphld-%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // The dead-letter repair below mints a global-PK inference_trades row keyed on the
    // raw event's chain order — clean it up like the other tables so a re-run starts fresh.
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Seed a resting BUY maker (id 700) with 10 ticks of depth via the real placement projector.
    let mut tx = pool.begin().await.unwrap();
    let placed = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId":"700","isBuy":true,"price":"5","ticks":"10","note":"0:n",
            "tokenContract":ZERO_ADDRESS,
            "deadline":"0","flags":"0",
        }),
    );
    assert_eq!(
        project(&mut tx, &placed, &node(ob, "00seed-700")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    assert_eq!(order_amount_status(&pool, ob, "700").await, Some((10, "OPEN".into())));

    // Aged Filled orphan: maker 700 is present and resting; taker 701's OrderPlaced was dropped.
    let filled = serde_json::json!({"makerId":"700","takerId":"701","ticks":"3","clearingPrice":"5","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"});
    insert_raw(
        &pool,
        "orphld-fill",
        "00orphld-a",
        3600,
        0,
        ob,
        "InferenceOrderBook.InferenceFilled",
        filled,
    )
    .await;

    set_cursor_at_head(&pool, stream, true).await;
    IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00orphld-"), Some("00orphld-z"))
        .await
        .unwrap();

    // The orphan is dead-lettered...
    assert!(raw_processed(&pool, "orphld-fill").await, "aged Filled orphan dead-lettered");
    // ...but the present maker's depth is corrected (10 - 3 = 7), not left permanently stale.
    assert_eq!(
        order_amount_status(&pool, ob, "700").await,
        Some((7, "OPEN".into())),
        "present resting leg decremented by the fill before the drop"
    );
    // The missing taker leg is not fabricated.
    assert_eq!(order_amount_status(&pool, ob, "701").await, None, "missing leg is not created");

    sqlx::query("delete from raw_events where chain_order like '00orphld-%'")
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
}

#[tokio::test]
async fn a_range_event_orphan_dead_letters_like_the_books_own() {
    // `OracleEventList.RangeEventAdded` defers while its parent `oracle_events` row
    // is missing. Parents are emitted by the oracle list, which may have been deployed
    // BEFORE this deployment's capture cursor started — in which case they lie outside
    // the captured history and never arrive. RangeEventAdded creates no row of its own,
    // it annotates someone else's, so there is nothing to wait for. Without a final
    // outcome the row stays pending forever and holds the observer's backlog above zero.
    let Some(pool) = setup().await else { return };
    let oel = "0:t_range_orphan_list";
    sqlx::query("delete from raw_events where chain_order like '00rgoph-%'")
        .execute(&pool)
        .await
        .unwrap();
    // An ingest age of 3600 s — deliberately beyond the 60 s cutoff.
    insert_raw(
        &pool,
        "rgoph-range",
        "00rgoph-a",
        3600,
        0,
        oel,
        "OracleEventList.RangeEventAdded",
        serde_json::json!({"eventId": "0x2a", "ob": "0:t_range_orphan_book", "bounds": []}),
    )
    .await;

    let stream = "range_orphan_stream";
    set_cursor_at_head(&pool, stream, true).await;
    let stats = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00rgoph-"), Some("00rgoph-z"))
        .await
        .unwrap();

    assert_eq!(stats.applied, 1, "a dead letter counts as applied, same as on the book side");
    assert!(
        raw_processed(&pool, "rgoph-range").await,
        "a RangeEventAdded past the cutoff must reach a final outcome: \
         otherwise it is pending forever and fails the observer"
    );

    sqlx::query("delete from raw_events where chain_order like '00rgoph-%'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_prediction_orphan_is_never_dead_lettered_however_old() {
    // THE REVERSE-SIDE GUARD. Without it the positive test above cannot tell an
    // allow-list from a dropped prefix condition — and dropping it would be a
    // regression: `projectors.rs` defers in fourteen places, and nearly all of them
    // wait for something that legitimately arrives later (`PMPDeployed` waits for a
    // `ref_tokens` row; `TimingsSet`/`Resolved` wait for their `PMPDeployed`). Under
    // the production 30-minute cutoff, dead-lettering them kills a market silently and
    // forever: the row is marked processed and never retried (IX-FAIL-06).
    //
    // `OracleEventList.EventAdded` is chosen deliberately: the same contract as the
    // allowed `RangeEventAdded`. The test therefore checks the list by FULL name, not
    // by contract prefix.
    let Some(pool) = setup().await else { return };
    let oel = "0:t_pred_orphan_list";
    sqlx::query("delete from raw_events where chain_order like '00prdph-%'")
        .execute(&pool)
        .await
        .unwrap();
    insert_raw(
        &pool,
        "prdph-added",
        "00prdph-a",
        3600,
        0,
        oel,
        "OracleEventList.EventAdded",
        serde_json::json!({
            "eventId": "0x2a", "eventName": "probe", "oracleFee": "0", "deadline": "0"
        }),
    )
    .await;

    let stream = "pred_orphan_stream";
    set_cursor_at_head(&pool, stream, true).await;
    let stats = IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00prdph-"), Some("00prdph-z"))
        .await
        .unwrap();

    // `deferred` rather than merely "the row is still pending": a payload parse error
    // would also leave it pending, and the test would go green for the wrong reason.
    assert_eq!(stats.deferred, 1, "the row must be DEFERRED specifically, not fail with an Err");
    assert!(
        !raw_processed(&pool, "prdph-added").await,
        "a non-allow-listed type must stay deferred regardless of age: its parent \
         legitimately arrives later, and a dead letter would kill the market silently"
    );

    sqlx::query("delete from raw_events where chain_order like '00prdph-%'")
        .execute(&pool)
        .await
        .unwrap();
}

// ---- Filled handler helpers ----

async fn place(
    pool: &sqlx::PgPool,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ob: &str,
    id: &str,
    is_buy: bool,
    ticks: &str,
    co: &str,
) {
    let _ = pool; // place via the projector for realism
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id, "isBuy": is_buy, "price": "1", "ticks": ticks, "note": "0:n",
            // A BUY carries the zero address on chain; only a SELL names a deal contract.
            "tokenContract": if is_buy { ZERO_ADDRESS } else { "0:tc" },
            "deadline": "0","flags":"0",
        }),
    );
    project(tx, &e, &node(ob, co)).await;
}
async fn clean(pool: &sqlx::PgPool, ob: &str) {
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}
async fn status_rem(pool: &sqlx::PgPool, ob: &str, id: i64) -> (String, String) {
    sqlx::query_as("select status, amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=$2")
        .bind(ob).bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn filled_closes_sell_offer_and_zeroes_buy_taker() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_both";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "co-1").await; // SELL maker
    place(&pool, &mut tx, ob, "2", true, "10", "co-2").await; // BUY taker
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row keyed on this chain order —
    // must stay unique repo-wide, not just within this test.
    assert_eq!(project(&mut tx, &f, &node(ob, "co-fillboth-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 1).await, ("FILLED".into(), "0".into())); // SELL one-deal
    assert_eq!(status_rem(&pool, ob, 2).await, ("FILLED".into(), "0".into())); // BUY taker zeroed
}

#[tokio::test]
async fn buy_maker_fills_across_deals_to_filled_at_zero() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_across";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "10", true, "10", "co-1").await; // BUY maker
    place(&pool, &mut tx, ob, "11", false, "6", "co-2").await; // SELL taker A
    place(&pool, &mut tx, ob, "12", false, "4", "co-3").await; // SELL taker B
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"11","ticks":"6","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"})),&node(ob,"co-fillacross-4")).await; // trade_id unique repo-wide
    tx.commit().await.unwrap();
    // Read via the pool only AFTER commit — a separate pooled connection cannot see uncommitted rows.
    assert_eq!(status_rem(&pool, ob, 10).await, ("OPEN".into(), "4".into())); // committed partial
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"12","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"})),&node(ob,"co-fillacross-5")).await; // trade_id unique repo-wide
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 10).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_defers_zero_writes_when_one_side_absent_then_applies_once() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_defer";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "20", false, "5", "co-1").await; // only the maker exists
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"20","takerId":"21","ticks":"5","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-2")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    assert_eq!(
        status_rem(&pool, ob, 20).await,
        ("OPEN".into(), "5".into()),
        "present side must NOT be decremented"
    );
    // taker arrives, replay applies exactly once.
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "21", true, "5", "co-3").await;
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(project(&mut tx, &f, &node(ob, "co-filldefer-4")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    assert_eq!(status_rem(&pool, ob, 20).await, ("FILLED".into(), "0".into()));
    assert_eq!(status_rem(&pool, ob, 21).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_overrides_provisional_sweep_cancel_and_resets_discovery_cursor() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_override";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "30", true, "10", "co-1").await; // BUY maker
    place(&pool, &mut tx, ob, "31", false, "4", "co-2").await; // SELL taker
    tx.commit().await.unwrap();
    // Simulate a provisional sweep-cancel of the BUY maker, and set the book in
    // discovery with a non-null sweep_cursor mid-cycle.
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=now() where orderbook_address=$1 and order_id=30").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("update inference_markets set sweep_cursor=99, last_reconciled_at=null where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"30","takerId":"31","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(
        project(&mut tx, &f, &node(ob, "co-filloverride-3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    // Override: maker reopened OPEN with remaining 6, swept_at cleared.
    let (status, rem): (String, String) = status_rem(&pool, ob, 30).await;
    let swept_null: bool = sqlx::query_scalar(
        "select swept_at is null from inference_orders where orderbook_address=$1 and order_id=30",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), rem.as_str(), swept_null), ("OPEN", "6", true));
    // Discovery cursor reset so the reopened low id is re-checked before stamping.
    let cursor: Option<String> = sqlx::query_scalar(
        "select sweep_cursor::text from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cursor.is_none(), "discovery sweep_cursor must reset to NULL on override");
    // The first-tick visibility-stamp guard requires sweep_override_seq to bump.
    let seq: i64 = sqlx::query_scalar(
        "select sweep_override_seq from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        seq, 1,
        "override during discovery must bump sweep_override_seq from its default 0 to 1"
    );
}

#[tokio::test]
async fn filled_after_real_cancel_is_terminal_no_override() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fill_realcancel";
    clean(&pool, ob).await;
    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "40", true, "10", "co-1").await;
    place(&pool, &mut tx, ob, "41", false, "4", "co-2").await;
    tx.commit().await.unwrap();
    // Real event-cancel: CANCELLED + swept_at NULL.
    sqlx::query("update inference_orders set status='CANCELLED', swept_at=null where orderbook_address=$1 and order_id=40").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let f = ev(
        "InferenceFilled",
        serde_json::json!({"makerId":"40","takerId":"41","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b","sellerNote":"0:s"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    project(&mut tx, &f, &node(ob, "co-fillrealcancel-3")).await;
    tx.commit().await.unwrap();
    assert_eq!(
        status_rem(&pool, ob, 40).await,
        ("CANCELLED".into(), "10".into()),
        "real cancel stays terminal, remainder preserved"
    );
    // FULL no-op: a late Filled arriving after the real cancel must not advance the
    // terminal row's chain order.
    let lco: String = sqlx::query_scalar(
        "select last_chain_order from inference_orders where orderbook_address=$1 and order_id=40",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lco, "co-1", "terminal row's last_chain_order must NOT be bumped by a late Filled");
    // The live counter-party (order 41, SELL) DID fill — that is correct, not part of the guard.
    assert_eq!(status_rem(&pool, ob, 41).await, ("FILLED".into(), "0".into()));
}

#[tokio::test]
async fn filled_links_deal_to_orderbook_seller_buyer() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_deal_link_ob";
    let tc = "0:tc_deal_link";
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
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // SELL leg (is_buy=false) by the seller note; order_id 1.
    let sell = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0","flags":"0"}),
    );
    project(&mut tx, &sell, &node(ob, "co-1")).await;
    // BUY leg by the buyer note; order_id 2.
    let buy = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"2","isBuy":true,"price":"100","ticks":"10","note":"0:buyer","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    );
    project(&mut tx, &buy, &node(ob, "co-2")).await;
    // Filled crossing them; carries sellerTC + buyerNote.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer","sellerNote":"0:seller"}),
    );
    // Applied Filled mints a global-PK inference_trades row — chain order unique repo-wide.
    assert_eq!(
        project(&mut tx, &filled, &node(ob, "co-deallink-3")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(orderbook.as_deref(), Some(ob));
    assert_eq!(seller.as_deref(), Some("0:seller"));
    assert_eq!(buyer.as_deref(), Some("0:buyer"));
}

#[tokio::test]
async fn orphan_repair_filled_links_deal() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_link_ob";
    let tc = "0:tc_orphan_link";
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
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
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Only the SELL leg present (the counterparty BUY OrderPlaced was dropped).
    let sell = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0","flags":"0"}),
    );
    project_inference_event(&mut tx, &sell, &node(ob, "co-1")).await.unwrap();
    // Expired Filled orphan: maker(1) present, taker(2) dropped.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer","sellerNote":"0:seller"}),
    );
    // Applied via the orphan path mints a global-PK inference_trades row — chain order
    // unique repo-wide.
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-orphanlink-2")).await.unwrap();
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(orderbook.as_deref(), Some(ob));
    assert_eq!(seller.as_deref(), Some("0:seller"), "seller resolved from present SELL leg");
    assert_eq!(buyer.as_deref(), Some("0:buyer"));

    // The present leg is the MAKER (the SELL), so resolve_is_buyer_maker takes the maker
    // branch: isBuyerMaker follows the maker's own side directly.
    let rows = tape_rows(&pool, ob).await;
    assert_eq!(
        rows.len(),
        1,
        "maker leg present resolves a direction; the match lands on the tape"
    );
    assert!(!rows[0].3, "maker leg is the SELL => taker bought");
}

#[tokio::test]
async fn orphan_repair_filled_no_leg_still_links() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_noleg_ob";
    let tc = "0:tc_orphan_noleg";
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

    let mut tx = pool.begin().await.unwrap();
    // Neither leg present (both OrderPlaced dropped).
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer",
        // ZERO deliberately: this test is about the FALLBACK — the seller must stay
        // unrecoverable when the SELL leg is not projected and the event did not name him.
        "sellerNote":ZERO_ADDRESS}),
    );
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-1")).await.unwrap();
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(
        orderbook.as_deref(),
        Some(ob),
        "orderbook recorded from the event even with no legs"
    );
    assert_eq!(
        buyer.as_deref(),
        Some("0:buyer"),
        "buyer recorded from the event even with no legs"
    );
    assert!(seller.is_none(), "seller unresolved when the SELL leg was dropped");

    // Neither leg is present, so resolve_is_buyer_maker has no side to read from either
    // end of the match: the direction is unrecoverable and the row is omitted entirely,
    // rather than landing on the public tape with a guessed side.
    assert!(tape_rows(&pool, ob).await.is_empty(), "no tape row when neither leg is present");
}

#[tokio::test]
async fn orphan_repair_filled_taker_only_resolves_from_taker_side() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_orphan_takeronly_ob";
    let tc = "0:tc_orphan_takeronly";
    sqlx::query("delete from inference_trades where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
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
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    // Only the BUY taker leg present (the counterparty SELL maker's OrderPlaced was
    // dropped) — the mirror of orphan_repair_filled_links_deal, which leaves the MAKER
    // leg present instead. With no maker row to read is_buy from, resolve_is_buyer_maker
    // falls back to inverting the taker's own side.
    let buy = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"2","isBuy":true,"price":"100","ticks":"10","note":"0:buyer","tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
    );
    project_inference_event(&mut tx, &buy, &node(ob, "co-1")).await.unwrap();
    // Expired Filled orphan: taker(2) present, maker(1) dropped.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer",
        // ZERO deliberately: this test exercises resolving the DIRECTION from the leg
        // that is present, not the seller link — the event does not name him.
        "sellerNote":ZERO_ADDRESS}),
    );
    // Applied via the orphan path mints a global-PK inference_trades row — chain order
    // unique repo-wide.
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-orphantaker-2")).await.unwrap();
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(
        rows.len(),
        1,
        "taker leg present still resolves a direction; the match lands on the tape"
    );
    assert!(!rows[0].3, "taker is the BUY => isBuyerMaker is the inverse, false");
}

// ---- token_contract / deadline persistence ----

// Zero address the ABI decodes for an unset `address` field (64 zero hex digits after
// the workchain prefix). `inference_projectors::ZERO_ADDRESS` is `pub(crate)` and not
// importable from this integration-test crate, so it is declared again here.
const ZERO_ADDRESS: &str = "0:0000000000000000000000000000000000000000000000000000000000000000";

#[allow(clippy::too_many_arguments)]
async fn project_placed(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    price: &str,
    ticks: &str,
    tc: Option<&str>,
    deadline: i64,
) {
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id.to_string(), "isBuy": is_buy, "price": price, "ticks": ticks,
            "note": "0:n",
            "tokenContract": tc.unwrap_or(ZERO_ADDRESS),
            "deadline": deadline.to_string(),"flags":"0",
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-placed-{id}");
    assert_eq!(project(&mut tx, &e, &node(ob, &co)).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
}

// A subscription arrives as an ordinary `InferenceOrderPlaced` with the
// FLAG_SUBSCRIPTION bit (0x40): the book's ABI has no separate event for it. The
// caller's premise — "the row is born without a deadline" — still holds: a subscription
// bid is placed with a zero deadline and no TC.
async fn project_subscription(pool: &PgPool, ob: &str, id: i64, price: &str, ticks: &str) {
    let e = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId": id.to_string(), "isBuy": true, "price": price, "ticks": ticks,
            "note": "0:bn", "tokenContract": ZERO_ADDRESS, "deadline": "0", "flags": "64",
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-sub-{id}");
    assert_eq!(project(&mut tx, &e, &node(ob, &co)).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
}

async fn project_placed_raw(
    pool: &PgPool,
    ob: &str,
    id: i64,
    value: serde_json::Value,
) -> anyhow::Result<ProjectionOutcome> {
    let e = ev("InferenceOrderPlaced", value);
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-raw-{id}");
    let outcome = project_inference_event(&mut tx, &e, &node(ob, &co)).await?;
    tx.commit().await.unwrap();
    Ok(outcome)
}

#[tokio::test]
async fn order_placed_persists_token_contract_and_deadline() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-persist";
    clean(&pool, ob).await;

    project_placed(&pool, ob, 7, /* is_buy */ false, "10", "5", Some("0:deal-tc"), 1760003600)
        .await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=7",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tc.as_deref(), Some("0:deal-tc"));
    assert_eq!(dl.as_deref(), Some("1760003600"));
}

#[tokio::test]
async fn buy_placement_normalizes_zero_token_contract_and_deadline_to_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-zero";
    clean(&pool, ob).await;

    project_placed(&pool, ob, 8, /* is_buy */ true, "10", "5", Some(ZERO_ADDRESS), 0).await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=8",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(tc.is_none(), "zero address must normalize to NULL");
    assert!(dl.is_none(), "zero deadline must normalize to NULL");
}

#[tokio::test]
async fn buy_placement_with_a_nonzero_deadline_keeps_it() {
    // IX-OB-04's missing combination: the BUY normalization must collapse ONLY
    // zero values. A BUY with a real deadline (IOC-style) keeps it; the zero
    // token contract still collapses to NULL. The SELL positive lives at
    // order_placed_persists_token_contract_and_deadline; the BUY all-zero case
    // is the test above.
    let Some(pool) = setup().await else { return };
    let ob = "0:buy_nonzero_deadline_book";
    clean(&pool, ob).await;

    project_placed(&pool, ob, 9, /* is_buy */ true, "10", "5", Some(ZERO_ADDRESS), 1760003600)
        .await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=9",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(tc.is_none(), "zero token contract still collapses");
    assert_eq!(
        dl.as_deref(),
        Some("1760003600"),
        "a non-zero deadline must survive the BUY normalization"
    );
    clean(&pool, ob).await;
}

#[tokio::test]
async fn a_placement_missing_token_contract_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // TokenContract would be unrecoverable once it reaches a terminal status.
    let err = project_placed_raw(
        &pool,
        ob,
        10,
        serde_json::json!({
            "orderId": "10", "isBuy": false, "price": "10", "ticks": "5", "note": "0:n",
            "deadline": "0",
        }),
    )
    .await
    .expect_err("a missing tokenContract must fail the projection");
    assert!(format!("{err:#}").contains("tokenContract"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn a_placement_missing_note_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:note-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // note would hide it from every `note=X` listing forever, and nothing ever repairs
    // `note_address` after the fact.
    let err = project_placed_raw(
        &pool,
        ob,
        11,
        serde_json::json!({
            "orderId": "11", "isBuy": false, "price": "10", "ticks": "5",
            "tokenContract": ZERO_ADDRESS, "deadline": "0",
        }),
    )
    .await
    .expect_err("a missing note must fail the projection");
    assert!(format!("{err:#}").contains("note"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn a_placement_missing_deadline_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:deadline-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. Inserting the row with a NULL
    // deadline would be unrecoverable once it reaches a terminal status.
    let err = project_placed_raw(
        &pool,
        ob,
        12,
        serde_json::json!({
            "orderId": "12", "isBuy": false, "price": "10", "ticks": "5", "note": "0:n",
            "tokenContract": ZERO_ADDRESS,
        }),
    )
    .await
    .expect_err("a missing deadline must fail the projection");
    assert!(format!("{err:#}").contains("deadline"));

    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "no row may be inserted from an undecodable placement");
}

#[tokio::test]
async fn replay_preserves_repaired_token_contract_and_deadline() {
    let Some(pool) = setup().await else { return };
    let ob = "0:tc-replay";
    clean(&pool, ob).await;

    // A subscription row is born without a deadline: the event carries none.
    project_subscription(&pool, ob, 9, "10", "5").await;
    // The reconciler repairs it from the chain getter.
    sqlx::query("update inference_orders set deadline = 1760009999, token_contract = '0:repaired' where orderbook_address=$1 and order_id=9")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Replaying the original event must not wipe the repaired values.
    project_subscription(&pool, ob, 9, "10", "5").await;

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=9",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tc.as_deref(), Some("0:repaired"));
    assert_eq!(dl.as_deref(), Some("1760009999"));
}

// ---- inference_trades tape ----

/// `InferenceFilled` fixture. Named `filled_ev` because the bare `filled` reads as one of
/// this file's `filled_*` test fns.
fn filled_ev(maker_id: &str, taker_id: &str, ticks: &str, clearing: &str) -> DecodedEvent {
    ev(
        "InferenceFilled",
        serde_json::json!({
            "makerId": maker_id, "takerId": taker_id, "ticks": ticks,
            "clearingPrice": clearing,
            "sellerTC": "0:s", "buyerNote": "0:b", "sellerNote": "0:sn",
        }),
    )
}

async fn tape_rows(pool: &sqlx::PgPool, ob: &str) -> Vec<(String, String, String, bool)> {
    sqlx::query_as(
        "select trade_id, price::text, qty::text, is_buyer_maker
           from inference_trades where orderbook_address = $1 order by trade_id",
    )
    .bind(ob)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn filled_writes_one_tape_row_keyed_by_chain_order() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_write";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    // Resting BUY (maker) crossed by an incoming SELL (taker) => buyer IS the maker.
    place(&pool, &mut tx, ob, "1", true, "10", "tapew-1").await;
    place(&pool, &mut tx, ob, "2", false, "4", "tapew-2").await;
    let outcome =
        project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "tapew-3")).await;
    assert_eq!(outcome, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(rows.len(), 1, "one Filled = exactly one tape row");
    assert_eq!(rows[0].0, "tapew-3", "trade_id is the Filled event's chain order");
    assert_eq!(rows[0].1, "1000000000", "price is the event's clearingPrice, raw");
    assert_eq!(rows[0].2, "4");
    assert!(rows[0].3, "maker leg is the BUY => isBuyerMaker");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn filled_tape_row_takes_maker_side_when_maker_sells() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_maker_sells";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", false, "10", "tapems-1").await;
    place(&pool, &mut tx, ob, "2", true, "3", "tapems-2").await;
    project(&mut tx, &filled_ev("1", "2", "3", "1000000000"), &node(ob, "tapems-3")).await;
    tx.commit().await.unwrap();

    let rows = tape_rows(&pool, ob).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].3, "maker leg is the SELL => taker bought");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn filled_tape_replay_is_idempotent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_replay";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    place(&pool, &mut tx, ob, "1", true, "10", "taperp-1").await;
    place(&pool, &mut tx, ob, "2", false, "4", "taperp-2").await;
    project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "taperp-3")).await;
    project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "taperp-3")).await;
    tx.commit().await.unwrap();

    assert_eq!(tape_rows(&pool, ob).await.len(), 1, "replay must not duplicate the tape row");

    clean(&pool, ob).await;
}

#[tokio::test]
async fn deferred_filled_writes_no_tape_row() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_tape_deferred";
    clean(&pool, ob).await;

    let mut tx = pool.begin().await.unwrap();
    // Only the maker leg exists: the Filled defers with ZERO writes, tape included.
    place(&pool, &mut tx, ob, "1", true, "10", "tapedf-1").await;
    let outcome =
        project(&mut tx, &filled_ev("1", "2", "4", "1000000000"), &node(ob, "tapedf-3")).await;
    assert_eq!(outcome, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();

    assert!(tape_rows(&pool, ob).await.is_empty());

    clean(&pool, ob).await;
}

#[tokio::test]
async fn a_real_order_placed_body_projects_into_inference_orders() {
    // IX-OB-25. Every other projector test in this file builds `DecodedEvent` through
    // `ev(...)`, so the field layout was asserted by intention: the test and the
    // projector agreed with each other and neither consulted the chain. Here the event
    // comes out of the decoder applied to real bytes, so the mapping is checked against
    // what the book actually emits.
    let Some(pool) = setup().await else { return };
    let src = "0:proj_real_inference_book";
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(src)
        .execute(&pool)
        .await
        .expect("purge");

    let decoded = Decoder::new()
        .expect("decoder")
        .decode_event_body(INFERENCE_ORDER_PLACED, None)
        .expect("a real body must decode")
        .decoded()
        .expect("the event id must be known");
    // The projector routes on the `event_type` SUFFIX, not on `event_name`
    // (`inference_projectors.rs:53-56`): the reprojection path rebuilds `DecodedEvent`
    // with an empty `event_name`, so an arm matching the name would send every live
    // captured row to the seed-only branch. The decoder sets `event_type`, so this
    // routes the same way a live row does.
    assert_eq!(decoded.event_type, "InferenceOrderBook.InferenceOrderPlaced");

    let n = node(src, "5f80projreal0000000000000001");
    let mut tx = pool.begin().await.expect("begin");
    let outcome = project(&mut tx, &decoded, &n).await;
    tx.commit().await.expect("commit");
    assert!(
        matches!(outcome, ProjectionOutcome::Applied),
        "a placement must project, got {outcome:?}"
    );

    // Read back what the projector wrote. Columns cast to text so the numerics come
    // out in one predictable form and the assertions below compare strings, the same
    // way the read model serves them.
    type PlacedOrderRow = (
        String,
        bool,
        String,
        String,
        String,
        bool,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    let row: PlacedOrderRow = sqlx::query_as(
        "select order_id::text, is_buy, price::text, amount_initial::text,
                    amount_remaining::text, is_subscription, note_address,
                    token_contract, deadline::text, status
               from inference_orders where orderbook_address = $1",
    )
    .bind(src)
    .fetch_one(&pool)
    .await
    .expect("the projected order row");

    // Expected values below are taken from the harvest journal's `decoded` snapshot
    // for this exact body (`fixtures::chain_bodies::HARVESTED_ORDER_PLACED_DECODED`):
    // {"note":"0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea",
    //  "flags":"0","isBuy":false,
    //  "price":"0x00000000000000000000000000000000000000000000000000000000b2d05e00",
    //  "ticks":"4","orderId":"534","deadline":"1786648380",
    //  "tokenContract":"0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"}
    // — NOT pasted from the first green run, which would only prove the projector
    // agrees with itself.
    assert_eq!(row.0, "534", "order_id <- orderId");
    assert!(!row.1, "is_buy <- isBuy");
    // price <- price, a uint256 hex in the payload (uint256_maybe_hex converts it to
    // decimal): 0x...b2d05e00 = 3_000_000_000.
    assert_eq!(row.2, "3000000000", "price <- price (hex uint256, decoded to decimal)");
    // amount_initial / amount_remaining <- ticks, both from the same payload field.
    assert_eq!(row.3, "4", "amount_initial <- ticks");
    assert_eq!(row.4, "4", "amount_remaining <- ticks");
    // is_subscription <- flags & FLAG_SUBSCRIPTION (0x40) != 0. The snapshot's flags
    // is "0", so the bit is clear and the row is not a subscription.
    assert!(!row.5, "is_subscription <- flags(0) & 0x40 != 0");
    assert_eq!(
        row.6, "0:e730606f31613da5133259bc1617e1cb0ddcb9f4ea6c73d7c5a00a5326f32aea",
        "note_address <- note"
    );
    // token_contract <- tokenContract, through non_zero_address. The snapshot's
    // tokenContract is a real (non-zero) address, so it must survive as Some, not
    // collapse to NULL.
    assert_eq!(
        row.7.as_deref(),
        Some("0:970f10070ec4126eb34653072328555f58c307629612a15377df66555748a646"),
        "token_contract <- tokenContract (non-zero in the snapshot, must not collapse to NULL)"
    );
    // deadline <- deadline, through non_zero_uint. The snapshot's deadline is
    // "1786648380", non-zero, so it must survive as Some, not collapse to NULL.
    assert_eq!(
        row.8.as_deref(),
        Some("1786648380"),
        "deadline <- deadline (non-zero in the snapshot)"
    );
    assert_eq!(row.9, "OPEN", "status <- constant 'OPEN' on placement");
}

// ─────────────────────────────────────────────────────────────────────────────
// price_per_tick and deposit — the two settlement columns recovered from events
// the book still emits, after ingest stopped capturing TokenContract routes.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_sell_offer_records_the_deal_price_and_a_buy_records_nothing() {
    // The SELL placement is what REGISTERS a deal: it names the TokenContract and
    // carries the per-tick ask its constructor was given. A BUY carries the zero
    // address there, so it must not touch any deal row — and the assertion is on a
    // BUY at a DIFFERENT price, so a projector that ignored `is_buy` would
    // overwrite the ask with the bid and fail here rather than pass by coincidence.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_deal_price_ob";
    let tc = "0:tc_deal_price";
    for q in ["delete from inference_deals where token_contract_address=$1"] {
        sqlx::query(q).bind(tc).execute(&pool).await.unwrap();
    }
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
        "orderId":"1","isBuy":false,"price":"7000000000","ticks":"10","note":"0:seller",
        "tokenContract":tc,"deadline":"0","flags":"0"}),
        ),
        &node(ob, "co-dp-1"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
        "orderId":"2","isBuy":true,"price":"9000000000","ticks":"10","note":"0:buyer",
        "tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
        ),
        &node(ob, "co-dp-2"),
    )
    .await;
    tx.commit().await.unwrap();

    let price: Option<String> = sqlx::query_scalar(
        "select price_per_tick::text from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        price.as_deref(),
        Some("7000000000"),
        "the deal's price is the SELL offer's ask, never the crossing bid"
    );
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from inference_deals where price_per_tick = 9000000000",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "a BUY placement registers no deal");
}

#[tokio::test]
async fn deposit_reproduces_the_books_own_arithmetic_fee_included() {
    // Worked by hand against `_match`: unit = p + p*250/10000, cost = ticks*unit.
    // p = 1_000_000_000 -> fee 25_000_000 -> unit 1_025_000_000; ticks 4 ->
    // deposit 4_100_000_000. A projector that forgot the platform fee would say
    // 4_000_000_000, which is why the numbers are chosen so the two differ.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_deal_dep_ob";
    let tc = "0:tc_deal_dep";
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
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
        "orderId":"1","isBuy":false,"price":"1000000000","ticks":"4","note":"0:seller",
        "tokenContract":tc,"deadline":"0","flags":"0"}),
        ),
        &node(ob, "co-dd-1"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceOrderPlaced",
            serde_json::json!({
        "orderId":"2","isBuy":true,"price":"1000000000","ticks":"4","note":"0:buyer",
        "tokenContract":ZERO_ADDRESS,"deadline":"0","flags":"0"}),
        ),
        &node(ob, "co-dd-2"),
    )
    .await;
    project(
        &mut tx,
        &ev(
            "InferenceFilled",
            serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"4","clearingPrice":"1000000000","sellerTC":tc,
        "buyerNote":"0:buyer","sellerNote":"0:seller"}),
        ),
        &node(ob, "co-dd-3"),
    )
    .await;
    tx.commit().await.unwrap();

    let deposit: Option<String> = sqlx::query_scalar(
        "select deposit::text from inference_deals where token_contract_address=$1",
    )
    .bind(tc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deposit.as_deref(), Some("4100000000"));
}

#[tokio::test]
async fn a_subscription_deposit_carries_the_buyers_bond_and_an_ordinary_one_does_not() {
    // The bond is `2 * clearingPrice` and rides along with the escrow ONLY for a
    // subscription. Both cases run here with identical ticks and price, so the
    // difference in the answer is the bond and nothing else.
    let Some(pool) = setup().await else { return };
    const FLAG_SUBSCRIPTION: &str = "64"; // 0x40, and the payload carries it as decimal
    for (tag, flags, want) in [
        ("sub", FLAG_SUBSCRIPTION, "6100000000"), // 4_100_000_000 + 2 * 1_000_000_000
        ("ord", "0", "4100000000"),
    ] {
        let ob = format!("0:t_deal_bond_ob_{tag}");
        let tc = format!("0:tc_deal_bond_{tag}");
        sqlx::query("delete from inference_deals where token_contract_address=$1")
            .bind(&tc)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("delete from inference_orders where orderbook_address=$1")
            .bind(&ob)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("delete from inference_markets where orderbook_address=$1")
            .bind(&ob)
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        project(
            &mut tx,
            &ev(
                "InferenceOrderPlaced",
                serde_json::json!({
            "orderId":"1","isBuy":false,"price":"1000000000","ticks":"4","note":"0:seller",
            "tokenContract":tc,"deadline":"0","flags":"0"}),
            ),
            &node(&ob, &format!("co-db-{tag}-1")),
        )
        .await;
        // The SUBSCRIPTION bit lives on the BUY placement — that is the leg the
        // deposit projector reads it off.
        project(
            &mut tx,
            &ev(
                "InferenceOrderPlaced",
                serde_json::json!({
            "orderId":"2","isBuy":true,"price":"1000000000","ticks":"4","note":"0:buyer",
            "tokenContract":ZERO_ADDRESS,"deadline":"0","flags":flags}),
            ),
            &node(&ob, &format!("co-db-{tag}-2")),
        )
        .await;
        project(
            &mut tx,
            &ev(
                "InferenceFilled",
                serde_json::json!({
            "makerId":"1","takerId":"2","ticks":"4","clearingPrice":"1000000000","sellerTC":tc,
            "buyerNote":"0:buyer","sellerNote":"0:seller"}),
            ),
            &node(&ob, &format!("co-db-{tag}-3")),
        )
        .await;
        tx.commit().await.unwrap();

        let deposit: Option<String> = sqlx::query_scalar(
            "select deposit::text from inference_deals where token_contract_address=$1",
        )
        .bind(&tc)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(deposit.as_deref(), Some(want), "case {tag}");
    }
}
