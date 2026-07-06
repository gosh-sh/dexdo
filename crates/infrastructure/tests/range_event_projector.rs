// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Projector coverage for OracleEventList.RangeEventAdded → oracle_events range
// columns (range_ob_address + range_bounds_jsonb). These back the `resolvesFrom`
// block/filter on /api/v1/prediction/markets (read-time join via
// confirmed_pmp_address). Gated on TEST_DATABASE_URL — see reprojection.rs.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors;
use dodex_infrastructure::projectors::ProjectionOutcome;
use serde_json::json;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

/// Seed oracle → eventlist and, when `with_event`, the child `oracle_events` row
/// (the `EventAdded` projection) that RangeEventAdded updates. Returns eventlist_id.
async fn seed(pool: &PgPool, tag: &str, event_id_decimal: &str, with_event: bool) -> i64 {
    let oracle_addr = format!("0:{tag}_oracle");
    let eventlist_addr = format!("0:{tag}_oel");
    // Cascade-delete any residue from a prior run.
    sqlx::query("delete from oracles where address = $1")
        .bind(&oracle_addr)
        .execute(pool)
        .await
        .expect("purge oracle");

    let oracle_id: i64 = sqlx::query_scalar(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, '0xff') returning id"#,
    )
    .bind(format!("{tag}-oracle"))
    .bind(&oracle_addr)
    .bind(format!("{tag}-deploy-msg"))
    .fetch_one(pool)
    .await
    .expect("insert oracle");

    let eventlist_id: i64 = sqlx::query_scalar(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, list_index, description)
           values ($1, $2, $3, 1, '') returning id"#,
    )
    .bind(format!("{eventlist_addr}-msg"))
    .bind(oracle_id)
    .bind(&eventlist_addr)
    .fetch_one(pool)
    .await
    .expect("insert oel");

    if with_event {
        sqlx::query(
            r#"insert into oracle_events
                   (eventlist_id, internal_id_in_eventlist, event_name, deadline)
               values ($1, $2::numeric, 'RangeEvent', 1710000000)"#,
        )
        .bind(eventlist_id)
        .bind(event_id_decimal)
        .execute(pool)
        .await
        .expect("insert oracle_event");
    }

    eventlist_id
}

fn range_event(eventlist_addr: &str, event_id_hex: &str) -> (DecodedEvent, EventNode) {
    let event = DecodedEvent {
        contract_kind: "OracleEventList",
        event_name: "RangeEventAdded".to_string(),
        event_type: "OracleEventList.RangeEventAdded".to_string(),
        value: json!({
            "eventId": event_id_hex,
            "ob": "0:6330b82c9d866f68e989d4f71c79e6f4757602c065933b7e63179b00acd9aa0e",
            // 0x64=100, 0xc8=200, 0x12c=300 — stored as decimal strings.
            "bounds": [
                "0x0000000000000000000000000000000000000000000000000000000000000064",
                "0x00000000000000000000000000000000000000000000000000000000000000c8",
                "0x000000000000000000000000000000000000000000000000000000000000012c",
            ],
        }),
    };
    let node = EventNode {
        msg_id: format!("range_{eventlist_addr}"),
        msg_chain_order: None,
        src: Some(eventlist_addr.to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: None,
    };
    (event, node)
}

#[tokio::test]
async fn range_event_added_records_ob_and_bounds_on_oracle_event() {
    let Some(pool) = setup().await else { return };
    let tag = "range_records_ob";
    // event_id 42 == hex 0x2a.
    let eventlist_id = seed(&pool, tag, "42", true).await;
    let (event, node) = range_event(&format!("0:{tag}_oel"), "0x2a");

    let mut tx = pool.begin().await.expect("begin");
    let outcome = projectors::project_event(&mut tx, &event, &node).await.expect("project");
    let (ob, bounds): (Option<String>, Option<Value>) = sqlx::query_as(
        r#"select range_ob_address, range_bounds_jsonb from oracle_events
            where eventlist_id = $1 and internal_id_in_eventlist = 42::numeric"#,
    )
    .bind(eventlist_id)
    .fetch_one(&mut *tx)
    .await
    .expect("read oracle_event range cols");
    drop(tx); // rollback

    assert_eq!(outcome, ProjectionOutcome::Applied, "RangeEventAdded on a present event applies");
    assert_eq!(
        ob.as_deref(),
        Some("0:6330b82c9d866f68e989d4f71c79e6f4757602c065933b7e63179b00acd9aa0e"),
        "range_ob_address must be the InferenceOrderBook from the event"
    );
    assert_eq!(
        bounds,
        Some(json!(["100", "200", "300"])),
        "range_bounds_jsonb must be the bounds as decimal strings"
    );
}

#[tokio::test]
async fn range_event_added_defers_when_event_absent() {
    let Some(pool) = setup().await else { return };
    let tag = "range_defer";
    // Eventlist present, but the EventAdded child row is NOT yet projected.
    seed(&pool, tag, "99", false).await;
    let (event, node) = range_event(&format!("0:{tag}_oel"), "0x63"); // 0x63 == 99

    let mut tx = pool.begin().await.expect("begin");
    let outcome = projectors::project_event(&mut tx, &event, &node).await.expect("project");
    drop(tx);

    assert_eq!(
        outcome,
        ProjectionOutcome::Deferred,
        "RangeEventAdded before its EventAdded row must defer, not drop the linkage"
    );
}
