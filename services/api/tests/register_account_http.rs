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
/// tests never contend on the `accounts` unique indexes. The secret is the
/// canonical test owner key (`"00"*32`) the fake reader reports as the note's
/// on-chain owner, and the public key is the one that secret derives — the
/// use case rejects a mismatched pair (-1130).
fn fresh_note() -> (String, String, serde_json::Value) {
    let scope = uuid::Uuid::new_v4().simple().to_string(); // 32 hex chars
    let pn_address = format!("0:reg-{scope}");
    let pn_dih_hex = scope.clone(); // 128-bit, unique, fits numeric(78,0)
    let pn_pubkey_hex = dodex_application::derive_ed25519_pubkey_hex(&"00".repeat(32)).unwrap();
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": pn_pubkey_hex,
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
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, _scope, body) = fresh_note();
    pn.set_not_deployed(&pn_address);

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::NOT_FOUND));
    assert_eq!(err.code, -2013);

    // Probe-before-write: an undeployed note must leave no account row.
    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&pn_address)
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    assert_eq!(count, 0, "undeployed note must not write an account row");
}

#[tokio::test]
async fn register_missing_field_returns_1102() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
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

    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&pn_address)
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    assert_eq!(count, 0, "a missing field must not write an account row");
}

#[tokio::test]
async fn register_blank_field_returns_1102() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let (_pn_address, scope, _body) = fresh_note();
    // Whitespace-only pnAddress: present in JSON, blank after trim. The
    // handler's non_empty() guard must reject it (distinct from an omitted
    // field) before the chain probe ever runs.
    let body = json!({
        "pnAddress": "   ",
        "pnPubkeyHex": "ab".repeat(32),
        "pnSeckeyHex": "00".repeat(32),
        "pnDihHex": scope,
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1102);

    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind("   ")
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    assert_eq!(count, 0, "a blank field must not write an account row");
}

#[tokio::test]
async fn register_unknown_field_returns_1130() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let (pn_address, scope, mut body) = fresh_note();
    body["surprise"] = json!("unexpected");
    let _ = scope;

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1130);

    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&pn_address)
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    assert_eq!(count, 0, "an unknown field must not write an account row");
}

#[tokio::test]
async fn register_malformed_hex_returns_1130() {
    let Some((service, _pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, scope, _body) = fresh_note();
    // Note is deployed, so the chain probe passes; the malformed public key
    // is not the key the seckey derives, so the use case's key-pair check
    // rejects it (-1130) before any write.
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

#[tokio::test]
async fn register_wrong_key_returns_2016() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, scope, _body) = fresh_note();
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });
    // Deployed note, but a well-formed seckey that is NOT the note's owner
    // ("11"*32 vs the fixture owner "00"*32). Its public key matches that
    // seckey, so the key-pair check passes; the on-chain binding then rejects
    // it (-2016) before any row is written.
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": dodex_application::derive_ed25519_pubkey_hex(&"11".repeat(32)).unwrap(),
        "pnSeckeyHex": "11".repeat(32),
        "pnDihHex": scope,
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -2016);

    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&pn_address)
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    assert_eq!(count, 0, "wrong-key registration must not write an account row");
}

#[tokio::test]
async fn register_pubkey_seckey_mismatch_returns_1130() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (pn_address, scope, _body) = fresh_note();
    pn.set_details_for(&pn_address, PnDetails { balance: vec![], locked_in_orders: vec![] });
    // The seckey IS the note's owner ("00"*32 — the binding would pass), but
    // the public key is a different well-formed key. The pair could never
    // sign, so the credential would be dead on arrival: the backend rejects
    // it (-1130) and writes no row.
    let body = json!({
        "pnAddress": pn_address,
        "pnPubkeyHex": dodex_application::derive_ed25519_pubkey_hex(&"11".repeat(32)).unwrap(),
        "pnSeckeyHex": "00".repeat(32),
        "pnDihHex": scope,
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");

    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&pn_address)
        .fetch_one(&pool)
        .await
        .expect("count accounts");
    cleanup_account(&pool, &pn_address).await;

    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
    assert_eq!(err.code, -1130);
    assert_eq!(count, 0, "an inconsistent key pair must not write an account row");
}

#[tokio::test]
async fn register_reused_dih_fresh_address_conflicts() {
    let Some((service, pool, _kek, pn)) = common::setup().await else { return };
    let (addr_a, dih, body_a) = fresh_note();
    let (addr_b, _scope_b, mut body_b) = fresh_note();
    // Note B is a wholly different note (fresh `pn_address`) carrying A's
    // deposit-identifier hash — the anti-squat case the `pn_dih` unique index
    // guards, distinct from the both-indexes collision of the twice-same-note
    // test. The second registration must conflict (-2015), not mint a row.
    body_b["pnDihHex"] = serde_json::Value::String(dih);
    pn.set_details_for(&addr_a, PnDetails { balance: vec![], locked_in_orders: vec![] });
    pn.set_details_for(&addr_b, PnDetails { balance: vec![], locked_in_orders: vec![] });

    let first = post_register(&service, &body_a).await;
    assert_eq!(first.status_code, Some(StatusCode::OK));

    let mut second = post_register(&service, &body_b).await;
    let status = second.status_code;
    let err = second.take_json::<ErrorBody>().await.expect("error body");

    let count_b: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&addr_b)
        .fetch_one(&pool)
        .await
        .expect("count accounts b");
    cleanup_account(&pool, &addr_a).await;
    cleanup_account(&pool, &addr_b).await;

    assert_eq!(status, Some(StatusCode::CONFLICT));
    assert_eq!(err.code, -2015);
    assert_eq!(count_b, 0, "a fresh address reusing a taken pn_dih writes no row");
}
