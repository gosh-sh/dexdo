// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for IndexerRepository::persist_page (capture path).
// Gated on TEST_DATABASE_URL exactly like reprojection.rs: unset → skip.
//
//   TEST_DATABASE_URL=postgres://user:pass@localhost:5432/db \
//       cargo test -p dodex-infrastructure --test capture

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventEdge;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

mod fixtures;

use fixtures::chain_bodies::INFERENCE_ORDER_PLACED;

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

async fn purge(pool: &PgPool, queries: &[(&str, &str)]) {
    for (sql, key) in queries {
        sqlx::query(sql).bind(*key).execute(pool).await.expect("purge");
    }
}

/// Builds an edge with a decodable / non-decodable body. `body` is the
/// base64 BOC string, or None for an undecodable edge.
fn edge(msg_id: &str, chain_order: Option<&str>, src: &str, body: Option<&str>) -> EventEdge {
    EventEdge {
        cursor: "c".to_string(),
        node: EventNode {
            msg_id: msg_id.to_string(),
            msg_chain_order: chain_order.map(str::to_string),
            src: Some(src.to_string()),
            src_dapp_id: None,
            dst: Some(src.to_string()),
            body: body.map(|b| json!(b)),
            created_at: Some(json!(1_700_000_000_i64)),
        },
    }
}

// Real OrderBook.OrderPlaced body fixture (decoder.rs unit tests). Decodes to
// event_type "OrderBook.OrderPlaced", orderId "2".
const ORDER_PLACED_BODY: &str = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAACAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJGgAAAAAAAAAAAAAAAn+OzoAAAAAAAAAAAFcScEalnJSVsVKAm0LrR0TbuPbU18Mkb7ENEBG22bNzhvrIubdt2wtAAQAQAAAAAAAAAXM=";

// A stream of this test's own: `persist_page` upserts `at_head` into `indexer_cursors`,
// and the production row is read by the inference orders endpoint's fail-closed gate.
const CAPTURE_TEST_STREAM: &str = "capture_persist_page_test_stream";

// A stream of this test's own, for the same reason the neighbouring cases have one:
// `persist_page` upserts `at_head` into `indexer_cursors`, and the production row is
// read by the inference orders endpoint's fail-closed gate.
const INFERENCE_BODY_TEST_STREAM: &str = "capture_real_inference_body_test_stream";

#[tokio::test]
async fn persist_page_stores_a_real_inference_body_as_a_decoded_row() {
    // IX-CAP-01. The neighbouring cases run a prediction-side `OrderBook.OrderPlaced`
    // body, so nothing proved capture on an inference payload — the decoder is shared
    // but the ABI set and the event-id space are not.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");
    let msg_id = "cap-real-inference-1";
    let orderbook = "0:cap_real_inference_book";
    // The order-book rows are purged too, at BOTH ends. This test writes a globally
    // visible pending row and then asserts it is still unprojected, so the one thing
    // that can go wrong is somebody projecting it first — and projecting
    // `InferenceOrderPlaced` seeds an `inference_markets` and an `inference_orders`
    // row under this address. The nextest group makes that race impossible today;
    // purging makes its leftovers recoverable if the group is ever loosened, instead
    // of stranding two rows in the shared database forever.
    let cleanup: [(&str, &str); 3] = [
        ("delete from raw_events where msg_id = $1", msg_id),
        ("delete from inference_orders where orderbook_address = $1", orderbook),
        ("delete from inference_markets where orderbook_address = $1", orderbook),
    ];
    purge(&pool, &cleanup).await;
    purge(
        &pool,
        &[("delete from indexer_cursors where stream_name = $1", INFERENCE_BODY_TEST_STREAM)],
    )
    .await;

    let edges = vec![edge(
        msg_id,
        Some("5f80capreal0000000000000001"),
        orderbook,
        Some(INFERENCE_ORDER_PLACED),
    )];
    let result = repo
        .persist_page(INFERENCE_BODY_TEST_STREAM, &edges, Some("cursor-1"), &decoder, false)
        .await
        .expect("persist_page");
    assert_eq!(result.inserted, 1, "one edge in, one row out");

    let (event_type, decoded_is_null, processed_at_is_null, body): (
        Option<String>,
        bool,
        bool,
        Option<String>,
    ) = sqlx::query_as(
        "select event_type, decoded is null, processed_at is null, body_json #>> '{}'
           from raw_events where msg_id = $1",
    )
    .bind(msg_id)
    .fetch_one(&pool)
    .await
    .expect("the captured row");

    assert_eq!(event_type.as_deref(), Some("InferenceOrderBook.InferenceOrderPlaced"));
    assert!(!decoded_is_null, "a decodable inference body must land decoded, not as an orphan");
    assert!(processed_at_is_null, "capture must leave the row pending for the projector");
    // Not decoration: this pins the fact the whole wave's fixture supply rests on —
    // capture stores the body verbatim, so real bodies can be harvested from any
    // populated indexer database with a SQL query. If that ever changes, the next
    // harvest would silently come up empty; this fails first instead.
    assert_eq!(
        body.as_deref(),
        Some(INFERENCE_ORDER_PLACED),
        "body_json must hold the base64 verbatim"
    );

    purge(&pool, &cleanup).await;
}

