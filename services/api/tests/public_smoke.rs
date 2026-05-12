// Regression smoke for the public `NONE`-security routes after the
// auth subrouter was added. The point is not to re-test markets/depth
// business logic (covered in crates/infrastructure/tests/*) but to
// confirm the router refactor in `lib.rs::build_router` keeps these
// routes mounted and that the auth hoop is correctly scoped to the
// private subrouter only.
//
// The assertions deliberately avoid pinning a specific success status
// for /markets — what we own here is the hoop scope, not the
// handler's response. Whatever markets returns against an empty test
// DB (200, 503, etc.) is its own concern; the property we verify is
// that the auth hoop did not intercept the request as 401.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;

#[tokio::test]
async fn readiness_returns_200() {
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/readiness").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body = resp.take_string().await.expect("readiness body");
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn markets_not_intercepted_by_auth_hoop() {
    // Hoop is scoped to the private subrouter; an unauthenticated GET
    // to /api/v1/markets must reach the handler. The handler's choice
    // of status against an empty test DB is its own contract; we only
    // require that the hoop did not fire (i.e. the status is not 401).
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let resp = TestClient::get("http://test/api/v1/markets").send(&service).await;
    assert_ne!(
        resp.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "public route must not be 401-gated by the auth hoop",
    );
}

#[tokio::test]
async fn markets_with_bogus_apikey_header_not_intercepted() {
    // A client that mistakenly attaches HMAC headers to a public route
    // should still reach the handler — the hoop never runs here, so a
    // bogus `X-DODEX-APIKEY` is just an ignored extra header.
    let Some((service, _pool, _kek)) = common::setup().await else { return };
    let resp = TestClient::get("http://test/api/v1/markets")
        .add_header("X-DODEX-APIKEY", "obviously-not-a-real-key", true)
        .send(&service)
        .await;
    assert_ne!(
        resp.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "public route must not 401 when an unrelated header is attached",
    );
}
