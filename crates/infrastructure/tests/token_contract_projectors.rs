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

    let (ticks, owed): (i32, String) = sqlx::query_as(
        "select finalized_ticks, finalized_owed_total::text from inference_deals where token_contract_address=$1")
        .bind(tc).fetch_one(&pool).await.unwrap();
    assert_eq!(ticks, 2, "each distinct tick counted once");
    assert_eq!(owed, "20");
    let rows: i64 =
        sqlx::query_scalar("select count(*) from inference_ticks where token_contract_address=$1")
            .bind(tc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 2);
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
