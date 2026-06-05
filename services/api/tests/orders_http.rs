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

/// Full DTO mirror of the server's `OrderResponse`. The fields not
/// asserted on individually in test bodies are still validated by the
/// happy-path `readonly_user_data_key_can_fetch_orders` test below,
/// which checks every field is structurally well-formed (non-empty
/// where required, positive timestamps). That gives us a single
/// serialization-shape regression point — anything else relies on the
/// type-level Deserialize derive failing if the server's JSON skews.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
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
                min_notional)
           values ($1, $2, 1, 'YES', $3,
                   -- quantity_precision = decimals (6): these HTTP tests
                   -- exercise filtering/pagination, not amount scaling, so the
                   -- chain-atoms → display descale is a no-op here. The real
                   -- bps/atom decode is pinned with contract numbers in the
                   -- infra depth tests.
                   3, 6, '0.001', '0.01',
                   '1.00')"#,
    )
    .bind(market_id)
    .bind(&scope.pmp)
    .bind(&scope.symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

/// Seed a single `live_orders` row. `raw_status` is the DB-side enum
/// ("OPEN", "FILLED", "CANCELLED"); `amount_remaining` is the residual
/// after fills. The mapping from row state to the public `OrderStatus`
/// is exercised on the read path; these tests only need raw inputs.
#[allow(clippy::too_many_arguments)]
async fn insert_order(
    pool: &PgPool,
    scope: &Scope,
    owner: &str,
    order_id: i64,
    is_buy: bool,
    placed_chain_order: &str,
    raw_status: &str,
    amount_remaining: i64,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, $3, 12340::numeric,
                   1000::numeric, $4::numeric, $5,
                   $6, $7, $8,
                   $8,
                   to_timestamp(1700000000), to_timestamp(1700000001),
                   to_timestamp(1700000000), to_timestamp(1700000001))"#,
    )
    .bind(&scope.book)
    .bind(order_id)
    .bind(is_buy)
    .bind(amount_remaining)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(raw_status)
    .bind(placed_chain_order)
    .execute(pool)
    .await
    .unwrap_or_else(|err| panic!("insert {raw_status} order: {err}"));
}

// happy path with all four status buckets

#[tokio::test]
async fn readonly_user_data_key_can_fetch_orders() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    // Seed one row per public status bucket, with distinct placed_chain_order
    // values so DESC sort is deterministic. Sides alternate BUY/SELL so the
    // `is_buy` → `OrderSide` mapping is asserted positively below — a regression
    // hardcoding `OrderSide::Buy` (or `Sell`) would surface here.
    insert_order(&pool, &scope, &owner, 10, true, "5f80000000000000000a", "OPEN", 1000).await;
    insert_order(&pool, &scope, &owner, 11, false, "5f80000000000000000b", "OPEN", 750).await;
    insert_order(&pool, &scope, &owner, 12, true, "5f80000000000000000c", "FILLED", 0).await;
    insert_order(&pool, &scope, &owner, 13, false, "5f80000000000000000d", "CANCELLED", 0).await;

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
    // Every field decoded by the DTO must be structurally well-formed.
    // This is the single regression point that catches a server-side
    // JSON shape skew (rename, type change, missing field).
    for o in &body.orders {
        assert_eq!(o.market_address, scope.pmp);
        assert_eq!(o.symbol, scope.symbol);
        assert!(!o.order_id.is_empty(), "order_id present for non-REJECTED rows");
        // `clientOrderId` is an empty string when the seed did not
        // supply one; either form is well-formed (no NULL on the
        // wire). Decoded as plain String, not Option<String>.
        let _ = &o.client_order_id;
        // insert_order seeds raw price = 12340 basis points (on the 10-bps tick
        // grid); the read path decodes bps → probability (raw / FULL_PERCENT)
        // at price_precision=3, so "12340" → "1.234". Exact equality catches a
        // wire-shape regression (e.g. price/origQty swap in the JSON mapper)
        // that a non-empty check would miss.
        assert_eq!(o.price, "1.234", "price decoded from basis points at price_precision=3");
        assert!(!o.orig_qty.is_empty(), "orig_qty rendered as decimal string");
        // `executedQty` is allowed to be "0" or non-zero depending on
        // status; we only assert it decoded cleanly, not its value.
        let _ = &o.executed_qty;
        assert_eq!(o.time_in_force, "GTC");
        assert_eq!(o.order_type, "LIMIT");
        // Per-row side check — seeds at the top alternate BUY/SELL by
        // `order_id` parity. Vacuous "BUY or SELL" would pass even
        // against a hardcoded mapping, so assert the exact value.
        let expected_side = if o.order_id.parse::<i64>().expect("numeric order_id") % 2 == 0 {
            "BUY"
        } else {
            "SELL"
        };
        assert_eq!(o.side, expected_side, "side mapping for order_id={}", o.order_id);
        // time/updateTime are unix ms (api-spec §Orders). Storage is µs
        // (SQL `extract(epoch from ...) * 1_000_000`), projector divides
        // by 1_000. Seed `chain_created_at = to_timestamp(1700000000)`
        // → 1_700_000_000_000 ms; `chain_updated_at = to_timestamp(1700000001)`
        // → 1_700_000_001_000 ms. Exact equality catches a unit-conversion
        // regression that a `> 0` check misses — µs would be 1e3× larger.
        assert_eq!(o.time, 1_700_000_000_000, "time must be unix ms (seed * 1000)");
        assert_eq!(o.update_time, 1_700_000_001_000, "updateTime must be unix ms (seed * 1000)");
    }
    assert!(body.next_cursor.is_none());
}

