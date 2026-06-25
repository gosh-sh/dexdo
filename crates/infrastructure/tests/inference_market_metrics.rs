// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for the inference-market state + staleness metric queries.
// Gated on TEST_DATABASE_URL: when unset the test prints a skip notice and
// returns.
//
//   cargo test -p dodex-infrastructure --test inference_market_metrics

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

// State queries are whole-table aggregates, so this test asserts deltas (counts)
// and lower bounds (staleness) rather than absolute values — other rows in the
// shared test DB are expected. model_hash is left NULL on every row: the UNIQUE
// partial index `inference_markets_model_hash_idx` ignores NULLs, so distinct
// rows never collide.
#[tokio::test]
async fn state_counts_and_staleness_reflect_inserted_rows() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let discovering = "inf_metrics_test.discovering";
    let visible = "inf_metrics_test.visible";
    let failing = "inf_metrics_test.failing";

    let addrs =
        vec![discovering.to_string(), visible.to_string(), failing.to_string()];
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("purge");

    let (d0, v0, f0) =
        repo.inference_market_state_counts().await.expect("state counts before");

    // discovering: seeded, never reconciled, never failed.
    sqlx::query("insert into inference_markets (orderbook_address) values ($1)")
        .bind(discovering)
        .execute(&pool)
        .await
        .expect("insert discovering");
    // visible: reconciled, with a deliberately stale price (1000s) and sweep (500s).
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, last_reconciled_at, reference_price_at, last_swept_at)
           values ($1, now(), now() - interval '1000 seconds', now() - interval '500 seconds')"#,
    )
    .bind(visible)
    .execute(&pool)
    .await
    .expect("insert visible");
    // failing: invisible, has recorded a failure.
    sqlx::query(
        "insert into inference_markets (orderbook_address, last_reconcile_failed_at) values ($1, now())",
    )
    .bind(failing)
    .execute(&pool)
    .await
    .expect("insert failing");

    let (d1, v1, f1) =
        repo.inference_market_state_counts().await.expect("state counts after");
    assert_eq!(d1 - d0, 1, "discovering bucket should gain exactly 1");
    assert_eq!(v1 - v0, 1, "visible bucket should gain exactly 1");
    assert_eq!(f1 - f0, 1, "failing bucket should gain exactly 1");

    // Our visible row makes the oldest reference_price_at at least 1000s old and
    // the oldest last_swept_at at least 500s old; any other visible row can only
    // be older (larger lag), never younger — so these lower bounds always hold.
    let (price_lag, sweep_lag) =
        repo.inference_staleness_seconds().await.expect("staleness");
    assert!(price_lag >= 1000, "price_lag {price_lag} should be >= 1000");
    assert!(sweep_lag >= 500, "sweep_lag {sweep_lag} should be >= 500");

    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&addrs)
        .execute(&pool)
        .await
        .expect("cleanup");
}

// Whole-table aggregate, so assert deltas (other rows in the shared test DB are
// expected). Orders use a test-unique orderbook_address; status values exercise
// each of the three buckets.
#[tokio::test]
async fn order_status_counts_reflect_inserted_rows() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let ob = "inf_metrics_test.orders";
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("purge");

    let (o0, f0, c0) =
        repo.inference_order_status_counts().await.expect("order counts before");

    // One order in each status. Only the NOT NULL columns are set; order_id is
    // the per-book PK component.
    for (order_id, status) in [(1i64, "OPEN"), (2, "FILLED"), (3, "CANCELLED")] {
        sqlx::query(
            r#"insert into inference_orders
                   (orderbook_address, order_id, is_buy, price, amount_initial,
                    amount_remaining, status, last_chain_order)
               values ($1, $2, true, 100, 100, 100, $3, $2::text)"#,
        )
        .bind(ob)
        .bind(order_id)
        .bind(status)
        .execute(&pool)
        .await
        .expect("insert order");
    }

    let (o1, f1, c1) =
        repo.inference_order_status_counts().await.expect("order counts after");
    assert_eq!(o1 - o0, 1, "OPEN bucket should gain exactly 1");
    assert_eq!(f1 - f0, 1, "FILLED bucket should gain exactly 1");
    assert_eq!(c1 - c0, 1, "CANCELLED bucket should gain exactly 1");

    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("cleanup");
}
