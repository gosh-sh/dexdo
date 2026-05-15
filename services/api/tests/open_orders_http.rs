// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP coverage for GET /api/v1/openOrders through the production router.

mod common;

use common::canonical_query;
use common::now_ms;
use common::sign;
use common::SEED_API_SECRET;
use dodex_infrastructure::crypto::Kek;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenOrderBody {
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    status: String,
    time_in_force: String,
    #[serde(rename = "type")]
    order_type: String,
    side: String,
    time: i64,
    update_time: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenOrdersPageBody {
    orders: Vec<OpenOrderBody>,
    next_cursor: Option<String>,
}

struct Scope {
    api_key: String,
    api_secret_hex: String,
    pmp: String,
    symbol: String,
    book: String,
}

fn canonical_market_address(address: &str) -> String {
    address.replace(':', "%3A")
}

impl Scope {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            api_key: format!("dk_open_orders_readonly_{id}"),
            api_secret_hex: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
                .into(),
            pmp: format!("0:open_orders_http_{id}_pmp"),
            symbol: format!("OPEN_ORDERS_HTTP_{id}_YES"),
            book: format!("0:open_orders_http_{id}_book"),
        }
    }

    async fn cleanup(&self, pool: &PgPool) {
        sqlx::query("delete from api_keys where api_key = $1")
            .bind(&self.api_key)
            .execute(pool)
            .await
            .expect("purge api_key");
        sqlx::query("delete from live_orders where orderbook_address = $1")
            .bind(&self.book)
            .execute(pool)
            .await
            .expect("purge live_orders");
        sqlx::query("delete from market_outcomes where symbol = $1")
            .bind(&self.symbol)
            .execute(pool)
            .await
            .expect("purge market_outcomes");
        sqlx::query("delete from markets where pmp_address = $1")
            .bind(&self.pmp)
            .execute(pool)
            .await
            .expect("purge markets");
    }
}

async fn seed_readonly_key(pool: &PgPool, kek: &Kek, scope: &Scope) {
    use dodex_infrastructure::crypto;

    let account_id: uuid::Uuid =
        sqlx::query_scalar("select id from accounts where label = 'test-mm-001'")
            .fetch_one(pool)
            .await
            .expect("seeded account exists");
    let secret = hex::decode(&scope.api_secret_hex).expect("secret hex");
    let secret_enc = crypto::seal(kek, &secret).expect("seal secret");

    sqlx::query(
        r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
           values ($1, $2, $3, array['USER_DATA'::auth_permission])"#,
    )
    .bind(account_id)
    .bind(&scope.api_key)
    .bind(&secret_enc)
    .execute(pool)
    .await
    .expect("insert readonly api_key");
}

async fn trading_pn(pool: &PgPool) -> String {
    sqlx::query_scalar("select pn_address from accounts where label = 'test-mm-001'")
        .fetch_one(pool)
        .await
        .expect("seeded trading PN exists")
}

async fn insert_market(pool: &PgPool, scope: &Scope) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())
           returning id"#,
    )
    .bind(&scope.pmp)
    .bind(&scope.book)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   3, 2, '0.001', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(&scope.pmp)
    .bind(&scope.symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

async fn insert_open_order(pool: &PgPool, scope: &Scope, owner: &str) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, 42::numeric, 1, false, 12345::numeric,
                   1000::numeric, 750::numeric, $2,
                   'client-42', 'OPEN', '5f800000000000000042',
                   '5f800000000000000042',
                   to_timestamp(1700000000), to_timestamp(1700000001),
                   to_timestamp(1700000000), to_timestamp(1700000001))"#,
    )
    .bind(&scope.book)
    .bind(owner)
    .execute(pool)
    .await
    .expect("insert open order");
}

#[tokio::test]
async fn readonly_user_data_key_can_fetch_open_orders() {
    let Some((service, pool, kek)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;
    insert_open_order(&pool, &scope, &owner).await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OpenOrdersPageBody>().await.expect("open orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body.orders.len(), 1);
    assert_eq!(body.orders[0].market_address, scope.pmp);
    assert_eq!(body.orders[0].symbol, scope.symbol);
    assert_eq!(body.orders[0].order_id, "42");
    assert_eq!(body.orders[0].client_order_id, "client-42");
    assert_eq!(body.orders[0].price, "12.345");
    assert_eq!(body.orders[0].orig_qty, "10.00");
    assert_eq!(body.orders[0].executed_qty, "2.50");
    assert_eq!(body.orders[0].status, "PARTIALLY_FILLED");
    assert_eq!(body.orders[0].time_in_force, "GTC");
    assert_eq!(body.orders[0].order_type, "LIMIT");
    assert_eq!(body.orders[0].side, "SELL");
    assert_eq!(body.orders[0].time, 1_700_000_000_000);
    assert_eq!(body.orders[0].update_time, 1_700_000_001_000);
    assert!(body.next_cursor.is_none());
}

#[tokio::test]
async fn one_sided_market_filter_returns_1102() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let market = "0:open_orders_one_sided";
    let canonical_market = canonical_market_address(market);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("marketAddress", market)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn unknown_market_symbol_returns_1121() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let market = "0:open_orders_unknown_pair";
    let symbol = "OPEN_ORDERS_UNKNOWN_PAIR";
    let canonical_market = canonical_market_address(market);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("marketAddress", market)
        .query("symbol", symbol)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1121);
}

