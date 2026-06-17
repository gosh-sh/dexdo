// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for persist_page's ignored_event_types skip. Gated on
// TEST_DATABASE_URL; prints a skip notice and returns when unset.

use std::collections::HashSet;
use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventEdge;
use dodex_infrastructure::graphql::EventNode;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use serde_json::json;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

// Real OrderBook.OrderPlaced event body (base64 BOC), from the decoder unit
// test fixture. Decodes to event_type "OrderBook.OrderPlaced".
const ORDER_PLACED_BODY: &str = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAACAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJGgAAAAAAAAAAAAAAAn+OzoAAAAAAAAAAAFcScEalnJSVsVKAm0LrR0TbuPbU18Mkb7ENEBG22bNzhvrIubdt2wtAAQAQAAAAAAAAAXM=";

// Not valid base64 (and not a BOC), so the decoder returns None — the edge is
// "undecodable": it carries no event_type to match against the ignore list.
const UNDECODABLE_BODY: &str = "@@@not-a-valid-boc@@@";

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