#[tokio::test]
async fn captures_decodable_event_without_projecting() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let test = "capture_no_project";
    let orderbook = format!("0:{test}_book");
    let msg_id = format!("{test}-msg");
    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let edges = vec![edge(
        &msg_id,
        Some("5f80capture0000000000000001"),
        &orderbook,
        Some(ORDER_PLACED_BODY),
    )];
    let result = repo
        .persist_page(CAPTURE_TEST_STREAM, &edges, Some("cursor-1"), &decoder, false)
        .await
        .expect("persist_page");

    assert_eq!(result.inserted, 1, "decodable edge must be inserted");
    assert_eq!(result.decoded, 1, "OrderPlaced body must decode");

    let (event_type, decoded_is_set, processed_is_null): (Option<String>, bool, bool) =
        sqlx::query_as(
            "select event_type, decoded is not null, processed_at is null
               from raw_events where msg_id = $1",
        )
        .bind(&msg_id)
        .fetch_one(&pool)
        .await
        .expect("read raw_events");
    assert_eq!(event_type.as_deref(), Some("OrderBook.OrderPlaced"));
    assert!(decoded_is_set, "decoded jsonb must be stored at capture time");
    assert!(processed_is_null, "capture must NOT project — processed_at stays NULL");

    let live_count: i64 =
        sqlx::query_scalar("select count(*) from live_orders where orderbook_address = $1")
            .bind(&orderbook)
            .fetch_one(&pool)
            .await
            .expect("count live_orders");
    assert_eq!(
        live_count, 0,
        "capture must NOT write the read-model; projection is the loop's job"
    );

    // Purge before returning: this row is a valid, typed, decoded OrderPlaced
    // left with processed_at NULL, so a reprojection-suite reproject_pending
    // running against the shared test DB would pick it up and apply it. Clean it
    // (and any read-model it could spawn) so it cannot contaminate other tests.
    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;
}

