// HTTP-level integration tests for GET /api/v1/account/balances.
//
// The handler resolves the market through the real PostgresReadModelRepository
// + test DB, then drives the chain side through FakePnStateReader. Tests
// seed both the markets row and the live_orders rows directly.

mod common;

use common::canonical_query;
use common::now_ms;
use common::sign;
use common::SEED_API_KEY;
use common::SEED_API_SECRET;
use dodex_application::PnStake;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct BalancesBody {
    #[serde(rename = "marketAddress")]
    market_address: String,
    #[serde(rename = "updateTime")]
    update_time: i64,
    balances: Vec<OutcomeBalanceItem>,
}

#[derive(Debug, Deserialize)]
struct OutcomeBalanceItem {
    #[serde(rename = "outcomeId")]
    outcome_id: u32,
    symbol: String,
    free: String,
    #[serde(rename = "lockedInOrders")]
    locked_in_orders: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

/// Percent-encode a query-param value using the same subset as
/// `form_urlencoded` (i.e. `url::Url::query_pairs_mut`). Only
/// unreserved chars (`A-Z a-z 0-9 - . _ ~`) and `*` pass through
/// unchanged; everything else becomes `%XX`. This matches what Salvo's
/// `TestClient::query` sends on the wire, so the HMAC covers the same
/// bytes as the server's `raw_query_string`.
fn pct_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'*' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn sign_get(api_secret: &str, recv: &str, ts: &str, market: &str) -> String {
    let market_enc = pct_encode(market);
    let canonical = canonical_query(&[
        ("marketAddress", &market_enc),
        ("recvWindow", recv),
        ("timestamp", ts),
    ]);
    sign(api_secret, &canonical, b"")
}

async fn seed_market(pool: &PgPool, tag: &str) -> (String, String) {
    let pmp = format!("0:{tag}-pmp");
    let ob = format!("0:{tag}-ob");
    // Clean slate.
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(&ob).execute(pool).await.unwrap();
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(&pmp).execute(pool).await.unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, $2, 1, 'NACKL', 42::numeric, 24::numeric, $3, 2, now())"#,
    )
    .bind(&pmp).bind(tag).bind(&ob)
    .execute(pool).await.unwrap();
    let id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(&pmp).fetch_one(pool).await.unwrap();
    for (oid, sym, name) in [(0i32, format!("{tag}-NO"), "NO"), (1, format!("{tag}-YES"), "YES")] {
        sqlx::query(
            r#"insert into market_outcomes (
                  market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                  price_precision, quantity_precision, tick_size, step_size,
                  min_notional, max_batch_size)
               values ($1, $2, $3, $4, $5, 3, 2, '0.001', '0.01', '1', 5)"#,
        )
        .bind(id).bind(&pmp).bind(oid).bind(name).bind(&sym)
        .execute(pool).await.unwrap();
    }
    (pmp, ob)
}

async fn seed_open_sell(pool: &PgPool, ob: &str, order_id: i64, outcome: i32, owner: &str, amt: &str) {
    sqlx::query(
        r#"insert into live_orders (
              orderbook_address, order_id, outcome_id, is_buy, price,
              amount_initial, amount_remaining, status, last_chain_order,
              placed_chain_order, owner_pn_address)
           values ($1, $2::numeric, $3, false, '500'::numeric, $4::numeric, $4::numeric,
                   'OPEN', '0', '0', $5)"#,
    )
    .bind(ob).bind(order_id).bind(outcome).bind(amt).bind(owner)
    .execute(pool).await.unwrap();
}

async fn seeded_pn_address(pool: &PgPool) -> String {
    // Resolve the seeded API key to its account's pn_address. Going
    // through api_keys → accounts (not "first row by created_at") is
    // deterministic — `created_at` defaults to now() and seed inserts
    // share the same instant, so an order-by would be non-deterministic.
    sqlx::query_scalar(
        "select a.pn_address \
         from accounts a \
         join api_keys k on k.account_id = a.id \
         where k.api_key = $1 limit 1",
    )
    .bind(SEED_API_KEY)
    .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn happy_path_returns_outcomes_sorted_by_id() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pmp, ob) = seed_market(&pool, "happy-bal").await;
    let pn_addr = seeded_pn_address(&pool).await;
    seed_open_sell(&pool, &ob, 1001, 1, &pn_addr, "100").await; // outcome 1: 1.00 locked
    seed_open_sell(&pool, &ob, 1002, 1, &pn_addr, "50").await;  // outcome 1: +0.50 = 1.50 total

    pn.set_stake(Some(PnStake {
        amount: vec!["1000".into(), "500".into()],          // outcome 0=10.00, 1=5.00
        debt_amount: vec!["0".into(), "0".into()],
        coupons_amount: vec!["0".into(), "0".into()],
    }));

    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string(), &pmp);
    let mut resp = TestClient::get("http://test/api/v1/account/balances")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("marketAddress", &pmp)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    let status = resp.status_code;
    let body = resp.take_json::<BalancesBody>().await.expect("balances body");
    assert_eq!(status, Some(StatusCode::OK));
    assert_eq!(body.market_address, pmp);
    assert!(body.update_time > 0);
    assert_eq!(body.balances.len(), 2);
    assert_eq!(body.balances[0].outcome_id, 0);
    assert_eq!(body.balances[0].symbol, "happy-bal-NO");
    assert_eq!(body.balances[0].free, "10.00");
    assert_eq!(body.balances[0].locked_in_orders, "0");
    assert_eq!(body.balances[1].outcome_id, 1);
    assert_eq!(body.balances[1].symbol, "happy-bal-YES");
    assert_eq!(body.balances[1].free, "5.00");
    assert_eq!(body.balances[1].locked_in_orders, "1.50");
}

#[tokio::test]
async fn no_stake_yields_zero_free_with_nonzero_locked() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pmp, ob) = seed_market(&pool, "nostake-bal").await;
    let pn_addr = seeded_pn_address(&pool).await;
    seed_open_sell(&pool, &ob, 2001, 0, &pn_addr, "75").await;
    pn.set_stake(None);

    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string(), &pmp);
    let mut resp = TestClient::get("http://test/api/v1/account/balances")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("marketAddress", &pmp)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body = resp.take_json::<BalancesBody>().await.expect("ok");
    assert_eq!(body.balances[0].free, "0");
    assert_eq!(body.balances[0].locked_in_orders, "0.75");
}

#[tokio::test]
async fn missing_market_address_returns_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let ts = now_ms();
    // Don't pass marketAddress to the canonical-query helper either —
    // signature must match what we actually send.
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/account/balances")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1102);
}

#[tokio::test]
async fn unknown_market_returns_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let market = "0:does-not-exist";
    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string(), market);
    let mut resp = TestClient::get("http://test/api/v1/account/balances")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("marketAddress", market)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1121);
}

#[tokio::test]
async fn stake_array_mismatch_returns_1500() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pmp, _ob) = seed_market(&pool, "shape-bal").await;
    pn.set_stake(Some(PnStake {
        amount: vec!["1".into()], // length 1 but num_outcomes = 2
        debt_amount: vec!["0".into(), "0".into()],
        coupons_amount: vec!["0".into(), "0".into()],
    }));
    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string(), &pmp);
    let mut resp = TestClient::get("http://test/api/v1/account/balances")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("marketAddress", &pmp)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1500);
}
