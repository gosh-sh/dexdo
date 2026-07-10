// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for `PostgresReadModelRepository::list_inference_orders`: the
// one-statement snapshot query and its fail-closed gate. Gated on TEST_DATABASE_URL —
// see crates/infrastructure/tests/reprojection.rs for the docker-compose harness.
//
//   cargo nextest run -p dodex-infrastructure --test inference_orders_repo

use std::env;
use std::time::Duration;

use dodex_application::InferenceOrderStatus::Filled;
use dodex_application::InferenceOrderStatus::Live;
use dodex_application::InferenceOrderStatus::{self};
use dodex_application::InferenceOrdersCursor;
use dodex_application::InferenceOrdersQuery;
use dodex_application::InferenceReadRepository;
use dodex_application::InferenceSide::Buy;
use dodex_application::InferenceSide::Sell;
use dodex_application::InferenceSide::{self};
use dodex_application::OrdersLimit;
use dodex_domain::DomainError;
use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::CAPTURE_STREAM; // "blockchain_events"
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn test_pool() -> Option<PgPool> {
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

/// The capture cursor is a single global row. Tests that flip it must not run beside
/// tests that need it true — mirroring `AT_HEAD_GATE_LOCK` in the reconciler tests.
static CAPTURE_CURSOR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn seed_at_head(pool: &PgPool) {
    // `updated_at` must be refreshed on every call, not just on first insert: the
    // capture-lag gate test backdates this same row and then calls `seed_at_head` to prove
    // "a fresh poll reopens it" — an `ON CONFLICT` clause that left `updated_at` untouched
    // would leave that row stale forever and every test after it would see a closed gate.
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at) \
         values ($1, 'c', true, now()) \
         on conflict (stream_name) do update set at_head = true, updated_at = now()",
    )
    .bind(CAPTURE_STREAM)
    .execute(pool)
    .await
    .expect("seed at_head");
}

/// Delete a book's rows across every table the gate or the page reads. A gate test that
/// panics between inserting an unprojected `raw_events` row and cleaning it up leaves the
/// gate's second arm true for that book forever, and every later run of that test fails on
/// a condition it never created.
async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from raw_events where src_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_orders where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}

/// Seed a reconciled inference market: fee=250, precision 9/0, quote SHELL — mirrors
/// `inference_read_repo.rs`'s `seed_market` helper for the fields this suite needs.
async fn seed_reconciled_market(pool: &PgPool, ob: &str) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, producer, model_name, model_version,
                platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, created_at_chain, last_reconciled_at)
           values ($1, null, 'ref', 'producer', 'model', 'v1',
                   250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001', now(), now())"#,
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("seed reconciled market");
}

async fn seed_order(
    pool: &PgPool,
    ob: &str,
    order_id: i64,
    is_buy: bool,
    status: &str,
    token_contract: Option<&str>,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, status, last_chain_order, token_contract,
                chain_created_at, chain_updated_at)
           values ($1, $2, $3, 10, 5, 5, false, $4, $5, $6, now(), now())"#,
    )
    .bind(ob)
    .bind(order_id)
    .bind(is_buy)
    .bind(status)
    .bind(format!("co-{ob}-{order_id}"))
    .bind(token_contract)
    .execute(pool)
    .await
    .expect("seed inference_orders");
}

/// All-statuses / default-limit query for a book. `.token_contract()`, `.note()`,
/// `.side()`, `.status()`, `.limit()` and `.cursor()` narrow it.
fn query(ob: &str) -> InferenceOrdersQuery {
    InferenceOrdersQuery {
        orderbook_address: ob.to_string(),
        token_contract: None,
        note: None,
        side: None,
        statuses: InferenceOrderStatus::ALL.to_vec(),
        limit: OrdersLimit::DEFAULT,
        cursor: None,
    }
}

trait QueryBuilderExt {
    fn token_contract(self, tc: &str) -> Self;
    // No test in this file currently filters by note; kept for parity with the query's
    // other filter dimensions and for tests added later.
    #[allow(dead_code)]
    fn note(self, note: &str) -> Self;
    fn side(self, side: InferenceSide) -> Self;
    fn status(self, statuses: &[InferenceOrderStatus]) -> Self;
    fn limit(self, limit: u16) -> Self;
    fn cursor(self, cursor: &str) -> Self;
}

impl QueryBuilderExt for InferenceOrdersQuery {
    fn token_contract(mut self, tc: &str) -> Self {
        self.token_contract = Some(tc.to_string());
        self
    }

    fn note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }

    fn side(mut self, side: InferenceSide) -> Self {
        self.side = Some(side);
        self
    }

    fn status(mut self, statuses: &[InferenceOrderStatus]) -> Self {
        self.statuses = statuses.to_vec();
        self
    }

    fn limit(mut self, limit: u16) -> Self {
        self.limit = OrdersLimit::new(limit).expect("valid limit in test");
        self
    }

    fn cursor(mut self, cursor: &str) -> Self {
        self.cursor =
            Some(InferenceOrdersCursor::new(cursor.to_string()).expect("valid cursor in test"));
        self
    }
}

