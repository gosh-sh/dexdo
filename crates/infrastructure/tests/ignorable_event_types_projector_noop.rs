// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test asserting that every type in IGNORABLE_EVENT_TYPES is a
// genuine projector no-op (projects to `Applied` and writes nothing). Gated on
// TEST_DATABASE_URL; prints a skip notice and returns when unset.

use std::env;
use std::time::Duration;

use dodex_infrastructure::config::IGNORABLE_EVENT_TYPES;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors;
use dodex_infrastructure::projectors::ProjectionOutcome;
use serde_json::json;
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

#[tokio::test]
async fn every_ignorable_event_type_is_a_projector_no_op() {
    let Some(pool) = setup().await else { return };

    // The startup guard lets `ignored_event_types` shed exactly
    // `IGNORABLE_EVENT_TYPES`, and the ingest filter drops a matching edge
    // before `project_event` runs and before the `raw_events` insert. That is
    // only safe while each of these types is a genuine projector no-op:
    // `Applied` with no read-model write. The disjointness/coverage unit tests
    // pin `IGNORABLE_EVENT_TYPES` against the metric-critical set but not
    // against the projector arm. This is the test that fails if one of these
    // types ever gains a real handler without being removed from the
    // allow-list — otherwise the guard would keep permitting the drop and a
    // now-state-changing event would silently vanish before the read model.
    for event_type in IGNORABLE_EVENT_TYPES {
        let (kind, name) = event_type.split_once('.').expect("event_type is kind.name");
        let event = DecodedEvent {
            contract_kind: kind,
            event_name: name.to_string(),
            event_type: event_type.to_string(),
            // The no-op arm matches on `event_type` alone and never reads
            // `value`; an empty object suffices. A future real handler would
            // parse fields here and either error or write — both caught below.
            value: json!({}),
        };
        let node = EventNode {
            msg_id: format!("ignorable_no_op_{name}"),
            msg_chain_order: None,
            src: None,
            src_dapp_id: None,
            dst: None,
            body: None,
            created_at: None,
        };

        // Run in a transaction we never commit: even if a future handler
        // writes, the row rolls back, so the test never pollutes the DB. A
        // write of any kind assigns the transaction an id, so
        // `txid_current_if_assigned()` is NULL iff `project_event` touched no
        // table — a concurrency-proof no-op signal that needs no row counting.
        let mut tx = pool.begin().await.expect("begin");
        let outcome = projectors::project_event(&mut tx, &event, &node)
            .await
            .unwrap_or_else(|e| panic!("{event_type} must project as a no-op, got error: {e}"));
        let write_xid: Option<i64> = sqlx::query_scalar("select txid_current_if_assigned()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        drop(tx); // rollback

        assert_eq!(
            outcome,
            ProjectionOutcome::Applied,
            "{event_type} is in IGNORABLE_EVENT_TYPES but no longer projects to Applied; \
             remove it from the allow-list or the ingest filter will silently drop it"
        );
        assert!(
            write_xid.is_none(),
            "{event_type} is in IGNORABLE_EVENT_TYPES but its projector wrote to the database; \
             it is no longer a no-op and must be removed from the allow-list, else the ingest \
             filter will silently drop a state-changing event before the read model"
        );
    }
}
