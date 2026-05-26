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
async fn resolve_for_cancel_blank_oracle_list_hash_fails_closed() {
    // Same invariant as resolve_for_new_order: a reconciled market must
    // carry a non-blank oracle_list_hash. The cancel path also lifts a
    // NULL value to MarketInconsistent at the repo boundary.
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfc_blank_ohash_pmp";
    let symbol = "RFC_BLANK_OHASH_YES";
    let owner = "0:rfc_blank_ohash_owner";
    purge(&pool, pmp, symbol).await;

    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at, last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, NULL, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210, now())
           returning id"#,
    )
    .bind(pmp)
    .fetch_one(&pool)
    .await
    .expect("insert market with NULL hash");
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
    .expect("insert outcome");
    seed_live_order(&pool, pmp, 999, 7, owner, "OPEN", "1500000", None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            999,
            owner,
            NOW_TRADING,
        )
        .await
        .expect_err("NULL oracle_list_hash must fail closed");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_for_cancel_negative_token_type_fails_closed() {
    // Same invariant as resolve_for_new_order: `markets.token_type` is
    // `integer` (signed) but the chain ABI is uint32. A negative value
    // surfaces as MarketInconsistent at the repo boundary with a warn
    // (market_address, raw).
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfc_neg_tt_pmp";
    let symbol = "RFC_NEG_TT_YES";
    let owner = "0:rfc_neg_tt_owner";
    purge(&pool, pmp, symbol).await;
    sqlx::query(
        r#"insert into ref_tokens (
              token_type, token_code, decimals,
              min_notional, lot_size, tick_size_bps,
              price_precision, quantity_precision)
                values (-1, '__NEG_TT_RFC__', 0,
                        0::numeric, 0::numeric, 0::numeric, 0, 0)
           on conflict (token_type) do nothing"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_trading_market(&pool, pmp, symbol, -1, 7).await;
    seed_live_order(&pool, pmp, 999, 7, owner, "OPEN", "1500000", None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .resolve_for_cancel(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            999,
            owner,
            NOW_TRADING,
        )
        .await
        .expect_err("negative token_type must fail closed");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::MarketInconsistent));
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
// owner_pn_address match, m.last_reconciled_at IS NOT NULL) but filters
// `live_orders.order_id` with `= ANY($3::text[])` and returns matched
// rows in a `HashMap<u64, OrderForCancelBatch>` keyed by chain
// `order_id`. The trait contract — every key is in
// `input.order_ids[]` — is enforced by the WHERE clause; the natural
// HashMap dedup plus the `(orderbook_address, order_id)` PK guarantees
// no key collisions. Identity and market_status are lifted to
// `CancelBatchResolution` once — every row joins the same (markets,
// market_outcomes) snapshot, so per-row projection would be redundant.
// The single-cancel suite above already pins the predicate semantics;
// these tests focus on what is bulk-specific: per-id presence,
// shortfall, and per-row owner filtering inside one request.

fn into_orders(
    resolution: Option<CancelBatchResolution>,
) -> std::collections::HashMap<u64, OrderForCancelBatch> {
    resolution.map(|r| r.orders).unwrap_or_default()
}

