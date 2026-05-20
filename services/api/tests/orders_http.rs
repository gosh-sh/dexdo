// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP coverage for GET /api/v1/orders through the production router.

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
#[allow(dead_code)]
struct OrderBody {
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
struct OrdersPageBody {
    orders: Vec<OrderBody>,
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
            api_key: format!("dk_orders_readonly_{id}"),
            api_secret_hex: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
                .into(),
            pmp: format!("0:orders_http_{id}_pmp"),
            symbol: format!("ORDERS_HTTP_{id}_YES"),
            book: format!("0:orders_http_{id}_book"),
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

async fn seed_trade_only_key(pool: &PgPool, kek: &Kek, api_key: &str, secret_hex: &str) {
    use dodex_infrastructure::crypto;

    let account_id: uuid::Uuid =
        sqlx::query_scalar("select id from accounts where label = 'test-mm-001'")
            .fetch_one(pool)
            .await
            .expect("seeded account exists");
    let secret = hex::decode(secret_hex).expect("secret hex");
    let secret_enc = crypto::seal(kek, &secret).expect("seal secret");

    sqlx::query(
        r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
           values ($1, $2, $3, array['TRADE'::auth_permission])"#,
    )
    .bind(account_id)
    .bind(api_key)
    .bind(&secret_enc)
    .execute(pool)
    .await
    .expect("insert trade_only api_key");
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

/// Insert a NEW order (amount_remaining == amount_initial).
async fn insert_new_order(
    pool: &PgPool,
    scope: &Scope,
    owner: &str,
    order_id: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, false, 12345::numeric,
                   1000::numeric, 1000::numeric, $3,
                   $4, 'OPEN', $5,
                   $5,
                   to_timestamp(1700000000), to_timestamp(1700000001),
                   to_timestamp(1700000000), to_timestamp(1700000001))"#,
    )
    .bind(&scope.book)
    .bind(order_id)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .expect("insert NEW order");
}

/// Insert a PARTIALLY_FILLED order (amount_remaining > 0 and < amount_initial).
async fn insert_partial_order(
    pool: &PgPool,
    scope: &Scope,
    owner: &str,
    order_id: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, false, 12345::numeric,
                   1000::numeric, 750::numeric, $3,
                   $4, 'OPEN', $5,
                   $5,
                   to_timestamp(1700000010), to_timestamp(1700000011),
                   to_timestamp(1700000010), to_timestamp(1700000011))"#,
    )
    .bind(&scope.book)
    .bind(order_id)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .expect("insert PARTIALLY_FILLED order");
}

/// Insert a FILLED order (amount_remaining == 0, status == FILLED).
async fn insert_filled_order(
    pool: &PgPool,
    scope: &Scope,
    owner: &str,
    order_id: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, false, 12345::numeric,
                   1000::numeric, 0::numeric, $3,
                   $4, 'FILLED', $5,
                   $5,
                   to_timestamp(1700000020), to_timestamp(1700000021),
                   to_timestamp(1700000020), to_timestamp(1700000021))"#,
    )
    .bind(&scope.book)
    .bind(order_id)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .expect("insert FILLED order");
}

/// Insert a CANCELLED order (status == CANCELLED).
async fn insert_cancelled_order(
    pool: &PgPool,
    scope: &Scope,
    owner: &str,
    order_id: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, false, 12345::numeric,
                   1000::numeric, 0::numeric, $3,
                   $4, 'CANCELLED', $5,
                   $5,
                   to_timestamp(1700000030), to_timestamp(1700000031),
                   to_timestamp(1700000030), to_timestamp(1700000031))"#,
    )
    .bind(&scope.book)
    .bind(order_id)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .expect("insert CANCELLED order");
}

// ---- test 1: happy path with all four status buckets --------------------

