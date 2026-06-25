// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference order-book projectors.
// Gated on TEST_DATABASE_URL like the other read-model tests.

use dodex_infrastructure::database;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::{env, time::Duration};

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