#[tokio::test]
async fn existing_pair_with_no_orders_returns_empty_array() {
    let Some((service, pool, _kek)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope).await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OpenOrdersPageBody>().await.expect("open orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    assert!(body.orders.is_empty());
    assert!(body.next_cursor.is_none());
}

#[tokio::test]
async fn missing_signature_uses_existing_auth_error() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn missing_apikey_uses_existing_auth_error() {
    let Some((service, _pool, _kek)) = common::setup().await else {
        return;
    };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn missing_timestamp_uses_existing_auth_error() {
    let Some((service, _pool, _kek)) = common::setup().await else {
        return;
    };

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("signature", "deadbeef")
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn pagination_returns_next_cursor_and_finishes() {
    let Some((service, pool, kek)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    for (i, t) in (1..=3i64).zip([1_700_000_010i64, 1_700_000_020, 1_700_000_030]) {
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_initial, amount_remaining, owner_pn_address,
                    client_order_id, status, last_chain_order,
                    placed_chain_order,
                    chain_created_at, chain_updated_at,
                    created_at, updated_at)
               values ($1, $2::numeric, 1, true, 12345::numeric,
                       1000::numeric, 1000::numeric, $3,
                       $4, 'OPEN', $5,
                       $5,
                       to_timestamp($6::bigint), to_timestamp($6::bigint),
                       to_timestamp($6::bigint), to_timestamp($6::bigint))"#,
        )
        .bind(&scope.book)
        .bind(i)
        .bind(&owner)
        .bind(format!("client-{i}"))
        .bind(format!("5f8000000000000{:05}", i))
        .bind(t)
        .execute(&pool)
        .await
        .expect("insert live order");
    }

    // Page 1: limit=2.
    let ts1 = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("limit", "2"),
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts1.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "2")
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts1.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let page1 = resp.take_json::<OpenOrdersPageBody>().await.expect("page1");
    assert_eq!(page1.orders.len(), 2);
    assert_eq!(page1.orders[0].order_id, "1");
    assert_eq!(page1.orders[1].order_id, "2");
    let cursor = page1.next_cursor.expect("cursor on partial page");

    // Page 2: pass the cursor.
    let ts2 = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("cursor", &cursor),
        ("limit", "2"),
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts2.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("cursor", cursor.as_str())
        .query("limit", "2")
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts2.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let page2 = resp.take_json::<OpenOrdersPageBody>().await.expect("page2");
    scope.cleanup(&pool).await;

    assert_eq!(page2.orders.len(), 1);
    assert_eq!(page2.orders[0].order_id, "3");
    assert!(page2.next_cursor.is_none());
}

#[tokio::test]
async fn bad_limit_returns_1102() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("limit", "501"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "501")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn limit_zero_returns_1102() {
    // Companion to the infra-level `repo_returns_empty_page_for_limit_zero`:
    // the repo accepts `limit = 0` and returns a clean empty page, but the
    // HTTP/use-case layer must reject it as out-of-range with -1102 before
    // the SQL ever runs.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical =
        canonical_query(&[("limit", "0"), ("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "0")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn unparseable_limit_returns_1102() {
    // Regression: `limit=abc` previously surfaced as -1130 (InvalidParameter)
    // via `optional_typed_query::<i64>`, breaking the openOrders error
    // contract that documents -1102 for any out-of-range / malformed limit.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("limit", "abc"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "abc")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn empty_cursor_returns_1102() {
    // Empty cursor query value is treated as a malformed cursor — the use
    // case trims to "" and returns MissingParameter (-1102 / 400). Distinct
    // from "no cursor parameter at all", which means "first page".
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical =
        canonical_query(&[("cursor", ""), ("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("cursor", "")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn whitespace_cursor_returns_1102() {
    // A whitespace-only cursor query value (e.g. URL-encoded " ") is also
    // treated as malformed — the use case trims to "" and rejects with
    // MissingParameter (-1102 / 400). Same rationale as empty_cursor.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("cursor", "%20"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/openOrders?cursor=%20&recvWindow=5000&timestamp={ts}&signature={sig}",
    );
    let mut resp = TestClient::get(url)
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn limit_above_u16_max_returns_1102() {
    // Regression: `limit=65536` previously fell through u16 parsing and returned
    // -1130 instead of the spec-required -1102. Confirm both `limit=501` (above
    // OPEN_ORDERS_MAX_LIMIT but in u16 range) and `limit=65536` (above u16 range)
    // map to the same out-of-range error.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("limit", "65536"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "65536")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn out_of_range_cursor_returns_empty_page() {
    // A well-formed cursor whose value lex-exceeds every placed_chain_order
    // returns an empty page with next_cursor: null — not an error.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    // Real msg_chain_order values from the gateway are zero-padded hex-shaped
    // strings starting with 5f80... (see graphql.rs samples and migration
    // 0016 commentary). An all-`ff` 32-char string lex-exceeds any of them
    // by the leading nibble, so the SQL predicate `placed_chain_order > $cursor`
    // matches zero rows.
    let cursor = "ffffffffffffffffffffffffffffffff";
    let canonical = canonical_query(&[
        ("cursor", cursor),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/openOrders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("cursor", cursor)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body = resp.take_json::<OpenOrdersPageBody>().await.expect("open orders body");
    assert!(body.orders.is_empty());
    assert!(body.next_cursor.is_none());
}
