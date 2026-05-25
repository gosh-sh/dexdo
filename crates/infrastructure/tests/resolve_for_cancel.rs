// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::resolve_for_cancel
// — the single-SELECT order-resolution query for DELETE /api/v1/order.
// Gated on TEST_DATABASE_URL.

use std::env;
use std::time::Duration;

use dodex_application::CancelBatchResolution;
use dodex_application::MarketReadRepository;
use dodex_application::OrderForCancelBatch;
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

    assert_eq!(resolved.market_status, MarketStatus::Trading);
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
async fn resolve_for_cancel_zero_remaining_open_row_is_invisible() {
    // Pin: a row with `status='OPEN'` but `amount_remaining=0` is the
    // transient slice between `OrderFilled` zeroing remaining and
    // flipping status to 'FILLED'. The SELECT carries an
    // `amount_remaining > 0` predicate precisely to keep that slice
    // out of cancel; without this test a regression that drops the
    // predicate would not fail any of the other 8 cases (they all
    // seed with non-zero remaining).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_zero_remaining_pmp";
    let symbol = "RESOLVE_CANCEL_ZERO_REMAINING_YES";
    let pn = "0:resolve_cancel_zero_remaining_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    // `status='OPEN'` but `amount_remaining=0` — the transient slice.
    seed_live_order(&pool, pmp, 700, 7, pn, "OPEN", "0", Some("z")).await;

    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            700,
            pn,
            NOW_TRADING,
        )
        .await
        .expect_err("OPEN row with amount_remaining=0 must surface as UnknownOrder");
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
    assert_eq!(resolved.market_status, MarketStatus::Cancelled);
}

// ---- resolve_for_cancel_batch -------------------------------------------
//
// Bulk variant for DELETE /api/v1/batchOrders. The SQL mirrors
// `resolve_for_cancel`'s predicate set (status='OPEN', amount_remaining>0,
// owner_pn_address match, m.last_reconciled_at IS NOT NULL) but joins
// `live_orders.order_id` against `unnest($3::text[]) WITH ORDINALITY` and
// projects `bind_idx` (the input array position) so the use case can
// reconstruct `order_id` without parsing the row back. Identity and
// market_status are lifted to `CancelBatchResolution` once — every row
// joins the same (markets, market_outcomes) snapshot, so per-row
// projection would be redundant. The single-cancel suite above already
// pins the predicate semantics; these tests focus on what is
// bulk-specific: the WITH-ORDINALITY position mapping, partial
// shortfall, and per-row owner filtering inside one request.

fn sort_orders(mut orders: Vec<OrderForCancelBatch>) -> Vec<OrderForCancelBatch> {
    // `bind_idx` is the input-position contract: PG returns matched
    // rows in arbitrary order; sorting by bind_idx restores
    // request-array order regardless of Postgres' execution choices.
    orders.sort_by_key(|r| r.bind_idx);
    orders
}

fn into_orders(resolution: Option<CancelBatchResolution>) -> Vec<OrderForCancelBatch> {
    resolution.map(|r| sort_orders(r.orders)).unwrap_or_default()
}