#[tokio::test]
async fn bulk_insert_counts_new_and_conflicting_and_dedups_within_page() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let test = "capture_counts";
    let src = format!("0:{test}_src");
    let existing = format!("{test}-existing");
    let fresh = format!("{test}-fresh");
    let dup = format!("{test}-dup");
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", existing.as_str()),
            ("delete from raw_events where msg_id = $1", fresh.as_str()),
            ("delete from raw_events where msg_id = $1", dup.as_str()),
        ],
    )
    .await;

    // Pre-insert `existing` so it conflicts on the next page.
    let pre = vec![edge(&existing, Some("5f80capture_counts_000000001"), &src, None)];
    repo.persist_page(CAPTURE_TEST_STREAM, &pre, None, &decoder, false).await.expect("pre-insert");

    // Page: the conflicting `existing`, a `fresh` row, and `dup` twice.
    let edges = vec![
        edge(&existing, Some("5f80capture_counts_000000001"), &src, None),
        edge(&fresh, Some("5f80capture_counts_000000002"), &src, None),
        edge(&dup, Some("5f80capture_counts_000000003"), &src, None),
        edge(&dup, Some("5f80capture_counts_000000003"), &src, None),
    ];
    let result = repo
        .persist_page(CAPTURE_TEST_STREAM, &edges, None, &decoder, false)
        .await
        .expect("persist_page");

    // After in-page de-dup: 3 unique candidates (existing, fresh, dup); 1
    // conflicts (existing) → inserted 2 (fresh, dup), skipped 1.
    assert_eq!(result.inserted, 2, "fresh + dup insert once each");
    assert_eq!(result.skipped, 1, "the pre-existing msg_id conflicts");

    let dup_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(&dup)
        .fetch_one(&pool)
        .await
        .expect("count dup");
    assert_eq!(dup_count, 1, "a within-page duplicate msg_id must land exactly once");

    // Clean up. This test used to purge only at its start, which is enough for
    // ITS next run and wrong for everyone else's: the three rows stayed in
    // `raw_events` forever, and `projection_lag_seconds_empty_queue_is_zero`
    // (reprojection.rs) skips its assert whenever the eligible queue is not
    // empty — so a leak here silently retires that contract on any long-lived
    // database.
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", existing.as_str()),
            ("delete from raw_events where msg_id = $1", fresh.as_str()),
            ("delete from raw_events where msg_id = $1", dup.as_str()),
            ("delete from indexer_cursors where stream_name = $1", CAPTURE_TEST_STREAM),
        ],
    )
    .await;
}

#[tokio::test]
async fn edge_missing_chain_order_is_dropped() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let test = "capture_no_chain_order";
    let src = format!("0:{test}_src");
    let msg_id = format!("{test}-msg");
    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id.as_str())]).await;

    let edges = vec![edge(&msg_id, None, &src, None)];
    let result = repo
        .persist_page(CAPTURE_TEST_STREAM, &edges, None, &decoder, false)
        .await
        .expect("persist_page");

    assert_eq!(result.inserted, 0, "an edge without msg_chain_order is not inserted");
    assert_eq!(result.undecoded, 1, "the dropped edge is counted as undecoded");

    let exists: bool =
        sqlx::query_scalar("select exists(select 1 from raw_events where msg_id = $1)")
            .bind(&msg_id)
            .fetch_one(&pool)
            .await
            .expect("exists");
    assert!(!exists, "no raw_events row for a keyless edge");
}

#[tokio::test]
async fn persist_page_advances_cursor() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    // Unique stream name so this test does not race the live cursor row.
    let stream = "capture_cursor_test_stream";
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("purge cursor");

    repo.persist_page(stream, &[], Some("cursor-xyz"), &decoder, true).await.expect("persist_page");

    let cursor = repo.load_cursor(stream).await.expect("load_cursor");
    assert_eq!(cursor.as_deref(), Some("cursor-xyz"), "end_cursor must be persisted");
}

#[tokio::test]
async fn persist_page_writes_at_head_flag() {
    // The at_head argument must be written through to indexer_cursors (the
    // inference sweep + orphan dead-letter gate on it). A regression binding a
    // constant would silently freeze all sweeps or run them mid-backfill.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "capture_at_head_test_stream";
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("purge cursor");

    // has_next_page=false => at_head=true.
    repo.persist_page(stream, &[], Some("c1"), &decoder, true).await.expect("persist at_head=true");
    assert!(
        repo.at_head(stream).await.expect("read at_head"),
        "at_head=true must be written through"
    );

    // A later page with more to fetch => at_head=false.
    repo.persist_page(stream, &[], Some("c2"), &decoder, false)
        .await
        .expect("persist at_head=false");
    assert!(
        !repo.at_head(stream).await.expect("read at_head"),
        "at_head must flip back to false when more pages follow"
    );
}

