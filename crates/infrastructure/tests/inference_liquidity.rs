// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level coverage for resting liquidity on the inference read path: the
// `?liquidity=` filter and the per-market top of book on
// /api/v1/inference/markets, and the levels and whole-book totals on
// /api/v1/inference/depth. All of them read `inference_orders` through the same
// "OPEN, something left on it, not past its deadline" definition, so they are
// exercised against the same seeded books here.
// Gated on TEST_DATABASE_URL — see inference_read_repo.rs for the harness.

use std::env;
use std::time::Duration;

use dodex_application::InferenceMarketsListing;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceMarketsSort;
use dodex_application::InferenceReadRepository;
use dodex_domain::DomainError;
use dodex_domain::InferenceDepthSnapshot;
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

/// Request clock for every read below. Fixed rather than wall-clock so the
/// deadline cases are deterministic: a book seeded with `NOW - 1` is expired
/// on every run, and one with `NOW + 1` never is.
const NOW: i64 = 1_700_001_000;

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
#[allow(clippy::too_many_arguments)]
async fn seed_order(
    pool: &PgPool,
    tag: &str,
    order_id: i64,
    is_buy: bool,
    amount_remaining: &str,
    status: &str,
    is_subscription: bool,
    deadline: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, is_subscription,
                deadline, last_chain_order)
           values ($1, $2::numeric, $3, 1000::numeric,
                   1000::numeric, $4::numeric, $5, $6, $7::numeric, $8)"#,
    )
    .bind(ob_of(tag))
    .bind(order_id)
    .bind(is_buy)
    .bind(amount_remaining)
    .bind(status)
    .bind(is_subscription)
    .bind(deadline)
    .bind(format!("{order_id:04}"))
    .execute(pool)
    .await
    .expect("insert inference_order");
}

/// An open order with no deadline — the chain's `0`, good-till-cancel, which
/// never expires.
async fn open_order(pool: &PgPool, tag: &str, order_id: i64, is_buy: bool, ticks: &str) {
    seed_order(pool, tag, order_id, is_buy, ticks, "OPEN", false, None).await;
}

/// An open order at an explicit `price`, with an optional deadline.
async fn seed_order_priced(
    pool: &PgPool,
    tag: &str,
    order_id: i64,
    is_buy: bool,
    ticks: &str,
    price: i64,
    deadline: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, is_subscription,
                deadline, last_chain_order)
           values ($1, $2::numeric, $3, $4::numeric,
                   1000::numeric, $5::numeric, 'OPEN', false, $6::numeric, $7)"#,
    )
    .bind(ob_of(tag))
    .bind(order_id)
    .bind(is_buy)
    .bind(price)
    .bind(ticks)
    .bind(deadline)
    .bind(format!("{order_id:04}"))
    .execute(pool)
    .await
    .expect("insert priced order");
}

/// A priced order in a terminal state — it left the book, so it sets no quote.
async fn seed_order_closed(
    pool: &PgPool,
    tag: &str,
    order_id: i64,
    is_buy: bool,
    ticks: &str,
    price: i64,
    status: &str,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, is_subscription, last_chain_order)
           values ($1, $2::numeric, $3, $4::numeric,
                   1000::numeric, $5::numeric, $6, false, $7)"#,
    )
    .bind(ob_of(tag))
    .bind(order_id)
    .bind(is_buy)
    .bind(price)
    .bind(ticks)
    .bind(status)
    .bind(format!("{order_id:04}"))
    .execute(pool)
    .await
    .expect("insert closed order");
}

/// An open order that expires at `deadline` (unix seconds).
async fn open_order_until(
    pool: &PgPool,
    tag: &str,
    order_id: i64,
    is_buy: bool,
    ticks: &str,
    deadline: i64,
) {
    seed_order(pool, tag, order_id, is_buy, ticks, "OPEN", false, Some(deadline)).await;
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
            now: NOW,
        }))
        .await
        .expect("listing");
    // An absence check means nothing if the book merely fell off this page.
    assert!(!page.has_more, "?liquidity={} outgrew one page", filter.as_str());
    let got: Vec<String> = page.markets.iter().map(|m| m.orderbook_address.clone()).collect();

    for tag in present {
        assert!(got.contains(&ob_of(tag)), "?liquidity={} must return {tag}", filter.as_str(),);
    }
    for tag in absent {
        assert!(!got.contains(&ob_of(tag)), "?liquidity={} must not return {tag}", filter.as_str(),);
    }
}

