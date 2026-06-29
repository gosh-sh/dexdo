// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/inference/markets through the
// production router. Gated on TEST_DATABASE_URL via common::setup().

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde_json::Value;
use sqlx::PgPool;

async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed(pool: &PgPool, ob: &str, model_ref: Option<&str>, reference_price: Option<&str>) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, producer, model_name, version,
                platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, reference_price,
                created_at_chain, last_reconciled_at)
           values ($1, null, $2,
                   case when $2 is null then null else 'qwen' end,
                   case when $2 is null then null else 'qwen2.5-32b' end,
                   case when $2 is null then null else 'instruct' end,
                   250, 2, 9, 0, '0.000000001', '1', '0.000000001', $3::numeric,
                   to_timestamp(1700000000), now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .bind(model_ref)
    .bind(reference_price)
    .execute(pool)
    .await
    .expect("seed inference market");
}

#[tokio::test]
async fn happy_path_lists_inference_market() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_http_happy";
    purge(&pool, ob).await;
    seed(&pool, ob, Some("qwen--qwen2.5-32b--instruct"), Some("1010")).await;

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("json");
    let m = &body["markets"][0];
    assert_eq!(m["inferenceOrderBookAddress"], ob);
    assert_eq!(m["model"]["ref"], "qwen--qwen2.5-32b--instruct");
    assert_eq!(m["model"]["producer"], "qwen");
    assert_eq!(m["status"], "TRADING");
    assert_eq!(m["quoteAsset"], "SHELL");
    assert_eq!(m["takerCommission"], "0.025");
    assert_eq!(m["makerCommission"], "-0.02");
    assert_eq!(m["referencePrice"], "0.000001010");
    assert_eq!(m["createdAt"], 1700000000);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn ref_falls_back_to_model_hash() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let ob = "0:inf_http_reffallback";
    purge(&pool, ob).await;
    seed(&pool, ob, None, None).await;
    // model_ref NULL; give it a distinct model_hash so `ref` falls back to it.
    sqlx::query("update inference_markets set model_hash = 9943 where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("json");
    let m = &body["markets"][0];
    assert_eq!(m["model"]["ref"], "9943");
    assert!(m["model"]["producer"].is_null());
    assert!(m["referencePrice"].is_null());

    purge(&pool, ob).await;
}

#[tokio::test]
async fn address_with_filter_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&producer=qwen",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn invalid_status_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/inference/markets?status=BOGUS")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn unknown_address_is_1121() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:nope_inf",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1121);
}

/// Seed a reconciled, listable market with a chosen `producer` and chain time.
/// `model_ref = ob` (unique, non-null) so `ref` resolves; `model_hash` NULL.
async fn seed_listed(pool: &PgPool, ob: &str, producer: &str, created_at_chain_secs: Option<i64>) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, producer, platform_fee_bps,
                quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, created_at_chain, last_reconciled_at)
           values ($1, null, $1, $2, 250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001',
                   case when $3::bigint is null then null else to_timestamp($3::double precision) end,
                   now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .bind(producer)
    .bind(created_at_chain_secs)
    .execute(pool)
    .await
    .expect("seed listed market");
}

async fn purge_many(pool: &PgPool, obs: &[&str]) {
    let v: Vec<String> = obs.iter().map(|s| s.to_string()).collect();
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&v)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn producer_filter_narrows_results() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let prod = "inf_http_prodfilter";
    let a = "0:inf_http_pf_a";
    let b = "0:inf_http_pf_b";
    purge_many(&pool, &[a, b]).await;
    seed_listed(&pool, a, prod, Some(1_700_000_100)).await;
    seed_listed(&pool, b, "other_producer", Some(1_700_000_200)).await;

    let mut resp =
        TestClient::get(format!("http://test/api/v1/inference/markets?producer={prod}"))
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("json");
    let addrs: Vec<&str> = body["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["inferenceOrderBookAddress"].as_str().unwrap())
        .collect();
    assert!(addrs.contains(&a));
    assert!(!addrs.contains(&b), "producer filter must exclude other producers");

    purge_many(&pool, &[a, b]).await;
}