#[tokio::test]
async fn persist_page_with_null_cursor_preserves_prior_cursor() {
    // The cursor upsert uses coalesce(excluded.cursor, prior): a page that yields
    // no end_cursor must NOT reset the stream to genesis. A coalesce typo would
    // silently null the cursor and replay the whole chain.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "capture_coalesce_test_stream";
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("purge cursor");

    repo.persist_page(stream, &[], Some("keep-me"), &decoder, false).await.expect("persist cursor");
    assert_eq!(repo.load_cursor(stream).await.expect("load").as_deref(), Some("keep-me"));

    repo.persist_page(stream, &[], None, &decoder, false).await.expect("persist null cursor");
    assert_eq!(
        repo.load_cursor(stream).await.expect("load").as_deref(),
        Some("keep-me"),
        "coalesce(excluded.cursor, prior) must preserve the cursor when end_cursor is NULL"
    );
}

#[tokio::test]
async fn aggregate_barrier_can_replace_a_prior_cursor_with_null() {
    let Some(pool) = setup().await else { return };
    let stream = "capture_aggregate_replace_test_stream";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(stream);

    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("purge cursor");

    repo.set_capture_barrier(Some("old-global-cursor"), true).await.expect("seed barrier");
    repo.set_capture_barrier(None, false).await.expect("clear barrier");

    assert_eq!(repo.load_cursor(stream).await.expect("load barrier"), None);
    assert!(!repo.at_head(stream).await.expect("load at_head"));
}

#[tokio::test]
async fn first_dual_stream_start_invalidates_the_old_global_cursor() {
    let Some(pool) = setup().await else { return };
    let aggregate = "capture_initialize_aggregate_test_stream";
    let source_a = "capture_initialize_source_a";
    let source_b = "capture_initialize_source_b";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(aggregate);

    for stream in [aggregate, source_a, source_b] {
        sqlx::query("delete from indexer_cursors where stream_name = $1")
            .bind(stream)
            .execute(&pool)
            .await
            .expect("purge cursor");
    }
    repo.set_capture_barrier(Some("old-global-cursor"), true).await.expect("seed aggregate");
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head) values ($1, 'source-a', true)",
    )
    .bind(source_a)
    .execute(&pool)
    .await
    .expect("seed one source");

    repo.initialize_capture_barrier(&[source_a, source_b]).await.expect("initialize barrier");

    assert_eq!(repo.load_cursor(aggregate).await.expect("load aggregate"), None);
    assert!(!repo.at_head(aggregate).await.expect("load at_head"));
}

#[tokio::test]
async fn dual_stream_restart_preserves_barrier_but_closes_at_head() {
    let Some(pool) = setup().await else { return };
    let aggregate = "capture_restart_aggregate_test_stream";
    let source_a = "capture_restart_source_a";
    let source_b = "capture_restart_source_b";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(aggregate);

    for stream in [aggregate, source_a, source_b] {
        sqlx::query("delete from indexer_cursors where stream_name = $1")
            .bind(stream)
            .execute(&pool)
            .await
            .expect("purge cursor");
    }
    repo.set_capture_barrier(Some("synchronized-cursor"), true).await.expect("seed aggregate");
    for stream in [source_a, source_b] {
        sqlx::query(
            "insert into indexer_cursors (stream_name, cursor, at_head) values ($1, 'source', true)",
        )
        .bind(stream)
        .execute(&pool)
        .await
        .expect("seed source");
    }

    repo.initialize_capture_barrier(&[source_a, source_b]).await.expect("initialize barrier");

    assert_eq!(
        repo.load_cursor(aggregate).await.expect("load aggregate").as_deref(),
        Some("synchronized-cursor")
    );
    assert!(!repo.at_head(aggregate).await.expect("load at_head"));
}