/// The book's depth snapshot — the whole-book tick totals now live on it.
async fn depth(pool: &PgPool, tag: &str) -> Result<InferenceDepthSnapshot, anyhow::Error> {
    let repo = PostgresReadModelRepository::new(pool.clone());
    repo.get_inference_depth(&ob_of(tag), 100, NOW).await
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
        .list_inference_markets(&InferenceMarketsRequest::One {
            orderbook_address: ob_of("gate"),
            now: NOW,
        })
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
    seed_order(&pool, "closed", 1, true, "100", "FILLED", false, None).await;
    seed_order(&pool, "closed", 2, false, "100", "CANCELLED", false, None).await;
    seed_order(&pool, "closed", 3, true, "0", "OPEN", false, None).await;

    assert_listing(&pool, LiquidityFilter::Any, &[], &["closed"]).await;

    // The totals must agree with the filter: same rows, same verdict.
    let d = depth(&pool, "closed").await.expect("depth");
    assert_eq!(d.total_bid_ticks, "0");
    assert_eq!(d.total_ask_ticks, "0");
}

#[tokio::test]
async fn depth_totals_cover_the_whole_book_per_side() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "totals", CHAIN_TIME).await;
    open_order(&pool, "totals", 1, true, "100").await;
    open_order(&pool, "totals", 2, true, "50").await;
    open_order(&pool, "totals", 3, false, "25").await;
    // A subscription is an order like any other here — depth counts it, so this
    // counts it. Excluding it would make the two endpoints disagree.
    seed_order(&pool, "totals", 4, false, "5", "OPEN", true, None).await;

    let d = depth(&pool, "totals").await.expect("depth");
    assert_eq!(d.total_bid_ticks, "150");
    assert_eq!(d.total_ask_ticks, "30", "the subscription's 5 ticks are part of the ask side");
    // quantity_precision is 0 — ticks are whole units, so no decimal point.
    assert!(!d.total_bid_ticks.contains('.'), "ticks must render as whole units");
}

#[tokio::test]
async fn depth_totals_on_an_empty_book_are_zeros() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "empty", CHAIN_TIME).await;

    let d = depth(&pool, "empty").await.expect("an empty book is not an error");
    assert_eq!(d.total_bid_ticks, "0");
    assert_eq!(d.total_ask_ticks, "0");
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

    let err = depth(&pool, "unreconciled").await.expect_err("not yet reconciled");
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

    let err = depth(&pool, "noscale").await.expect_err("must fail closed");
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob_of("noscale"))
        .execute(&pool)
        .await
        .expect("purge the corrupt fixture");
}

// ---------------------------------------------------------------------------
// Expiry. The book skips a maker past its deadline when matching (`_isExpired`
// in InferenceOrderBook.sol: `deadline != 0 && block.timestamp >= deadline`),
// so such an order is not liquidity even while its stored status is still
// OPEN — the chain has simply not emitted `InferenceOrderExpired` yet.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_past_deadline_order_is_not_liquidity() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "stale", CHAIN_TIME).await;
    // The only order on the book lapsed one second ago.
    open_order_until(&pool, "stale", 1, true, "100", NOW - 1).await;

    // Gone from the filter...
    assert_listing(&pool, LiquidityFilter::Any, &[], &["stale"]).await;
    assert_listing(&pool, LiquidityFilter::Buy, &[], &["stale"]).await;

    // ...and from the totals, which read the same book.
    let d = depth(&pool, "stale").await.expect("depth");
    assert_eq!(d.total_bid_ticks, "0", "a lapsed order is not resting ticks");
}

#[tokio::test]
async fn the_expiry_boundary_matches_the_contract() {
    let Some(pool) = setup().await else { return };
    // `_isExpired` is `block.timestamp >= deadline`, so a deadline exactly at
    // `now` has already passed and one a second later has not.
    seed_book(&pool, "boundary_at", CHAIN_TIME).await;
    open_order_until(&pool, "boundary_at", 1, true, "100", NOW).await;
    seed_book(&pool, "boundary_after", CHAIN_TIME).await;
    open_order_until(&pool, "boundary_after", 1, true, "100", NOW + 1).await;

    assert_listing(&pool, LiquidityFilter::Buy, &["boundary_after"], &["boundary_at"]).await;
    assert_eq!(depth(&pool, "boundary_at").await.expect("depth").total_bid_ticks, "0");
    assert_eq!(depth(&pool, "boundary_after").await.expect("depth").total_bid_ticks, "100");
}

#[tokio::test]
async fn a_null_deadline_never_expires() {
    let Some(pool) = setup().await else { return };
    // NULL is the chain's `0`: good-till-cancel. It must survive any clock.
    seed_book(&pool, "gtc", CHAIN_TIME).await;
    open_order(&pool, "gtc", 1, true, "100").await;

    assert_listing(&pool, LiquidityFilter::Buy, &["gtc"], &[]).await;
    assert_eq!(depth(&pool, "gtc").await.expect("depth").total_bid_ticks, "100");
}

