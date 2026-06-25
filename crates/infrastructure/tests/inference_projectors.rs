// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference order-book projectors.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use dodex_infrastructure::database;
use dodex_infrastructure::inference_projectors::project_inference_event;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Postgres, Transaction};
use std::{env, time::Duration};

fn ev(event_name: &str, value: serde_json::Value) -> DecodedEvent {
    DecodedEvent { contract_kind: "InferenceOrderBook", event_name: event_name.to_string(),
        event_type: format!("InferenceOrderBook.{event_name}"), value }
}
fn node(src: &str, chain_order: &str) -> EventNode {
    EventNode { msg_id: format!("m_{chain_order}"), msg_chain_order: Some(chain_order.to_string()),
        src: Some(src.to_string()), src_dapp_id: None, dst: None, body: None,
        created_at: Some(serde_json::json!(1_700_000_000)) }
}
async fn project(tx: &mut Transaction<'_, Postgres>, e: &DecodedEvent, n: &EventNode) -> ProjectionOutcome {
    project_inference_event(tx, e, n).await.unwrap()
}

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => { eprintln!("skipping: TEST_DATABASE_URL not set"); return None; }
    };
    let pool = PgPoolOptions::new().max_connections(2)
        .acquire_timeout(Duration::from_secs(5)).connect(&url).await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

#[tokio::test]
async fn skeleton_insert_needs_only_orderbook_and_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:skeleton_smoke_ob";
    sqlx::query("delete from inference_markets where orderbook_address = $1").bind(ob)
        .execute(&pool).await.unwrap();
    // Skeleton: only the two seed columns. Must not violate NOT NULL anywhere.
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain)
         values ($1, to_timestamp(1700000000)) on conflict (orderbook_address) do nothing")
        .bind(ob).execute(&pool).await.expect("skeleton insert must succeed");
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
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let e = ev("OrderPlaced", serde_json::json!({
        "orderId":"5","isBuy":true,"price":"100","ticks":"10","note":"0:note5",
        "tokenContract":"0:tc","deadline":"0" }));
    assert_eq!(project(&mut tx, &e, &node(ob, "co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();

    // Market skeleton seeded, still invisible.
    let reconciled: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("select last_reconciled_at from inference_markets where orderbook_address=$1")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert!(reconciled.is_none());
    // Order rests OPEN with full amount.
    let (status, init, rem, is_buy): (String, String, String, bool) = sqlx::query_as(
        "select status, amount_initial::text, amount_remaining::text, is_buy from inference_orders where orderbook_address=$1 and order_id=5")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), init.as_str(), rem.as_str(), is_buy), ("OPEN","10","10",true));
}

#[tokio::test]
async fn order_placed_replay_does_not_reset_partial_fill() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_op_replay_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let e = ev("OrderPlaced", serde_json::json!({"orderId":"9","isBuy":false,"price":"7","ticks":"10","note":"0:n","tokenContract":"0:tc","deadline":"0"}));
    project(&mut tx, &e, &node(ob,"co-1")).await;
    tx.commit().await.unwrap();
    // Simulate a partial fill landing (manually) then replay the placement.
    sqlx::query("update inference_orders set amount_remaining=4 where orderbook_address=$1 and order_id=9").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx, &e, &node(ob,"co-1")).await; // replay
    tx.commit().await.unwrap();
    let rem: String = sqlx::query_scalar("select amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=9").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(rem, "4", "replay must not reset amount_remaining to full ticks");
}

#[tokio::test]
async fn subscription_placed_rests_a_buy() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_sub_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let e = ev("SubscriptionPlaced", serde_json::json!({
        "orderId":"3","buyerNote":"0:bn","maxPrice":"50","ticks":"8","cycleBudget":"0","autoRenew":false }));
    assert_eq!(project(&mut tx,&e,&node(ob,"co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (is_buy, is_sub, price, rem): (bool,bool,String,String) = sqlx::query_as(
        "select is_buy, is_subscription, price::text, amount_remaining::text from inference_orders where orderbook_address=$1 and order_id=3")
        .bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((is_buy,is_sub,price.as_str(),rem.as_str()), (true,true,"50","8"));
}

#[tokio::test]
async fn order_cancelled_is_terminal_and_defers_when_absent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_cancel_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    // Cancel with no prior placement => Deferred (zero writes).
    let mut tx = pool.begin().await.unwrap();
    let c = ev("OrderCancelled", serde_json::json!({"orderId":"2","refundedShell":"0"}));
    assert_eq!(project(&mut tx,&c,&node(ob,"co-1")).await, ProjectionOutcome::Deferred);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1 and order_id=2").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
    // Place then cancel => CANCELLED, swept_at NULL.
    let mut tx = pool.begin().await.unwrap();
    project(&mut tx,&ev("OrderPlaced",serde_json::json!({"orderId":"2","isBuy":true,"price":"1","ticks":"5","note":"0:n","tokenContract":"0:tc","deadline":"0"})),&node(ob,"co-2")).await;
    assert_eq!(project(&mut tx,&c,&node(ob,"co-3")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let (status, swept_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is null from inference_orders where orderbook_address=$1 and order_id=2").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), swept_null), ("CANCELLED", true));
}

#[tokio::test]
async fn observability_event_seeds_market_only() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_obs_ob";
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let r = ev("Refunded", serde_json::json!({"note":"0:n","amount":"1"}));
    assert_eq!(project(&mut tx,&r,&node(ob,"co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let m: i64 = sqlx::query_scalar("select count(*) from inference_markets where orderbook_address=$1").bind(ob).fetch_one(&pool).await.unwrap();
    let o: i64 = sqlx::query_scalar("select count(*) from inference_orders where orderbook_address=$1").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!((m,o),(1,0), "observability seeds the market but creates no order");
}

#[tokio::test]
async fn routes_by_event_type_when_event_name_is_empty() {
    // The reprojection loop reconstructs DecodedEvent with event_name EMPTY (only
    // event_type is persisted). This guards against routing on event_name, which
    // would send every live captured row to the seed-only path.
    let Some(pool)=setup().await else {return}; let ob="0:t_empty_name";
    sqlx::query("delete from inference_orders where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1").bind(ob).execute(&pool).await.unwrap();
    let mut tx=pool.begin().await.unwrap();
    let loop_shaped = DecodedEvent {
        contract_kind: "", event_name: String::new(),                 // <-- as the live loop builds it
        event_type: "InferenceOrderBook.OrderPlaced".to_string(),
        value: serde_json::json!({"orderId":"7","isBuy":true,"price":"1","ticks":"3","note":"0:n","tokenContract":"0:tc","deadline":"0"}),
    };
    assert_eq!(project(&mut tx,&loop_shaped,&node(ob,"co-1")).await, ProjectionOutcome::Applied);
    tx.commit().await.unwrap();
    let status: String = sqlx::query_scalar("select status from inference_orders where orderbook_address=$1 and order_id=7").bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "OPEN", "empty event_name must still reach the OrderPlaced handler");
}
