// HTTP-level integration tests for GET /api/v1/account.
//
// The handler talks to the chain through PnStateReader; tests use the
// FakePnStateReader fixture from `common` to preload deterministic
// PnDetails values. The seeded credentials in common::SEED_API_KEY /
// SEED_API_SECRET satisfy the auth hoop.

mod common;

use common::canonical_query;
use common::now_ms;
use common::sign;
use common::SEED_API_KEY;
use common::SEED_API_SECRET;
use dodex_application::PnDetails;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct AccountBody {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "updateTime")]
    update_time: i64,
    balances: Vec<AccountBalanceItem>,
}

#[derive(Debug, Deserialize)]
struct AccountBalanceItem {
    asset: String,
    free: String,
    locked: String,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

fn sign_get(api_secret: &str, recv: &str, ts: &str) -> String {
    let canonical = canonical_query(&[("recvWindow", recv), ("timestamp", ts)]);
    sign(api_secret, &canonical, b"")
}

#[tokio::test]
async fn happy_path_returns_balances_sorted_by_asset() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    pn.set_details(PnDetails {
        balance: vec![
            (3, "25000000000".to_string()),  // 25_000 USDC at 6 decimals
            (1, "10000000000".to_string()),  // 10 NACKL at 9 decimals
        ],
        locked_in_orders: vec![
            (3, "3750000000".to_string()),
            (1, "1500000000".to_string()),
        ],
    });

    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string());
    let mut resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    let status = resp.status_code;
    let body: AccountBody = resp.take_json().await.unwrap_or_else(|e| panic!("expected account body, got: {e}"));
    assert_eq!(status, Some(StatusCode::OK));
    assert!(!body.account_id.is_empty());
    assert_eq!(body.update_time > 0, true);
    assert_eq!(body.balances.len(), 2);
    assert_eq!(body.balances[0].asset, "NACKL");
    assert_eq!(body.balances[0].free, "10.000000000");
    assert_eq!(body.balances[0].locked, "1.500000000");
    assert_eq!(body.balances[1].asset, "USDC");
    assert_eq!(body.balances[1].free, "25000.000000");
    assert_eq!(body.balances[1].locked, "3750.000000");
}

#[tokio::test]
async fn missing_apikey_returns_1003() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string());
    let mut resp = TestClient::get("http://test/api/v1/account")
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn chain_fetch_failure_collapses_to_1500() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    pn.fail_details("gateway down");
    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string());
    let mut resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1500);
}

#[tokio::test]
async fn unknown_token_type_collapses_to_1500() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    pn.set_details(PnDetails {
        balance: vec![(99, "1".to_string())], // not in ref_tokens
        locked_in_orders: vec![],
    });
    let ts = now_ms();
    let sig = sign_get(SEED_API_SECRET, "5000", &ts.to_string());
    let mut resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body = resp.take_json::<ErrorBody>().await.expect("err");
    assert_eq!(body.code, -1500);
}