#[tokio::test]
async fn expiry_is_counted_per_side() {
    let Some(pool) = setup().await else { return };
    // The bid has lapsed, the ask has not: the book quotes one side only, so
    // BOTH must stop matching it while SELL still does.
    seed_book(&pool, "half_stale", CHAIN_TIME).await;
    open_order_until(&pool, "half_stale", 1, true, "100", NOW - 1).await;
    open_order_until(&pool, "half_stale", 2, false, "70", NOW + 1000).await;

    assert_listing(&pool, LiquidityFilter::Sell, &["half_stale"], &[]).await;
    assert_listing(&pool, LiquidityFilter::Buy, &[], &["half_stale"]).await;
    assert_listing(&pool, LiquidityFilter::Both, &[], &["half_stale"]).await;
    assert_listing(&pool, LiquidityFilter::Any, &["half_stale"], &[]).await;

    let d = depth(&pool, "half_stale").await.expect("depth");
    assert_eq!(d.total_bid_ticks, "0");
    assert_eq!(d.total_ask_ticks, "70");
}

#[tokio::test]
async fn depth_levels_add_up_to_the_depth_totals() {
    let Some(pool) = setup().await else { return };
    // The invariant the whole design rests on: one definition of resting,
    // three readers. Seed a book where every exclusion rule fires at once and
    // check depth reports exactly what the totals do.
    seed_book(&pool, "agree", CHAIN_TIME).await;
    open_order(&pool, "agree", 1, true, "100").await; // counts
    open_order_until(&pool, "agree", 2, true, "50", NOW + 1000).await; // counts
    open_order_until(&pool, "agree", 3, true, "999", NOW - 1).await; // lapsed
    seed_order(&pool, "agree", 4, true, "999", "CANCELLED", false, None).await; // closed
    seed_order(&pool, "agree", 5, true, "0", "OPEN", false, None).await; // exhausted
    open_order_until(&pool, "agree", 6, true, "999", NOW).await; // lapsed at the boundary

    // Depth builds each side in its own branch, so the ask side gets its own
    // survivors and exclusions.
    open_order(&pool, "agree", 7, false, "40").await; // counts
    open_order_until(&pool, "agree", 8, false, "999", NOW - 1).await; // lapsed
    open_order_until(&pool, "agree", 9, false, "999", NOW).await; // lapsed at the boundary

    let snap = depth(&pool, "agree").await.expect("depth");
    // Survivors on a side rest at the same price, so they collapse into one level.
    let sum = |levels: &[dodex_domain::PriceLevel]| -> u64 {
        levels.iter().map(|l| l.quantity.parse::<u64>().expect("integer ticks")).sum()
    };
    assert_eq!(sum(&snap.bids), 150);
    assert_eq!(sum(&snap.asks), 40);

    // The totals come from a window over every price group, not from adding up
    // the levels that were returned — so agreeing with them is a real check.
    assert_eq!(snap.total_bid_ticks, "150");
    assert_eq!(snap.total_ask_ticks, "40");
}

#[tokio::test]
async fn depth_totals_are_not_capped_by_limit() {
    let Some(pool) = setup().await else { return };
    // The point of the totals: `limit` decides how many levels come back, and
    // nothing else. Five distinct prices per side, asked for one level.
    seed_book(&pool, "uncapped", CHAIN_TIME).await;
    for i in 1..=5i64 {
        seed_order_priced(&pool, "uncapped", i, true, "10", 1000 + i, None).await;
        seed_order_priced(&pool, "uncapped", 100 + i, false, "20", 5000 + i, None).await;
    }

    let repo = PostgresReadModelRepository::new(pool.clone());
    let snap = repo.get_inference_depth(&ob_of("uncapped"), 1, NOW).await.expect("depth");
    assert_eq!(snap.bids.len(), 1, "limit caps the levels");
    assert_eq!(snap.asks.len(), 1);
    assert_eq!(snap.total_bid_ticks, "50", "but not the total: 5 levels x 10 ticks");
    assert_eq!(snap.total_ask_ticks, "100", "5 levels x 20 ticks");
}

// ---------------------------------------------------------------------------
// Top of book on the market object. `best_bid` / `best_ask` must be the first
// level `/api/v1/inference/depth` would return for the same book — same
// resting definition, same scaling — so a client can screen on them without
// a depth call per book.
// ---------------------------------------------------------------------------

/// The market row for `tag`, through the single-book path.
async fn market(pool: &PgPool, tag: &str) -> dodex_domain::InferenceMarket {
    let repo = PostgresReadModelRepository::new(pool.clone());
    repo.list_inference_markets(&InferenceMarketsRequest::One {
        orderbook_address: ob_of(tag),
        now: NOW,
    })
    .await
    .expect("one market")
    .markets
    .pop()
    .expect("market row")
}

