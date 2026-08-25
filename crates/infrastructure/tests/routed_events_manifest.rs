// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Direction 4 of the shape guard: "ABI event -> arm".
//!
//! The list of events is DERIVED from the ABI; only the SCOPE is written by hand —
//! which contracts the indexer must serve. This is the reverse of what
//! `UNPERSISTED_DEX_EVENTS` did: that one listed events WE decided not to project,
//! and by construction could know nothing about an event unknown to us.

use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

use dodex_infrastructure::config::IGNORABLE_EVENT_TYPES;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::projectors::project_event;
use dodex_infrastructure::projectors::ProjectionOutcome;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Contracts whose ABIs the indexer serves IN FULL.
const FULLY_IN_SCOPE: &[(&str, &str)] = &[
    ("InferenceOrderBook", "contracts/airegistry/InferenceOrderBook.abi.json"),
    ("TokenContract", "contracts/airegistry/TokenContract.abi.json"),
    // RootModel was loaded by wave 0 to resolve the `ContractDeployed` collision by
    // `dst`. Its events need no projection, but both must still have an ARM:
    // without one the event goes to `Unknown`, is marked processed and lost forever.
    ("RootModel", "contracts/airegistry/RootModel.abi.json"),
];

/// A pinpoint scope inside foreign ABIs: `PrivateNote` and `OracleEventList` also
/// host the prediction path, hence names here rather than a whole file.
const PARTIALLY_IN_SCOPE: &[&str] = &[
    "PrivateNote.InferenceOrderPlacedConfirmed",
    "PrivateNote.InferenceFilledConfirmed",
    "PrivateNote.InferenceOrderRemoved",
    "PrivateNote.InferenceOrderRejectedMirror",
    "PrivateNote.InferenceDealClosed",
    "PrivateNote.DealCredited",
    "PrivateNote.BookCredited",
    // The range link: the matrix keeps it in scope (§1), and without an explicit
    // entry it would drop out exactly the way the two `*Confirmed` events did.
    "OracleEventList.RangeEventAdded",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn abi_event_names(rel_path: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(repo_root().join(rel_path))
        .unwrap_or_else(|e| panic!("read {rel_path}: {e}"));
    let abi: serde_json::Value = serde_json::from_str(&raw).expect("abi json");
    let events = abi["events"].as_array().expect("abi.events array");
    assert!(
        !events.is_empty(),
        "{rel_path}: events is empty — the ABI key changed and the guard went blind"
    );
    events.iter().map(|e| e["name"].as_str().expect("event name").to_string()).collect()
}

fn in_scope_event_types() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (kind, path) in FULLY_IN_SCOPE {
        for name in abi_event_names(path) {
            out.insert(format!("{kind}.{name}"));
        }
    }
    out.extend(PARTIALLY_IN_SCOPE.iter().map(|s| s.to_string()));
    out
}

/// The probe node. Three fields are mandatory, each for its own reason:
/// `src` — both sub-routers start by seeding a skeleton and without it fail BEFORE
/// the `match`; `msg_chain_order` — `node_chain_order` returns `Err` without it;
/// `created_at` — supplying a time is simpler than working out which projectors
/// need one.
fn probe_node() -> EventNode {
    EventNode {
        msg_id: "manifest_probe".to_string(),
        msg_chain_order: Some("00probe-1".to_string()),
        src: Some("0:manifest_probe".to_string()),
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: Some(serde_json::json!(1_700_000_000)),
    }
}

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

#[test]
fn the_in_scope_set_is_thirty_four_events() {
    let n = in_scope_event_types().len();
    assert_eq!(
        n, 34,
        "the scope changed ({n} instead of 34). This is NOT a reason to fix the number: \
         first decide whether the new event needs a projector, and only then change \
         both the number and the arm"
    );
}

#[test]
fn no_in_scope_event_may_be_dropped_at_ingest() {
    // A GUARD, green from its first run. `ignored_event_types` drops an edge BEFORE
    // the `raw_events` write, i.e. outside the recovery boundary: neither replay nor
    // reprojection brings such an event back. For the inference scope that means a
    // permanently wrong read model, and a decision of that weight should not be made
    // by editing one line of config.
    //
    // It has been read as unfalsifiable — the two namespaces look disjoint, since
    // IGNORABLE_EVENT_TYPES holds only `OrderBook.*` and `PMP.*`. They are disjoint
    // by CONTENT today, not by construction: the scope includes named
    // `PrivateNote.*` and `OracleEventList.*` entries, and nothing stops one of
    // those being added to the ignorable list — `no_op_event_arms.rs` records that
    // `PMP.StakeForfeited` was deliberately kept out of it, so the question does get
    // asked. Verified by adding `PrivateNote.InferenceOrderRemoved` to that const:
    // this test goes red.
    let scope = in_scope_event_types();
    for t in IGNORABLE_EVENT_TYPES {
        assert!(
            !scope.contains(t),
            "{t} is in the inference scope and at the same time allowed to be dropped at \
             ingest. A dropped edge never reaches raw_events, so nothing can recover it"
        );
    }
}