#[tokio::test]
async fn resolve_for_cancel_batch_happy_path_multiple_rows() {
    // Three ids requested, all three resolve. Exercises the
    // `unnest($3::text[]) WITH ORDINALITY` join on a non-trivial input
    // set — `bind_idx` must come back as the request position (0, 1, 2)
    // regardless of the order PG actually returns the rows in.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_happy_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_HAPPY_YES";
    let pn = "0:resolve_cancel_batch_happy_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 1001, 7, pn, "OPEN", "1500000", Some("a")).await;
    seed_live_order(&pool, pmp, 1002, 7, pn, "OPEN", "1500000", Some("b")).await;
    seed_live_order(&pool, pmp, 1003, 7, pn, "OPEN", "1500000", None).await;

    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[1001, 1002, 1003],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve happy path")
        .expect("matched rows yield a resolution");

    // Identity pulled up to the resolution: pinned once, not per row.
    // Seed values are `token_type=3`, `event_id=42::numeric`,
    // `oracle_list_hash=1::numeric`.
    assert_eq!(resolution.market_status, MarketStatus::Trading);
    assert_eq!(resolution.token_type, 3);
    assert_eq!(resolution.event_id, "42");
    assert_eq!(resolution.oracle_list_hash, "1");

    let orders = sort_orders(resolution.orders);
    assert_eq!(orders.len(), 3);
    // Input was [1001, 1002, 1003]; bind_idx encodes the position.
    assert_eq!(orders[0].bind_idx, 0);
    assert_eq!(orders[0].client_order_id.as_deref(), Some("a"));
    assert_eq!(orders[1].bind_idx, 1);
    assert_eq!(orders[1].client_order_id.as_deref(), Some("b"));
    assert_eq!(orders[2].bind_idx, 2);
    assert!(orders[2].client_order_id.is_none());
}

#[tokio::test]
async fn resolve_for_cancel_batch_whitespace_only_client_order_id_surfaces_as_none() {
    // `client_order_id` is `text` in Postgres — whitespace round-trips
    // verbatim. The infra layer trims and collapses blank-after-trim to
    // None so the response renders the empty-string convention. NULL
    // and non-empty are already pinned by the happy-path and partial
    // tests; without explicit whitespace seeding a future SQL-side
    // change (e.g. dropping the trim) would slip past the bulk suite.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_ws_cid_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_WS_CID_YES";
    let pn = "0:resolve_cancel_batch_ws_cid_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 9001, 7, pn, "OPEN", "1500000", Some("   ")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[9001],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve with whitespace-only client_order_id"),
    );
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].bind_idx, 0);
    assert!(orders[0].client_order_id.is_none());
}

#[tokio::test]
async fn resolve_for_cancel_batch_partial_shortfall_returns_only_matching() {
    // Three ids requested, only two exist in live_orders. The bulk SELECT
    // returns the matched subset; the use case layer is what converts a
    // shortfall into `UnknownOrder` for the whole batch. Pinning the repo
    // contract here keeps that boundary honest.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_shortfall_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_SHORTFALL_YES";
    let pn = "0:resolve_cancel_batch_shortfall_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 2001, 7, pn, "OPEN", "1500000", Some("a")).await;
    seed_live_order(&pool, pmp, 2003, 7, pn, "OPEN", "1500000", Some("c")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[2001, 2002, 2003],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve partial shortfall"),
    );
    assert_eq!(orders.len(), 2);
    // Input [2001, 2002, 2003]; bind_idx 1 (id 2002) is absent.
    assert_eq!(orders[0].bind_idx, 0);
    assert_eq!(orders[1].bind_idx, 2);
}

#[tokio::test]
async fn resolve_for_cancel_batch_wrong_owner_filtered_out() {
    // Critical bulk-specific case: a request that mixes own ids with
    // another account's id MUST NOT leak the attacker's row. The
    // `owner_pn_address = $4` predicate excludes it from the result;
    // the use case then sees a shortfall and rejects the whole batch.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_wrong_owner_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_WRONG_OWNER_YES";
    let mine = "0:resolve_cancel_batch_wrong_owner_mine_pn";
    let attacker = "0:resolve_cancel_batch_wrong_owner_attacker_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 3001, 7, mine, "OPEN", "1500000", Some("m1")).await;
    seed_live_order(&pool, pmp, 3002, 7, mine, "OPEN", "1500000", Some("m2")).await;
    seed_live_order(&pool, pmp, 3099, 7, attacker, "OPEN", "1500000", Some("att")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[3001, 3002, 3099],
            mine,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve with wrong owner mixed in"),
    );
    assert_eq!(orders.len(), 2);
    // Input [3001, 3002, 3099]; bind_idx 2 (attacker's id) is absent.
    assert_eq!(orders[0].bind_idx, 0);
    assert_eq!(orders[1].bind_idx, 1);
    assert!(orders.iter().all(|r| r.bind_idx != 2));
}