#[tokio::test]
async fn best_bid_and_ask_are_the_top_of_book() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "top", CHAIN_TIME).await;
    // Bids at 1000 / 1200 / 900, asks at 2000 / 1800 / 2500. Best bid is the
    // highest bid, best ask the lowest ask — not the newest, not the biggest.
    for (id, is_buy, price) in [
        (1i64, true, 1000),
        (2, true, 1200),
        (3, true, 900),
        (4, false, 2000),
        (5, false, 1800),
        (6, false, 2500),
    ] {
        seed_order_priced(&pool, "top", id, is_buy, "100", price, None).await;
    }

    let m = market(&pool, "top").await;
    // price_precision is 9 on these fixtures, so a raw 1200 renders as 0.000001200.
    assert_eq!(m.best_bid.as_deref(), Some("0.000001200"), "best bid is the highest bid");
    assert_eq!(m.best_ask.as_deref(), Some("0.000001800"), "best ask is the lowest ask");
}

#[tokio::test]
async fn an_empty_side_has_no_quote() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "onesided", CHAIN_TIME).await;
    seed_order_priced(&pool, "onesided", 1, true, "100", 1000, None).await;

    let m = market(&pool, "onesided").await;
    assert_eq!(m.best_bid.as_deref(), Some("0.000001000"));
    assert!(m.best_ask.is_none(), "no ask rests, so there is no ask quote");

    // And a book with nothing at all quotes neither side.
    seed_book(&pool, "noquote", CHAIN_TIME).await;
    let m = market(&pool, "noquote").await;
    assert!(m.best_bid.is_none() && m.best_ask.is_none());
}

#[tokio::test]
async fn the_top_of_book_skips_lapsed_and_closed_orders() {
    let Some(pool) = setup().await else { return };
    seed_book(&pool, "topstale", CHAIN_TIME).await;
    // The best-priced bid has lapsed and the next one is cancelled, so the
    // quote must fall through to the third. Same for the ask side.
    seed_order_priced(&pool, "topstale", 1, true, "100", 9999, Some(NOW - 1)).await;
    seed_order_closed(&pool, "topstale", 2, true, "100", 9000, "CANCELLED").await;
    seed_order_priced(&pool, "topstale", 3, true, "100", 1000, None).await;
    seed_order_priced(&pool, "topstale", 4, false, "100", 1, Some(NOW - 1)).await;
    seed_order_priced(&pool, "topstale", 5, false, "100", 2000, None).await;

    let m = market(&pool, "topstale").await;
    assert_eq!(m.best_bid.as_deref(), Some("0.000001000"), "lapsed and cancelled bids skipped");
    assert_eq!(m.best_ask.as_deref(), Some("0.000002000"), "the lapsed ask does not set the quote");
}

#[tokio::test]
async fn the_top_of_book_agrees_with_depth() {
    let Some(pool) = setup().await else { return };
    // The invariant: whatever depth reports as its first level is what the
    // market object quotes. Seed a book where every exclusion rule fires.
    seed_book(&pool, "topagree", CHAIN_TIME).await;
    seed_order_priced(&pool, "topagree", 1, true, "100", 5000, Some(NOW - 1)).await;
    seed_order_priced(&pool, "topagree", 2, true, "100", 1500, None).await;
    seed_order_priced(&pool, "topagree", 3, true, "100", 1400, Some(NOW + 1000)).await;
    seed_order_priced(&pool, "topagree", 4, false, "100", 1700, None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let depth = repo.get_inference_depth(&ob_of("topagree"), 100, NOW).await.expect("depth");
    let m = market(&pool, "topagree").await;

    assert_eq!(
        m.best_bid.as_deref(),
        depth.bids.first().map(|l| l.price.as_str()),
        "bestBid must equal depth's first bid level",
    );
    assert_eq!(
        m.best_ask.as_deref(),
        depth.asks.first().map(|l| l.price.as_str()),
        "bestAsk must equal depth's first ask level",
    );
}

#[tokio::test]
async fn the_listing_carries_the_top_of_book_too() {
    let Some(pool) = setup().await else { return };
    // Both fetch paths build the quote from the same LATERAL, but only a test
    // through the listing proves the join survives the keyset/ORDER BY clause.
    seed_book(&pool, "toplist", CHAIN_TIME).await;
    seed_order_priced(&pool, "toplist", 1, true, "100", 1234, None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::Listing(InferenceMarketsListing {
            liquidity: Some(LiquidityFilter::Buy),
            sort: InferenceMarketsSort::CreatedAtDesc,
            cursor: None,
            limit: LISTING_LIMIT,
            now: NOW,
        }))
        .await
        .expect("listing");
    let m = page
        .markets
        .iter()
        .find(|m| m.orderbook_address == ob_of("toplist"))
        .expect("seeded book in the page");
    assert_eq!(m.best_bid.as_deref(), Some("0.000001234"));
    assert!(m.best_ask.is_none());
}
