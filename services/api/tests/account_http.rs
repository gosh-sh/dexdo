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
use common::SEED_API_KEY_2;
use common::SEED_API_SECRET;
use common::SEED_API_SECRET_2;
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
            (3, "25000000000".to_string()), // 25_000 USDC at 6 decimals
            (1, "10000000000".to_string()), // 10 NACKL at 9 decimals
        ],
        locked_in_orders: vec![(3, "3750000000".to_string()), (1, "1500000000".to_string())],
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
    let body: AccountBody =
        resp.take_json().await.unwrap_or_else(|e| panic!("expected account body, got: {e}"));
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

#[tokio::test]
async fn account_id_differs_per_credential() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };

    let pn1 = common::seeded_pn_address_for_key(&pool, SEED_API_KEY).await;
    let pn2 = common::seeded_pn_address_for_key(&pool, SEED_API_KEY_2).await;

    // Different free values so we can assert the right PN was queried.
    pn.set_details_for(
        &pn1,
        PnDetails {
            balance: vec![(1, "10000000000".to_string())], // 10 NACKL
            locked_in_orders: vec![],
        },
    );
    pn.set_details_for(
        &pn2,
        PnDetails {
            balance: vec![(1, "20000000000".to_string())], // 20 NACKL
            locked_in_orders: vec![],
        },
    );

    let ts1 = now_ms();
    let sig1 = sign_get(SEED_API_SECRET, "5000", &ts1.to_string());
    let mut resp1 = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts1.to_string())
        .query("signature", sig1)
        .send(&service)
        .await;
    assert_eq!(resp1.status_code, Some(StatusCode::OK));
    let body1: AccountBody = resp1.take_json().await.expect("body1");

    let ts2 = now_ms();
    let sig2 = sign_get(SEED_API_SECRET_2, "5000", &ts2.to_string());
    let mut resp2 = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY_2, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts2.to_string())
        .query("signature", sig2)
        .send(&service)
        .await;
    assert_eq!(resp2.status_code, Some(StatusCode::OK));
    let body2: AccountBody = resp2.take_json().await.expect("body2");

    assert_ne!(
        body1.account_id, body2.account_id,
        "two distinct credentials must surface as distinct accountIds"
    );
    // Each response must reflect its own PN's balance, proving the handler
    // routed to the correct PN address.
    assert_eq!(
        body1.balances[0].free, "10.000000000",
        "SEED_API_KEY's balance must reflect pn1's preloaded value"
    );
    assert_eq!(
        body2.balances[0].free, "20.000000000",
        "SEED_API_KEY_2's balance must reflect pn2's preloaded value"
    );
}

#[tokio::test]
async fn trade_only_key_returns_1002_on_account_route() {
    // A key with TRADE-only permission must be rejected with -1002 on
    // USER_DATA-gated endpoints like /api/v1/account.
    let Some((service, pool, kek, _pn)) = common::setup().await else { return };

    let scope = uuid::Uuid::new_v4().simple().to_string();
    let trade_only_secret_hex = "ccddee0011223344556677889900aabbccddee0011223344556677889900aabb";
    let trade_only_key = format!("dk_test_tradeonly_acct_{scope}");

    common::insert_trade_only_key(&pool, &kek, &trade_only_key, trade_only_secret_hex).await;

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(trade_only_secret_hex, &canonical, b"");

    let mut resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", trade_only_key.as_str(), true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<ErrorBody>().await.expect("error body");

    common::cleanup_trade_only_key(&pool, &trade_only_key).await;

    assert_eq!(status, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(body.code, -1002);
}
