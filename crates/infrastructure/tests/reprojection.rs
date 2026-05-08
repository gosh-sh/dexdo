// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for IndexerRepository::reproject_pending. Gated on the
// TEST_DATABASE_URL env var: when unset, every test prints a skip notice and
// returns early. Set it to a Postgres URL the suite is allowed to migrate
// (the suite calls `database::run_migrations`). Tests use unique per-test
// prefixes for msg_ids and addresses so they can run concurrently against
// the same database without colliding.
//
//   TEST_DATABASE_URL=postgres://user:pass@localhost:5432/db \
//       cargo test -p dodex-infrastructure --test reprojection

use std::env;
use std::time::Duration;

use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;

async fn setup() -> Option<PgPool> {
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

async fn purge(pool: &PgPool, queries: &[(&str, &str)]) {
    for (sql, key) in queries {
        sqlx::query(sql).bind(*key).execute(pool).await.expect("purge");
    }
}

async fn insert_raw(
    pool: &PgPool,
    msg_id: &str,
    src: &str,
    event_type: &str,
    decoded: &serde_json::Value,
) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, to_timestamp($2), $3, $3, $4, '{}'::jsonb, $5)"#,
    )
    .bind(msg_id)
    .bind(1_700_000_000_f64)
    .bind(src)
    .bind(event_type)
    .bind(decoded)
    .execute(pool)
    .await
    .expect("insert raw_events");
}

async fn processed_at_is_set(pool: &PgPool, msg_id: &str) -> bool {
    sqlx::query_scalar("select processed_at is not null from raw_events where msg_id = $1")
        .bind(msg_id)
        .fetch_one(pool)
        .await
        .expect("read processed_at")
}

#[tokio::test]
async fn applied_outcome_stamps_processed_at_and_writes_read_model() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_applied_oracle";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-name");
    let msg_id = format!("{test}-msg");

    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "oracle": oracle_addr,
        "pubkey": "0x0000000000000000000000000000000000000000000000000000000000001234",
        "name": oracle_name,
    });
    insert_raw(&pool, &msg_id, &oracle_addr, "RootOracle.OracleDeployed", &decoded).await;

    repo.reproject_pending(1000).await.expect("reproject");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "Applied outcome must stamp processed_at"
    );

    let oracle_exists: bool = sqlx::query_scalar(
        "select exists(select 1 from oracles where address = $1)",
    )
    .bind(&oracle_addr)
    .fetch_one(&pool)
    .await
    .expect("oracle exists");
    assert!(oracle_exists, "projector must populate oracles on Applied");
}

#[tokio::test]
async fn deferred_row_is_replayed_after_parent_arrives() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_deferred_eventlist";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let oracle_deploy_msg = format!("{test}-oracle-deploy");
    let eventlist_addr = format!("0:{test}_evlist");
    let msg_id = format!("{test}-evlist-msg");

    purge(
        &pool,
        &[
            ("delete from oracle_event_lists where address = $1", eventlist_addr.as_str()),
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "eventListAddress": eventlist_addr,
        "index": "1",
    });
    insert_raw(&pool, &msg_id, &oracle_addr, "Oracle.OracleEventListDeployed", &decoded).await;

    // Pass 1: parent OracleDeployed has not been seen → Deferred.
    repo.reproject_pending(1000).await.expect("reproject pass 1");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "deferred row must keep processed_at null until the parent appears"
    );

    let evlist_count: i64 = sqlx::query_scalar(
        "select count(*) from oracle_event_lists where address = $1",
    )
    .bind(&eventlist_addr)
    .fetch_one(&pool)
    .await
    .expect("count event lists pass 1");
    assert_eq!(evlist_count, 0, "no projection should happen while parent is missing");

    // Insert the parent oracle directly (simulating the OracleDeployed projector).
    sqlx::query(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, $4)"#,
    )
    .bind(&oracle_name)
    .bind(&oracle_addr)
    .bind(&oracle_deploy_msg)
    .bind("0xff")
    .execute(&pool)
    .await
    .expect("insert parent oracle");

    // Pass 2: parent now exists → Applied.
    repo.reproject_pending(1000).await.expect("reproject pass 2");
    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "processed_at must be stamped once the parent is present"
    );

    let evlist_count: i64 = sqlx::query_scalar(
        "select count(*) from oracle_event_lists where address = $1",
    )
    .bind(&eventlist_addr)
    .fetch_one(&pool)
    .await
    .expect("count event lists pass 2");
    assert_eq!(evlist_count, 1, "Applied outcome must populate oracle_event_lists");
}

#[tokio::test]
async fn already_processed_rows_are_not_picked_up() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_already_processed";
    let oracle_addr = format!("0:{test}_oracle");
    let msg_id = format!("{test}-msg");

    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let frozen_ts = "2020-01-01T00:00:00+00:00";
    let decoded = json!({
        "oracle": oracle_addr,
        "pubkey": "0x00",
        "name": format!("{test}-name"),
    });

    sqlx::query(
        r#"insert into raw_events
               (msg_id, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded, processed_at)
           values ($1, to_timestamp(1700000000), $2, $2, $3, '{}'::jsonb, $4, $5::timestamptz)"#,
    )
    .bind(&msg_id)
    .bind(&oracle_addr)
    .bind("RootOracle.OracleDeployed")
    .bind(&decoded)
    .bind(frozen_ts)
    .execute(&pool)
    .await
    .expect("insert pre-processed raw_events");

    repo.reproject_pending(1000).await.expect("reproject");

    let processed_at_str: String = sqlx::query_scalar(
        "select processed_at::text from raw_events where msg_id = $1",
    )
    .bind(&msg_id)
    .fetch_one(&pool)
    .await
    .expect("read processed_at");
    assert!(
        processed_at_str.starts_with("2020-01-01"),
        "processed_at on already-processed row must not be overwritten, got {processed_at_str}"
    );

    let oracle_exists: bool = sqlx::query_scalar(
        "select exists(select 1 from oracles where address = $1)",
    )
    .bind(&oracle_addr)
    .fetch_one(&pool)
    .await
    .expect("oracle exists");
    assert!(
        !oracle_exists,
        "projector must not run for rows that already carry processed_at"
    );
}

#[tokio::test]
async fn unknown_event_type_is_marked_processed() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_unknown_event";
    let msg_id = format!("{test}-msg");

    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id.as_str())]).await;

    // OrderBook.OrderPlaced is decoded by the ABI but has no projector wired
    // up yet → projectors::project_event returns Unknown.
    insert_raw(
        &pool,
        &msg_id,
        "0:reproj_unknown_event_src",
        "OrderBook.OrderPlaced",
        &json!({}),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "Unknown outcome must stamp processed_at to keep the row out of the retry queue"
    );
}
