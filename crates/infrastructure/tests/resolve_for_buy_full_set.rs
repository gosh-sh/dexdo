// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::resolve_for_buy_full_set
// — the market-level resolver feeding `POST /api/v1/buyFullSet`. Unlike
// `resolve_for_new_order`, this one does not join `market_outcomes`:
// `splitFullSet` is a market-level chain op, so the tests seed
// markets-only rows to prove no implicit join leaked in. Gated on
// TEST_DATABASE_URL.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
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

async fn purge_market(pool: &PgPool, pmp_address: &str) {
    sqlx::query("delete from market_outcomes where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge market_outcomes");
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge markets");
}

/// Seed a reconciled market with the given (token_type, oracle_list_hash)
/// and timing columns producing `MarketStatus::Trading` at `now =
/// stake_start + 100` (between stake_start and result_start, frozen_at
/// set). No `market_outcomes` row — the resolver is supposed to ignore
/// outcomes; an accidental join regression would surface as an empty
/// result here even though the market exists.
async fn seed_trading_market(
    pool: &PgPool,
    pmp_address: &str,
    token_type: i32,
    oracle_list_hash: &str,
    event_id: &str,
) {
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at, last_reconciled_at)
           values ($1, $1, $1, $2, 'USDC',
                   $4::numeric, $3::numeric, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210, now())"#,
    )
    .bind(pmp_address)
    .bind(token_type)
    .bind(oracle_list_hash)
    .bind(event_id)
    .execute(pool)
    .await
    .expect("insert market");
}

#[tokio::test]
async fn happy_trading_returns_slim_projection() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:rfbfs_trading_pmp";
    purge_market(&pool, pmp).await;
    seed_trading_market(&pool, pmp, 3, "1", "42").await;

    // now between stake_start and result_start, frozen_at set → Trading.
    let resolved = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect("resolve happy path");

    assert_eq!(resolved.status, MarketStatus::Trading);
    assert_eq!(resolved.event_id, "42");
    assert_eq!(resolved.oracle_list_hash, "1");
    assert_eq!(resolved.token_type, 3);
}

#[tokio::test]
async fn happy_awaiting_freeze_returns_slim_projection() {
    // stake_end reached but frozen_at NULL → AwaitingFreeze. The
    // resolver must surface this status verbatim (the use case is the
    // one that allow-lists AWAITING_FREEZE | TRADING per api-spec).
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfbfs_awaiting_freeze_pmp";
    purge_market(&pool, pmp).await;
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at, last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, 1::numeric, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   NULL, now())"#,
    )
    .bind(pmp)
    .execute(&pool)
    .await
    .expect("insert awaiting-freeze market");

    let repo = PostgresReadModelRepository::new(pool.clone());
    let resolved = repo
        // now >= stake_end and frozen_at IS NULL → AwaitingFreeze.
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect("resolve awaiting freeze");
    assert_eq!(resolved.status, MarketStatus::AwaitingFreeze);
}

#[tokio::test]
async fn unknown_market_is_typed_miss() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let err = repo
        .resolve_for_buy_full_set(&MarketAddress("0:rfbfs_no_such_market".into()), 1_700_000_250)
        .await
        .expect_err("unknown market must surface as typed miss");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::InvalidMarketOrSymbol));
}

#[tokio::test]
async fn pre_reconcile_market_is_invisible() {
    // `last_reconciled_at IS NULL` markets are hidden symmetric with
    // `/api/v1/markets`. The trading-path resolver must collapse to
    // InvalidMarketOrSymbol just like for unknown rows; differentiating
    // would expose the pre-reconcile bucket to callers.
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfbfs_pre_reconcile_pmp";
    purge_market(&pool, pmp).await;
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, orderbook_address,
                stake_start, stake_end, result_start, result_end)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, null,
                   1700000100, 1700000200, 1700000300, 1700000400)"#,
    )
    .bind(pmp)
    .execute(&pool)
    .await
    .expect("insert pre-reconcile market");

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect_err("pre-reconcile market must not be visible");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::InvalidMarketOrSymbol));
}

#[tokio::test]
async fn null_oracle_list_hash_fails_closed() {
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfbfs_null_ohash_pmp";
    purge_market(&pool, pmp).await;
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                frozen_at, last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   42::numeric, NULL, $1,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   1700000210, now())"#,
    )
    .bind(pmp)
    .execute(&pool)
    .await
    .expect("insert market with NULL hash");

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect_err("NULL oracle_list_hash must fail closed");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::MarketInconsistent));
}

#[tokio::test]
async fn negative_token_type_fails_closed() {
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfbfs_neg_tt_pmp";
    purge_market(&pool, pmp).await;
    // FK to ref_tokens — seed a sentinel -1 row idempotently, matching
    // the same trick used by resolve_for_new_order's neg-token-type test.
    sqlx::query(
        r#"insert into ref_tokens (
              token_type, token_code, decimals,
              min_notional, lot_size, tick_size_bps,
              price_precision, quantity_precision)
                values (-1, '__NEG_TT_RFBFS__', 0,
                        0::numeric, 0::numeric, 0::numeric, 0, 0)
           on conflict (token_type) do nothing"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_trading_market(&pool, pmp, -1, "1", "42").await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect_err("negative token_type must fail closed");
    let dom = err.downcast_ref::<DomainError>().expect("DomainError");
    assert!(matches!(dom, DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_for_buy_full_set_derives_cancelled_status() {
    // The use case rejects everything except {Trading, AwaitingFreeze},
    // but the resolver itself must surface the actual derived status so
    // the rejection lands at the application boundary with -2010 rather
    // than collapsing to a generic InvalidMarketOrSymbol miss. A
    // cancelled market should resolve with status = Cancelled, leaving
    // the use case to fail closed (HTTP-side coverage of the rejection
    // already exists in `buy_full_set_http::non_open_market_returns_400_minus_2010`;
    // this test pins the DB-side derivation feeding it).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:rfbfs_cancelled_pmp";
    purge_market(&pool, pmp).await;
    seed_trading_market(&pool, pmp, 3, "1", "42").await;
    sqlx::query("update markets set is_cancelled = true where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .expect("flip is_cancelled");

    let resolved = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect("resolve cancelled");
    assert_eq!(resolved.status, MarketStatus::Cancelled);
}

#[tokio::test]
async fn resolver_ignores_market_outcomes() {
    // splitFullSet operates at the market level; the resolver must
    // succeed even when `market_outcomes` is empty for the row.
    // `seed_trading_market` deliberately inserts only the `markets`
    // row, so a future regression that bolts a JOIN onto this resolver
    // would turn this test red by collapsing to InvalidMarketOrSymbol.
    let Some(pool) = setup().await else { return };
    let pmp = "0:rfbfs_no_outcomes_pmp";
    purge_market(&pool, pmp).await;
    seed_trading_market(&pool, pmp, 3, "1", "42").await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let resolved = repo
        .resolve_for_buy_full_set(&MarketAddress(pmp.into()), 1_700_000_250)
        .await
        .expect("resolve must not require market_outcomes");
    assert_eq!(resolved.status, MarketStatus::Trading);
}
