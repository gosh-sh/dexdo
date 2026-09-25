// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level coverage for resting liquidity on the inference read path: the
// `?liquidity=` filter on /api/v1/inference/markets and the per-book totals
// behind /api/v1/inference/liquidity. Both read `inference_orders` through the
// same "OPEN and something left on it" definition `/api/v1/inference/depth`
// aggregates, so they are exercised against the same seeded books here.
// Gated on TEST_DATABASE_URL — see inference_read_repo.rs for the harness.

use std::env;
use std::time::Duration;

use dodex_application::InferenceMarketsListing;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceMarketsSort;
use dodex_application::InferenceReadRepository;
use dodex_domain::DomainError;
use dodex_domain::InferenceLiquidity;
use dodex_domain::LiquidityFilter;
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

/// Chain time for this suite's books. Deliberately low: other suites assert on
/// the *head* of the newest-first listing, so fixtures that sort above theirs
/// would displace them. Nothing here depends on rank — see [`assert_listing`].
const CHAIN_TIME: i64 = 1_700_000_500;

/// Page size for the filtered listings below. Generous on purpose: with the
/// filter applied the whole test database yields a few dozen books, so one
/// page holds every match and this suite's books are always in it whatever
/// else is seeded.
const LISTING_LIMIT: u16 = 200;

fn ob_of(tag: &str) -> String {
    format!("0:inf_liq_{tag}")
}

/// Seed a reconciled inference book. price_precision 9 / quantity_precision 0,
/// matching the real trading rules: ticks are whole units, so a tick total
/// renders as a bare integer.
async fn seed_book(pool: &PgPool, tag: &str, created_at_chain_secs: i64) {
    let ob = ob_of(tag);
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(&ob)
        .execute(pool)
        .await
        .expect("purge inference_orders");
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(&ob)
        .execute(pool)
        .await
        .expect("purge inference_markets");

    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref,
                platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional,
                created_at_chain, last_reconciled_at)
           values ($1, null, $2,
                   250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001',
                   to_timestamp($3::double precision), now())"#,
    )
    .bind(&ob)
    .bind(format!("model-{tag}"))
    .bind(created_at_chain_secs)
    .execute(pool)
    .await
    .expect("seed inference_markets");
}

/// Insert one `inference_orders` row. `amount_remaining` is what the read path
/// counts; `status` lets a test place a closed order that must not register as
/// liquidity, and `is_subscription` pins that subscriptions count exactly as
/// depth counts them.
async fn seed_order(
    pool: &PgPool,
    tag: &str,
    order_id: i64,
    is_buy: bool,
    amount_remaining: &str,
    status: &str,
    is_subscription: bool,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, is_subscription, last_chain_order)
           values ($1, $2::numeric, $3, 1000::numeric,
                   1000::numeric, $4::numeric, $5, $6, $7)"#,
    )
    .bind(ob_of(tag))
    .bind(order_id)
    .bind(is_buy)
    .bind(amount_remaining)
    .bind(status)
    .bind(is_subscription)
    .bind(format!("{order_id:04}"))
    .execute(pool)
    .await
    .expect("insert inference_order");
}

async fn open_order(pool: &PgPool, tag: &str, order_id: i64, is_buy: bool, ticks: &str) {
    seed_order(pool, tag, order_id, is_buy, ticks, "OPEN", false).await;
}

/// Assert which of this suite's books a `?liquidity=` listing does and does
/// not return.
///
/// Membership, not equality: the database is shared, so a page holds other
/// suites' books too. That is safe to assemble — a book only reaches a
/// filtered page by carrying orders, and a book with orders carries the
/// trading rules the read model needs, so widening the page cannot drag in a
/// row that fails closed.
async fn assert_listing(pool: &PgPool, filter: LiquidityFilter, present: &[&str], absent: &[&str]) {
    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::Listing(InferenceMarketsListing {
            liquidity: Some(filter),
            sort: InferenceMarketsSort::CreatedAtDesc,
            cursor: None,
            limit: LISTING_LIMIT,
        }))
        .await
        .expect("listing");
    let got: Vec<String> = page.markets.iter().map(|m| m.orderbook_address.clone()).collect();

    for tag in present {
        assert!(got.contains(&ob_of(tag)), "?liquidity={} must return {tag}", filter.as_str(),);
    }
    for tag in absent {
        assert!(!got.contains(&ob_of(tag)), "?liquidity={} must not return {tag}", filter.as_str(),);
    }
}