#[tokio::test]
async fn persist_page_handles_mixed_decodable_and_undecodable_edges() {
    // One page with a decodable edge (ORDER_PLACED_BODY) and an undecodable
    // edge (body None). Asserts the counts and per-row DB state are correct —
    // catching UNNEST column misalignment and the 'null'::jsonb vs SQL-NULL
    // asymmetry.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let test = "capture_mixed";
    let orderbook = format!("0:{test}_book");
    let msg_decodable = format!("{test}-decodable");
    let msg_undecodable = format!("{test}-undecodable");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook.as_str()),
            ("delete from raw_events where msg_id = $1", msg_decodable.as_str()),
            ("delete from raw_events where msg_id = $1", msg_undecodable.as_str()),
        ],
    )
    .await;

    let edges = vec![
        edge(
            &msg_decodable,
            Some("5f80capture_mixed_00000000000001"),
            &orderbook,
            Some(ORDER_PLACED_BODY),
        ),
        edge(&msg_undecodable, Some("5f80capture_mixed_00000000000002"), &orderbook, None),
    ];
    let result = repo
        .persist_page(CAPTURE_TEST_STREAM, &edges, None, &decoder, false)
        .await
        .expect("persist_page");

    assert_eq!(result.inserted, 2, "both edges must be inserted");
    assert_eq!(result.decoded, 1, "only the OrderPlaced body decodes");
    assert_eq!(result.undecoded, 1, "the body-None edge is undecoded");

    // Decodable row: event_type set, decoded non-null, processed_at null.
    let (event_type, decoded_is_set, processed_is_null): (Option<String>, bool, bool) =
        sqlx::query_as(
            "select event_type, decoded is not null, processed_at is null
               from raw_events where msg_id = $1",
        )
        .bind(&msg_decodable)
        .fetch_one(&pool)
        .await
        .expect("read decodable row");
    assert_eq!(
        event_type.as_deref(),
        Some("OrderBook.OrderPlaced"),
        "decodable row must have event_type set"
    );
    assert!(decoded_is_set, "decodable row must have decoded jsonb stored");
    assert!(processed_is_null, "capture must not project — processed_at stays NULL");

    // Undecodable row: event_type null, decoded null.
    let (event_type_u, decoded_is_set_u): (Option<String>, bool) =
        sqlx::query_as("select event_type, decoded is not null from raw_events where msg_id = $1")
            .bind(&msg_undecodable)
            .fetch_one(&pool)
            .await
            .expect("read undecodable row");
    assert!(event_type_u.is_none(), "undecodable row must have event_type NULL");
    assert!(!decoded_is_set_u, "undecodable row must have decoded NULL");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook.as_str()),
            ("delete from raw_events where msg_id = $1", msg_decodable.as_str()),
            ("delete from raw_events where msg_id = $1", msg_undecodable.as_str()),
        ],
    )
    .await;
}

/// `edge` with an explicit `dst` (the decoder's routing key). The shared
/// helper pins `dst = src`, which is right for order-book events but cannot
/// express the ambiguous-collision case: a colliding event id whose `dst` no
/// route knows. Kept separate so the neighbours' calls stay untouched.
fn edge_with_dst(
    msg_id: &str,
    chain_order: Option<&str>,
    src: &str,
    dst: Option<&str>,
    body: Option<&str>,
) -> EventEdge {
    EventEdge {
        cursor: "c".to_string(),
        node: EventNode {
            msg_id: msg_id.to_string(),
            msg_chain_order: chain_order.map(str::to_string),
            src: Some(src.to_string()),
            src_dapp_id: None,
            dst: dst.map(str::to_string),
            body: body.map(|b| json!(b)),
            created_at: Some(json!(1_700_000_000_i64)),
        },
    }
}

