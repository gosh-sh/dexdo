// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for the whole-book totals on GET /api/v1/inference/depth and the
// `?liquidity=` filter on GET /api/v1/inference/markets, driven through the
// production router. The repo-level semantics (which rows count, how ticks
// scale) live in crates/infrastructure/tests/inference_liquidity.rs — what
// these tests pin is the wire contract: public access, camelCase DTO,
// parameter validation and error codes.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct DepthBody {
    #[serde(rename = "totalBidTicks")]
    total_bid_ticks: String,
    #[serde(rename = "totalAskTicks")]
    total_ask_ticks: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}

/// Seed a reconciled book, mirroring inference_depth_http.rs: price_precision
/// 9, quantity_precision 0 (ticks are whole units).
async fn seed_market(pool: &PgPool, ob: &str, created_at_chain_secs: i64) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, platform_fee_bps, quote_token_type,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                created_at_chain, version, last_reconciled_at)
           values ($1, null, 'r', 250, 2, 9, 0, '0.000000001', '1', '0.000000001',
                   to_timestamp($2::double precision), '4.0.30', now())"#,
    )
    .bind(ob)
    .bind(created_at_chain_secs)
    .execute(pool)
    .await
    .expect("seed market");
}

async fn seed_order(pool: &PgPool, ob: &str, id: i64, is_buy: bool, ticks: &str) {
    seed_order_until(pool, ob, id, is_buy, ticks, None).await;
}

/// `seed_order` with an explicit deadline; `None` is good-till-cancel.
async fn seed_order_until(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    ticks: &str,
    deadline: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, deadline, last_chain_order)
           values ($1, $2::numeric, $3, 1000::numeric,
                   $4::numeric, $4::numeric, 'OPEN', $5::numeric, $6)"#,
    )
    .bind(ob)
    .bind(id)
    .bind(is_buy)
    .bind(ticks)
    .bind(deadline)
    .bind(format!("{id:04}"))
    .execute(pool)
    .await
    .expect("seed order");
}

/// An open order at an explicit price, with an optional deadline.
async fn seed_order_priced(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    ticks: &str,
    price: i64,
    deadline: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, deadline, last_chain_order)
           values ($1, $2::numeric, $3, $4::numeric,
                   1000::numeric, $5::numeric, 'OPEN', $6::numeric, $7)"#,
    )
    .bind(ob)
    .bind(id)
    .bind(is_buy)
    .bind(price)
    .bind(ticks)
    .bind(deadline)
    .bind(format!("{id:04}"))
    .execute(pool)
    .await
    .expect("seed priced order");
}

/// A deadline safely in the past for any wall-clock this test can see. These
/// HTTP tests go through the handler, which stamps its own `now_seconds()`, so
/// the fixture is pinned relative to real time rather than a fixed constant.
fn long_past() -> i64 {
    1_600_000_000
}

/// A deadline far enough ahead that no run can reach it.
fn far_future() -> i64 {
    4_102_444_800
}

