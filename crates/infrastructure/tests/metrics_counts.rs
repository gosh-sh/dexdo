// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for IndexerRepository::count_events_by_type. Gated on
// TEST_DATABASE_URL: when unset the test prints a skip notice and returns.
//
//   TEST_DATABASE_URL=postgres://user:pass@localhost:5432/db \
//       cargo test -p dodex-infrastructure --test metrics_counts

use std::collections::HashMap;
use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use serde_json::json;
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

async fn insert_raw(pool: &PgPool, msg_id: &str, event_type: &str) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $1, now(), null, null, $2, '{}'::jsonb, $3)
           on conflict (msg_id) do nothing"#,
    )
    .bind(msg_id)
    .bind(event_type)
    .bind(json!({}))
    .execute(pool)
    .await
    .expect("insert raw_events");
}

#[tokio::test]
async fn count_events_by_type_returns_per_type_counts() {
    let Some(pool) = setup().await else { return };

    // Synthetic, test-unique event types: isolated from real rows and from
    // other concurrent tests that share this database.
    let created = "metrics_counts_test.Created";
    let partial = "metrics_counts_test.Partial";
    let other = "metrics_counts_test.Other";
    let never = "metrics_counts_test.Never";

    sqlx::query("delete from raw_events where event_type = any($1)")
        .bind(vec![created.to_string(), partial.to_string(), other.to_string(), never.to_string()])
        .execute(&pool)
        .await
        .expect("purge");

    for i in 0..3 {
        insert_raw(&pool, &format!("metrics_counts_test.c.{i}"), created).await;
    }
    for i in 0..2 {
        insert_raw(&pool, &format!("metrics_counts_test.p.{i}"), partial).await;
    }
    insert_raw(&pool, "metrics_counts_test.o.0", other).await;

    let repo = IndexerRepository::new(pool.clone());
    let rows =
        repo.count_events_by_type(&[created, partial, never]).await.expect("count_events_by_type");
    let counts: HashMap<String, i64> = rows.into_iter().collect();

    assert_eq!(counts.get(created), Some(&3));
    assert_eq!(counts.get(partial), Some(&2));
    // Types with zero rows are omitted (the caller defaults them to 0).
    assert_eq!(counts.get(never), None);
    // Untracked types are never returned, even though rows exist for them.
    assert_eq!(counts.get(other), None);
}
