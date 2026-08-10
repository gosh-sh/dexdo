// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference order-book projectors.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::inference_projectors::project_inference_event;
use dodex_infrastructure::inference_projectors::repair_expired_inference_orphan;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

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
        "tokenContract":ZERO_ADDRESS,"deadline":"0" }),
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
        serde_json::json!({"orderId":"9","isBuy":false,"price":"7","ticks":"10","note":"0:n","tokenContract":"0:tc","deadline":"0"}),
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
async fn subscription_placed_rests_a_buy() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_sub_ob";
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
        "InferenceSubscriptionPlaced",
        serde_json::json!({
        "orderId":"3","buyerNote":"0:bn","maxPrice":"50","ticks":"8","cycleBudget":"0","autoRenew":false }),
    );
    assert_eq!(project(&mut tx, &e, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (is_buy, is_sub, price, rem): (bool,bool,String,String) = sqlx::query_as(
        "select is_buy, is_subscription, price::text, amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=3")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((is_buy, is_sub, price.as_str(), rem.as_str()), (true, true, "50", "8"));
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
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"2","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0"})),&node(ob,"co-2")).await;
    assert_eq!(project(&mut tx, &c, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=2").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("CANCELLED", true));
}

// The chain decides expiry, not the reader: a resting order whose deadline has
// passed keeps its OPEN status until InferenceOrderExpired arrives, and only then
// becomes EXPIRED. Nothing derives the status from `deadline` vs wall-clock.
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
    project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":"3","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000"})),&node(ob,"xo-2")).await;
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
        project(&mut tx,&ev("InferenceOrderPlaced",serde_json::json!({"orderId":id,"isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"1700000000"})),&node(ob,&format!("eo-{id}"))).await;
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
    let r = ev("InferenceRefunded", serde_json::json!({"note":"0:n","amount":"1"}));
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
        value: serde_json::json!({"orderId":"7","isBuy":true,"price":"1","ticks":"3","note":"0:n","tokenContract":ZERO_ADDRESS,"deadline":"0"}),
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
async fn expired_orphans_dropped_both_types_using_ingest_age_not_chain_time() {
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
    let filled = serde_json::json!({"makerId":"900","takerId":"901","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"});
    let cancel = serde_json::json!({"orderId":"902","refunded":"0"});
    // (a) aged-ingest Filled orphan => dropped.        (b) aged-ingest OrderCancelled orphan => dropped (BOTH types).
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

    // Orphan dead-lettering only fires once capture has reached head.
    let stream = "orphan_drop_athead_stream";
    set_cursor_at_head(&pool, stream, true).await;
    IndexerRepository::new(pool.clone())
        .with_capture_stream(stream)
        .with_inference_orphan_cutoff(Duration::from_secs(60))
        .reproject_pending_from(50, Some("00orphan-"), Some("00orphan-z"))
        .await
        .unwrap();

    assert!(raw_processed(&pool, "orphan-fill").await, "aged Filled orphan must be dropped");
    assert!(
        raw_processed(&pool, "orphan-cancel").await,
        "aged OrderCancelled orphan must be dropped"
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
    let filled = serde_json::json!({"makerId":"800","takerId":"801","ticks":"1","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"});
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

    // Seed a resting BUY maker (id 700) with 10 ticks of depth via the real placement projector.
    let mut tx = pool.begin().await.unwrap();
    let placed = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
            "orderId":"700","isBuy":true,"price":"5","ticks":"10","note":"0:n",
            "tokenContract":ZERO_ADDRESS,
            "deadline":"0",
        }),
    );
    assert_eq!(
        project(&mut tx, &placed, &node(ob, "00seed-700")).await,
        ProjectionOutcome::Applied
    );
    tx.commit().await.unwrap();
    assert_eq!(order_amount_status(&pool, ob, "700").await, Some((10, "OPEN".into())));

    // Aged Filled orphan: maker 700 is present and resting; taker 701's OrderPlaced was dropped.
    let filled = serde_json::json!({"makerId":"700","takerId":"701","ticks":"3","clearingPrice":"5","sellerTC":"0:s","buyerNote":"0:b"});
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
            "deadline": "0",
        }),
    );
    project(tx, &e, &node(ob, co)).await;
}
async fn clean(pool: &sqlx::PgPool, ob: &str) {
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
        serde_json::json!({"makerId":"1","takerId":"2","ticks":"10","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
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
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"11","ticks":"6","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"})),&node(ob,"co-4")).await;
    tx.commit().await.unwrap();
    // Read via the pool only AFTER commit — a separate pooled connection cannot see uncommitted rows.
    assert_eq!(status_rem(&pool, ob, 10).await, ("OPEN".into(), "4".into())); // committed partial
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("InferenceFilled",serde_json::json!({"makerId":"10","takerId":"12","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"})),&node(ob,"co-5")).await;
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
        serde_json::json!({"makerId":"20","takerId":"21","ticks":"5","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"}),
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
    assert_eq!(project(&mut tx, &f, &node(ob, "co-4")).await, ProjectionOutcome::Applied);
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
        serde_json::json!({"makerId":"30","takerId":"31","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"}),
    );
    assert_eq!(project(&mut tx, &f, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
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
        serde_json::json!({"makerId":"40","takerId":"41","ticks":"4","clearingPrice":"1","sellerTC":"0:s","buyerNote":"0:b"}),
    );
    project(&mut tx, &f, &node(ob, "co-3")).await;
    tx.commit().await.unwrap();
    assert_eq!(
        status_rem(&pool, ob, 40).await,
        ("CANCELLED".into(), "10".into()),
        "real cancel stays terminal, remainder preserved"
    );
    // FULL no-op: the late Filled (co-3) must not advance the terminal row's chain order.
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
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0"}),
    );
    project(&mut tx, &sell, &node(ob, "co-1")).await;
    // BUY leg by the buyer note; order_id 2.
    let buy = ev(
        "InferenceOrderPlaced",
        serde_json::json!({
        "orderId":"2","isBuy":true,"price":"100","ticks":"10","note":"0:buyer","tokenContract":ZERO_ADDRESS,"deadline":"0"}),
    );
    project(&mut tx, &buy, &node(ob, "co-2")).await;
    // Filled crossing them; carries sellerTC + buyerNote.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer"}),
    );
    assert_eq!(project(&mut tx, &filled, &node(ob, "co-3")).await, ProjectionOutcome::Applied);
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
        "orderId":"1","isBuy":false,"price":"100","ticks":"10","note":"0:seller","tokenContract":tc,"deadline":"0"}),
    );
    project_inference_event(&mut tx, &sell, &node(ob, "co-1")).await.unwrap();
    // Expired Filled orphan: maker(1) present, taker(2) dropped.
    let filled = ev(
        "InferenceFilled",
        serde_json::json!({
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer"}),
    );
    repair_expired_inference_orphan(&mut tx, &filled, &node(ob, "co-2")).await.unwrap();
    tx.commit().await.unwrap();

    let (orderbook, seller, buyer): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "select orderbook_address, seller_note, buyer_note from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(orderbook.as_deref(), Some(ob));
    assert_eq!(seller.as_deref(), Some("0:seller"), "seller resolved from present SELL leg");
    assert_eq!(buyer.as_deref(), Some("0:buyer"));
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
        "makerId":"1","takerId":"2","ticks":"10","clearingPrice":"100","sellerTC":tc,"buyerNote":"0:buyer"}),
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
            "deadline": deadline.to_string(),
        }),
    );
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-placed-{id}");
    assert_eq!(project(&mut tx, &e, &node(ob, &co)).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
}