#[tokio::test]
async fn status_filter_narrows_orders_through_http() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    insert_order(&pool, &scope, &owner, 20, true, "5f800000000000000020", "OPEN", 1000).await;
    insert_order(&pool, &scope, &owner, 21, true, "5f800000000000000021", "FILLED", 0).await;
    insert_order(&pool, &scope, &owner, 22, true, "5f800000000000000022", "CANCELLED", 0).await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("status", "FILLED"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("status", "FILLED")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OrdersPageBody>().await.expect("orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body.orders.len(), 1);
    assert_eq!(body.orders[0].order_id, "21");
    assert_eq!(body.orders[0].status, "FILLED");
}

/// `?status=PARTIALLY_FILLED` exercises the 3-conjunct OPEN heap predicate
/// (`status='OPEN' AND amount_remaining < amount_initial AND amount_remaining > 0`)
/// end-to-end through the production router. A regression that loosened
/// any conjunct — e.g. dropping the `> 0` guard and leaking a stale
/// projector-bug row, or dropping the `< amount_initial` guard and
/// returning unfilled NEW rows — would surface here.
#[tokio::test]
async fn partially_filled_status_filter_narrows_orders_through_http() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    // amount_initial is hardcoded to 1000 in insert_order; amount_remaining
    // = 1000 → NEW, 0 → FILLED/CANCELED (per raw_status), 0 < x < 1000 → PARTIAL.
    insert_order(&pool, &scope, &owner, 30, true, "5f800000000000000030", "OPEN", 1000).await;
    insert_order(&pool, &scope, &owner, 31, true, "5f800000000000000031", "OPEN", 750).await;
    insert_order(&pool, &scope, &owner, 32, true, "5f800000000000000032", "FILLED", 0).await;
    insert_order(&pool, &scope, &owner, 33, true, "5f800000000000000033", "CANCELLED", 0).await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("status", "PARTIALLY_FILLED"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("status", "PARTIALLY_FILLED")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OrdersPageBody>().await.expect("orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body.orders.len(), 1, "only the partial-fill row passes the 3-conjunct filter");
    assert_eq!(body.orders[0].order_id, "31");
    assert_eq!(body.orders[0].status, "PARTIALLY_FILLED");
    assert!(body.next_cursor.is_none());
}

/// Multi-token `?status=NEW,FILLED` reaches the production router with a
/// real comma in the canonical and renders the disjunctive SQL predicate
/// for two buckets. A regression that mis-parsed a valid multi-token CSV
/// (treating the entire string as one token, or dropping all but one
/// element) would slip past the single-token and invalid-CSV cases the
/// rest of this suite covers.
#[tokio::test]
async fn multi_token_status_csv_narrows_orders_through_http() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    insert_order(&pool, &scope, &owner, 40, true, "5f800000000000000040", "OPEN", 1000).await;
    insert_order(&pool, &scope, &owner, 41, true, "5f800000000000000041", "OPEN", 750).await;
    insert_order(&pool, &scope, &owner, 42, true, "5f800000000000000042", "FILLED", 0).await;
    insert_order(&pool, &scope, &owner, 43, true, "5f800000000000000043", "CANCELLED", 0).await;

    let ts = now_ms();
    let canonical_market = canonical_market_address(&scope.pmp);
    // %2C in canonical to match the URL bytes the HMAC verifier signs;
    // TestClient::query would re-encode and break the signature, so the
    // URL is built manually (mirrors empty_cursor_returns_minus_1102).
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("status", "NEW%2CFILLED"),
        ("symbol", &scope.symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?marketAddress={canonical_market}&recvWindow=5000&status=NEW%2CFILLED&symbol={}&timestamp={ts}&signature={sig}",
        scope.symbol,
    );
    let mut resp = TestClient::get(url)
        .add_header("X-DODEX-APIKEY", scope.api_key.as_str(), true)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<OrdersPageBody>().await.expect("orders body");
    scope.cleanup(&pool).await;

    assert_eq!(status, Some(StatusCode::OK));
    // DESC placed_chain_order: 42 (FILLED) > 40 (NEW); PARTIAL=41 and
    // CANCELED=43 must be excluded.
    let ids: Vec<&str> = body.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids, vec!["42", "40"], "only NEW + FILLED tokens pass");
    let statuses: Vec<&str> = body.orders.iter().map(|o| o.status.as_str()).collect();
    assert_eq!(statuses, vec!["FILLED", "NEW"]);
    assert!(body.next_cursor.is_none());
}

