// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/inference/trades.
//
// `inference_trades.trade_id` is a GLOBAL primary key over a shared test DB, so every
// seeded id carries a per-test prefix — see the same note in
// crates/infrastructure/tests/inference_trades_repo.rs.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct TradeBody {
    #[serde(rename = "tradeId")]
    trade_id: String,
    price: String,
    qty: String,
    #[serde(rename = "quoteQty")]
    quote_qty: String,
    time: i64,
    #[serde(rename = "isBuyerMaker")]
    is_buyer_maker: bool,
}

async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from inference_trades where orderbook_address = $1")
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

/// `version` is deliberately not seeded — the tape does not read it.
async fn seed_market(pool: &PgPool, ob: &str) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, platform_fee_bps, quote_token_type,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                created_at_chain, last_reconciled_at)
           values ($1, null, 'r', 250, 2, 9, 0, '1', '1', '1',
                   to_timestamp(1700000000), now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("seed market");
}

async fn seed_trade(pool: &PgPool, ob: &str, trade_id: &str, price: &str, qty: &str, ibm: bool) {
    sqlx::query(
        r#"insert into inference_trades
               (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time)
           values ($1, $2, $3::numeric, $4::numeric, $5, to_timestamp(1700000000))"#,
    )
    .bind(trade_id)
    .bind(ob)
    .bind(price)
    .bind(qty)
    .bind(ibm)
    .execute(pool)
    .await
    .expect("seed trade");
}

#[tokio::test]
async fn happy_path_returns_bare_array_newest_first() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_trades_http_happy";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_trade(&pool, ob, "hap-1", "1500000000", "4", true).await;
    seed_trade(&pool, ob, "hap-2", "2000000000", "3", false).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    // Deserialized straight into a Vec: the body is a bare array, not an envelope.
    let body: Vec<TradeBody> = resp.take_json().await.expect("tape body");
    assert_eq!(body.len(), 2);
    assert_eq!(body[0].trade_id, "hap-2");
    assert_eq!(body[0].price, "2.000000000");
    assert_eq!(body[0].qty, "3");
    assert_eq!(body[0].quote_qty, "6.000000000");
    assert_eq!(body[0].time, 1_700_000_000_000);
    assert!(!body[0].is_buyer_maker);
    assert_eq!(body[1].trade_id, "hap-1");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn book_without_matches_returns_empty_array() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_trades_http_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    // Asserted on the raw JSON: the body must be `[]` — never `null`, never an object
    // with an empty field. A book that has opened but not traded is the steady state.
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body, serde_json::json!([]));

    purge(&pool, ob).await;
}

#[tokio::test]
async fn out_of_range_limit_clamps_instead_of_failing() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_trades_http_clamp";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_trade(&pool, ob, "clp-1", "1000000000", "1", true).await;
    seed_trade(&pool, ob, "clp-2", "1000000000", "1", true).await;

    // 0 clamps up to 1 (inference convention), not -1102 like the prediction tape.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}&limit=0"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Vec<TradeBody> = resp.take_json().await.expect("tape body");
    assert_eq!(body.len(), 1);

    // Above the max clamps down and still serves.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}&limit=999999"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Vec<TradeBody> = resp.take_json().await.expect("tape body");
    assert_eq!(body.len(), 2);

    // Present-but-blank collapses to "absent" (optional_typed_query returns Ok(None)),
    // so it takes the default — NOT -1102 like a blank address, which uses non_empty_query.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}&limit="
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Vec<TradeBody> = resp.take_json().await.expect("tape body");
    assert_eq!(body.len(), 2, "blank limit falls back to the default");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn non_numeric_limit_is_1130() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_trades_http_badlimit";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress={ob}&limit=abc"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn missing_or_blank_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    for url in [
        "http://test/api/v1/inference/trades",
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress=",
    ] {
        let mut resp = TestClient::get(url).send(&service).await;
        assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST), "{url}");
        let body: Value = resp.take_json().await.expect("json");
        assert_eq!(body["code"], -1102, "{url}");
    }
}

#[tokio::test]
async fn unknown_book_is_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/trades?inferenceOrderBookAddress=0:nope_inf_trades",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1121);
}
