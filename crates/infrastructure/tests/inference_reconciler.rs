// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference reconciler path.
// Gated on TEST_DATABASE_URL: unset → skip.
//
//   cargo test -p dodex-infrastructure --test inference_reconciler

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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
async fn at_head_round_trips() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head) values ('t_at_head','c',true)
         on conflict (stream_name) do update set at_head = excluded.at_head",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(repo.at_head("t_at_head").await.unwrap());
    sqlx::query("update indexer_cursors set at_head=false where stream_name='t_at_head'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!repo.at_head("t_at_head").await.unwrap());
    // Missing stream ⇒ not at head.
    assert!(!repo.at_head("t_missing_stream").await.unwrap());
}
