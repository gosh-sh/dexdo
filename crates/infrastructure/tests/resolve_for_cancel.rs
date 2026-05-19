// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::resolve_for_cancel
// — the single-SELECT order-resolution query for DELETE /api/v1/order.
// Gated on TEST_DATABASE_URL.

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

async fn purge(pool: &PgPool, pmp_address: &str, symbol: &str) {
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge live_orders");
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

/// Seed a Trading market with one outcome. Returns the orderbook
/// address (== pmp_address for this fixture, same convention as
/// `resolve_for_new_order` tests). The timing block puts `now =
/// stake_end + 50` inside the Trading window.
async fn seed_trading_market(
    pool: &PgPool,
    pmp_address: &str,
    symbol: &str,
    token_type: i32,
    outcome_id: i32,
) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at,
                last_reconciled_at)
           values ($1, $1, $1, $2, 'USDC',
                   42::numeric, 1::numeric, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210,
                   now())
           returning id"#,
    )
    .bind(pmp_address)
    .bind(token_type)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, $3, 'YES', $4,
                   2, 4, '0.01', '0.0001',
                   '5.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp_address)
    .bind(outcome_id)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

#[allow(clippy::too_many_arguments)]
async fn seed_live_order(
    pool: &PgPool,
    orderbook_address: &str,
    order_id: u64,
    outcome_id: i32,
    owner_pn_address: &str,
    status: &str,
    amount_remaining: &str,
    client_order_id: Option<&str>,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy,
                price, amount_remaining, amount_initial, client_order_id,
                status, last_chain_order, owner_pn_address,
                placed_chain_order)
           values ($1, $2::numeric, $3, true,
                   615::numeric, $4::numeric, 1500000::numeric, $5,
                   $6, '0001', $7,
                   '0001')"#,
    )
    .bind(orderbook_address)
    .bind(order_id.to_string())
    .bind(outcome_id)
    .bind(amount_remaining)
    .bind(client_order_id)
    .bind(status)
    .bind(owner_pn_address)
    .execute(pool)
    .await
    .expect("insert live_orders");
}

const NOW_TRADING: i64 = 1_700_000_250;

#[tokio::test]
async fn resolve_for_cancel_happy_path() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_happy_pmp";
    let symbol = "RESOLVE_CANCEL_HAPPY_YES";
    let pn = "0:resolve_cancel_happy_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 123, 7, pn, "OPEN", "1500000", Some("42")).await;

    let resolved = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            123,
            pn,
            NOW_TRADING,
        )
        .await
        .expect("resolve happy path");

    assert_eq!(resolved.status, MarketStatus::Trading);
    assert_eq!(resolved.event_id, "42");
    assert_eq!(resolved.oracle_list_hash, "1");
    assert_eq!(resolved.token_type, 3);
    assert_eq!(resolved.client_order_id.as_deref(), Some("42"));
}

#[tokio::test]
async fn resolve_for_cancel_null_client_order_id_surfaces_as_none() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_null_cid_pmp";
    let symbol = "RESOLVE_CANCEL_NULL_CID_YES";
    let pn = "0:resolve_cancel_null_cid_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 124, 7, pn, "OPEN", "1500000", None).await;

    let resolved = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            124,
            pn,
            NOW_TRADING,
        )
        .await
        .expect("resolve happy path with NULL cid");

    assert!(resolved.client_order_id.is_none());
}

#[tokio::test]
async fn resolve_for_cancel_unknown_order_id() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_unknown_pmp";
    let symbol = "RESOLVE_CANCEL_UNKNOWN_YES";
    let pn = "0:resolve_cancel_unknown_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    // No live_orders row → UnknownOrder.

    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            999,
            pn,
            NOW_TRADING,
        )
        .await
        .expect_err("unknown order must surface as typed miss");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::UnknownOrder);
}

#[tokio::test]
async fn resolve_for_cancel_wrong_owner_is_unknown_order() {
    // Pin: existence of another account's order MUST NOT leak via
    // error-code differentiation. Wrong owner === no such order.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_wrong_owner_pmp";
    let symbol = "RESOLVE_CANCEL_WRONG_OWNER_YES";
    let real_pn = "0:resolve_cancel_wrong_owner_real_pn";
    let attacker_pn = "0:resolve_cancel_wrong_owner_attacker_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 200, 7, real_pn, "OPEN", "1500000", Some("real")).await;

    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            200,
            attacker_pn,
            NOW_TRADING,
        )
        .await
        .expect_err("wrong owner must surface as UnknownOrder");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::UnknownOrder);
}

