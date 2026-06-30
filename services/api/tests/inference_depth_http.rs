// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/inference/depth.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct DepthBody {
    #[serde(rename = "inferenceOrderBookAddress")]
    orderbook_address: String,
    #[serde(rename = "lastUpdateId")]
    last_update_id: String,
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

async fn seed_market(pool: &PgPool, ob: &str) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, platform_fee_bps, quote_token_type,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                created_at_chain, last_reconciled_at)
           values ($1, null, 'r', 250, 2, 9, 0, '0.000000001', '1', '0.000000001',
                   to_timestamp(1700000000), now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("seed market");
}

async fn seed_order(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    price: &str,
    amount: &str,
    co: &str,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                status, last_chain_order)
           values ($1, $2::numeric, $3, $4::numeric, $5::numeric, $5::numeric, 'OPEN', $6)"#,
    )
    .bind(ob)
    .bind(id)
    .bind(is_buy)
    .bind(price)
    .bind(amount)
    .bind(co)
    .execute(pool)
    .await
    .expect("seed order");
}

#[tokio::test]
async fn happy_path_returns_scaled_depth() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_depth_http_happy";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 1, true, "1000000000", "5", "co-01").await;
    seed_order(&pool, ob, 2, false, "1050000000", "7", "co-02").await;
    seed_order(&pool, ob, 3, true, "990000000", "2", "co-03").await; // lower bid

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.orderbook_address, ob);
    assert_eq!(body.last_update_id, "co-03"); // max chain order across all touches
                                              // Bids best-first (descending), scaled by price_precision 9.
    assert_eq!(
        body.bids,
        vec![
            ["1.000000000".to_string(), "5".to_string()],
            ["0.990000000".to_string(), "2".to_string()],
        ]
    );
    assert_eq!(body.asks, vec![["1.050000000".to_string(), "7".to_string()]]);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn missing_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/inference/depth").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn unknown_book_is_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress=0:nope_inf_depth",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1121);
}

#[tokio::test]
async fn blank_address_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Present-but-blank address: `non_empty_query` trims to empty -> MissingParameter.
    let mut resp = TestClient::get("http://test/api/v1/inference/depth?inferenceOrderBookAddress=")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn corrupt_price_row_returns_503_1500() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    // inference_orders.price is numeric(78,0) with no CHECK; a negative value is
    // unsigned-undecodable -> MarketInconsistent -> 503/-1500. Proves both the
    // BigUint guard and the status mapping at the public boundary.
    let ob = "0:inf_depth_http_badprice";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 1, true, "1000000000", "5", "co-01").await;
    sqlx::query(
        "update inference_orders set price = -1 where orderbook_address = $1 and order_id = 1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1500);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn empty_book_returns_200_empty_lists() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_depth_http_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: DepthBody = resp.take_json().await.expect("depth body");
    assert_eq!(body.orderbook_address, ob);
    assert_eq!(body.last_update_id, "");
    assert!(body.bids.is_empty());
    assert!(body.asks.is_empty());

    purge(&pool, ob).await;
}

#[tokio::test]
async fn invalid_limit_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/depth?inferenceOrderBookAddress=0:any&limit=abc",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}
