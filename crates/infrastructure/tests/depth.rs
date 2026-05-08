// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::get_depth focused on the
// "OrderBook not deployed yet" empty-book contract documented in
// services/api/README.md. Gated on TEST_DATABASE_URL — see
// crates/infrastructure/tests/reprojection.rs for the docker-compose harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

async fn purge_market(pool: &PgPool, pmp_address: &str, symbol: &str) {
    sqlx::query("delete from market_outcomes where symbol = $1")
        .bind(symbol)
        .execute(pool)
        .await
        .expect("purge market_outcomes");
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge markets");
}

async fn insert_market_with_outcome(
    pool: &PgPool,
    pmp_address: &str,
    symbol: &str,
    orderbook_address: Option<&str>,
) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())
           returning id"#,
    )
    .bind(pmp_address)
    .bind(orderbook_address)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   2, 2, '0.01', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp_address)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

#[tokio::test]
async fn null_orderbook_address_returns_empty_book() {
    // services/api/README.md:97-99 contracts that an existing market whose
    // OrderBook has not been deployed yet must yield an empty book with
    // lastUpdateId = 0. markets.orderbook_address is nullable in migration
    // 0001, so a literal NULL must take the same path as a blank string —
    // not surface as a sqlx decode error.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_null_orderbook_pmp";
    let symbol = "DEPTH_NULL_ORDERBOOK_YES";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, None).await;

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth must not error on NULL orderbook_address");

    assert_eq!(depth.last_update_id, 0);
    assert!(depth.bids.is_empty(), "bids must be empty for not-yet-deployed orderbook");
    assert!(depth.asks.is_empty(), "asks must be empty for not-yet-deployed orderbook");
}

#[tokio::test]
async fn blank_orderbook_address_returns_empty_book() {
    // Same contract as the NULL case but for a whitespace-only string. Kept
    // separate to pin both branches of `Option::and_then(filter_orderbook)`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_blank_orderbook_pmp";
    let symbol = "DEPTH_BLANK_ORDERBOOK_YES";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, Some("   ")).await;

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth must not error on blank orderbook_address");

    assert_eq!(depth.last_update_id, 0);
    assert!(depth.bids.is_empty());
    assert!(depth.asks.is_empty());
}