#[tokio::test]
async fn resolve_for_cancel_batch_happy_path_multiple_rows() {
    // Three ids requested, all three resolve. Exercises the
    // `lo.order_id::text = ANY($3::text[])` filter on a non-trivial
    // input set — every input id must surface as a key in the
    // returned HashMap, regardless of the order PG returns rows in.
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

    let orders = resolution.orders;
    assert_eq!(orders.len(), 3);
    assert_eq!(orders[&1001].client_order_id.as_deref(), Some("a"));
    assert_eq!(orders[&1002].client_order_id.as_deref(), Some("b"));
    assert!(orders[&1003].client_order_id.is_none());
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
    assert!(orders[&9001].client_order_id.is_none());
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
    // Input [2001, 2002, 2003]; 2002 is absent in live_orders.
    assert!(orders.contains_key(&2001));
    assert!(!orders.contains_key(&2002));
    assert!(orders.contains_key(&2003));
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
    // Input [3001, 3002, 3099]; attacker's 3099 must be absent.
    assert!(orders.contains_key(&3001));
    assert!(orders.contains_key(&3002));
    assert!(!orders.contains_key(&3099));
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
    // Contract: the repo derives `market_status` from the same
    // `markets` row that produced the matched orders, so the use
    // case's post-SELECT `market_status == Trading` re-check sees a
    // consistent snapshot. Seed timings make the market RESOLVING at
    // `NOW_RESOLVING`; rows still satisfy the order-level predicates
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
    assert!(resolution.orders.contains_key(&5001));
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
    // Input [6001, 6002]; only 6001 survives (6002 is CANCELLED).
    assert!(orders.contains_key(&6001));
    assert!(!orders.contains_key(&6002));
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
    // Input [7001, 7002]; only 7001 survives (7002 has amount_remaining=0).
    assert!(orders.contains_key(&7001));
    assert!(!orders.contains_key(&7002));
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
    // Input [8001, 8002]; only 8001 survives (8002 is on the NO book).
    assert!(orders.contains_key(&8001));
    assert!(!orders.contains_key(&8002));
}

#[tokio::test]
async fn resolve_for_cancel_batch_null_oracle_list_hash_trims_to_empty_string() {
    // SQL contract pin: `m.oracle_list_hash::text` projects NULL as
    // `None`, which the infra layer collapses to `String::new()` after
    // emitting the corruption-warn. The application-layer fake cannot
    // exercise this branch (it constructs `CancelBatchResolution`
    // directly); without a real-Postgres test a future SQL change that
    // dropped the `::text` cast or pre-filtered NULL rows would slip
    // past CI and surface only as an empty `oracleListHash` reaching
    // the chain. The column is `numeric(78,0)` so a "blank" value is
    // not structurally reachable today — the defence-in-depth
    // `trim().is_empty()` guard is exercised by the NULL branch alone.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_null_olh_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_NULL_OLH_YES";
    let pn = "0:resolve_cancel_batch_null_olh_pn";
    purge(&pool, pmp, symbol).await;

    // Inline insert: `seed_trading_market` always binds `oracle_list_hash
    // = 1::numeric`. The reconciled `last_reconciled_at = now()` keeps
    // the row past the visibility filter so it actually surfaces and the
    // NULL branch in `resolve_for_cancel_batch` runs.
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, NULL, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210,
                   now())
           returning id"#,
    )
    .bind(pmp)
    .fetch_one(&pool)
    .await
    .expect("insert market with NULL oracle_list_hash");
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
    seed_live_order(&pool, pmp, 7001, 7, pn, "OPEN", "1500000", Some("nolh")).await;

    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[7001],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("NULL oracle_list_hash must not turn into a typed error")
        .expect("the matched row still yields a resolution");

    // The trim-to-empty branch is the contract: a corrupted/missing
    // `oracle_list_hash` does not crash, does not surface as a typed
    // miss, and does not leak a placeholder string — it propagates as
    // an empty string with a warn, leaving the use case + chain layer
    // to fail loudly if they cannot accept it.
    assert_eq!(resolution.oracle_list_hash, "");
    assert_eq!(resolution.event_id, "42");
    assert_eq!(resolution.token_type, 3);
    assert_eq!(resolution.orders.len(), 1);
    assert_eq!(resolution.orders[&7001].client_order_id.as_deref(), Some("nolh"));
}

#[tokio::test]
async fn resolve_for_cancel_batch_duplicate_input_ids_dedup_to_one_key() {
    // SQL contract pin: `lo.order_id::text = ANY($3::text[])` matches
    // each live row at most once regardless of how many times the
    // caller repeats the same id in the bind array. The HashMap key
    // is the natural identity, so the duplicate is structurally
    // collapsed. The use case rejects intra-batch dups upstream with
    // -1130 today, but that gate is one refactor away from being
    // moved or dropped — without this pin, the SQL contract that
    // would otherwise catch the dup is implicit.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_dup_input_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_DUP_INPUT_YES";
    let pn = "0:resolve_cancel_batch_dup_input_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;
    seed_live_order(&pool, pmp, 8001, 7, pn, "OPEN", "1500000", Some("dup")).await;

    let orders = into_orders(
        repo.resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            // Same id at positions 0 and 1; position 2 is a distinct id
            // that does NOT exist. The HashMap return type collapses
            // the dup to one entry; the absent id contributes nothing.
            &[8001, 8001, 8002],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("bulk resolve with duplicate input ids"),
    );

    assert_eq!(orders.len(), 1);
    assert_eq!(orders[&8001].client_order_id.as_deref(), Some("dup"));
    assert!(!orders.contains_key(&8002));
}