#[tokio::test]
async fn resolve_for_cancel_wrong_market_for_order_id_is_unknown_order() {
    // Pin: the `(marketAddress, symbol)` from the request is part of
    // the where-clause. An orderId that exists under one market but is
    // queried against a different market collapses to UnknownOrder.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp_a = "0:resolve_cancel_wrong_market_pmp_a";
    let pmp_b = "0:resolve_cancel_wrong_market_pmp_b";
    let symbol_a = "RESOLVE_CANCEL_WRONG_MARKET_A";
    let symbol_b = "RESOLVE_CANCEL_WRONG_MARKET_B";
    let pn = "0:resolve_cancel_wrong_market_pn";
    purge(&pool, pmp_a, symbol_a).await;
    purge(&pool, pmp_b, symbol_b).await;
    seed_trading_market(&pool, pmp_a, symbol_a, 3, 7).await;
    seed_trading_market(&pool, pmp_b, symbol_b, 3, 7).await;
    // Order 300 lives under market A.
    seed_live_order(&pool, pmp_a, 300, 7, pn, "OPEN", "1500000", Some("xa")).await;

    // Query market B for order 300 → UnknownOrder.
    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp_b.into()),
            &Symbol(symbol_b.into()),
            300,
            pn,
            NOW_TRADING,
        )
        .await
        .expect_err("orderId under the wrong market must surface as UnknownOrder");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::UnknownOrder);
}

#[tokio::test]
async fn resolve_for_cancel_closed_order_is_unknown_order() {
    // A CANCELLED or FILLED row must not be cancellable again — the
    // ownership SELECT filters by status='OPEN'.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_closed_pmp";
    let symbol = "RESOLVE_CANCEL_CLOSED_YES";
    let pn = "0:resolve_cancel_closed_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 400, 7, pn, "CANCELLED", "0", Some("c1")).await;

    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            400,
            pn,
            NOW_TRADING,
        )
        .await
        .expect_err("CANCELLED order must surface as UnknownOrder");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::UnknownOrder);
}

#[tokio::test]
async fn resolve_for_cancel_pre_reconcile_market_is_invisible() {
    // Mirrors the read-side visibility gate: a market without
    // `last_reconciled_at` is not surfaced through the API. The cancel
    // path joins `live_orders` to `markets`; a pre-reconcile market
    // means the order is invisible too → UnknownOrder.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_pre_reconcile_pmp";
    let symbol = "RESOLVE_CANCEL_PRE_RECONCILE_YES";
    let pn = "0:resolve_cancel_pre_reconcile_pn";
    purge(&pool, pmp, symbol).await;

    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, $1,
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
           values ($1, $2, 7, 'YES', $3,
                   2, 4, '0.01', '0.0001', '5.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(&pool)
    .await
    .expect("insert market_outcomes");
    seed_live_order(&pool, pmp, 500, 7, pn, "OPEN", "1500000", Some("x")).await;

    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            500,
            pn,
            NOW_TRADING,
        )
        .await
        .expect_err("pre-reconcile market must be invisible to cancel");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::UnknownOrder);
}

#[tokio::test]
async fn resolve_for_cancel_derives_non_trading_status_for_caller_check() {
    // The repo MUST surface the actual derived status — the use case is
    // what rejects everything other than Trading. A cancelled-market
    // row should still resolve, with status = Cancelled, so the caller
    // gets `OrderValidationFailed` rather than `UnknownOrder`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_cancelled_market_pmp";
    let symbol = "RESOLVE_CANCEL_CANCELLED_MARKET_YES";
    let pn = "0:resolve_cancel_cancelled_market_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 600, 7, pn, "OPEN", "1500000", None).await;
    sqlx::query("update markets set is_cancelled = true where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .expect("flip is_cancelled");

    let resolved = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            600,
            pn,
            NOW_TRADING,
        )
        .await
        .expect("cancelled-market row must still resolve");
    assert_eq!(resolved.status, MarketStatus::Cancelled);
}