#[tokio::test]
async fn every_in_scope_abi_event_reaches_an_arm() {
    // A GUARD, green from its first run: wave 0 gave arms to all 34 events. It turns
    // red on a NEW ABI event that has no arm.
    //
    // `expect` rather than `else { return }`: this test is the wave's centrepiece,
    // and skipping it silently without Postgres is indistinguishable from passing.
    let pool = setup().await.expect(
        "the routing guard requires Postgres: TEST_DATABASE_URL is unset or the database is down",
    );

    // Flags rather than a counter: the invariant is "the probe carries events of
    // BOTH projected families through to their arms", and it cannot be expressed as
    // a number — the threshold drifts along with the scope.
    let mut book_reached = false;
    let mut settlement_reached = false;
    let mut deal_closed_reached = false;

    for event_type in in_scope_event_types() {
        let mut tx = pool.begin().await.unwrap();
        let e = DecodedEvent {
            contract_kind: "",
            event_name: String::new(),
            event_type: event_type.clone(),
            // An empty payload on purpose: the goal is to reach the ARM, not to project.
            value: serde_json::json!({}),
        };
        let outcome = project_event(&mut tx, &e, &probe_node()).await;

        // For no-op types an arm alone is not enough: it must also be empty, otherwise
        // "no-op" turns into a silent write. `txid_current_if_assigned()` returns NULL
        // exactly when the transaction wrote nothing.
        //
        // THE `PrivateNote.` PREFIX HAS ONE EXCEPTION, and naming it here is the
        // point. Every note-side mirror is observability-only — the book is the
        // authority on an order and the note's copy exists for its owner — except
        // `InferenceDealClosed`, which since ingest stopped capturing TokenContract
        // routes is the only live signal that a deal ended, and therefore writes
        // `settled_at_chain`. Left inside the prefix rule it would be asserted to
        // write nothing, which is the opposite of its job.
        let is_declared_no_op = (event_type.starts_with("PrivateNote.")
            && event_type != "PrivateNote.InferenceDealClosed")
            || event_type.starts_with("RootModel.");
        let write_xid: Option<i64> = if is_declared_no_op {
            sqlx::query_scalar("select txid_current_if_assigned()")
                .fetch_one(&mut *tx)
                .await
                .unwrap()
        } else {
            None
        };
        let _ = tx.rollback().await;

        // `Err` IS ALLOWED and means the arm exists: the projector got as far as
        // parsing fields and did not find them in the empty payload. What is not
        // allowed is exactly `Ok(Unknown)` — "nobody knows this type" — which means
        // the row will be marked processed and lost forever.
        assert!(
            !matches!(outcome, Ok(ProjectionOutcome::Unknown)),
            "{event_type} is in the scoped ABI but is not routed: it will go to Unknown, \
             be marked processed on first sight and never be retried"
        );

        if is_declared_no_op {
            assert_eq!(
                outcome.expect("a no-op arm has no right to return an error"),
                ProjectionOutcome::Applied,
                "{event_type} must be a no-op returning Applied"
            );
            assert!(write_xid.is_none(), "{event_type} is declared a no-op but wrote something");
        } else if outcome.is_ok() {
            if event_type.starts_with("InferenceOrderBook.") {
                book_reached = true;
            }
            if event_type.starts_with("TokenContract.") {
                settlement_reached = true;
            }
        }
        // Counted whether it returned Ok or Err: an empty payload has no `deal`
        // field, so this arm legitimately errors on the probe. What is checked is
        // that it did not fall into the no-op branch above — see the exception
        // there.
        if event_type == "PrivateNote.InferenceDealClosed" {
            deal_closed_reached = true;
        }
    }

    // INSURANCE AGAINST AN EMPTY GUARD. The assert above rejects only `Ok(Unknown)`,
    // so an `Err` on EVERY event would leave it green and vacuous. That is exactly
    // what happens if the probe is handed over without `src`: both sub-routers start
    // by seeding a skeleton, which requires `node.src` and fails BEFORE the `match`.
    assert!(
        book_reached,
        "no InferenceOrderBook.* returned Ok — the probe does not reach their arms \
         (most likely it has no `src`), and the guard is empty for all nine book events"
    );
    assert!(
        settlement_reached,
        "no TokenContract.* returned Ok — the same for the fifteen settlement events"
    );
    // The exception is asserted to EXIST, not merely to be excluded: if
    // `InferenceDealClosed` ever leaves the scoped set, the carve-out above becomes
    // dead and this says so instead of silently protecting nothing.
    assert!(
        deal_closed_reached,
        "PrivateNote.InferenceDealClosed is no longer in the scoped set, so the no-op \
         carve-out above protects nothing and should go with it"
    );
}
