// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// HTTP integration tests for GET /api/v1/prediction/depth through the production
// router. Gated on TEST_DATABASE_URL via common::setup(); see
// services/api/README.md and docker-compose.test.yml. The handler keys the
// order-book lookup on the renamed `predictionMarketAddress` query param
// (services/api/src/lib.rs) and echoes it back in the body — a happy-path
// round-trip is the only thing that proves the param read still resolves after
// the rename (the repo-level depth.rs test exercises the DepthSnapshot, not the
// HTTP DTO). Each test seeds a uniquely-named market so the shared test DB can
// run them in parallel, and cleans up after itself.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct DepthBody {
    #[serde(rename = "predictionMarketAddress")]
    market_address: String,
    symbol: String,
    #[serde(rename = "lastUpdateId")]
    #[allow(dead_code)]
    last_update_id: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

async fn purge(pool: &PgPool, pmp: &str, book: &str) {
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(book)
        .execute(pool)
        .await
        .expect("purge live_orders");
    sqlx::query("delete from market_outcomes where pmp_address = $1")
        .bind(pmp)
        .execute(pool)
        .await
        .expect("purge market_outcomes");
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(pool)
        .await
        .expect("purge markets");
}

/// Seed a reconciled USDC market + one outcome (price_precision 2,
/// quantity_precision 2; USDC decimals 6) so the
/// (predictionMarketAddress, symbol) pair resolves and on-grid raw order
/// values scale cleanly — see depth.rs for the basis-point/atom convention.
async fn seed_market(pool: &PgPool, pmp: &str, symbol: &str, book: &str) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                is_cancelled, last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   false, now())
           returning id"#,
    )
    .bind(pmp)
    .bind(book)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, 1, 'YES', $3,
                   2, 2, '0.01', '0.01', '1.00')"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

async fn seed_order(
    pool: &PgPool,
    book: &str,
    order_id: i64,
    is_buy: bool,
    price: &str,
    amount: &str,
    chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, $3, $4::numeric,
                   $5::numeric, $5::numeric, 'OPEN',
                   $6, $6)"#,
    )
    .bind(book)
    .bind(order_id)
    .bind(is_buy)
    .bind(price)
    .bind(amount)
    .bind(chain_order)
    .execute(pool)
    .await
    .expect("insert live_order");
}

#[tokio::test]
async fn happy_path_returns_depth_keyed_on_prediction_market_address() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:depth_http_happy_pmp";
    let book = "0:depth_http_happy_book";
    let symbol = "DEPTH_HTTP_HAPPY_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book).await;
    // live_orders mirrors the chain: price in basis points (10^-4), amount in
    // token atoms (USDC decimals 6). On-grid for precision 2:
    //   6100 bps -> "0.61", 100_000_000 atoms -> "100.00".
    seed_order(&pool, book, 1, true, "6100", "100000000", "01").await;
    seed_order(&pool, book, 2, false, "6200", "50000000", "02").await;

    // No auth headers: a public route must not be 401-gated. Sending the
    // renamed `predictionMarketAddress` must resolve — if the handler still
    // read `marketAddress`, this would be -1102 instead of 200.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/prediction/depth?predictionMarketAddress={pmp}&symbol={symbol}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "public depth route returns 200");
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.market_address, pmp, "predictionMarketAddress echoed from the request");
    assert_eq!(body.symbol, symbol);
    assert_eq!(body.bids, vec![["0.61".to_string(), "100.00".to_string()]], "scaled bid level");
    assert_eq!(body.asks, vec![["0.62".to_string(), "50.00".to_string()]], "scaled ask level");

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn empty_book_returns_200_with_prediction_market_address() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:depth_http_empty_pmp";
    let book = "0:depth_http_empty_book";
    let symbol = "DEPTH_HTTP_EMPTY_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/prediction/depth?predictionMarketAddress={pmp}&symbol={symbol}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.market_address, pmp);
    assert!(body.bids.is_empty(), "a reconciled market with no orders has an empty book");
    assert!(body.asks.is_empty());

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn missing_prediction_market_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // No `predictionMarketAddress` → the handler's mandatory-param read fails.
    let mut resp = TestClient::get("http://test/api/v1/prediction/depth?symbol=DEPTH_HTTP_X")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}
