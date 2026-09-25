// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/inference/liquidity and the
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
struct LiquidityBody {
    #[serde(rename = "serverTime")]
    #[allow(dead_code)]
    server_time: i64,
    #[serde(rename = "inferenceOrderBookAddress")]
    orderbook_address: String,
    #[serde(rename = "contractVersion")]
    contract_version: Option<String>,
    #[serde(rename = "bidTicks")]
    bid_ticks: String,
    #[serde(rename = "askTicks")]
    ask_ticks: String,
    #[serde(rename = "bidOrders")]
    bid_orders: i64,
    #[serde(rename = "askOrders")]
    ask_orders: i64,
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
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price,
                amount_initial, amount_remaining, status, last_chain_order)
           values ($1, $2::numeric, $3, 1000::numeric,
                   $4::numeric, $4::numeric, 'OPEN', $5)"#,
    )
    .bind(ob)
    .bind(id)
    .bind(is_buy)
    .bind(ticks)
    .bind(format!("{id:04}"))
    .execute(pool)
    .await
    .expect("seed order");
}

#[tokio::test]
async fn happy_path_returns_tick_totals_per_side() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_happy";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_000).await;
    seed_order(&pool, ob, 1, true, "100").await;
    seed_order(&pool, ob, 2, true, "50").await;
    seed_order(&pool, ob, 3, false, "25").await;

    // No auth headers: a public route must not be 401-gated.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/liquidity?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "public liquidity route returns 200");
    let body: LiquidityBody = resp.take_json().await.expect("liquidity body");
    assert_eq!(body.orderbook_address, ob, "address echoed from the request");
    assert_eq!(body.contract_version.as_deref(), Some("4.0.30"));
    assert_eq!(body.bid_ticks, "150");
    assert_eq!(body.bid_orders, 2);
    assert_eq!(body.ask_ticks, "25");
    assert_eq!(body.ask_orders, 1);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn empty_book_returns_200_with_zero_totals() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_liq_http_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob, 1_700_000_000).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/liquidity?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: LiquidityBody = resp.take_json().await.expect("liquidity body");
    assert_eq!(body.bid_ticks, "0");
    assert_eq!(body.ask_ticks, "0");
    assert_eq!(body.bid_orders, 0);
    assert_eq!(body.ask_orders, 0);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn missing_orderbook_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // The aggregate is per-book by contract; without the address there is
    // nothing to scope it to.
    let mut resp = TestClient::get("http://test/api/v1/inference/liquidity").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn unknown_book_is_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/liquidity?inferenceOrderBookAddress=0:inf_liq_http_nope",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1121);
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