/// HTTP-layer owner scoping: the handler must filter by
/// `ctx.trading_pn.pn_address` so a row owned by a different PrivateNote
/// on the same book never appears in the response. The repo-layer
/// equivalent (`returns_only_owner_rows_across_all_statuses`) pins the SQL
/// predicate; this test pins the handler → use case → repo wiring so a
/// regression that drops the owner from `GetOrdersInput`, reads the wrong
/// context field, or stops binding `$1` would surface end-to-end.
#[tokio::test]
async fn foreign_owner_rows_are_filtered_out() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;
    let foreign_owner = format!("0:foreign_owner_{}", uuid::Uuid::new_v4().simple());

    insert_order(&pool, &scope, &owner, 30, true, "5f800000000000000030", "OPEN", 1000).await;
    insert_order(&pool, &scope, &foreign_owner, 31, true, "5f800000000000000031", "OPEN", 1000)
        .await;

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
    assert_eq!(body.orders.len(), 1, "foreign-owned row must not appear");
    assert_eq!(body.orders[0].order_id, "30");
    assert!(body.next_cursor.is_none());
}

// Half-supplied (marketAddress, symbol) pair → -1102 / 400. Both
// directions are pinned: the use case's invariant is "either both or
// neither", and a regression that loosens it in one direction must
// surface immediately.

#[tokio::test]
async fn market_address_without_symbol_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
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