#[tokio::test]
async fn depth_reports_whole_book_totals() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_happy";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_000).await;
    // Two bids at distinct prices and one ask, so the totals are not simply the
    // single level each side returns.
    seed_order_priced(&pool, ob, 1, true, "100", 1200, None).await;
    seed_order_priced(&pool, ob, 2, true, "50", 1100, None).await;
    seed_order_priced(&pool, ob, 3, false, "25", 2000, None).await;

    // No auth headers: a public route must not be 401-gated.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "public depth route returns 200");
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.bids.len(), 2);
    assert_eq!(body.total_bid_ticks, "150");
    assert_eq!(body.total_ask_ticks, "25");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn depth_totals_ignore_the_level_limit() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_limit";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_000).await;
    seed_order_priced(&pool, ob, 1, true, "100", 1200, None).await;
    seed_order_priced(&pool, ob, 2, true, "50", 1100, None).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}&limit=1"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.bids.len(), 1, "limit caps the levels");
    assert_eq!(body.total_bid_ticks, "150", "and nothing else");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn depth_on_an_empty_book_totals_zero() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_000).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert!(body.bids.is_empty() && body.asks.is_empty());
    assert_eq!(body.total_bid_ticks, "0", "an empty side totals zero, it is not absent");
    assert_eq!(body.total_ask_ticks, "0");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn markets_liquidity_filter_selects_by_side() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_filter";
    purge(&pool, ob).await;
    // Chain time is deliberately low — other suites assert on the head of the
    // newest-first listing — and the page below is wide enough that rank does
    // not matter.
    seed_market(&pool, ob, 1_700_000_500).await;
    // Bid-only book: BUY and ANY must surface it, SELL and BOTH must not.
    seed_order(&pool, ob, 1, true, "100").await;

    let listed = |side: &str| {
        let service = &service;
        let side = side.to_string();
        async move {
            // A wide page, and membership rather than position: the test
            // database is shared, so other suites' books share the listing.
            let mut resp = TestClient::get(format!(
                "http://test/api/v1/inference/markets?limit=200&liquidity={side}"
            ))
            .send(service)
            .await;
            assert_eq!(resp.status_code, Some(StatusCode::OK));
            let body: Value = resp.take_json().await.expect("markets body");
            body["markets"]
                .as_array()
                .expect("markets array")
                .iter()
                .any(|m| m["inferenceOrderBookAddress"] == ob)
        }
    };

    assert!(listed("BUY").await, "?liquidity=BUY must include a book with a resting bid");
    assert!(listed("ANY").await);
    assert!(!listed("SELL").await, "?liquidity=SELL must exclude a bid-only book");
    assert!(!listed("BOTH").await, "BOTH needs an ask too");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn markets_rejects_an_unknown_liquidity_value() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/inference/markets?liquidity=MAYBE")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn a_blank_liquidity_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // An unbound template variable must not quietly drop the filter: that
    // would list every book, including ones that quote nothing.
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?liquidity=").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn markets_rejects_liquidity_with_single_book_lookup() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Single-book mode is mutually exclusive with every filter; `liquidity`
    // joins that set rather than being silently ignored. Presence is what
    // counts here, so even a blank value conflicts.
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&liquidity=BUY",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);

    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&liquidity=",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn a_lapsed_order_is_absent_from_the_totals_and_the_filter() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_lapsed";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_500).await;
    // One lapsed bid, one live ask. The handler stamps its own clock, so the
    // deadlines are pinned relative to real time.
    seed_order_until(&pool, ob, 1, true, "100", Some(long_past())).await;
    seed_order_until(&pool, ob, 2, false, "70", Some(far_future())).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.total_bid_ticks, "0", "a lapsed bid is not resting liquidity");
    assert!(body.bids.is_empty());
    assert_eq!(body.total_ask_ticks, "70");

    // The filter reads the same book: SELL matches, BUY does not.
    let listed = |side: &str| {
        let service = &service;
        let side = side.to_string();
        async move {
            let mut resp = TestClient::get(format!(
                "http://test/api/v1/inference/markets?limit=200&liquidity={side}"
            ))
            .send(service)
            .await;
            let body: Value = resp.take_json().await.expect("markets body");
            body["markets"]
                .as_array()
                .expect("markets array")
                .iter()
                .any(|m| m["inferenceOrderBookAddress"] == ob)
        }
    };
    assert!(listed("SELL").await, "the live ask still counts");
    assert!(!listed("BUY").await, "the lapsed bid does not");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn orders_hide_lapsed_rows_until_include_expired() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_orders_expiry";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_500).await;
    seed_order_until(&pool, ob, 1, true, "100", Some(long_past())).await;
    seed_order_until(&pool, ob, 2, true, "50", Some(far_future())).await;

    let ids = |query: &str| {
        let service = &service;
        let query = query.to_string();
        async move {
            let mut resp = TestClient::get(format!(
                "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&status=LIVE{query}"
            ))
            .send(service)
            .await;
            assert_eq!(resp.status_code, Some(StatusCode::OK));
            let body: Value = resp.take_json().await.expect("orders body");
            let mut v: Vec<String> = body["orders"]
                .as_array()
                .expect("orders array")
                .iter()
                .map(|o| o["orderId"].as_str().expect("orderId").to_string())
                .collect();
            v.sort();
            v
        }
    };

    assert_eq!(ids("").await, vec!["2".to_string()], "default hides the lapsed order");
    assert_eq!(
        ids("&includeExpired=true").await,
        vec!["1".to_string(), "2".to_string()],
        "includeExpired=true restores it",
    );
    assert_eq!(ids("&includeExpired=false").await, vec!["2".to_string()], "explicit false");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn a_non_boolean_include_expired_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // A typo must not silently fall back to the default — the caller would
    // never learn that expired rows were being hidden.
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress=0:x&includeExpired=yes",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn a_blank_include_expired_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Same rule as every other filter on this endpoint: an unbound template
    // variable must not quietly become the default, which hides rows.
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress=0:x&includeExpired=",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn markets_carry_the_top_of_book() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_top";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_500).await;
    // Two live bids and one lapsed bid priced above both: the quote must be the
    // best LIVE bid, not the best price on the book.
    seed_order_priced(&pool, ob, 1, true, "100", 1200, None).await;
    seed_order_priced(&pool, ob, 2, true, "100", 1000, None).await;
    seed_order_priced(&pool, ob, 3, true, "100", 9999, Some(long_past())).await;
    seed_order_priced(&pool, ob, 4, false, "100", 2000, Some(far_future())).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("markets body");
    let m = &body["markets"][0];
    // price_precision 9 on this fixture, so raw 1200 renders as 0.000001200.
    assert_eq!(m["bestBid"], "0.000001200", "the lapsed 9999 bid must not set the quote");
    assert_eq!(m["bestAsk"], "0.000002000");
    assert_eq!(m["totalAskTicks"], "100", "one live 100-tick ask; the bids are not counted");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn a_dry_book_quotes_neither_side() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_noquote";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_500).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("markets body");
    let m = &body["markets"][0];
    assert!(m["bestBid"].is_null(), "an empty side is null, not absent or zero");
    assert!(m["bestAsk"].is_null());
    assert_eq!(m["totalAskTicks"], "0", "an empty ask side totals zero, not null");

    purge(&pool, ob).await;
}