#[tokio::test]
async fn gate_blocks_only_token_contract_queries_that_scope_live_sells() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await; // otherwise the capture-lag arm alone closes the gate
    let ob = "0:gate";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, /* is_buy */ false, "OPEN", Some("0:tc")).await; // live SELL, TC known

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Positive control: arms 2 and 3 are clear, so the unsafe query is served.
    repo.list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
        .await
        .expect("gate must be open before the test introduces its own condition");

    // Introduce exactly one condition: the live SELL's TokenContract becomes unknown.
    sqlx::query("update inference_orders set token_contract = null where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Unsafe: asks about a TC among live SELLs.
    let err = repo
        .list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
        .await
        .unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    // Safe: no tokenContract filter — the unknown row is returned, visibly null.
    let page = repo.list_inference_orders(&query(ob)).await.unwrap();
    assert_eq!(page.orders.len(), 1);
    assert!(page.orders[0].token_contract.is_none());

    // Safe: makes no claim about live SELLs.
    repo.list_inference_orders(&query(ob).side(Buy).token_contract("0:tc")).await.unwrap();
    repo.list_inference_orders(&query(ob).token_contract("0:tc").status(&[Filled])).await.unwrap();

    // Once repaired, the gate opens.
    sqlx::query("update inference_orders set token_contract = '0:tc' where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let page = repo
        .list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
        .await
        .unwrap();
    assert_eq!(page.orders.len(), 1);
}

#[tokio::test]
async fn gate_refuses_while_the_book_has_captured_but_unprojected_events() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:gate-pending";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", Some("0:tc-other")).await;

    // A decoded placement waiting to be projected. It may be the SELL being asked about,
    // so no row exists yet to be found and an empty page would be a false "not in use".
    sqlx::query(
        r#"insert into raw_events (msg_id, src_address, event_type, chain_order, decoded, processed_at)
           values ('m-1', $1, 'InferenceOrderBook.InferenceOrderPlaced', 'co-1', '{}'::jsonb, null)"#,
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
        .await
        .unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    // Safe queries are unaffected: they claim nothing about live SELL usage.
    repo.list_inference_orders(&query(ob)).await.unwrap();

    // Once projected, the gate opens.
    sqlx::query("update raw_events set processed_at = now() where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    repo.list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live])).await.unwrap();
}

#[tokio::test]
async fn gate_refuses_while_any_message_for_the_book_is_unprojected() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:gate-unprojected";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", Some("0:tc-other")).await;
    let repo = PostgresReadModelRepository::new(pool.clone());

    // Two shapes, one meaning: our view of this book is incomplete. A decodable message
    // awaiting projection, and one stored undecoded — capture writes NULL event_type and
    // decoded for a failed decode, a missing body, and an id no loaded ABI claims alike.
    // For an order book whose every event IS in that ABI, the last case can only be drift.
    for (msg, etype, decoded) in [
        ("m-pending", Some("InferenceOrderBook.InferenceOrderPlaced"), Some("{}")),
        ("m-undecoded", None, None),
    ] {
        sqlx::query("delete from raw_events where src_address=$1")
            .bind(ob)
            .execute(&pool)
            .await
            .unwrap();
        // Positive control INSIDE the loop: with the row gone the gate is open, so the
        // refusal below is attributable to this iteration's row and not to a leftover.
        repo.list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
            .await
            .unwrap_or_else(|e| panic!("gate must be open before inserting {msg}: {e}"));

        sqlx::query(
            "insert into raw_events (msg_id, src_address, chain_order, event_type, decoded, processed_at) \
             values ($1, $2, 'co-9', $3, $4::jsonb, null)",
        )
        .bind(msg)
        .bind(ob)
        .bind(etype)
        .bind(decoded)
        .execute(&pool)
        .await
        .unwrap();

        let err = repo
            .list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
            .await
            .unwrap_err();
        assert!(
            matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)),
            "msg={msg}"
        );

        // Listings that assert nothing about live SELL usage stay available.
        repo.list_inference_orders(&query(ob)).await.unwrap();
    }

    // Once projected, the gate opens.
    sqlx::query("update raw_events set processed_at = now() where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let repo = PostgresReadModelRepository::new(pool.clone());
    repo.list_inference_orders(&query(ob).token_contract("0:tc-other").side(Sell).status(&[Live]))
        .await
        .unwrap();
}