async fn project_subscription(pool: &PgPool, ob: &str, id: i64, price: &str, ticks: &str) {
    let e = ev(
        "InferenceSubscriptionPlaced",
        serde_json::json!({
            "orderId": id.to_string(), "buyerNote": "0:bn", "maxPrice": price, "ticks": ticks,
            "cycleBudget": "0", "autoRenew": false,
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

async fn project_subscription_raw(
    pool: &PgPool,
    ob: &str,
    id: i64,
    value: serde_json::Value,
) -> anyhow::Result<ProjectionOutcome> {
    let e = ev("InferenceSubscriptionPlaced", value);
    let mut tx = pool.begin().await.unwrap();
    let co = format!("co-sub-raw-{id}");
    let outcome = project_inference_event(&mut tx, &e, &node(ob, &co)).await?;
    tx.commit().await.unwrap();
    Ok(outcome)
}

#[tokio::test]
async fn a_subscription_missing_buyer_note_fails_projection_instead_of_inserting_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:buyer-note-drift";
    clean(&pool, ob).await;

    // ABI or decoder drift: the mandatory field is gone. A subscription carries neither
    // tokenContract nor deadline, so buyerNote is the only mandatory sibling field.
    let err = project_subscription_raw(
        &pool,
        ob,
        13,
        serde_json::json!({
            "orderId": "13", "maxPrice": "10", "ticks": "5",
            "cycleBudget": "0", "autoRenew": false,
        }),
    )
    .await
    .expect_err("a missing buyerNote must fail the projection");
    assert!(format!("{err:#}").contains("buyerNote"));

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
