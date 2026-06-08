// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// HTTP integration tests for GET /api/v1/oracles. Public endpoint — no auth
// envelope. Skipped when TEST_DATABASE_URL is unset.

mod common;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use serde_json::Value;

// Unix timestamp well in the future (year 2030) so events are not filtered
// out by the deadline > now availability check.
const FUTURE: i64 = 1_900_000_000;

async fn seed(pool: &sqlx::PgPool, oracle_addr: &str, oracle_name: &str) {
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_addr)
        .execute(pool)
        .await
        .unwrap();
    let oracle_id: i64 = sqlx::query_scalar(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, '0xff') returning id"#,
    )
    .bind(oracle_name)
    .bind(oracle_addr)
    .bind(format!("{oracle_name}-deploy"))
    .fetch_one(pool)
    .await
    .unwrap();
    let eventlist_id: i64 = sqlx::query_scalar(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, list_index, description)
           values ($1, $2, $3, 0, 'Election markets.') returning id"#,
    )
    .bind(format!("{oracle_addr}-list-deploy"))
    .bind(oracle_id)
    .bind(format!("{oracle_addr}-list"))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name, oracle_fee, deadline,
                describe, outcome_names_jsonb, meta_reconciled_at, last_seen_at, updated_at)
           values ($1, 1::numeric, 'Election', 100::numeric, $2, 'Will X win?',
                   '{"0":"NO","1":"YES"}'::jsonb, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(FUTURE)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn happy_path_lists_oracle() {
    let Some((service, pool, _kek, _pn)) = common::setup().await else { return };
    let oracle = "0:oracles_http_happy";
    seed(&pool, oracle, "oracles-http-happy").await;

    let mut resp = TestClient::get("http://test/api/v1/oracles")
        .query("oracleAddress", oracle)
        .send(&service)
        .await;

    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body: Value = resp.take_json().await.expect("json");

    assert!(body.get("serverTime").and_then(Value::as_i64).is_some());
    assert_eq!(body["hasMore"], Value::Bool(false));
    // Single page: the top-level shape still carries nextCursor, as null.
    assert!(body.as_object().unwrap().contains_key("nextCursor"));
    assert!(body["nextCursor"].is_null());
    let oracles = body["oracles"].as_array().expect("oracles array");
    assert_eq!(oracles.len(), 1);
    assert_eq!(oracles[0]["name"], "oracles-http-happy");
    let lists = oracles[0]["eventLists"].as_array().unwrap();
    assert_eq!(lists[0]["description"], "Election markets.");
    let events = lists[0]["events"].as_array().unwrap();
    assert_eq!(events[0]["eventName"], "Election");
    // eventId renders as 0x-hex of internal_id_in_eventlist (=1), end-to-end.
    assert_eq!(
        events[0]["eventId"],
        "0x0000000000000000000000000000000000000000000000000000000000000001"
    );
    assert_eq!(events[0]["deadline"], FUTURE);
    assert_eq!(events[0]["oracleFee"]["asset"], "SHELL");
    assert_eq!(events[0]["oracleFee"]["amount"], "100");
    assert_eq!(events[0]["outcomes"].as_array().unwrap().len(), 2);

    sqlx::query("delete from oracles where address = $1")
        .bind(oracle)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_limit_is_400() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let resp = TestClient::get("http://test/api/v1/oracles?limit=abc").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn invalid_deadline_before_is_400() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let resp =
        TestClient::get("http://test/api/v1/oracles?deadlineBefore=soon").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn invalid_event_id_is_400() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let resp = TestClient::get("http://test/api/v1/oracles?eventId=0xZZZ").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn corrupted_cursor_is_400() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    let resp = TestClient::get("http://test/api/v1/oracles?cursor=%21%21%21").send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
}

#[tokio::test]
async fn reachable_without_auth() {
    let Some((service, _pool, _kek, _pn)) = common::setup().await else { return };
    // No X-DODEX-APIKEY / signature headers — must NOT be 401.
    let resp = TestClient::get("http://test/api/v1/oracles").send(&service).await;
    assert_ne!(
        resp.status_code,
        Some(StatusCode::UNAUTHORIZED),
        "GET /api/v1/oracles must not be 401-gated by the auth hoop",
    );
}
