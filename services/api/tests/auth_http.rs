// HTTP-level integration tests for the auth hoop and the
// permission-gated stub `POST /api/v1/order`. Each test sends a real
// request through the production router (constructed by
// `dodex_api::build_router`) against the test DB seeded with
// `seed::seed_accounts`, then asserts on status + spec error body.

mod common;

use common::SEED_API_KEY;
use common::SEED_API_SECRET;
use common::canonical_query;
use common::now_ms;
use common::sign;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde::Deserialize;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderStubResponse {
    account_id: String,
    status: String,
}

// ---- missing envelope fields -> -1003 -----------------------------------

#[tokio::test]
async fn missing_apikey_header_returns_1003() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn missing_signature_returns_1003() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn missing_timestamp_returns_1003() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("signature", "deadbeef")
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

// ---- unrecognized credential -> -1002 -----------------------------------

#[tokio::test]
async fn unknown_apikey_returns_1002() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    // Sign with the legitimate secret — but route under an unknown key.
    // The server can't find a row and returns -1002 before HMAC compute.
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", "dk_live_unknown_999", true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1002);
}

// ---- timing / signature checks ------------------------------------------

#[tokio::test]
async fn stale_timestamp_returns_1021() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    // Two minutes in the past — well outside the 5s default recvWindow.
    let ts = now_ms() - 120_000;
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1021);
}

#[tokio::test]
async fn wrong_signature_returns_1022() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    // Build a valid-shaped but wrong signature: HMAC over a different
    // canonical string. Spec verification must reject it.
    let canonical_wrong = canonical_query(&[("recvWindow", "5000"), ("timestamp", "0")]);
    let bad_sig = sign(SEED_API_SECRET, &canonical_wrong, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &bad_sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1022);
}

#[tokio::test]
async fn recv_window_overshoot_silently_clamps() {
    // Spec says max recvWindow is 60_000; clients sending more should
    // not 400 — the server silently caps. With timestamp = now and a
    // valid signature the request still succeeds.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "999999"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "999999")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::OK));
}

// ---- happy path ---------------------------------------------------------

#[tokio::test]
async fn valid_signature_returns_200_with_account_id() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body = resp.take_json::<OrderStubResponse>().await.expect("stub response");
    assert_eq!(body.status, "STUB");
    // The seeded account_id is a real UUID — we don't pin its value
    // (it's generated by `gen_random_uuid()` in the migration), but
    // we do verify it parses and is non-nil.
    let parsed = uuid::Uuid::parse_str(&body.account_id).expect("accountId is uuid");
    assert!(!parsed.is_nil(), "accountId must be non-nil");
}

// ---- permission gating --------------------------------------------------

#[tokio::test]
async fn user_data_only_key_returns_1002_on_trade_route() {
    // The seeded keys all carry [USER_DATA, TRADE]. To exercise the
    // permission gate without mutating shared seed data, we insert a
    // one-off api_key with USER_DATA only against an arbitrary seeded
    // account and tear it down at the end.
    let Some((service, pool, kek)) = common::setup().await else { return };

    let readonly_secret_hex =
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let readonly_api_key = "dk_live_test_readonly_zz";

    insert_readonly_key(&pool, &kek, readonly_api_key, readonly_secret_hex).await;

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(readonly_secret_hex, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", readonly_api_key, true)
        .query("recvWindow", "5000")
        .query("timestamp", &ts.to_string())
        .query("signature", &sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<ErrorBody>().await.expect("error body");

    // Cleanup before any assertion so a failure does not leak fixture
    // rows into the next run of this same test.
    cleanup_readonly_key(&pool, readonly_api_key).await;

    assert_eq!(status, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(body.code, -1002);
}

// ---- fixture helpers for the permission test ----------------------------

async fn insert_readonly_key(pool: &PgPool, kek: &Kek, api_key: &str, secret_hex: &str) {
    use dodex_infrastructure::crypto;

    // Pick any seeded account; permissions live on api_keys, not on
    // the account, so the choice does not affect the test.
    let account_id: uuid::Uuid = sqlx::query_scalar(
        "select id from accounts where label = 'test-mm-001'",
    )
    .fetch_one(pool)
    .await
    .expect("seeded account exists");

    let secret = hex::decode(secret_hex).unwrap();
    let secret_enc = crypto::seal(kek, &secret).expect("seal readonly secret");

    sqlx::query(
        r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
           values ($1, $2, $3, array['USER_DATA'::auth_permission])"#,
    )
    .bind(account_id)
    .bind(api_key)
    .bind(&secret_enc)
    .execute(pool)
    .await
    .expect("insert readonly api_key");
}

async fn cleanup_readonly_key(pool: &PgPool, api_key: &str) {
    sqlx::query("delete from api_keys where api_key = $1")
        .bind(api_key)
        .execute(pool)
        .await
        .expect("cleanup readonly api_key");
}

use dodex_infrastructure::crypto::Kek;
