// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Events the contracts emit that the read model deliberately does not persist.
// Each needs a projector arm all the same: without one `project_event` returns
// `Unknown`, the row is marked processed on first sight and never retried, and
// the only trace is a one-off warning. Gated on TEST_DATABASE_URL.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors;
use dodex_infrastructure::projectors::ProjectionOutcome;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Carry no state the read model serves, so their arms write nothing.
///
/// The note-side inference mirrors follow the precedent set by
/// `InferenceOrderPlacedConfirmed` / `InferenceFilledConfirmed`: the book is the
/// authority on an order, the note's copy is for its owner. The stake-forfeit
/// family is read straight out of `raw_events` by dodex-rewards, which is why
/// none of these are in `IGNORABLE_EVENT_TYPES` — that list is permission to
/// drop an event at ingest, before `raw_events` is written, and dropping these
/// would cut rewards off from the payload it settles on.
const UNPERSISTED_DEX_EVENTS: [&str; 9] = [
    "PMP.StakeForfeited",
    "PrivateNote.StakeForfeitConfirmed",
    "PrivateNote.StakeDroppedLocally",
    "PrivateNote.DealCredited",
    "PrivateNote.BookCredited",
    "PrivateNote.InferenceOrderRemoved",
    "PrivateNote.InferenceOrderRejectedMirror",
    "PrivateNote.InferenceDealClosed",
    "RootPN.DealWriteOffReported",
];

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

fn event(event_type: &'static str) -> DecodedEvent {
    let (kind, name) = event_type.split_once('.').expect("event_type is kind.name");
    DecodedEvent {
        contract_kind: kind,
        event_name: name.to_string(),
        event_type: event_type.to_string(),
        // The no-op arms match on `event_type` and never read `value`; a handler
        // that starts parsing fields would either error or write, and both show up.
        value: json!({}),
    }
}

fn node(msg_id: &str, src: Option<&str>) -> EventNode {
    EventNode {
        msg_id: msg_id.to_string(),
        msg_chain_order: None,
        src: src.map(str::to_string),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: None,
    }
}

#[tokio::test]
async fn unpersisted_dex_events_project_as_no_ops() {
    let Some(pool) = setup().await else { return };

    for event_type in UNPERSISTED_DEX_EVENTS {
        let e = event(event_type);
        let n = node(&format!("no_op_{event_type}"), None);

        // Rolled back either way, so a future handler that writes cannot pollute
        // the DB. `txid_current_if_assigned()` is NULL iff nothing was written.
        let mut tx = pool.begin().await.expect("begin");
        let outcome = projectors::project_event(&mut tx, &e, &n)
            .await
            .unwrap_or_else(|err| panic!("{event_type} must project, got error: {err}"));
        let write_xid: Option<i64> = sqlx::query_scalar("select txid_current_if_assigned()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        drop(tx);

        assert_eq!(
            outcome,
            ProjectionOutcome::Applied,
            "{event_type} has no projector arm: it would be marked processed on first sight and \
             never retried, so adding a handler later needs an explicit backfill"
        );
        assert!(
            write_xid.is_none(),
            "{event_type} wrote to the read model; it is no longer an observability-only event, \
             so it needs a real handler and a row in the schema docs"
        );
    }
}

// Routed into the inference projector, which seeds a market skeleton for ANY
// event on an unknown book before dispatching. That write is the point, so this
// one only pins that the event resolves to a handler rather than to `Unknown`.
#[tokio::test]
async fn inference_order_rejected_resolves_to_a_handler() {
    let Some(pool) = setup().await else { return };
    let event_type = "InferenceOrderBook.InferenceOrderRejected";

    let mut tx = pool.begin().await.expect("begin");
    let outcome = projectors::project_event(
        &mut tx,
        &event(event_type),
        &node("no_op_inference_rejected", Some("0:t_rejected_ob")),
    )
    .await
    .unwrap_or_else(|err| panic!("{event_type} must project, got error: {err}"));
    drop(tx);

    assert_eq!(
        outcome,
        ProjectionOutcome::Applied,
        "{event_type} fell through to Unknown. It carries no orderId — nothing rested, so there \
         is no row to key on — but it still needs an arm to avoid being dropped silently"
    );
}