#[tokio::test]
async fn symbol_without_market_address_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let symbol = "ORDERS_ONE_SIDED_SYMBOL";
    let canonical = canonical_query(&[
        ("recvWindow", "5000"),
        ("symbol", symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("symbol", symbol)
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

// Present-but-blank `marketAddress` / `symbol` trips -1102 instead of
// silently collapsing to "no filter". Mirrors the cursor contract — a
// client sending `?marketAddress=&symbol=` is signalling an unbound
// template variable, not "all markets". The HMAC verifier signs the
// raw query bytes, so the URL is built manually to keep blank values
// intact (TestClient::query would re-encode and break the signature).

#[tokio::test]
async fn blank_market_address_with_symbol_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let symbol = "ORDERS_BLANK_MA";
    let canonical = canonical_query(&[
        ("marketAddress", ""),
        ("recvWindow", "5000"),
        ("symbol", symbol),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?marketAddress=&recvWindow=5000&symbol={symbol}&timestamp={ts}&signature={sig}",
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
async fn blank_symbol_with_market_address_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let market = "0:orders_blank_symbol";
    let canonical_market = canonical_market_address(market);
    let canonical = canonical_query(&[
        ("marketAddress", &canonical_market),
        ("recvWindow", "5000"),
        ("symbol", ""),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?marketAddress={canonical_market}&recvWindow=5000&symbol=&timestamp={ts}&signature={sig}",
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
async fn both_blank_market_pair_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("marketAddress", ""),
        ("recvWindow", "5000"),
        ("symbol", ""),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let url = format!(
        "http://test/api/v1/orders?marketAddress=&recvWindow=5000&symbol=&timestamp={ts}&signature={sig}",
    );
    let mut resp = TestClient::get(url)
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1102);
}

// unknown market pair → -1121 / 404

#[tokio::test]
async fn unknown_market_pair_returns_minus_1121() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
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

// status=NEW,WRONG_TOKEN → -1130 / 400

#[tokio::test]
async fn unknown_status_token_returns_minus_1130() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
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

// status=PENDING_NEW → -1130 / 400 (write-side synthetic)

#[tokio::test]
async fn pending_new_status_token_returns_minus_1130() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
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

// status=PENDING_CANCEL → -1130 / 400 (write-side synthetic)

#[tokio::test]
async fn pending_cancel_status_token_returns_minus_1130() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[
        ("recvWindow", "5000"),
        ("status", "PENDING_CANCEL"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("status", "PENDING_CANCEL")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1130);
}

// whitespace-only cursor → -1102 / 400

#[tokio::test]
async fn empty_cursor_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    // URL-encoded space (%20) sends a whitespace-only cursor. We build
    // the URL manually because `TestClient::query("cursor", " ")` lets
    // the URL encoder rewrite the value, which makes the server's HMAC
    // verifier (which signs the raw query bytes) reject the request
    // before the cursor-validation path even runs.
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

// limit out of range → -1102 / 400

#[tokio::test]
async fn limit_out_of_range_returns_minus_1102() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };

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

    // limit=-1 (negative).
    {
        let ts = now_ms();
        let canonical = canonical_query(&[
            ("limit", "-1"),
            ("recvWindow", "5000"),
            ("timestamp", &ts.to_string()),
        ]);
        let sig = sign(SEED_API_SECRET, &canonical, b"");

        let mut resp = TestClient::get("http://test/api/v1/orders")
            .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
            .query("limit", "-1")
            .query("recvWindow", "5000")
            .query("timestamp", ts.to_string())
            .query("signature", sig)
            .send(&service)
            .await;
        assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
        let body = resp.take_json::<ErrorBody>().await.expect("error body");
        assert_eq!(body.code, -1102);
    }

    // limit=65536 parses as i64 but exceeds both u16 and ORDERS_MAX_LIMIT;
    // it must remain an out-of-range numeric (-1102), not parse-invalid (-1130).
    {
        let ts = now_ms();
        let canonical = canonical_query(&[
            ("limit", "65536"),
            ("recvWindow", "5000"),
            ("timestamp", &ts.to_string()),
        ]);
        let sig = sign(SEED_API_SECRET, &canonical, b"");

        let mut resp = TestClient::get("http://test/api/v1/orders")
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
}

// non-numeric limit → -1130 / 400 (out-of-range numerics get -1102; this is the parse-failure path)

#[tokio::test]
async fn non_numeric_limit_returns_minus_1130() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };

    let ts = now_ms();
    let canonical = canonical_query(&[
        ("limit", "abc"),
        ("recvWindow", "5000"),
        ("timestamp", &ts.to_string()),
    ]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/orders")
        .add_header("X-DODEX-APIKEY", common::SEED_API_KEY, true)
        .query("limit", "abc")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1130);
}

// pagination round-trip in DESC order

/// Fetch one page of `/api/v1/orders` for a market-scoped query with
/// an optional cursor. Hides the canonical-query + sign + TestClient
/// boilerplate so the test bodies above stay focused on what they
/// assert. Returns the parsed `OrdersPageBody`.
async fn get_orders_page(
    service: &salvo::Service,
    scope: &Scope,
    limit: u32,
    cursor: Option<&str>,
) -> OrdersPageBody {
    let ts = now_ms();
    let ts_string = ts.to_string();
    let limit_string = limit.to_string();
    let canonical_market = canonical_market_address(&scope.pmp);

    // The signature must cover the exact same parameter set (and
    // ASCII-sorted order) the wire request carries.
    let mut params: Vec<(&str, &str)> = Vec::new();
    if let Some(c) = cursor {
        params.push(("cursor", c));
    }
    params.push(("limit", limit_string.as_str()));
    params.push(("marketAddress", canonical_market.as_str()));
    params.push(("recvWindow", "5000"));
    params.push(("symbol", scope.symbol.as_str()));
    params.push(("timestamp", ts_string.as_str()));
    let canonical = canonical_query(&params);
    let sig = sign(&scope.api_secret_hex, &canonical, b"");

    let mut req = TestClient::get("http://test/api/v1/orders").add_header(
        "X-DODEX-APIKEY",
        scope.api_key.as_str(),
        true,
    );
    if let Some(c) = cursor {
        req = req.query("cursor", c);
    }
    let mut resp = req
        .query("limit", limit_string.as_str())
        .query("marketAddress", scope.pmp.as_str())
        .query("symbol", scope.symbol.as_str())
        .query("recvWindow", "5000")
        .query("timestamp", ts_string.as_str())
        .query("signature", sig)
        .send(service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK), "get_orders_page");
    resp.take_json::<OrdersPageBody>().await.expect("orders page body")
}

#[tokio::test]
async fn pagination_roundtrip_returns_descending_order() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    seed_readonly_key(&pool, &kek, &scope).await;
    insert_market(&pool, &scope).await;
    let owner = trading_pn(&pool).await;

    // Seed 5 mixed-status rows with distinct placed_chain_order values
    // (ascending hex strings so DESC sort reverses them: row 5 comes first).
    insert_order(&pool, &scope, &owner, 1, false, "5f80000000000000000000001", "OPEN", 1000).await;
    insert_order(&pool, &scope, &owner, 2, false, "5f80000000000000000000002", "OPEN", 750).await;
    insert_order(&pool, &scope, &owner, 3, false, "5f80000000000000000000003", "FILLED", 0).await;
    insert_order(&pool, &scope, &owner, 4, false, "5f80000000000000000000004", "CANCELLED", 0)
        .await;
    insert_order(&pool, &scope, &owner, 5, false, "5f80000000000000000000005", "OPEN", 1000).await;

    // Page 1: rows 5 and 4 (DESC).
    let page1 = get_orders_page(&service, &scope, 2, None).await;
    assert_eq!(page1.orders.len(), 2, "page 1 must have 2 rows");
    assert_eq!(page1.orders[0].order_id, "5");
    assert_eq!(page1.orders[1].order_id, "4");
    let cursor = page1.next_cursor.expect("cursor on partial page");

    // Page 2: rows 3 and 2.
    let page2 = get_orders_page(&service, &scope, 2, Some(&cursor)).await;
    assert_eq!(page2.orders.len(), 2, "page 2 must have 2 rows");
    assert_eq!(page2.orders[0].order_id, "3");
    assert_eq!(page2.orders[1].order_id, "2");
    let cursor2 = page2.next_cursor.expect("cursor on second partial page");

    // Page 3: row 1 only, nextCursor=null.
    let page3 = get_orders_page(&service, &scope, 2, Some(&cursor2)).await;
    scope.cleanup(&pool).await;

    assert_eq!(page3.orders.len(), 1, "last page must have 1 row");
    assert_eq!(page3.orders[0].order_id, "1");
    assert!(page3.next_cursor.is_none(), "last page must have nextCursor=null");
}

// missing signature → -1003 / 401

#[tokio::test]
async fn missing_auth_returns_minus_1003() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
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

// TRADE-only key → -1002 / 401

#[tokio::test]
async fn trade_only_key_returns_minus_1002() {
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };

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