#[tokio::test]
async fn gate_refuses_when_the_capture_cursor_is_stale_even_if_it_says_at_head() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    let ob = "0:gate-stale";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", Some("0:tc")).await;

    // at_head=true describes the LAST poll. An hour-old poll proves nothing about now.
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head, updated_at) \
         values ($1, 'c', true, now() - interval '1 hour') \
         on conflict (stream_name) do update set at_head = true, updated_at = excluded.updated_at",
    )
    .bind(CAPTURE_STREAM)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_orders(&query(ob).token_contract("0:tc").status(&[Live]))
        .await
        .unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    seed_at_head(&pool).await; // a fresh poll reopens it
    repo.list_inference_orders(&query(ob).token_contract("0:tc").side(Sell).status(&[Live]))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_token_contract_query_never_emits_a_buy_branch() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:tc-buy";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, /* is_buy */ false, "OPEN", Some("0:tc")).await;

    let repo = PostgresReadModelRepository::new(pool.clone());

    // side=BUY + tokenContract: impossible, answered without scanning the TC's history.
    let page =
        repo.list_inference_orders(&query(ob).token_contract("0:tc").side(Buy)).await.unwrap();
    assert!(page.orders.is_empty());
    assert!(!page.last_update_id.is_empty(), "an impossible filter still resolves the book");

    // side absent + tokenContract: only SELL branches, so the SELL row is found.
    let page = repo.list_inference_orders(&query(ob).token_contract("0:tc")).await.unwrap();
    assert_eq!(page.orders.len(), 1);
    assert_eq!(page.orders[0].order_id, "1");

    // An unknown book is still -1121 on the impossible-filter path.
    let err = repo
        .list_inference_orders(&query("0:nope").token_contract("0:tc").side(Buy))
        .await
        .unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));
}

#[tokio::test]
async fn live_excludes_cancelled_and_fully_filled_and_reports_the_order_id() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:live";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", Some("0:tc")).await;
    seed_order(&pool, ob, 2, false, "CANCELLED", Some("0:tc-cancelled")).await;
    seed_order(&pool, ob, 3, false, "FILLED", Some("0:tc-filled")).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_orders(&query(ob).token_contract("0:tc").side(Sell).status(&[Live]))
        .await
        .unwrap();
    assert_eq!(page.orders.len(), 1);
    assert_eq!(page.orders[0].order_id, "1");
    assert!(!page.last_update_id.is_empty());

    for tc in ["0:tc-cancelled", "0:tc-filled"] {
        let none = repo
            .list_inference_orders(&query(ob).token_contract(tc).side(Sell).status(&[Live]))
            .await
            .unwrap();
        assert!(none.orders.is_empty(), "tc={tc}");
    }
}

#[tokio::test]
async fn an_empty_page_still_carries_the_watermark() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:empty";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    seed_order(&pool, ob, 1, false, "OPEN", Some("0:tc-other")).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    // The acceptance case: a TokenContract that was never placed. The snapshot's LEFT JOIN
    // yields one all-NULL page row here; decoding it must not error.
    let page = repo
        .list_inference_orders(&query(ob).token_contract("0:tc-never").side(Sell).status(&[Live]))
        .await
        .unwrap();
    assert!(page.orders.is_empty());
    assert!(!page.has_more);
    assert!(page.next_cursor.is_none());
    assert!(!page.last_update_id.is_empty(), "an empty page is still dated");
}

#[tokio::test]
async fn pagination_walks_order_id_descending() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:page";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    for id in 1..=5 {
        seed_order(&pool, ob, id, false, "OPEN", Some(&format!("0:tc-{id}"))).await;
    }

    let repo = PostgresReadModelRepository::new(pool.clone());
    let first = repo.list_inference_orders(&query(ob).limit(2)).await.unwrap();
    assert_eq!(first.orders.iter().map(|o| o.order_id.as_str()).collect::<Vec<_>>(), ["5", "4"]);
    assert!(first.has_more);
    assert_eq!(first.next_cursor.as_deref(), Some("4"));

    let second = repo.list_inference_orders(&query(ob).limit(2).cursor("4")).await.unwrap();
    assert_eq!(second.orders.iter().map(|o| o.order_id.as_str()).collect::<Vec<_>>(), ["3", "2"]);
}

#[tokio::test]
async fn rows_with_null_chain_timestamps_are_still_returned() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let ob = "0:null-ts";
    purge(&pool, ob).await;
    seed_reconciled_market(&pool, ob).await;
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                is_subscription, status, last_chain_order, token_contract,
                chain_created_at, chain_updated_at)
           values ($1, 1, false, 10, 5, 5, false, 'OPEN', 'co-1', '0:tc', null, null)"#,
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo.list_inference_orders(&query(ob)).await.unwrap();
    assert_eq!(page.orders.len(), 1, "a missing chain timestamp must not hide a live SELL");
    assert!(page.orders[0].created_at.is_none());
}

#[tokio::test]
async fn unknown_book_is_invalid_market() {
    let Some(pool) = test_pool().await else { return };
    let _guard = CAPTURE_CURSOR_LOCK.lock().await;
    seed_at_head(&pool).await;
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.list_inference_orders(&query("0:nope")).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));
}
