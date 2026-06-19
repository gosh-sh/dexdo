// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for persist_page's ignored_event_types skip. Gated on
// TEST_DATABASE_URL; prints a skip notice and returns when unset.

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use dodex_infrastructure::config::IGNORABLE_EVENT_TYPES;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::DecodedEvent;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventEdge;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_infrastructure::projectors;
use dodex_infrastructure::projectors::ProjectionOutcome;
use serde_json::json;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

// Real OrderBook.OrderPlaced event body (base64 BOC), from the decoder unit
// test fixture. Decodes to event_type "OrderBook.OrderPlaced".
const ORDER_PLACED_BODY: &str = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAACAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJGgAAAAAAAAAAAAAAAn+OzoAAAAAAAAAAAFcScEalnJSVsVKAm0LrR0TbuPbU18Mkb7ENEBG22bNzhvrIubdt2wtAAQAQAAAAAAAAAXM=";

// Not valid base64 (and not a BOC), so the decoder returns None — the edge is
// "undecodable": it carries no event_type to match against the ignore list.
const UNDECODABLE_BODY: &str = "@@@not-a-valid-boc@@@";

// Real PMP.StakeAccepted event body (base64 BOC), captured from shellnet
// message 9ded852d5f4de2645703534b568f0bfa9a6c94a609e05bce0b9f6d04862352a3.
// It decodes cleanly but PMP.StakeAccepted has no projector arm, so
// project_event returns Unknown — the outcome that drives the no-handler noise
// routing. A shellnet redeploy may retire the message; the fixture is
// self-contained regardless.
const UNHANDLED_EVENT_BODY: &str =
    "te6ccgEBAQEAPQAAdUdlUlyAAs07z2h9tsBjTWvTVO1y7hoNFyPFA00R2BHKcjXndidgAAAAIAAAAAAAAAAAAAAAJUC+QAAQ";
const UNHANDLED_EVENT_TYPE: &str = "PMP.StakeAccepted";

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

fn edge_with_body(msg_id: &str, cursor: &str, body: Value) -> EventEdge {
    EventEdge {
        cursor: cursor.to_string(),
        node: EventNode {
            msg_id: msg_id.to_string(),
            msg_chain_order: Some(msg_id.to_string()),
            src: Some(
                "0:00000000000000000000000000000000000000000000000000000000000000ab".to_string(),
            ),
            src_dapp_id: None,
            dst: None,
            body: Some(body),
            created_at: None,
        },
    }
}

fn edge(msg_id: &str, cursor: &str) -> EventEdge {
    edge_with_body(msg_id, cursor, json!(ORDER_PLACED_BODY))
}