// Streams of these tests' own, for the reason the neighbours have one:
// `persist_page` upserts into `indexer_cursors`, and the production row is read
// by the inference orders endpoint's fail-closed gate.
const MALFORMED_BODY_TEST_STREAM: &str = "capture_malformed_body_test_stream";
const AMBIGUOUS_TEST_STREAM: &str = "capture_ambiguous_test_stream";

#[tokio::test]
async fn a_malformed_body_lands_undecoded_and_bumps_the_decode_error_counter() {
    // IX-CAP-05 + the decode_errors third of IX-MET-06. The existing mixed test's
    // undecodable edge is `body: None`, which returns from try_decode BEFORE the
    // counter — so it can never observe the increment. Only a body that parses as
    // a string and then fails the decoder reaches the Err arm that counts.
    // "this-is-not-a-boc" is not valid base64, so `decode_event_body` fails hard
    // (a decode error, not an unknown/ambiguous id).
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone()); // fresh instance => counters at zero
    let decoder = Decoder::new().expect("decoder");
    let msg_id = "cap-malformed-body-1";
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", msg_id),
            ("delete from indexer_cursors where stream_name = $1", MALFORMED_BODY_TEST_STREAM),
        ],
    )
    .await;

    let edges = vec![edge(
        msg_id,
        Some("5f80capmalformed000000000001"),
        "0:cap_malformed_book",
        Some("this-is-not-a-boc"),
    )];
    let result = repo
        .persist_page(MALFORMED_BODY_TEST_STREAM, &edges, None, &decoder, false)
        .await
        .expect("persist_page");
    assert_eq!(result.inserted, 1);
    assert_eq!(result.undecoded, 1, "the row must be stored undecoded, not dropped");
    assert_eq!(repo.decode_errors_count(), 1, "the counter is the only durable signal (IX-CAP-05)");
    // IX-MET-06 says the counters grow ON THEIR OWN events only — mutual exclusivity
    // is half the claim, and the ambiguous test asserts the mirror direction.
    assert_eq!(
        repo.decode_ambiguous_collisions_count(),
        0,
        "a hard decode failure is not an ambiguity"
    );

    let (event_type_is_null, decoded_is_null): (bool, bool) = sqlx::query_as(
        "select event_type is null, decoded is null from raw_events where msg_id = $1",
    )
    .bind(msg_id)
    .fetch_one(&pool)
    .await
    .expect("row");
    assert!(event_type_is_null && decoded_is_null, "a failed decode stores the row bare");
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", msg_id),
            ("delete from indexer_cursors where stream_name = $1", MALFORMED_BODY_TEST_STREAM),
        ],
    )
    .await;
}

#[tokio::test]
async fn a_colliding_body_with_an_unrouted_dst_bumps_the_ambiguous_counter() {
    // The ambiguous third of IX-MET-06. TC_CONTRACT_DEPLOYED's event id collides
    // with RootModel.ContractDeployed; with a dst the router does not know, the
    // decoder returns AmbiguousCollision and try_decode counts it.
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");
    let msg_id = "cap-ambiguous-collision-1";
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", msg_id),
            ("delete from indexer_cursors where stream_name = $1", AMBIGUOUS_TEST_STREAM),
        ],
    )
    .await;

    let edges = vec![edge_with_dst(
        msg_id,
        Some("5f80capambig0000000000000001"),
        "0:cap_ambig_src",
        Some("0:no-such-route-dst"),
        Some(fixtures::chain_bodies::TC_CONTRACT_DEPLOYED),
    )];
    let result = repo
        .persist_page(AMBIGUOUS_TEST_STREAM, &edges, None, &decoder, false)
        .await
        .expect("persist_page");
    assert_eq!(result.undecoded, 1, "ambiguity stores the row undecoded");
    assert_eq!(repo.decode_ambiguous_collisions_count(), 1);
    assert_eq!(repo.decode_errors_count(), 0, "ambiguity is not a decode error");
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", msg_id),
            ("delete from indexer_cursors where stream_name = $1", AMBIGUOUS_TEST_STREAM),
        ],
    )
    .await;
}
