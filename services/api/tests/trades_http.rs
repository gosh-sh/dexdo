// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// HTTP integration tests for GET /api/v1/trades through the production router.
// Gated on TEST_DATABASE_URL via common::setup(); see services/api/README.md
// and docker-compose.test.yml. Each test seeds a uniquely-named market so the
// shared test DB can be exercised in parallel, and cleans up after itself.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradeBody {
    trade_id: String,
    price: String,
    qty: String,
    quote_qty: String,
    time: i64,
    is_buyer_maker: bool,
}

async fn purge(pool: &PgPool, pmp: &str, book: &str) {
    sqlx::query("delete from trades where orderbook_address = $1")
        .bind(book)
        .execute(pool)
        .await
        .expect("purge trades");
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

/// Seed a reconciled USDC market (decimals 6) + one outcome (price_precision 3,
/// quantity_precision 2). `book` is the orderbook address; `cancelled` marks
/// the market terminal to prove the tape is lifecycle-independent.
async fn seed_market(pool: &PgPool, pmp: &str, symbol: &str, book: &str, cancelled: bool) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                is_cancelled, last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   $3, now())
           returning id"#,
    )
    .bind(pmp)
    .bind(book)
    .bind(cancelled)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, 1, 'YES', $3,
                   3, 2, '0.001', '0.01', '1.00')"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

async fn seed_trade(
    pool: &PgPool,
    trade_id: &str,
    book: &str,
    price: &str,
    qty: &str,
    is_buyer_maker: bool,
    chain_secs: f64,
) {
    sqlx::query(
        r#"insert into trades
               (trade_id, orderbook_address, outcome_id, price, qty,
                is_buyer_maker, chain_time)
           values ($1, $2, 1, $3::numeric, $4::numeric, $5,
                   to_timestamp($6::double precision))"#,
    )
    .bind(trade_id)
    .bind(book)
    .bind(price)
    .bind(qty)
    .bind(is_buyer_maker)
    .bind(chain_secs)
    .execute(pool)
    .await
    .expect("insert trade");
}

#[tokio::test]
async fn happy_path_returns_bare_array_newest_first_without_auth() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_happy_pmp";
    let book = "0:trades_http_happy_book";
    let symbol = "TRADES_HTTP_HAPPY_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book, false).await;
    seed_trade(&pool, "http-1", book, "6150", "500000", false, 1_710_000_004.0).await;
    seed_trade(&pool, "http-2", book, "6150", "1000000", true, 1_710_000_008.0).await;

    // No auth headers: a public route must not be 401-gated.
    let mut resp =
        TestClient::get(format!("http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}"))
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "public trades route returns 200");
    let trades: Vec<TradeBody> = resp.take_json().await.expect("bare JSON array");
    assert_eq!(trades.len(), 2);
    // Newest first by trade_id DESC.
    assert_eq!(trades[0].trade_id, "http-2");
    assert_eq!(trades[0].price, "0.615");
    assert_eq!(trades[0].qty, "1.00");
    assert_eq!(trades[0].quote_qty, "0.615000");
    assert_eq!(trades[0].time, 1_710_000_008_000);
    assert!(trades[0].is_buyer_maker);
    assert_eq!(trades[1].trade_id, "http-1");
    assert_eq!(trades[1].quote_qty, "0.307500");
    assert!(!trades[1].is_buyer_maker);

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn happy_path_respects_limit_newest_first() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_limit_pmp";
    let book = "0:trades_http_limit_book";
    let symbol = "TRADES_HTTP_LIMIT_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book, false).await;
    seed_trade(&pool, "http-limit-1", book, "6150", "1000000", true, 1_710_000_001.0).await;
    seed_trade(&pool, "http-limit-3", book, "6150", "1000000", true, 1_710_000_003.0).await;
    seed_trade(&pool, "http-limit-2", book, "6150", "1000000", true, 1_710_000_002.0).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}&limit=2"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "valid limit returns 200");
    let trades: Vec<TradeBody> = resp.take_json().await.expect("bare JSON array");
    let ids: Vec<&str> = trades.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(ids, ["http-limit-3", "http-limit-2"], "limit keeps the newest N");

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn empty_tape_is_bare_empty_array() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_empty_pmp";
    let book = "0:trades_http_empty_book";
    let symbol = "TRADES_HTTP_EMPTY_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book, false).await;

    let mut resp =
        TestClient::get(format!("http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}"))
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let trades: Vec<TradeBody> = resp.take_json().await.expect("bare JSON array");
    assert!(trades.is_empty(), "a reconciled market with no trades returns []");

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn missing_symbol_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/trades?marketAddress=0:trades_http_x")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn missing_market_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp =
        TestClient::get("http://test/api/v1/trades?symbol=TRADES_HTTP_YES").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn limit_out_of_range_is_1102() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_range_pmp";
    let book = "0:trades_http_range_book";
    let symbol = "TRADES_HTTP_RANGE_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book, false).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}&limit=1001"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1102, "out-of-range limit is -1102, like /orders");

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn non_numeric_limit_is_1130() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_nan_pmp";
    let book = "0:trades_http_nan_book";
    let symbol = "TRADES_HTTP_NAN_YES";
    purge(&pool, pmp, book).await;
    seed_market(&pool, pmp, symbol, book, false).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}&limit=abc"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1130, "non-numeric limit is -1130");

    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn unknown_pair_is_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp =
        TestClient::get("http://test/api/v1/trades?marketAddress=0:trades_http_nope&symbol=NOPE")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1121);
}

#[tokio::test]
async fn blank_orderbook_is_1500() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_blank_pmp";
    let blank = "   ";
    let symbol = "TRADES_HTTP_BLANK_YES";
    purge(&pool, pmp, blank).await;
    sqlx::query("delete from markets where orderbook_address = $1")
        .bind(blank)
        .execute(&pool)
        .await
        .expect("purge blank-orderbook residue");
    seed_market(&pool, pmp, symbol, blank, false).await;

    let mut resp =
        TestClient::get(format!("http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}"))
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body: Value = resp.take_json().await.expect("error body");
    assert_eq!(body["code"], -1500);

    purge(&pool, pmp, blank).await;
}

#[tokio::test]
async fn terminal_market_still_serves_tape() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let pmp = "0:trades_http_terminal_pmp";
    let book = "0:trades_http_terminal_book";
    let symbol = "TRADES_HTTP_TERMINAL_YES";
    purge(&pool, pmp, book).await;
    // Cancelled (terminal) market: the tape must remain readable after the
    // book closes.
    seed_market(&pool, pmp, symbol, book, true).await;
    seed_trade(&pool, "term-1", book, "6150", "1000000", true, 1_710_000_008.0).await;

    let mut resp =
        TestClient::get(format!("http://test/api/v1/trades?marketAddress={pmp}&symbol={symbol}"))
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "terminal market still serves its tape");
    let trades: Vec<TradeBody> = resp.take_json().await.expect("bare JSON array");
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].trade_id, "term-1");

    purge(&pool, pmp, book).await;
}
