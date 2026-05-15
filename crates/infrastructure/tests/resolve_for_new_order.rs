// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::resolve_for_new_order
// — the single-SELECT replacement for the trading-path market resolution
// previously routed through list_markets. Gated on TEST_DATABASE_URL.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::Symbol;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
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

/// Seed a fully-reconciled market in `Trading` status: stake_start ≤ now
/// < result_start, frozen_at set, no cancellation/resolution. `now` for
/// the test is computed from `stake_end + 1` so the status reliably
/// derives to Trading regardless of wall-clock.
async fn seed_trading_market(
    pool: &PgPool,
    pmp_address: &str,
    symbol: &str,
    token_type: i32,
    oracle_list_hash: &str,
) -> i64 {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at,
                last_reconciled_at)
           values ($1, $1, $1, $2, 'USDC',
                   42::numeric, $3::numeric, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210,
                   now())
           returning id"#,
    )
    .bind(pmp_address)
    .bind(token_type)
    .bind(oracle_list_hash)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 7, 'YES', $3,
                   2, 4, '0.01', '0.0001',
                   '5.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp_address)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");

    market_id
}

#[tokio::test]
async fn resolve_for_new_order_happy_path_returns_slim_projection() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_new_order_happy_pmp";
    let symbol = "RESOLVE_NEW_ORDER_HAPPY_YES";
    purge_market(&pool, pmp, symbol).await;
    // oracle_list_hash = 1 (decimal, non-zero) → exposed as decimal text.
    seed_trading_market(&pool, pmp, symbol, 3, "1").await;

    // now between stake_start and result_start → Trading.
    let now = 1_700_000_250;
    let resolved = repo
        .resolve_for_new_order(&MarketAddress(pmp.into()), &Symbol(symbol.into()), now)
        .await
        .expect("resolve happy path");

    assert_eq!(resolved.status, MarketStatus::Trading);
    assert_eq!(resolved.event_id, "42");
    assert_eq!(resolved.oracle_list_hash, "1");
    assert_eq!(resolved.token_type, 3);
    assert_eq!(resolved.outcome.outcome_id, 7);
    assert_eq!(resolved.outcome.symbol, Symbol(symbol.into()));
    assert_eq!(resolved.outcome.price_precision, 2);
    assert_eq!(resolved.outcome.quantity_precision, 4);
    assert_eq!(resolved.outcome.tick_size, "0.01");
    assert_eq!(resolved.outcome.step_size, "0.0001");
    assert_eq!(resolved.outcome.min_notional, "5.00");
    assert_eq!(resolved.outcome.max_batch_size, 100);
}

#[tokio::test]
async fn resolve_for_new_order_unknown_market_is_typed_miss() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let err = repo
        .resolve_for_new_order(
            &MarketAddress("0:resolve_new_order_no_such_market".into()),
            &Symbol("WHATEVER".into()),
            1_700_000_250,
        )
        .await
        .expect_err("unknown market must surface as typed miss");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);
}

#[tokio::test]
async fn resolve_for_new_order_unknown_symbol_is_typed_miss() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_new_order_symbol_miss_pmp";
    let symbol = "RESOLVE_NEW_ORDER_SYMBOL_MISS_YES";
    purge_market(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, "1").await;

    let err = repo
        .resolve_for_new_order(
            &MarketAddress(pmp.into()),
            &Symbol("PM-DOES-NOT-EXIST".into()),
            1_700_000_250,
        )
        .await
        .expect_err("unknown symbol within market must surface as typed miss");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);
}

#[tokio::test]
async fn resolve_for_new_order_pre_reconcile_row_is_invisible() {
    // Mirrors the read-side contract: a market without `last_reconciled_at`
    // is not yet visible to the API. The trading path must not see it
    // either — return InvalidMarketOrSymbol, the same shape the public
    // surface uses for "no such market".
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_new_order_pre_reconcile_pmp";
    let symbol = "RESOLVE_NEW_ORDER_PRE_RECONCILE_YES";
    purge_market(&pool, pmp, symbol).await;

    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, null,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210)
           returning id"#,
    )
    .bind(pmp)
    .fetch_one(&pool)
    .await
    .expect("insert pre-reconcile market");
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   2, 4, '0.01', '0.0001', '5.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(&pool)
    .await
    .expect("insert market_outcomes");

    let err = repo
        .resolve_for_new_order(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 1_700_000_250)
        .await
        .expect_err("pre-reconcile market must not be visible");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);
}

#[tokio::test]
async fn resolve_for_new_order_derives_cancelled_status() {
    // The use case rejects everything except Trading, but the resolver
    // itself must surface the actual derived status so the rejection
    // happens at the application boundary rather than via a generic
    // "not found". A cancelled market should resolve, with status =
    // Cancelled, leaving the use case to fail closed.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_new_order_cancelled_pmp";
    let symbol = "RESOLVE_NEW_ORDER_CANCELLED_YES";
    purge_market(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, "1").await;
    sqlx::query("update markets set is_cancelled = true where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .expect("flip is_cancelled");

    let resolved = repo
        .resolve_for_new_order(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 1_700_000_250)
        .await
        .expect("resolve cancelled");
    assert_eq!(resolved.status, MarketStatus::Cancelled);
}
