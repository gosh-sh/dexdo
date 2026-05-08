// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// End-to-end coverage for the `MarketStatus::Cancelled` derivation path that
// must trigger when only the on-chain `isCancelled` flag (written by the
// reconciler) is set and the cancellation event has not been observed.
// Gated on TEST_DATABASE_URL — see crates/infrastructure/tests/reprojection.rs
// for the docker-compose harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::TerminalKind;
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

async fn purge_market(pool: &PgPool, pmp_address: &str) {
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge market");
}

async fn insert_market(
    pool: &PgPool,
    pmp_address: &str,
    market_name: &str,
    orderbook_address: &str,
    is_cancelled: bool,
    cancelled_at: Option<i64>,
) {
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                is_cancelled, cancelled_at,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $2, $2, 3, 'USDC',
                   1::numeric, 0::numeric, $3,
                   $4, $5,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())"#,
    )
    .bind(pmp_address)
    .bind(market_name)
    .bind(orderbook_address)
    .bind(is_cancelled)
    .bind(cancelled_at)
    .execute(pool)
    .await
    .expect("insert market");
}

#[tokio::test]
async fn cancelled_status_when_only_reconciler_flag_is_set() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_flag_only";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    // Reconciler-only path: is_cancelled = true, cancelled_at = NULL.
    insert_market(&pool, &pmp, &market_name, &orderbook, true, None).await;

    // `now` between stake_start and stake_end — without cancellation the
    // status would be STAKING. The CANCELLED outcome here proves the read
    // path is consulting `is_cancelled` (not just `cancelled_at`).
    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let page = repo.list_markets(&request).await.expect("list markets");
    let market = page.markets.first().expect("market row returned");

    assert_eq!(market.status, MarketStatus::Cancelled);
    let terminal = market.terminal.as_ref().expect("terminal block populated");
    assert!(matches!(terminal.kind, TerminalKind::Cancelled));
    // `cancelled_at` was NULL in the row, so `terminal.at` falls back to `now`
    // (request time). Acceptable — once the reconciler runs again, it will
    // stamp `cancelled_at` and the response stabilises.
    assert_eq!(terminal.at, 1_700_000_150);
    assert!(terminal.cancel_reason.is_none());
}

#[tokio::test]
async fn cancelled_status_filter_includes_reconciler_only_rows() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_flag_filter";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, true, None).await;

    let request = MarketsRequest::Listing(dodex_application::MarketsListing {
        filter: dodex_application::MarketsFilter {
            statuses: vec![MarketStatus::Cancelled],
            quote_asset: Some("USDC".into()),
            oracle_name: None,
            closing_before: None,
        },
        sort: dodex_application::MarketsSort::CreatedAtDesc,
        cursor: None,
        limit: 100,
        now: 1_700_000_150,
    });

    let page = repo.list_markets(&request).await.expect("listing");
    assert!(
        page.markets.iter().any(|m| m.market_address.0 == pmp),
        "status=CANCELLED filter must include rows with is_cancelled=true and cancelled_at null; got {:?}",
        page.markets.iter().map(|m| (&m.market_address.0, m.status)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn reconciler_writes_cancelled_at_when_flag_flips() {
    let Some(pool) = setup().await else { return };

    let test = "markets_cancel_at_stamp";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;

    // Seed a non-cancelled market.
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;

    // Apply the same SET clause the reconciler emits (mirror of
    // `write_market_state` in reconciler.rs).
    sqlx::query(
        r#"update markets
              set is_cancelled = true,
                  cancelled_at = case
                      when true and cancelled_at is null then extract(epoch from now())::bigint
                      else cancelled_at
                  end
            where pmp_address = $1"#,
    )
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("simulate reconciler write");

    let cancelled_at: Option<i64> =
        sqlx::query_scalar("select cancelled_at from markets where pmp_address = $1")
            .bind(&pmp)
            .fetch_one(&pool)
            .await
            .expect("read cancelled_at");
    assert!(
        cancelled_at.is_some(),
        "reconciler must stamp cancelled_at when flipping is_cancelled and cancelled_at was null"
    );

    // A second pass with cancelled_at already populated must be a no-op
    // (idempotent — does not move the timestamp forward).
    let frozen = cancelled_at.unwrap();
    sqlx::query(
        r#"update markets
              set is_cancelled = true,
                  cancelled_at = case
                      when true and cancelled_at is null then extract(epoch from now())::bigint
                      else cancelled_at
                  end
            where pmp_address = $1"#,
    )
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("second reconciler write");

    let cancelled_at_after: Option<i64> =
        sqlx::query_scalar("select cancelled_at from markets where pmp_address = $1")
            .bind(&pmp)
            .fetch_one(&pool)
            .await
            .expect("read cancelled_at again");
    assert_eq!(
        cancelled_at_after,
        Some(frozen),
        "reconciler must not overwrite an existing cancelled_at"
    );
}
