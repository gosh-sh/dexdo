// HTTP-level integration tests for POST /api/v1/accounts.
//
// The endpoint is public: a client registers a deployed PrivateNote and
// gets back a credential it can immediately sign with. These tests run the
// real Postgres `AccountRegistry` (so the insert + insert-only conflict
// are exercised end to end) and use `FakePnStateReader` to stand in for
// the on-chain existence probe. Each test mints a UUID-unique note so the
// shared test DB stays collision-free under parallel runs.

mod common;

use common::canonical_query;
use common::now_ms;
use common::sign;
use dodex_application::PnDetails;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct RegisterBody {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "pnAddress")]
    pn_address: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(rename = "apiSecret")]
    api_secret: String,
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

/// A fresh note with a UUID-unique `pn_address` and `pn_dih` so concurrent
/// tests never contend on the `accounts` unique indexes. The public key is
/// fixed (no unique constraint) and the secret key is a valid 32-byte hex.
fn fresh_note() -> (String, String, serde_json::Value) {
    let scope = uuid::Uuid::new_v4().simple().to_string(); // 32 hex chars
    let pn_address = format!("0:reg-{scope}");
    let pn_dih_hex = scope.clone(); // 128-bit, unique, fits numeric(78,0)
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": "ab".repeat(32),
        "pnSeckeyHex": "00".repeat(32),
        "pnDihHex": pn_dih_hex,
    });
    (pn_address, scope, body)
}

async fn cleanup_account(pool: &PgPool, pn_address: &str) {
    // api_keys cascade on the account delete.
    sqlx::query("delete from accounts where pn_address = $1")
        .bind(pn_address)
        .execute(pool)
        .await
        .expect("cleanup registered account");
}

async fn post_register(service: &Service, body: &serde_json::Value) -> salvo::Response {
    TestClient::post("http://test/api/v1/accounts").json(body).send(service).await
}

#[tokio::test]
async fn register_returns_usable_credentials() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, _scope, body) = fresh_note();
    // Mark the note deployed on-chain (empty balances are enough).
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let reg: RegisterBody = resp.take_json().await.expect("register body");

    assert_eq!(status, Some(StatusCode::OK));
    assert!(!reg.account_id.is_empty());
    assert_eq!(reg.pn_address, pn_address);
    assert!(reg.api_key.starts_with("dk_live_"), "got api_key {}", reg.api_key);
    assert_eq!(reg.api_secret.len(), 64, "secret is 32 bytes of hex");
    assert!(hex::decode(&reg.api_secret).is_ok(), "secret is valid hex");
    let mut perms = reg.permissions.clone();
    perms.sort();
    assert_eq!(perms, vec!["TRADE".to_string(), "USER_DATA".to_string()]);

    // The minted credential must actually authenticate: sign a USER_DATA
    // request with it and confirm the auth hoop accepts it (the handler
    // then reads the same empty PN details we preloaded -> 200).
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&reg.api_secret, &canonical, b"");
    let auth_resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", reg.api_key.as_str(), true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(
        auth_resp.status_code,
        Some(StatusCode::OK),
        "freshly minted credential must authenticate on a USER_DATA route"
    );

    cleanup_account(&pool, &pn_address).await;
}

#[tokio::test]
async fn register_same_note_twice_conflicts() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, _scope, body) = fresh_note();
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });

    let first = post_register(&service, &body).await;
    assert_eq!(first.status_code, Some(StatusCode::OK));

    let mut second = post_register(&service, &body).await;
    let status = second.status_code;
    let err = second.take_json::<ErrorBody>().await.expect("error body");

    cleanup_account(&pool, &pn_address).await;

    assert_eq!(status, Some(StatusCode::CONFLICT));
    assert_eq!(err.code, -2015);
}

#[tokio::test]
async fn register_undeployed_note_returns_2013() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, _scope, body) = fresh_note();
    pn.set_not_deployed(&pn_address);

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code, -2013);
}

#[tokio::test]
async fn register_missing_field_returns_1102() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, scope, _body) = fresh_note();
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });
    // Omit pnSeckeyHex.
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": "ab".repeat(32),
        "pnDihHex": scope,
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn register_unknown_field_returns_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let (_pn_address, scope, mut body) = fresh_note();
    body["surprise"] = json!("unexpected");
    let _ = scope;

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn register_malformed_hex_returns_1130() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, scope, _body) = fresh_note();
    // Note is deployed, so the chain probe passes and the registry's field
    // validation is what rejects the malformed public key.
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": "zz".repeat(32),
        "pnSeckeyHex": "00".repeat(32),
        "pnDihHex": scope,
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1130);
}