#[tokio::test]
async fn resolve_for_cancel_batch_u64_max_id_round_trips_via_text() {
    // Pins the ceiling of the public-API id range. The SELECT binds
    // ids as text[] and casts both sides to text in
    // `lo.order_id::text = ANY($3::text[])`. The projection carries
    // `order_id::text`, parsed back into u64 at the application
    // boundary; values at `u64::MAX` must round-trip without
    // truncation. A regression that re-introduced `::numeric` /
    // `::int8` somewhere on the path would compile fine but
    // silently truncate above `i64::MAX`.
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

    let orders = resolution.orders;
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[&u64::MAX].client_order_id.as_deref(), Some("ceil"));
    assert_eq!(orders[&(u64::MAX - 1)].client_order_id.as_deref(), Some("ceil-1"));
}

#[tokio::test]
async fn resolve_for_cancel_batch_above_u64_stored_value_is_invisible() {
    // `live_orders.order_id` is `numeric(78, 0)` — the column can
    // legitimately store values above u64::MAX (chain ABI is uint128).
    // The application boundary caps at u64, so the bulk SELECT bind
    // cannot reference values above u64::MAX. This test pins that an
    // out-of-u64 stored row is silently filtered (no match) rather
    // than tripping the `parse::<u64>()` anyhow path in the result
    // assembler — that path is unreachable today because the
    // `lo.order_id = ANY($3::text[]::numeric[])` predicate requires
    // bind = stored numerically, and a u64 bind can never equal an
    // out-of-u64 stored value. Test exists so a future regression
    // that broadened the boundary (e.g. `&[u64]` → `&[String]`
    // without revisiting the parse path) would surface here.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:resolve_cancel_batch_above_u64_pmp";
    let symbol = "RESOLVE_CANCEL_BATCH_ABOVE_U64_YES";
    let pn = "0:resolve_cancel_batch_above_u64_pn";
    purge(&pool, pmp, symbol).await;
    seed_trading_market(&pool, pmp, symbol, 3, 7).await;

    // Seed an in-u64 row so the JOIN scope is non-empty and we can
    // tell "no rows joined" from "no rows matched the bind".
    seed_live_order(&pool, pmp, u64::MAX, 7, pn, "OPEN", "1500000", Some("in-range")).await;
    // Raw SQL: bind a `numeric` value above u64::MAX directly. The
    // typed `seed_live_order` helper takes `u64` and so cannot
    // express this; bypass it for the over-u64 row only.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy,
                price, amount_remaining, amount_initial, client_order_id,
                status, last_chain_order, owner_pn_address,
                placed_chain_order)
           values ($1, 18446744073709551616::numeric, 7, true,
                   615::numeric, 1500000::numeric, 1500000::numeric, $2,
                   'OPEN', '0001', $3,
                   '0001')"#,
    )
    .bind(pmp)
    .bind("over-range")
    .bind(pn)
    .execute(&pool)
    .await
    .expect("insert over-u64 live_orders row");

    // Caller can only bind u64-valued ids. Ask for the in-range one;
    // the over-range row is invisible to the SELECT and so cannot
    // reach the result-assembly parse path.
    let resolution = repo
        .resolve_for_cancel_batch(
            &MarketAddress(pmp.into()),
            &Symbol(symbol.into()),
            &[u64::MAX],
            pn,
            NOW_TRADING,
        )
        .await
        .expect("over-u64 stored row must not surface as a typed error")
        .expect("the in-range matched row still yields a resolution");

    let orders = resolution.orders;
    assert_eq!(orders.len(), 1);
    assert!(orders.contains_key(&u64::MAX));
}
