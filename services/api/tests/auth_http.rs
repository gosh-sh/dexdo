// HTTP-level integration tests for the auth hoop and the permission
// gate on `POST /api/v1/order`. Each test sends a real request through
// the production router (constructed by `dodex_api::build_router`)
// against the test DB seeded with `seed::seed_accounts`, then asserts
// on status + spec error body.
//
// The seeded test DB has no `markets` rows, so a fully-authorised
// request still 404s at `InvalidMarketOrSymbol` (-1121) before
// reaching the chain sender — that is the canonical "auth passed,
// handler ran" signal these tests use.

mod common;

use common::canonical_query;
use common::now_ms;
use common::sign;
use common::SEED_API_KEY;
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
    // Deserialised so serde does not error on the field but unused —
    // the tests assert only on `code`. Hard-pinning the text would
    // couple tests to the human-readable copy in `DomainError::msg`.
    #[allow(dead_code)]
    msg: String,
}

/// Minimal valid request body — well-formed JSON with every required
/// field present so request parsing succeeds and the handler attempts
/// market resolution. The `marketAddress` is intentionally fictitious;
/// no row matches it in the test DB, so the use case 404s with -1121.
const HANDLER_REACHABLE_BODY: &str = concat!(
    r#"{"marketAddress":"0:no-such-market","#,
    r#""symbol":"NO-SUCH-SYMBOL","#,
    r#""side":"BUY","#,
    r#""quantity":"1","#,
    r#""price":"1","#,
    r#""type":"LIMIT","#,
    r#""timeInForce":"GTC"}"#,
);

// ---- missing envelope fields -> -1003 -----------------------------------

#[tokio::test]
async fn missing_apikey_header_returns_1003() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
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
async fn missing_signature_returns_1003() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn missing_timestamp_returns_1003() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };

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
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    // Sign with the legitimate secret — but route under an unknown key.
    // The server can't find a row and returns -1002 before HMAC compute.
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", "dk_live_unknown_999", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1002);
}

// ---- timing / signature checks ------------------------------------------

#[tokio::test]
async fn stale_timestamp_returns_1021() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    // Two minutes in the past — well outside the 5s default recvWindow.
    let ts = now_ms() - 120_000;
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1021);
}

#[tokio::test]
async fn wrong_signature_returns_1022() {
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    // Build a valid-shaped but wrong signature: HMAC over a different
    // canonical string. Spec verification must reject it.
    let canonical_wrong = canonical_query(&[("recvWindow", "5000"), ("timestamp", "0")]);
    let bad_sig = sign(SEED_API_SECRET, &canonical_wrong, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", bad_sig)
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
    // valid signature the request must reach the handler, which then
    // 404s on the fictitious market (the canonical "auth passed"
    // signal in this suite).
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "999999"), ("timestamp", &ts.to_string())]);
    let body = HANDLER_REACHABLE_BODY.as_bytes();
    let sig = sign(SEED_API_SECRET, &canonical, body);

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "999999")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body.to_vec())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
}

#[tokio::test]
async fn malformed_recv_window_returns_1003() {
    // A present-but-unparseable `recvWindow` must be rejected rather
    // than silently falling back to the server-side default; silent
    // fallback would mask client SDK bugs and surface later as
    // confusing -1021 errors. The signature is correctly built over
    // the same canonical string the client sent, so the only thing
    // that can reject the request is the envelope parse.
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "abc"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .query("recvWindow", "abc")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1003);
}

#[tokio::test]
async fn body_exceeding_cap_returns_1009() {
    // The hoop caps body reads at 64 KB before HMAC compute so an
    // attacker can't tie up the pool with arbitrary uploads. A signed
    // request whose body breaches the cap must be rejected with
    // -1009 / HTTP 413 instead of a generic 500.
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let body = vec![b'x'; 128 * 1024];
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(SEED_API_SECRET, &canonical, &body);

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .add_header("content-type", "application/octet-stream", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::PAYLOAD_TOO_LARGE));
    let body = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(body.code, -1009);
}

// ---- happy path ---------------------------------------------------------

#[tokio::test]
async fn valid_signature_reaches_handler() {
    // A fully-authorised request with a well-formed body must pass
    // every auth check and arrive at the handler. With no markets
    // seeded, the handler then returns `InvalidMarketOrSymbol` -1121.
    // Receiving that error code (not a 401 family code) proves the
    // entire auth pipeline accepted the request.
    let Some((service, _pool, _kek, _pn_reader)) = common::setup().await else { return };
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let body = HANDLER_REACHABLE_BODY.as_bytes();
    let sig = sign(SEED_API_SECRET, &canonical, body);

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", SEED_API_KEY, true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body.to_vec())
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
}

// ---- permission gating --------------------------------------------------

#[tokio::test]
async fn user_data_only_key_returns_1002_on_trade_route() {
    // The seeded keys all carry [USER_DATA, TRADE]. To exercise the
    // permission gate without mutating shared seed data, we insert a
    // one-off api_key with USER_DATA only against an arbitrary seeded
    // account and tear it down at the end.
    let Some((service, pool, kek, _pn_reader)) = common::setup().await else { return };

    // Uuid-suffix the readonly key so parallel runs (and any future
    // re-entrancy of this test) never share the row.
    let scope = uuid::Uuid::new_v4().simple().to_string();
    let readonly_secret_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let readonly_api_key = format!("dk_test_readonly_{scope}");

    insert_readonly_key(&pool, &kek, &readonly_api_key, readonly_secret_hex).await;

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(readonly_secret_hex, &canonical, b"");

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", readonly_api_key.as_str(), true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;

    let status = resp.status_code;
    let body = resp.take_json::<ErrorBody>().await.expect("error body");

    // Cleanup before any assertion so a failure does not leak fixture
    // rows into the next run of this same test.
    cleanup_readonly_key(&pool, &readonly_api_key).await;

    assert_eq!(status, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(body.code, -1002);
}

// ---- fixture helpers for the permission test ----------------------------

async fn insert_readonly_key(pool: &PgPool, kek: &Kek, api_key: &str, secret_hex: &str) {
    use dodex_infrastructure::crypto;

    // Pick any seeded account; permissions live on api_keys, not on
    // the account, so the choice does not affect the test.
    let account_id: uuid::Uuid =
        sqlx::query_scalar("select id from accounts where label = 'test-mm-001'")
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
    // Cleanup runs ahead of the assertion so a failed assertion does not
    // leak fixtures. Best-effort: a panic here would mask the real
    // assertion message, so swallow and warn instead.
    if let Err(err) =
        sqlx::query("delete from api_keys where api_key = $1").bind(api_key).execute(pool).await
    {
        eprintln!("cleanup readonly api_key failed: {err}");
    }
}

// Pins the seeded-credential consts to the KEK derivation so a scheme
// change (or a hand-edited const) cannot silently desync them from what
// `seed_accounts_from_notes` writes. No DB required.
#[test]
fn seed_secret_consts_match_kek_derivation() {
    use dodex_infrastructure::crypto::derive_api_secret;
    let kek = common::test_kek();
    assert_eq!(SEED_API_SECRET, hex::encode(derive_api_secret(&kek, 0)));
    assert_eq!(common::SEED_API_SECRET_2, hex::encode(derive_api_secret(&kek, 1)));
}