/// Remove the `raw_events` rows for `msg_ids` and the `indexer_cursors` row for
/// `stream`, so a re-run starts clean.
async fn cleanup(pool: &PgPool, stream: &str, msg_ids: &[&str]) {
    for msg_id in msg_ids {
        sqlx::query("delete from raw_events where msg_id = $1")
            .bind(msg_id)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn persist_page_skips_ignored_event_type_but_advances_cursor() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_skip";
    let msg_id = "ignored_types_test_skip_msg";
    sqlx::query("delete from raw_events where msg_id = $1")
        .bind(msg_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .unwrap();

    // OrderPlaced is used only because its BOC fixture decodes reliably; it
    // exercises the generic skip path. `persist_page` accepts any set — the
    // startup config guard separately forbids this metric-critical type.
    let ignored: HashSet<&str> = ["OrderBook.OrderPlaced"].into_iter().collect();
    let res = repo
        .persist_page(stream, &[edge(msg_id, "cursor-1")], Some("cursor-1"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 1, "the OrderPlaced edge is type-ignored");
    assert_eq!(res.inserted, 0, "nothing inserted");

    let row_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(msg_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count, 0, "no raw_events row for the ignored edge");

    let cursor: Option<String> =
        sqlx::query_scalar("select cursor from indexer_cursors where stream_name = $1")
            .bind(stream)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(cursor.as_deref(), Some("cursor-1"), "cursor still advanced");

    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn persist_page_inserts_when_type_not_ignored() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_keep";
    let msg_id = "ignored_types_test_keep_msg";
    sqlx::query("delete from raw_events where msg_id = $1")
        .bind(msg_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .unwrap();

    let ignored: HashSet<&str> = HashSet::new();
    let res = repo
        .persist_page(stream, &[edge(msg_id, "cursor-1")], Some("cursor-1"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 0);
    assert_eq!(res.inserted, 1, "the edge is inserted when not ignored");

    let row_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(msg_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count, 1, "raw_events row present");

    sqlx::query("delete from raw_events where msg_id = $1")
        .bind(msg_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn persist_page_mixed_ignored_and_kept_edges_advances_cursor_once() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_mixed";
    let ids = ["mix_ignored_a", "mix_kept_b", "mix_ignored_c", "mix_kept_d"];
    cleanup(&pool, stream, &ids).await;

    // A single page interleaving ignored (decodable OrderPlaced) and kept
    // (undecodable) edges. The ignored edges hit the `continue`; the kept edges
    // are still inserted; and exactly one cursor upsert runs after the loop, at
    // the page's end cursor — independent of how many edges were skipped.
    let edges = vec![
        edge(ids[0], "c1"),                                    // OrderPlaced -> ignored
        edge_with_body(ids[1], "c2", json!(UNDECODABLE_BODY)), // undecodable -> kept
        edge(ids[2], "c3"),                                    // OrderPlaced -> ignored
        edge_with_body(ids[3], "c4", json!(UNDECODABLE_BODY)), // undecodable -> kept
    ];
    let ignored: HashSet<&str> = ["OrderBook.OrderPlaced"].into_iter().collect();
    let res = repo
        .persist_page(stream, &edges, Some("cursor-final"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 2, "both OrderPlaced edges are type-ignored");
    assert_eq!(res.undecoded, 2, "both undecodable edges counted undecoded");
    assert_eq!(res.inserted, 2, "the two kept (undecodable) edges are inserted");
    assert_eq!(res.decoded, 0, "no decodable edge survived the ignore filter");

    let kept: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = any($1)")
        .bind(vec![ids[1].to_string(), ids[3].to_string()])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 2, "rows present for the two kept edges");

    let ignored_rows: i64 =
        sqlx::query_scalar("select count(*) from raw_events where msg_id = any($1)")
            .bind(vec![ids[0].to_string(), ids[2].to_string()])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ignored_rows, 0, "no rows for the ignored edges");

    let cursor_rows: i64 =
        sqlx::query_scalar("select count(*) from indexer_cursors where stream_name = $1")
            .bind(stream)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cursor_rows, 1, "exactly one cursor row after the page");

    let cursor: Option<String> =
        sqlx::query_scalar("select cursor from indexer_cursors where stream_name = $1")
            .bind(stream)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten();
    assert_eq!(cursor.as_deref(), Some("cursor-final"), "cursor at the page end");

    cleanup(&pool, stream, &ids).await;
}

#[tokio::test]
async fn persist_page_matcher_is_exact_case_and_whitespace_sensitive() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_exact";
    let msg_id = "ignored_types_test_exact_msg";
    cleanup(&pool, stream, &[msg_id]).await;

    // Near-miss ignore entries (wrong case, leading/trailing whitespace) must
    // NOT match the exact decoded event_type — the OrderPlaced edge is kept.
    let ignored: HashSet<&str> =
        ["orderbook.orderplaced", " OrderBook.OrderPlaced ", "OrderBook.OrderPlaced\t"]
            .into_iter()
            .collect();
    let res = repo
        .persist_page(stream, &[edge(msg_id, "c1")], Some("c1"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 0, "no near-miss entry matches the exact type");
    assert_eq!(res.decoded, 1, "the edge decoded");
    assert_eq!(res.inserted, 1, "the edge is kept and inserted");

    cleanup(&pool, stream, &[msg_id]).await;
}

#[tokio::test]
async fn persist_page_does_not_ignore_undecodable_edge() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_undecodable";
    let msg_id = "ignored_types_test_undecodable_msg";
    cleanup(&pool, stream, &[msg_id]).await;

    // The ignore filter runs only on a decoded event_type. An edge that fails
    // to decode has no event_type to match, so it is kept (inserted as
    // undecoded) regardless of what the ignore set contains.
    let ignored: HashSet<&str> = ["OrderBook.OrderPlaced"].into_iter().collect();
    let res = repo
        .persist_page(
            stream,
            &[edge_with_body(msg_id, "c1", json!(UNDECODABLE_BODY))],
            Some("c1"),
            &decoder,
            &ignored,
        )
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 0, "an undecodable edge cannot be type-ignored");
    assert_eq!(res.undecoded, 1, "counted undecoded");
    assert_eq!(res.inserted, 1, "kept and inserted");

    let row_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(msg_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row_count, 1, "raw_events row present for the undecodable edge");

    cleanup(&pool, stream, &[msg_id]).await;
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

/// One captured `warn!` event: its tracing target plus the `message` and
/// `event_type` fields, enough to assert which sink the no-handler warning
/// would land in.
#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
    message: String,
    event_type: String,
}

/// Records every event into a shared buffer so a test can assert on the
/// tracing `target` the indexer chose, without a real file/stdout subscriber.
struct CaptureLayer {
    events: Arc<StdMutex<Vec<CapturedEvent>>>,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Fields {
            message: String,
            event_type: String,
        }
        impl Visit for Fields {
            // `%event_type` (Display) and the message literal both arrive via
            // record_debug as format_args, whose Debug output is the plain
            // string with no surrounding quotes.
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = format!("{value:?}"),
                    "event_type" => self.event_type = format!("{value:?}"),
                    _ => {}
                }
            }

            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "message" => self.message = value.to_string(),
                    "event_type" => self.event_type = value.to_string(),
                    _ => {}
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.events.lock().unwrap().push(CapturedEvent {
            target: event.metadata().target().to_string(),
            message: fields.message,
            event_type: fields.event_type,
        });
    }
}