async fn liquidity(pool: &PgPool, tag: &str) -> Result<InferenceLiquidity, anyhow::Error> {
    let repo = PostgresReadModelRepository::new(pool.clone());
    repo.get_inference_liquidity(&ob_of(tag)).await
}

/// The four books every filter test shares: one quoting each side, one quoting
/// both, one dry.
async fn seed_the_four(pool: &PgPool) {
    seed_book(pool, "bid_only", CHAIN_TIME).await;
    open_order(pool, "bid_only", 1, true, "100").await;

    seed_book(pool, "ask_only", CHAIN_TIME).await;
    open_order(pool, "ask_only", 1, false, "100").await;

    seed_book(pool, "two_sided", CHAIN_TIME).await;
    open_order(pool, "two_sided", 1, true, "100").await;
    open_order(pool, "two_sided", 2, false, "100").await;

    seed_book(pool, "dry", CHAIN_TIME).await;
}

#[tokio::test]
async fn filter_keeps_only_books_quoting_the_requested_side() {
    let Some(pool) = setup().await else { return };
    seed_the_four(&pool).await;

    assert_listing(&pool, LiquidityFilter::Buy, &["bid_only", "two_sided"], &["ask_only", "dry"])
        .await;
    assert_listing(&pool, LiquidityFilter::Sell, &["ask_only", "two_sided"], &["bid_only", "dry"])
        .await;
    assert_listing(&pool, LiquidityFilter::Any, &["bid_only", "ask_only", "two_sided"], &["dry"])
        .await;
    // BOTH is not "either side" — a one-sided book must not match.
    assert_listing(&pool, LiquidityFilter::Both, &["two_sided"], &["bid_only", "ask_only", "dry"])
        .await;
}

#[tokio::test]
async fn a_dry_book_is_hidden_by_the_filter_not_by_the_visibility_gate() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "gate", CHAIN_TIME).await;

    // Absent from every filtered listing...
    for filter in
        [LiquidityFilter::Buy, LiquidityFilter::Sell, LiquidityFilter::Any, LiquidityFilter::Both]
    {
        assert_listing(&pool, filter, &[], &["gate"]).await;
    }

    // ...yet perfectly visible on its own. The filter is what hides it, not the
    // reconcile gate — a dry book is still a tradable market.
    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob_of("gate") })
        .await
        .expect("single-book lookup");
    assert_eq!(page.markets.len(), 1);
}

#[tokio::test]
async fn closed_and_exhausted_orders_are_not_liquidity() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "closed", CHAIN_TIME).await;
    // Nothing here rests: a filled order, a cancelled one, and an OPEN row the
    // projector left at zero remaining.
    seed_order(&pool, "closed", 1, true, "100", "FILLED", false).await;
    seed_order(&pool, "closed", 2, false, "100", "CANCELLED", false).await;
    seed_order(&pool, "closed", 3, true, "0", "OPEN", false).await;

    assert_listing(&pool, LiquidityFilter::Any, &[], &["closed"]).await;

    // The totals must agree with the filter: same rows, same verdict.
    let liq = liquidity(&pool, "closed").await.expect("liquidity");
    assert_eq!(liq.bid_ticks, "0");
    assert_eq!(liq.ask_ticks, "0");
    assert_eq!(liq.bid_orders, 0);
    assert_eq!(liq.ask_orders, 0);
}