#[tokio::test]
async fn resolve_for_cancel_batch_pre_reconcile_market_invisible() {
    // Same visibility gate as single-cancel: a market without
    // `last_reconciled_at` does not expose its live_orders rows. With
    // only one such market in scope, the bulk SELECT returns empty —
    // the use case then surfaces UnknownOrder for the whole batch.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_pre_reconcile_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_PRE_RECONCILE_YES";
    let pn = "0:resolve_cancel_batch_pre_reconcile_pn";
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
    seed_live_order(&pool, pmp, 4001, 7, pn, "OPEN", "1500000", Some("x")).await;
    seed_live_order(&pool, pmp, 4002, 7, pn, "OPEN", "1500000", Some("y")).await;

    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[4001, 4002],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("pre-reconcile bulk resolve should return None, not error");
    assert!(resolution.is_none());
}

#[tokio::test]
async fn resolve_for_cancel_batch_carries_resolving_status_when_now_past_result_start() {
    // Now that the use case re-checks market_status from the bulk
    // SELECT to close the race against `resolve_for_new_order`'s
    // earlier snapshot, the repo must derive status from the same row
    // that produced the orders. Seed timings make the market RESOLVING
    // at `NOW_RESOLVING`; rows still satisfy the order-level predicates
    // (status='OPEN', amount_remaining>0) and surface, but with
    // `market_status = Resolving` so the use case rejects the batch
    // with `OrderValidationFailed` before chain dispatch.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_resolving_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_RESOLVING_YES";
    let pn = "0:resolve_cancel_batch_resolving_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 5001, 7, pn, "OPEN", "1500000", Some("r")).await;

    // result_start = 1_700_000_300; result_end = 1_700_000_400; frozen
    // is set. 1_700_000_350 lands the market in RESOLVING per
    // `compute_status`.
    const NOW_RESOLVING: i64 = 1_700_000_350;

    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[5001],
            pn,
            NOW_RESOLVING,
        )
        .await
        .expect("bulk resolve carries derived status")
        .expect("matched rows yield a resolution");

    assert_eq!(resolution.market_status, MarketStatus::Resolving);
    assert_eq!(resolution.orders.len(), 1);
    assert_eq!(resolution.orders[0].bind_idx, 0);
}

#[tokio::test]
async fn resolve_for_cancel_batch_closed_status_row_invisible() {
    // Peer of `resolve_for_cancel_closed_order_is_unknown_order`: a row
    // with `status='CANCELLED'` must not be returned by the bulk SELECT.
    // Without this test, a regression that drops `lo.status = 'OPEN'`
    // from the WITH-ORDINALITY join would still pass every other case
    // here (they all seed `'OPEN'`).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_closed_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_CLOSED_YES";
    let pn = "0:resolve_cancel_batch_closed_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 6001, 7, pn, "OPEN", "1500000", Some("ok")).await;
    seed_live_order(&pool, pmp, 6002, 7, pn, "CANCELLED", "0", Some("c")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[6001, 6002],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve filters closed rows out"),
    );
    assert_eq!(orders.len(), 1);
    // Input [6001, 6002]; only bind_idx 0 survives (6002 is CANCELLED).
    assert_eq!(orders[0].bind_idx, 0);
}

