// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration test for persist_page's ignored_event_types skip. Gated on
// TEST_DATABASE_URL; prints a skip notice and returns when unset.

use std::collections::HashSet;
use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::{EventEdge, EventNode};
use dodex_infrastructure::indexer_repo::IndexerRepository;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

// Real OrderBook.OrderPlaced event body (base64 BOC), from the decoder unit
// test fixture. Decodes to event_type "OrderBook.OrderPlaced".
const ORDER_PLACED_BODY: &str = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAACAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJGgAAAAAAAAAAAAAAAn+OzoAAAAAAAAAAAFcScEalnJSVsVKAm0LrR0TbuPbU18Mkb7ENEBG22bNzhvrIubdt2wtAAQAQAAAAAAAAAXM=";

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

fn edge(msg_id: &str, cursor: &str) -> EventEdge {
    EventEdge {
        cursor: cursor.to_string(),
        node: EventNode {
            msg_id: msg_id.to_string(),
            msg_chain_order: Some(msg_id.to_string()),
            src: Some("0:00000000000000000000000000000000000000000000000000000000000000ab".to_string()),
            src_dapp_id: None,
            dst: None,
            body: Some(json!(ORDER_PLACED_BODY)),
            created_at: None,
        },
    }
}

#[tokio::test]
async fn persist_page_skips_ignored_event_type_but_advances_cursor() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_skip";
    let msg_id = "ignored_types_test_skip_msg";
    sqlx::query("delete from raw_events where msg_id = $1").bind(msg_id).execute(&pool).await.unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1").bind(stream).execute(&pool).await.unwrap();

    let ignored: HashSet<&str> = ["OrderBook.OrderPlaced"].into_iter().collect();
    let res = repo
        .persist_page(stream, &[edge(msg_id, "cursor-1")], Some("cursor-1"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 1, "the OrderPlaced edge is type-ignored");
    assert_eq!(res.inserted, 0, "nothing inserted");

    let row_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(msg_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row_count, 0, "no raw_events row for the ignored edge");

    let cursor: Option<String> = sqlx::query_scalar("select cursor from indexer_cursors where stream_name = $1")
        .bind(stream).fetch_optional(&pool).await.unwrap().flatten();
    assert_eq!(cursor.as_deref(), Some("cursor-1"), "cursor still advanced");

    sqlx::query("delete from indexer_cursors where stream_name = $1").bind(stream).execute(&pool).await.unwrap();
}

#[tokio::test]
async fn persist_page_inserts_when_type_not_ignored() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new().expect("decoder");

    let stream = "ignored_types_test_keep";
    let msg_id = "ignored_types_test_keep_msg";
    sqlx::query("delete from raw_events where msg_id = $1").bind(msg_id).execute(&pool).await.unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1").bind(stream).execute(&pool).await.unwrap();

    let ignored: HashSet<&str> = HashSet::new();
    let res = repo
        .persist_page(stream, &[edge(msg_id, "cursor-1")], Some("cursor-1"), &decoder, &ignored)
        .await
        .expect("persist_page");

    assert_eq!(res.type_ignored, 0);
    assert_eq!(res.inserted, 1, "the edge is inserted when not ignored");

    let row_count: i64 = sqlx::query_scalar("select count(*) from raw_events where msg_id = $1")
        .bind(msg_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row_count, 1, "raw_events row present");

    sqlx::query("delete from raw_events where msg_id = $1").bind(msg_id).execute(&pool).await.unwrap();
    sqlx::query("delete from indexer_cursors where stream_name = $1").bind(stream).execute(&pool).await.unwrap();
}