#[tokio::test]
async fn totals_sum_ticks_and_orders_per_side() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "totals", CHAIN_TIME).await;
    open_order(&pool, "totals", 1, true, "100").await;
    open_order(&pool, "totals", 2, true, "50").await;
    open_order(&pool, "totals", 3, false, "25").await;
    // A subscription is an order like any other here — depth counts it, so this
    // counts it. Excluding it would make the two endpoints disagree.
    seed_order(&pool, "totals", 4, false, "5", "OPEN", true).await;

    let liq = liquidity(&pool, "totals").await.expect("liquidity");
    assert_eq!(liq.orderbook_address, ob_of("totals"));
    assert_eq!(liq.bid_ticks, "150");
    assert_eq!(liq.bid_orders, 2);
    assert_eq!(liq.ask_ticks, "30", "the subscription's 5 ticks are part of the ask side");
    assert_eq!(liq.ask_orders, 2);
    // quantity_precision is 0 — ticks are whole units, so no decimal point.
    assert!(!liq.bid_ticks.contains('.'), "ticks must render as whole units");
}

#[tokio::test]
async fn totals_on_an_empty_book_are_zeros_not_a_miss() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "empty", CHAIN_TIME).await;

    let liq = liquidity(&pool, "empty").await.expect("an empty book is not an error");
    assert_eq!(liq.bid_ticks, "0");
    assert_eq!(liq.ask_ticks, "0");
    assert_eq!(liq.bid_orders, 0);
    assert_eq!(liq.ask_orders, 0);
}

#[tokio::test]
async fn totals_carry_the_contract_version() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "version", CHAIN_TIME).await;
    sqlx::query("update inference_markets set version = '4.0.30' where orderbook_address = $1")
        .bind(ob_of("version"))
        .execute(&pool)
        .await
        .expect("stamp version");

    let liq = liquidity(&pool, "version").await.expect("liquidity");
    assert_eq!(
        liq.contract_version.as_deref(),
        Some("4.0.30"),
        "same value depth reports for this book",
    );
}

#[tokio::test]
async fn totals_reject_an_unknown_book() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err =
        repo.get_inference_liquidity("0:inf_liq_no_such_book").await.expect_err("unknown book");
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));
}

#[tokio::test]
async fn an_unreconciled_book_is_invisible_to_both_reads() {
    let Some(pool) = setup().await else { return };
    // A book still behind the visibility gate is hidden from the listing; the
    // totals must report the same miss rather than an empty book.
    seed_book(&pool, "unreconciled", CHAIN_TIME).await;
    open_order(&pool, "unreconciled", 1, true, "100").await;
    sqlx::query(
        "update inference_markets set last_reconciled_at = null where orderbook_address = $1",
    )
    .bind(ob_of("unreconciled"))
    .execute(&pool)
    .await
    .expect("clear last_reconciled_at");

    assert_listing(&pool, LiquidityFilter::Buy, &[], &["unreconciled"]).await;

    let err = liquidity(&pool, "unreconciled").await.expect_err("not yet reconciled");
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));
}

#[tokio::test]
async fn null_quantity_precision_fails_closed() {
    let Some(pool) = setup().await else { return };
    // A reconciled row missing its trading rules is corruption, not an empty
    // book: rendering ticks off a NULL scale would publish a wrong number.
    // Chain time 0, so the row sorts to the very TAIL of the newest-first
    // listing, and purged below: the shared database is walked end-to-end by
    // other suites' pagination tests, which assemble every visible row and
    // would fail closed on this one.
    seed_book(&pool, "noscale", 0).await;
    sqlx::query(
        "update inference_markets set quantity_precision = null where orderbook_address = $1",
    )
    .bind(ob_of("noscale"))
    .execute(&pool)
    .await
    .expect("clear quantity_precision");

    let err = liquidity(&pool, "noscale").await.expect_err("must fail closed");
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob_of("noscale"))
        .execute(&pool)
        .await
        .expect("purge the corrupt fixture");
}
