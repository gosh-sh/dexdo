// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/inference/orders.

mod common;

use dodex_infrastructure::indexer_repo::CAPTURE_STREAM;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct OrdersBody {
    #[serde(rename = "lastUpdateId")]
    last_update_id: String,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
    #[serde(rename = "hasMore")]
    has_more: bool,
    orders: Vec<OrderItem>,
}

#[derive(Debug, Deserialize)]
struct OrderItem {
    #[serde(rename = "orderId")]
    order_id: String,
    #[serde(rename = "tokenContract")]
    token_contract: Option<String>,
    side: String,
    status: String,
    #[serde(rename = "createdAt")]
    created_at: Option<i64>,
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

// `updated_at` must be refreshed on every call, not just on first insert: the repository
// test suite (`crates/infrastructure/tests/inference_orders_repo.rs`) back-dates this same
// singleton row to exercise the read gate's capture-lag arm, and the two binaries are
// serialized (see the `serial-capture-cursor` nextest group) so they run sequentially in
// the same `--workspace` run. An `on conflict` clause that left `updated_at` untouched would
// leave the row stale after that test and close the gate for every HTTP test that follows.
async fn seed_at_head(pool: &PgPool) {
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at) \
         values ($1, 'c', true, now()) \
         on conflict (stream_name) do update set at_head = true, updated_at = now()",
    )
    .bind(CAPTURE_STREAM)
    .execute(pool)
    .await
    .expect("seed at_head");
}

async fn seed_order(
    pool: &PgPool,
    ob: &str,
    id: i64,
    is_buy: bool,
    status: &str,
    tc: Option<&str>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, status, last_chain_order, token_contract,
                chain_created_at, chain_updated_at)
           values ($1, $2::numeric, $3, 1000000000, 5, 5, false, $4, 'co-' || $2::text, $5,
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(ob)
    .bind(id)
    .bind(is_buy)
    .bind(status)
    .bind(tc)
    .execute(pool)
    .await
    .expect("seed order");
}

#[tokio::test]
async fn live_sell_for_token_contract_is_reported_with_its_order_id() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await; // the gate refuses TokenContract lookups while capture lags
    let ob = "0:orders-live";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 834, false, "OPEN", Some("0:deal-tc")).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&tokenContract=0:deal-tc&side=SELL&status=LIVE"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 1);
    assert_eq!(body.orders[0].order_id, "834");
    assert_eq!(body.orders[0].side, "SELL");
    assert_eq!(body.orders[0].status, "LIVE");
    assert!(!body.last_update_id.is_empty(), "freshness watermark must be present");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn token_contract_never_placed_cancelled_or_filled_reports_not_in_use() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await;
    let ob = "0:orders-free";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "CANCELLED", Some("0:tc-cancelled")).await;
    seed_order(&pool, ob, 2, false, "FILLED", Some("0:tc-filled")).await;

    for tc in ["0:tc-never", "0:tc-cancelled", "0:tc-filled"] {
        let mut resp = TestClient::get(format!(
            "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&tokenContract={tc}&side=SELL&status=LIVE"
        ))
        .send(&service)
        .await;
        assert_eq!(resp.status_code, Some(StatusCode::OK), "tc={tc}");
        let body: OrdersBody = resp.take_json().await.expect("orders body");
        assert!(body.orders.is_empty(), "tc={tc} must not be in use");
        assert!(!body.last_update_id.is_empty());
    }

    purge(&pool, ob).await;
}

#[tokio::test]
async fn gate_refuses_a_token_contract_query_while_a_live_sell_has_no_token_contract() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await;
    let ob = "0:orders-gate";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", None).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&tokenContract=0:x&status=LIVE"
    ))
    .send(&service)
    .await;
    // -1500 is MarketInconsistent, which ApiError maps to 503. Not 404: the book exists and
    // is reconciled, and the client should retry once the reconciler has repaired it.
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1500);

    // Safe queries stay available, and the unknown row is visibly null.
    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 1);
    assert!(body.orders[0].token_contract.is_none());

    purge(&pool, ob).await;
}