#[tokio::test]
async fn readonly_user_data_key_can_fetch_orders() {
    let Some((service, pool, kek)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    // Seed one row per public status bucket, with distinct placed_chain_order
    // values so DESC sort is deterministic.
    insert_new_order(&pool, &scope, &owner, 10, "5f80000000000000000a").await;
    insert_partial_order(&pool, &scope, &owner, 11, "5f80000000000000000b").await;
    insert_filled_order(&pool, &scope, &owner, 12, "5f80000000000000000c").await;
    insert_cancelled_order(&pool, &scope, &owner, 13, "5f80000000000000000d").await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OrdersPageBody>().await.expect("orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    // All four rows must appear (DESC placed_chain_order order).
    assert_eq!(body.orders.len(), 4);
    let statuses: Vec<&str> = body.orders.iter().map(|o| o.status.as_str()).collect();
    assert!(statuses.contains(&"CANCELED"), "expected CANCELED in {statuses:?}");
    assert!(statuses.contains(&"FILLED"), "expected FILLED in {statuses:?}");
    assert!(statuses.contains(&"PARTIALLY_FILLED"), "expected PARTIALLY_FILLED in {statuses:?}");
    assert!(statuses.contains(&"NEW"), "expected NEW in {statuses:?}");
    // All rows belong to the requested market + symbol.
    for o in &body.orders {
        assert_eq!(o.market_address, scope.pmp);
        assert_eq!(o.symbol, scope.symbol);
    }
    assert!(body.next_cursor.is_none());
}

// ---- test 2: marketAddress without symbol → -1102 / 400 -----------------

#[tokio::test]
async fn returns_only_one_side_returns_minus_1102() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let market = "0:orders_one_sided";
    let canonical_market = canonical_market_address(market);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
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

// ---- test 3: unknown market pair → -1121 / 404 ---------------------------

#[tokio::test]
async fn unknown_market_pair_returns_minus_1121() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let market = "0:orders_unknown_pair";
    let symbol = "ORDERS_UNKNOWN_PAIR";
    let canonical_market = canonical_market_address(market);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
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

// ---- test 4: status=NEW,WRONG_TOKEN → -1130 / 400 -----------------------

#[tokio::test]
async fn unknown_status_token_returns_minus_1130() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    // Commas in status must be percent-encoded (%2C) so the client-side
    // canonical matches the server-side canonical (which preserves raw
    // URL bytes). Build the full URL manually to control encoding.
    let canonical = canonical_query(&[
        ("recvWindow", "5000"),
        ("status", "NEW%2CWRONG_TOKEN"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?recvWindow=5000&status=NEW%2CWRONG_TOKEN&timestamp={ts}&signature={sig}",
    );
    let mut resp = TestClient::get(url)
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1130);
}

// ---- test 5: status=PENDING_NEW → -1130 / 400 (write-side synthetic) ----

#[tokio::test]
async fn pending_new_status_token_returns_minus_1130() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    // PENDING_NEW has no comma, so no encoding issues — use the normal helper.
    let canonical = canonical_query(&[
        ("recvWindow", "5000"),
        ("status", "PENDING_NEW"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("status", "PENDING_NEW")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1130);
}

// ---- test 6: whitespace-only cursor → -1102 / 400 -----------------------

#[tokio::test]
async fn empty_cursor_returns_minus_1102() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    // Use URL-encoded space (%20) to send a whitespace-only cursor value,
    // matching the whitespace_cursor test pattern from the legacy file.
    let canonical = canonical_query(&[
        ("cursor", "%20"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?cursor=%20&recvWindow=5000&timestamp={ts}&signature={sig}",
    );
    let mut resp = TestClient::get(url)
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

// ---- test 7: limit out of range → -1102 / 400 ---------------------------

#[tokio::test]
async fn limit_out_of_range_returns_minus_1102() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };

    // limit=501 (above ORDERS_MAX_LIMIT).
    {
        let ts = now_ms();
        let canonical = canonical_query(&[
            ("limit", "501"),
            ("recvWindow", "5000"),
            ("timestamp", &ts.to_string()),
        ]);
        let sig = sign(SEED_API_SECRET, &canonical, b"");

        let mut resp = TestClient::get("http://test/api/v1/orders")
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

    // limit=0 (below minimum).
    {
        let ts = now_ms();
        let canonical = canonical_query(&[
            ("limit", "0"),
            ("recvWindow", "5000"),
            ("timestamp", &ts.to_string()),
        ]);
        let sig = sign(SEED_API_SECRET, &canonical, b"");

        let mut resp = TestClient::get("http://test/api/v1/orders")
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
}

// ---- test 8: pagination round-trip in DESC order -------------------------

#[tokio::test]
async fn pagination_roundtrip_returns_descending_order() {
    let Some((service, pool, kek)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    // Seed 5 mixed-status rows with distinct placed_chain_order values
    // (ascending hex strings so DESC sort reverses them: row 5 comes first).
    insert_new_order(&pool, &scope, &owner, 1, "5f80000000000000000000001").await;
    insert_partial_order(&pool, &scope, &owner, 2, "5f80000000000000000000002").await;
    insert_filled_order(&pool, &scope, &owner, 3, "5f80000000000000000000003").await;
    insert_cancelled_order(&pool, &scope, &owner, 4, "5f80000000000000000000004").await;
    insert_new_order(&pool, &scope, &owner, 5, "5f80000000000000000000005").await;

    // Page 1: limit=2 — DESC order should return rows 5 and 4.
    let ts1 = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("limit", "2"),
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts1.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("limit", "2")
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts1.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let page1 = resp.take_json::<OrdersPageBody>().await.expect("page1");
    assert_eq!(page1.orders.len(), 2, "page 1 must have 2 rows");
    // DESC: row 5 first, then row 4.
    assert_eq!(page1.orders[0].order_id, "5");
    assert_eq!(page1.orders[1].order_id, "4");
    let cursor = page1.next_cursor.expect("cursor on partial page");

    // Page 2: pass the cursor — expect rows 3 and 2 (still 1 left after).
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
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
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
    let page2 = resp.take_json::<OrdersPageBody>().await.expect("page2");
    assert_eq!(page2.orders.len(), 2, "page 2 must have 2 rows");
    assert_eq!(page2.orders[0].order_id, "3");
    assert_eq!(page2.orders[1].order_id, "2");
    let cursor2 = page2.next_cursor.expect("cursor on second partial page");

    // Page 3: expect only row 1, nextCursor=null.
    let ts3 = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("cursor", &cursor2),
        ("limit", "2"),
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts3.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("cursor", cursor2.as_str())
        .query("limit", "2")
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts3.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let page3 = resp.take_json::<OrdersPageBody>().await.expect("page3");
    scope.cleanup(&pool).await;

    assert_eq!(page3.orders.len(), 1, "last page must have 1 row");
    assert_eq!(page3.orders[0].order_id, "1");
    assert!(page3.next_cursor.is_none(), "last page must have nextCursor=null");
}

// ---- test 9: missing signature → -1003 / 401 ----------------------------

#[tokio::test]
async fn missing_auth_returns_minus_1003() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

// ---- test 10: TRADE-only key → -1002 / 401 ------------------------------

#[tokio::test]
async fn trade_only_key_returns_minus_1002() {
    let Some((service, pool, kek)) = common::setup().await else { return };

    let id = uuid::Uuid::new_v4().simple().to_string();
    let trade_key = format!("dk_orders_trade_only_{id}");
    let trade_secret_hex = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    seed_trade_only_key(&pool, &kek, &trade_key, trade_secret_hex).await;

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(trade_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", trade_key.as_str(), true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<ErrorBody>().await.expect("error body");

    // Cleanup before assertion.
    sqlx::query("delete from api_keys where api_key = $1")
        .bind(&trade_key)
        .execute(&pool)
        .await
        .expect("cleanup trade_only key");

    assert_eq!(status, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(body.code, -1002);
}