#[tokio::test]
async fn resolve_for_cancel_batch_zero_remaining_open_row_invisible() {
    // Peer of `resolve_for_cancel_zero_remaining_open_row_is_invisible`:
    // a row with `status='OPEN'` but `amount_remaining=0` is the
    // transient slice between `OrderFilled` zeroing remaining and the
    // status flip to `'FILLED'`. The `amount_remaining > 0` predicate
    // keeps it out of cancel; without this test a regression that drops
    // the predicate would slip past the other cases (all seed non-zero).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_zero_remaining_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_ZERO_REMAINING_YES";
    let pn = "0:resolve_cancel_batch_zero_remaining_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 7001, 7, pn, "OPEN", "1500000", Some("ok")).await;
    seed_live_order(&pool, pmp, 7002, 7, pn, "OPEN", "0", Some("z")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[7001, 7002],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve filters zero-remaining rows out"),
    );
    assert_eq!(orders.len(), 1);
    // Input [7001, 7002]; only bind_idx 0 survives (7002 has amount_remaining=0).
    assert_eq!(orders[0].bind_idx, 0);
}

#[tokio::test]
async fn resolve_for_cancel_batch_other_symbol_on_same_market_invisible() {
    // Pins the `mo.symbol = $2` join filter: an order on the NO book of
    // a two-outcome market must NOT surface when the request queries
    // the YES symbol of the same market. Without this test a regression
    // that drops the symbol predicate would let one outcome's owner
    // cancel another outcome's order on the same `pmp_address`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_other_symbol_pmp";
    let symbol_yes = "RESOLVE_CANCEL_BATCH_OTHER_SYMBOL_YES";
    let symbol_no = "RESOLVE_CANCEL_BATCH_OTHER_SYMBOL_NO";
    let pn = "0:resolve_cancel_batch_other_symbol_pn";
    purge(&pool, pmp, symbol_yes).await;
    purge(&pool, pmp, symbol_no).await;
    seed_trading_market(&pool, pmp, symbol_yes, 3, 7).await;

    // Add a second outcome (NO, outcome_id=8) onto the same market.
    let market_id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(pmp)
        .fetch_one(&pool)
        .await
        .expect("fetch market id");
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 8, 'NO', $3,
                   2, 4, '0.01', '0.0001', '5.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol_no)
    .execute(&pool)
    .await
    .expect("insert NO outcome");

    seed_live_order(&pool, pmp, 8001, 7, pn, "OPEN", "1500000", Some("y")).await;
    seed_live_order(&pool, pmp, 8002, 8, pn, "OPEN", "1500000", Some("n")).await;

    // Query YES; only the YES-side order may come back.
    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol_yes.into()),
            &[8001, 8002],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve filters other-outcome rows out"),
    );
    assert_eq!(orders.len(), 1);
    // Input [8001, 8002]; only bind_idx 0 survives (8002 is on the NO book).
    assert_eq!(orders[0].bind_idx, 0);
}

#[tokio::test]
async fn resolve_for_cancel_batch_u64_max_id_round_trips_via_with_ordinality() {
    // Pins the ceiling of the public-API id range. The SELECT binds
    // ids as text[] and casts to numeric[] inside `unnest WITH
    // ORDINALITY`; the projection then carries `bind_idx`, not the
    // numeric value, so values up to `u64::MAX` never round-trip
    // through `i64`/`u64::parse` and the application reconstructs
    // `order_id` from `input.order_ids[bind_idx]`. Without this test a
    // regression that re-introduced a `numeric::text::parse::<u64>`
    // round-trip on the matched ids would compile fine but silently
    // truncate or panic at the boundary.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_u64_max_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_U64_MAX_YES";
    let pn = "0:resolve_cancel_batch_u64_max_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, u64::MAX, 7, pn, "OPEN", "1500000", Some("ceil")).await;
    seed_live_order(&pool, pmp, u64::MAX - 1, 7, pn, "OPEN", "1500000", Some("ceil-1")).await;

    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[u64::MAX, u64::MAX - 1],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("u64::MAX-class ids match")
        .expect("matched rows yield a resolution");

    let orders = sort_orders(resolution.orders);
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].bind_idx, 0);
    assert_eq!(orders[0].client_order_id.as_deref(), Some("ceil"));
    assert_eq!(orders[1].bind_idx, 1);
    assert_eq!(orders[1].client_order_id.as_deref(), Some("ceil-1"));
}
