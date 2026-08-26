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
               (orderbook_address, model_hash, model_ref,
                version, platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, reference_price,
                created_at_chain, last_reconciled_at)
           values ($1, null, $2,
                   case when $2 is null then null else '4.0.30' end,
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
    // One flat field carrying the book's name verbatim, and no `model` object.
    assert_eq!(m["modelRefName"], "qwen--qwen2.5-32b--instruct");
    assert!(m["model"].is_null(), "the model object is gone, not merely emptied");
    // `contractVersion` is the deployed CONTRACT's version, from its own column.
    assert_eq!(m["contractVersion"], "4.0.30");
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
    assert_eq!(m["modelRefName"], "9943");
    assert!(m["referencePrice"].is_null());
    assert!(m["contractVersion"].is_null()); // version column unset -> null

    purge(&pool, ob).await;
}

#[tokio::test]
async fn address_with_filter_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&status=TRADING",
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
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?status=BOGUS").send(&service).await;
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

/// Seed a reconciled, listable market with a chosen chain time.
/// `model_ref = ob` (unique, non-null) so `modelRefName` resolves; `model_hash` NULL.
async fn seed_listed(pool: &PgPool, ob: &str, created_at_chain_secs: Option<i64>) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, platform_fee_bps,
                quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, created_at_chain, last_reconciled_at)
           values ($1, null, $1, 250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001',
                   case when $2::bigint is null then null else to_timestamp($2::double precision) end,
                   now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
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
async fn pagination_cursor_round_trip_with_null_chain_time() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let a = "0:inf_http_pg_a"; // newest
    let b = "0:inf_http_pg_b";
    let c = "0:inf_http_pg_c"; // NULL chain time -> sorts last
    purge_many(&pool, &[a, b, c]).await;
    seed_listed(&pool, a, Some(1_700_000_200)).await;
    seed_listed(&pool, b, Some(1_700_000_100)).await;
    seed_listed(&pool, c, None).await;

    // Walked two at a time so every page boundary is actually crossed. The
    // listing has no filter any more and the database is shared, so the
    // assertions are about these three rows within the whole ordered result
    // rather than about the contents of one page.
    let mut all: Vec<String> = Vec::new();
    let mut created_at_of_c = None;
    let mut cursor: Option<String> = None;
    loop {
        let url = match &cursor {
            None => "http://test/api/v1/inference/markets?limit=2".to_string(),
            Some(cur) => {
                format!("http://test/api/v1/inference/markets?limit=2&cursor={cur}")
            }
        };
        let mut resp = TestClient::get(url).send(&service).await;
        assert_eq!(resp.status_code, Some(StatusCode::OK));
        let page: Value = resp.take_json().await.expect("json");
        for m in page["markets"].as_array().unwrap() {
            let addr = m["inferenceOrderBookAddress"].as_str().unwrap().to_string();
            if addr == c {
                created_at_of_c = Some(m["createdAt"].clone());
            }
            all.push(addr);
        }
        if page["hasMore"] != Value::Bool(true) {
            assert!(page["nextCursor"].is_null(), "the last page must carry no cursor");
            break;
        }
        cursor = Some(page["nextCursor"].as_str().expect("nextCursor present").to_string());
    }

    let unique = all.iter().collect::<std::collections::HashSet<_>>().len();
    assert_eq!(unique, all.len(), "the cursor repeated a row across a page boundary");

    let at = |ob: &str| all.iter().position(|x| x == ob).expect("seeded row missing");
    assert!(at(a) < at(b), "newer chain time must sort first");
    assert!(at(b) < at(c), "a NULL chain time must sort last");
    assert_eq!(created_at_of_c, Some(Value::from(0)), "a NULL chain time renders as createdAt 0");

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
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?sort=bogus").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn non_numeric_limit_on_listing_is_1130() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let mut resp =
        TestClient::get("http://test/api/v1/inference/markets?limit=abc").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1130);
}

#[tokio::test]
async fn oversized_limit_clamps_and_returns_200() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // `limit=99999` must clamp rather than be rejected as -1130. The clamp is
    // what the 200 proves: without it the page size would be the raw value.
    let resp =
        TestClient::get("http://test/api/v1/inference/markets?limit=99999").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
}

#[tokio::test]
async fn address_with_non_numeric_limit_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // Single-market presence beats the typed parse → -1102 (not -1130).
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&limit=abc",
    )
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
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&limit=",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}

#[tokio::test]
async fn address_with_blank_status_is_1102() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // A present-but-blank `&status=` still conflicts with single-market lookup:
    // presence beats the blank-collapse, so this is -1102 and not a silent hit.
    let mut resp = TestClient::get(
        "http://test/api/v1/inference/markets?inferenceOrderBookAddress=0:x&status=",
    )
    .send(&service)
    .await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let body: Value = resp.take_json().await.expect("json");
    assert_eq!(body["code"], -1102);
}