#[tokio::test]
async fn buy_orders_carry_a_null_token_contract() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await;
    let ob = "0:orders-buy";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_order(&pool, ob, 1, true, "OPEN", None).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&side=BUY"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 1);
    assert_eq!(body.orders[0].side, "BUY");
    assert!(body.orders[0].token_contract.is_none());

    purge(&pool, ob).await;
}

#[tokio::test]
async fn pagination_returns_next_cursor_and_has_more() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await;
    let ob = "0:orders-page";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    for id in 1..=3 {
        seed_order(&pool, ob, id, false, "OPEN", Some(&format!("0:tc-{id}"))).await;
    }

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&limit=2"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 2);
    assert!(body.has_more);
    assert_eq!(body.next_cursor.as_deref(), Some("2"));

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}&limit=2&cursor=2"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 1);
    assert!(!body.has_more);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn rows_with_null_chain_timestamps_are_returned_with_null_times() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await;
    let ob = "0:orders-null-ts";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, status, last_chain_order, token_contract,
                chain_created_at, chain_updated_at)
           values ($1, 1, false, 1000000000, 5, 5, false, 'OPEN', 'co-1', '0:tc', null, null)"#,
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: OrdersBody = resp.take_json().await.expect("orders body");
    assert_eq!(body.orders.len(), 1, "a missing timestamp must not hide the row");
    assert!(body.orders[0].created_at.is_none());

    purge(&pool, ob).await;
}

#[tokio::test]
async fn missing_book_address_is_1102() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await; // a closed gate must not mask a parameter assertion
    let mut resp = TestClient::get("http://test/api/v1/inference/orders").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn unknown_book_is_1121() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await; // a closed gate must not mask a parameter assertion
    let mut resp =
        TestClient::get("http://test/api/v1/inference/orders?inferenceOrderBookAddress=0:nope")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1121);
}

#[tokio::test]
async fn pagination_error_mapping_matches_prediction_orders() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await; // a closed gate must not mask a parameter assertion
    let base = "http://test/api/v1/inference/orders?inferenceOrderBookAddress=0:any";

    // Numeric but out of range → 400/-1102.
    for q in ["&limit=0", "&limit=501"] {
        let mut resp = TestClient::get(format!("{base}{q}")).send(&service).await;
        assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST), "q={q}");
        let body: Value = resp.take_json().await.expect("json");
        assert_eq!(body["code"], -1102, "q={q}");
    }
    // Blank cursor → 400/-1102.
    let mut resp = TestClient::get(format!("{base}&cursor=%20")).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);

    // Non-numeric limit, malformed cursor, unknown tokens → 400/-1130.
    let huge = "9".repeat(100);
    for q in [
        "&limit=abc".to_string(),
        "&cursor=abc".to_string(),
        "&cursor=-1".to_string(),
        format!("&cursor={huge}"),
        "&side=SIDEWAYS".to_string(),
        "&status=NEW".to_string(),
    ] {
        let mut resp = TestClient::get(format!("{base}{q}")).send(&service).await;
        assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST), "q={q}");
        let body: Value = resp.take_json().await.expect("json");
        assert_eq!(body["code"], -1130, "q={q}");
    }
}

#[tokio::test]
async fn note_and_token_contract_together_is_1130() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    seed_at_head(&pool).await; // a closed gate must not mask a parameter assertion
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/orders?inferenceOrderBookAddress=0:any&note=0:n&tokenContract=0:tc",
    )
    .send(&service)
    .await;
    // Both parameters are present, so nothing is missing: -1130, HTTP 400. Kept as its own
    // test rather than folded into the pagination mapping, because the rule it pins is a
    // filter-combination rule, not a pagination one.
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}