#[tokio::test]
async fn pagination_cursor_round_trip_with_null_chain_time() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let prod = "inf_http_pgwalk"; // unique producer isolates the seeded rows
    let a = "0:inf_http_pg_a"; // newest
    let b = "0:inf_http_pg_b";
    let c = "0:inf_http_pg_c"; // NULL chain time -> sorts last
    purge_many(&pool, &[a, b, c]).await;
    seed_listed(&pool, a, prod, Some(1_700_000_200)).await;
    seed_listed(&pool, b, prod, Some(1_700_000_100)).await;
    seed_listed(&pool, c, prod, None).await;

    // Page 1: newest-first, NULL-time row absent, hasMore + nextCursor present.
    let mut r1 =
        TestClient::get(format!("http://test/api/v1/inference/markets?producer={prod}&limit=2"))
            .send(&service)
            .await;
    assert_eq!(r1.status_code, Some(StatusCode::OK));
    let p1: Value = r1.take_json().await.expect("json");
    let a1: Vec<&str> = p1["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["inferenceOrderBookAddress"].as_str().unwrap())
        .collect();
    assert_eq!(a1, vec![a, b]);
    assert_eq!(p1["hasMore"], Value::Bool(true));
    let cursor = p1["nextCursor"].as_str().expect("nextCursor present").to_string();

    // Page 2 via the cursor: NULL-time row appears last with createdAt 0.
    let mut r2 = TestClient::get(format!(
        "http://test/api/v1/inference/markets?producer={prod}&limit=2&cursor={cursor}"
    ))
    .send(&service)
    .await;
    assert_eq!(r2.status_code, Some(StatusCode::OK));
    let p2: Value = r2.take_json().await.expect("json");
    let a2: Vec<&str> = p2["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["inferenceOrderBookAddress"].as_str().unwrap())
        .collect();
    assert_eq!(a2, vec![c]);
    assert_eq!(p2["markets"][0]["createdAt"], 0);
    assert_eq!(p2["hasMore"], Value::Bool(false));
    assert!(p2["nextCursor"].is_null());

    purge_many(&pool, &[a, b, c]).await;
}

#[tokio::test]
async fn corrupt_cursor_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // base64url of "nocolon" — decodes but has no `<key>:<id>` separator -> -1130.
    let mut resp = TestClient::get("http://test/api/v1/inference/markets?cursor=bm9jb2xvbg")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn corrupt_precision_returns_503_1500() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    // One representative -1500 case at the HTTP boundary: proves
    // MarketInconsistent -> 503/-1500 mapping for the inference route. The full
    // fail-closed matrix is covered at the repo layer (Task 3).
    let ob = "0:inf_http_badprec";
    purge(&pool, ob).await;
    seed(&pool, ob, Some("r"), None).await;
    sqlx::query("update inference_markets set price_precision = -1 where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let mut resp = TestClient::get(format!(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress={ob}"
    ))
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1500);

    purge(&pool, ob).await;
}

// Additional cases from the "additional cases" paragraph.

#[tokio::test]
async fn bogus_sort_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/inference/markets?sort=bogus")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn non_numeric_limit_on_listing_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get("http://test/api/v1/inference/markets?limit=abc")
        .send(&service)
        .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn oversized_limit_clamps_and_returns_200() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Use a producer that will match nothing so the listing is empty — proves
    // that `limit=99999` is clamped (not rejected as -1130) rather than
    // returning all DB rows (which may include corrupt task-3 fixtures).
    let resp =
        TestClient::get("http://test/api/v1/inference/markets?limit=99999&producer=__no_such_producer__")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
}

#[tokio::test]
async fn address_with_non_numeric_limit_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Single-market presence beats the typed parse → -1102 (not -1130).
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&limit=abc")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn address_with_blank_limit_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // A present-but-blank `&limit=` still conflicts with single-market lookup.
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&limit=")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn address_with_blank_producer_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // A present-but-blank `&producer=` still conflicts with single-market lookup.
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&producer=")
            .send(&service)
            .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}