// Verifies the wiring the rest of the suite leaves untested at the fetch-loop
// callsite: when persist_page decodes an event with no projector handler, the
// FIRST sighting emits on the normal target (stdout + main log) and the REPEAT
// emits on EVENT_NOISE_TARGET. The dedup boolean and the sink truth-table are
// each tested in isolation; a swapped if/else or a dropped `target:` arg would
// pass both yet flood the main log or hide the operator's first signal.
// current_thread flavor keeps the persist_page future on this thread so the
// thread-local subscriber set below sees its events.
#[tokio::test(flavor = "current_thread")]
async fn persist_page_unknown_event_warns_first_on_normal_target_then_repeat_on_noise() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_noise_routing";
    let first = "noise_routing_first";
    let repeat = "noise_routing_repeat";
    cleanup(&pool, stream, &[first, repeat]).await;

    // Two edges decoding to the same handler-less event in one page: the first
    // is the first sighting, the second a repeat. Distinct msg_ids so both
    // insert and both reach the Unknown projector arm.
    let edges = vec![
        edge_with_body(first, "c1", json!(UNHANDLED_EVENT_BODY)),
        edge_with_body(repeat, "c2", json!(UNHANDLED_EVENT_BODY)),
    ];
    let ignored: HashSet<&str> = HashSet::new();

    let events = Arc::new(StdMutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer { events: events.clone() });
    let res = {
        let _sub = tracing::subscriber::set_default(subscriber);
        repo.persist_page(stream, &edges, Some("c2"), &decoder, &ignored)
            .await
            .expect("persist_page")
    };

    assert_eq!(res.decoded, 2, "both edges decoded to the handler-less event");
    assert_eq!(res.inserted, 2, "both edges inserted");

    let warnings: Vec<CapturedEvent> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| {
            e.event_type == UNHANDLED_EVENT_TYPE && e.message.contains("no handler for event type")
        })
        .cloned()
        .collect();

    assert_eq!(
        warnings.len(),
        2,
        "expected first-sighting + one repeat no-handler warning, got {warnings:?}"
    );
    assert_ne!(
        warnings[0].target,
        dodex_logging::EVENT_NOISE_TARGET,
        "first sighting must use the normal target (stdout + main log), not the noise target"
    );
    assert!(
        warnings[0].message.contains("first sighting"),
        "first sighting must carry the operator-facing message, got {:?}",
        warnings[0].message
    );
    assert_eq!(
        warnings[1].target,
        dodex_logging::EVENT_NOISE_TARGET,
        "the repeat must be diverted to EVENT_NOISE_TARGET so it lands in the noise log"
    );

    cleanup(&pool, stream, &[first, repeat]).await;
}
