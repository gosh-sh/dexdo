// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for IndexerRepository::reproject_pending. Gated on the
// TEST_DATABASE_URL env var: when unset, every test prints a skip notice and
// returns early. Set it to a Postgres URL the suite is allowed to migrate
// (the suite calls `database::run_migrations`). Tests use unique per-test
// prefixes for msg_ids and addresses so they can run concurrently against
// the same database without colliding.
//
// Tests serialise on REPROJECTION_LOCK because reproject_pending uses
// `for update skip locked` — a parallel test could otherwise observe a
// half-applied state (its row locked by another test's outer transaction
// that has not committed yet).
//
//   TEST_DATABASE_URL=postgres://user:pass@localhost:5432/db \
//       cargo test -p dodex-infrastructure --test reprojection

use std::env;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::OrderStatusFilter;
use dodex_application::OrdersLimit;
use dodex_application::OrdersQuery;
use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer;

static REPROJECTION_LOCK: Mutex<()> = Mutex::const_new(());

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

async fn insert_raw(
    pool: &PgPool,
    msg_id: &str,
    src: &str,
    event_type: &str,
    decoded: &serde_json::Value,
) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3), $4, $4, $5, '{}'::jsonb, $6)"#,
    )
    .bind(msg_id)
    // Per-msg deterministic key; lex-sortable so reproject ORDER BY chain_order
    // gives a stable order. Real payloads come from the GraphQL gateway's
    // `msg_chain_order` field.
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(1_700_000_000_f64)
    .bind(src)
    .bind(event_type)
    .bind(decoded)
    .execute(pool)
    .await
    .expect("insert raw_events");
}

async fn processed_at_is_set(pool: &PgPool, msg_id: &str) -> bool {
    sqlx::query_scalar("select processed_at is not null from raw_events where msg_id = $1")
        .bind(msg_id)
        .fetch_one(pool)
        .await
        .expect("read processed_at")
}

async fn insert_reconciled_market(pool: &PgPool, pmp: &str, symbol: &str, book: &str) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())
           returning id"#,
    )
    .bind(pmp)
    .bind(book)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, 1, 'YES', $3,
                   3, 2, '0.001', '0.01',
                   '1.00')"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

#[tokio::test]
async fn applied_outcome_stamps_processed_at_and_writes_read_model() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_applied_oracle";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-name");
    let msg_id = format!("{test}-msg");

    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "oracle": oracle_addr,
        "pubkey": "0x0000000000000000000000000000000000000000000000000000000000001234",
        "name": oracle_name,
    });
    insert_raw(&pool, &msg_id, &oracle_addr, "RootOracle.OracleDeployed", &decoded).await;

    repo.reproject_pending(1000).await.expect("reproject");

    assert!(processed_at_is_set(&pool, &msg_id).await, "Applied outcome must stamp processed_at");

    let oracle_exists: bool =
        sqlx::query_scalar("select exists(select 1 from oracles where address = $1)")
            .bind(&oracle_addr)
            .fetch_one(&pool)
            .await
            .expect("oracle exists");
    assert!(oracle_exists, "projector must populate oracles on Applied");
}

#[tokio::test]
async fn deferred_row_is_replayed_after_parent_arrives() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_deferred_eventlist";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let oracle_deploy_msg = format!("{test}-oracle-deploy");
    let eventlist_addr = format!("0:{test}_evlist");
    let msg_id = format!("{test}-evlist-msg");

    purge(
        &pool,
        &[
            ("delete from oracle_event_lists where address = $1", eventlist_addr.as_str()),
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "eventListAddress": eventlist_addr,
        "index": "1",
        "description": "Deferred reprojection list",
    });
    insert_raw(&pool, &msg_id, &oracle_addr, "Oracle.OracleEventListDeployed", &decoded).await;

    // Pass 1: parent OracleDeployed has not been seen → Deferred.
    repo.reproject_pending(1000).await.expect("reproject pass 1");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "deferred row must keep processed_at null until the parent appears"
    );

    let evlist_count: i64 =
        sqlx::query_scalar("select count(*) from oracle_event_lists where address = $1")
            .bind(&eventlist_addr)
            .fetch_one(&pool)
            .await
            .expect("count event lists pass 1");
    assert_eq!(evlist_count, 0, "no projection should happen while parent is missing");

    // Insert the parent oracle directly (simulating the OracleDeployed projector).
    sqlx::query(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, $4)"#,
    )
    .bind(&oracle_name)
    .bind(&oracle_addr)
    .bind(&oracle_deploy_msg)
    .bind("0xff")
    .execute(&pool)
    .await
    .expect("insert parent oracle");

    // Pass 2: parent now exists → Applied.
    repo.reproject_pending(1000).await.expect("reproject pass 2");
    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "processed_at must be stamped once the parent is present"
    );

    let evlist_count: i64 =
        sqlx::query_scalar("select count(*) from oracle_event_lists where address = $1")
            .bind(&eventlist_addr)
            .fetch_one(&pool)
            .await
            .expect("count event lists pass 2");
    assert_eq!(evlist_count, 1, "Applied outcome must populate oracle_event_lists");
}

#[tokio::test]
async fn already_processed_rows_are_not_picked_up() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_already_processed";
    let oracle_addr = format!("0:{test}_oracle");
    let msg_id = format!("{test}-msg");

    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", oracle_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let frozen_ts = "2020-01-01T00:00:00+00:00";
    let decoded = json!({
        "oracle": oracle_addr,
        "pubkey": "0x00",
        "name": format!("{test}-name"),
    });

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded, processed_at)
           values ($1, $2, to_timestamp(1700000000), $3, $3, $4, '{}'::jsonb, $5,
                   $6::timestamptz)"#,
    )
    .bind(&msg_id)
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(&oracle_addr)
    .bind("RootOracle.OracleDeployed")
    .bind(&decoded)
    .bind(frozen_ts)
    .execute(&pool)
    .await
    .expect("insert pre-processed raw_events");

    repo.reproject_pending(1000).await.expect("reproject");

    let processed_at_str: String =
        sqlx::query_scalar("select processed_at::text from raw_events where msg_id = $1")
            .bind(&msg_id)
            .fetch_one(&pool)
            .await
            .expect("read processed_at");
    assert!(
        processed_at_str.starts_with("2020-01-01"),
        "processed_at on already-processed row must not be overwritten, got {processed_at_str}"
    );

    let oracle_exists: bool =
        sqlx::query_scalar("select exists(select 1 from oracles where address = $1)")
            .bind(&oracle_addr)
            .fetch_one(&pool)
            .await
            .expect("oracle exists");
    assert!(!oracle_exists, "projector must not run for rows that already carry processed_at");
}

#[tokio::test]
async fn unknown_event_type_is_marked_processed() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_unknown_event";
    let msg_id = format!("{test}-msg");

    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id.as_str())]).await;

    // Nullifier.VoucherGenerated is decoded by the ABI but has no projector
    // wired up → projectors::project_event returns Unknown.
    insert_raw(
        &pool,
        &msg_id,
        "0:reproj_unknown_event_src",
        "Nullifier.VoucherGenerated",
        &json!({}),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "Unknown outcome must stamp processed_at to keep the row out of the retry queue"
    );
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

// Verifies the wiring the rest of the suite leaves untested: at the real
// reprojection warn! callsite, the FIRST sighting of an unhandled type emits on
// the normal target (stdout + main log) and the REPEAT emits on
// EVENT_NOISE_TARGET. The dedup boolean and the sink truth-table are each tested
// in isolation; a swapped if/else or a dropped `target:` arg would pass both yet
// flood the main log or hide the operator's first signal. current_thread flavor
// keeps the reproject future on this thread so the thread-local subscriber set
// below sees its events.
#[tokio::test(flavor = "current_thread")]
async fn unknown_event_warning_routes_first_to_normal_target_then_repeat_to_noise() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_noise_routing";
    // chain_order is derived from msg_id, so `-1` projects before `-2`: the
    // first sighting, then the repeat, in one reproject_pending pass.
    let first = format!("{test}-msg-1");
    let repeat = format!("{test}-msg-2");
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", first.as_str()),
            ("delete from raw_events where msg_id = $1", repeat.as_str()),
        ],
    )
    .await;

    // A synthetic event_type with no projector handler -> Unknown. Unique to
    // this test, so the event_type field filter below is unaffected by any
    // other pending rows the shared database may hold.
    let probe_type = "Test.NoiseRoutingProbe";
    let src = "0:reproj_noise_routing_src";
    insert_raw(&pool, &first, src, probe_type, &json!({})).await;
    insert_raw(&pool, &repeat, src, probe_type, &json!({})).await;

    let events = Arc::new(StdMutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer { events: events.clone() });
    {
        let _sub = tracing::subscriber::set_default(subscriber);
        repo.reproject_pending(1000).await.expect("reproject");
    }

    let warnings: Vec<CapturedEvent> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.event_type == probe_type && e.message.contains("no handler for event type"))
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

    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", first.as_str()),
            ("delete from raw_events where msg_id = $1", repeat.as_str()),
        ],
    )
    .await;
}

#[tokio::test]
async fn orderfilled_deferred_replays_after_orderplaced() {
    // Locks in the OrderBook deferred-replay contract: an OrderFilled that
    // arrives before its OrderPlaced must stay queued (processed_at = null),
    // and the next reprojection sweep — once the live_orders row exists —
    // must apply it. Without this, /api/v1/prediction/depth would inflate liquidity by
    // ignoring fills that landed out of order on the wire.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_deferred_orderfilled";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "42";
    let msg_id = format!("{test}-fill-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "isTaker": false,
    });
    insert_raw(&pool, &msg_id, &orderbook_addr, "OrderBook.OrderFilled", &decoded).await;

    // Pass 1: live_orders has no matching row → Deferred.
    repo.reproject_pending(1000).await.expect("reproject pass 1");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "deferred OrderFilled must keep processed_at null until the order exists"
    );

    let live_count: i64 =
        sqlx::query_scalar("select count(*) from live_orders where orderbook_address = $1")
            .bind(&orderbook_addr)
            .fetch_one(&pool)
            .await
            .expect("count live_orders pass 1");
    assert_eq!(live_count, 0, "no live_orders row should exist before OrderPlaced lands");

    // Insert the parent live_orders row directly (simulating the OrderPlaced
    // projector). amount_remaining = 100 so the deferred fill of 30 will leave
    // 70 once the replay applies.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 100::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000001', '5f800000000000000001')"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    // Pass 2: parent now exists → fill applies.
    repo.reproject_pending(1000).await.expect("reproject pass 2");
    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "processed_at must be stamped once the order exists and the fill applies"
    );

    let row: (String, String) = sqlx::query_as(
        "select amount_remaining::text, status from live_orders
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders state");
    assert_eq!(
        row.0, "70",
        "deferred OrderFilled must subtract filledAmount from amount_remaining once replayed"
    );
    assert_eq!(row.1, "OPEN", "partial fill must keep the order OPEN");
}

#[tokio::test]
async fn orderplaced_confirmed_deferred_replays_and_attaches_owner() {
    // Private account reads depend on the PN confirmation event to attach
    // ownership to the public OrderBook row. If the confirmation arrives first
    // it must defer, then replay once OrderPlaced creates the live_orders row.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_deferred_orderplaced_confirmed";
    let owner_pn = format!("0:{test}_owner");
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "77";
    let msg_id = format!("{test}-confirm-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    let decoded = json!({
        "orderBook": orderbook_addr,
        "orderId": order_id,
    });
    insert_raw(&pool, &msg_id, &owner_pn, "PrivateNote.OrderPlacedConfirmed", &decoded).await;

    repo.reproject_pending(1000).await.expect("reproject pass 1");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "deferred OrderPlacedConfirmed must keep processed_at null until the order exists"
    );

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 100::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000077', '5f800000000000000077')"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    repo.reproject_pending(1000).await.expect("reproject pass 2");
    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "processed_at must be stamped once the owner attachment applies"
    );

    let owner: Option<String> = sqlx::query_scalar(
        "select owner_pn_address from live_orders
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read owner");
    assert_eq!(owner.as_deref(), Some(owner_pn.as_str()));
}

#[tokio::test]
async fn orderplaced_sets_chain_timestamps_from_event_time() {
    // Locks in the contract that apply_order_placed reads chain time off the
    // EventNode rather than using `now()`. The API's orders.time / .updateTime
    // depend on these columns being set at projection time.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_chain_ts";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "11";
    let msg_id = format!("{test}-msg");
    let chain_seconds: i64 = 1_700_555_000;

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4,
                   'OrderBook.OrderPlaced', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id)
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(chain_seconds)
    .bind(&orderbook_addr)
    .bind(json!({
        "orderId": order_id,
        "outcomeId": "1",
        "isBuy": true,
        "price": "100",
        "amount": "50",
        "clientOrderId": "client-chain-ts",
    }))
    .execute(&pool)
    .await
    .expect("insert raw_events");

    repo.reproject_pending(1000).await.expect("reproject");

    let (chain_created_ms, chain_updated_ms): (i64, i64) = sqlx::query_as(
        r#"select (extract(epoch from chain_created_at) * 1000)::bigint,
                  (extract(epoch from chain_updated_at) * 1000)::bigint
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read chain ts");

    assert_eq!(chain_created_ms, chain_seconds * 1000);
    assert_eq!(chain_updated_ms, chain_seconds * 1000);
}

#[tokio::test]
async fn orderplaced_preserves_fractional_chain_seconds() {
    // Regression: `chain_seconds` used to be truncated to `i64` before the
    // bind, losing the millisecond component of fractional chain times the
    // gateway already round-trips. The projector now binds the full f64.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_chain_ts_fractional";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "12";
    let msg_id = format!("{test}-msg");
    let chain_seconds: f64 = 1_700_555_000.5;

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::double precision), $4, $4,
                   'OrderBook.OrderPlaced', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id)
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(chain_seconds)
    .bind(&orderbook_addr)
    .bind(json!({
        "orderId": order_id,
        "outcomeId": "1",
        "isBuy": true,
        "price": "100",
        "amount": "50",
        "clientOrderId": "client-frac",
    }))
    .execute(&pool)
    .await
    .expect("insert raw_events");

    repo.reproject_pending(1000).await.expect("reproject");

    let (chain_created_ms, chain_updated_ms): (i64, i64) = sqlx::query_as(
        r#"select (extract(epoch from chain_created_at) * 1000)::bigint,
                  (extract(epoch from chain_updated_at) * 1000)::bigint
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read chain ts");

    assert_eq!(chain_created_ms, 1_700_555_000_500);
    assert_eq!(chain_updated_ms, 1_700_555_000_500);
}

#[tokio::test]
async fn orderplaced_chain_created_at_is_first_write_wins() {
    // Regression: the ON CONFLICT clause used `least(...)` which lets a
    // replay carrying an earlier chain time pull `chain_created_at`
    // backward, violating the moment-of-birth invariant the cursor and
    // API contract assume. The projector now uses `coalesce(live, new)`
    // so the first write sticks.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_first_write_wins";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "33";
    let msg_id_first = format!("{test}-amsg");
    let msg_id_replay = format!("{test}-zmsg");
    let original_seconds: i64 = 1_700_000_500;
    let earlier_seconds: i64 = 1_700_000_100;

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_first.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_replay.as_str()),
        ],
    )
    .await;

    // First event: legitimate OrderPlaced establishes chain_created_at.
    insert_raw_with_ts(
        &pool,
        &msg_id_first,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        original_seconds,
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": "c",
        }),
    )
    .await;

    // Replay with an EARLIER chain time. `least(...)` would have moved
    // chain_created_at to `earlier_seconds`; `coalesce` keeps the original.
    insert_raw_with_ts(
        &pool,
        &msg_id_replay,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        earlier_seconds,
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": "c",
        }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    let chain_created_ms: i64 = sqlx::query_scalar(
        r#"select (extract(epoch from chain_created_at) * 1000)::bigint
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read chain_created_at");

    assert_eq!(
        chain_created_ms,
        original_seconds * 1000,
        "chain_created_at must stay pinned to the first write; replays cannot move it backward"
    );
}

#[tokio::test]
async fn orderplaced_placed_chain_order_is_first_write_wins() {
    // A replayed OrderPlaced carrying a different msg_chain_order must
    // NOT overwrite placed_chain_order. The cursor for /orders is
    // built from placed_chain_order, and a moving value would let a
    // paginated reader re-see an already-returned row.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_placed_chain_order_coalesce";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "44";
    let msg_id_first = format!("{test}-amsg");
    let msg_id_replay = format!("{test}-zmsg");
    let first_seconds: i64 = 1_700_000_500;
    let replay_seconds: i64 = 1_700_000_100;
    // insert_raw_with_ts builds chain_order as `5f80{msg_id:0>28}`. With
    // msg_id_first lex-smaller than msg_id_replay, the first event is
    // applied first by reproject_pending.
    let expected_placed_chain_order = format!("5f80{msg_id_first:0>28}");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_first.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_replay.as_str()),
        ],
    )
    .await;

    insert_raw_with_ts(
        &pool,
        &msg_id_first,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        first_seconds,
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": "c",
        }),
    )
    .await;
    insert_raw_with_ts(
        &pool,
        &msg_id_replay,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        replay_seconds,
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": "c",
        }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    let placed: String = sqlx::query_scalar(
        r#"select placed_chain_order
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read placed_chain_order");

    assert_eq!(
        placed, expected_placed_chain_order,
        "placed_chain_order must be the chain_order of the FIRST applied OrderPlaced; \
         replays cannot overwrite it"
    );
}

async fn insert_raw_with_ts(
    pool: &PgPool,
    msg_id: &str,
    src: &str,
    event_type: &str,
    chain_seconds: i64,
    decoded: &serde_json::Value,
) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4, $5, '{}'::jsonb, $6)"#,
    )
    .bind(msg_id)
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(chain_seconds)
    .bind(src)
    .bind(event_type)
    .bind(decoded)
    .execute(pool)
    .await
    .expect("insert raw_events with chain ts");
}

#[tokio::test]
async fn orderfilled_advances_chain_updated_at() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderfilled_chain_updated";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "21";
    let msg_id_place = format!("{test}-aplace-msg");
    let msg_id_fill = format!("{test}-bfill-msg");
    let place_seconds: i64 = 1_700_000_500;
    let fill_seconds: i64 = 1_700_000_900;

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
        ],
    )
    .await;

    // Seed an OrderPlaced raw_event so apply_order_placed creates the row.
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4,
                   'OrderBook.OrderPlaced', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id_place)
    .bind(format!("5f80{msg_id_place:0>28}"))
    .bind(place_seconds)
    .bind(&orderbook_addr)
    .bind(json!({
        "orderId": order_id, "outcomeId": "1", "isBuy": true,
        "price": "100", "amount": "100", "clientOrderId": "c",
    }))
    .execute(&pool)
    .await
    .expect("insert place");

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4,
                   'OrderBook.OrderFilled', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id_fill)
    .bind(format!("5f80{msg_id_fill:0>28}"))
    .bind(fill_seconds)
    .bind(&orderbook_addr)
    .bind(json!({ "orderId": order_id, "filledAmount": "10", "isTaker": false }))
    .execute(&pool)
    .await
    .expect("insert fill");

    repo.reproject_pending(1000).await.expect("reproject");

    let (chain_created_ms, chain_updated_ms): (i64, i64) = sqlx::query_as(
        r#"select (extract(epoch from chain_created_at) * 1000)::bigint,
                  (extract(epoch from chain_updated_at) * 1000)::bigint
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read ts");

    assert_eq!(chain_created_ms, place_seconds * 1000);
    assert_eq!(chain_updated_ms, fill_seconds * 1000);
}

#[tokio::test]
async fn ordercancelled_advances_chain_updated_at() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_ordercancelled_chain_updated";
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "22";
    let msg_id_place = format!("{test}-aplace-msg");
    let msg_id_cancel = format!("{test}-bcancel-msg");
    let place_seconds: i64 = 1_700_000_500;
    let cancel_seconds: i64 = 1_700_000_800;

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
        ],
    )
    .await;

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4,
                   'OrderBook.OrderPlaced', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id_place)
    .bind(format!("5f80{msg_id_place:0>28}"))
    .bind(place_seconds)
    .bind(&orderbook_addr)
    .bind(json!({
        "orderId": order_id, "outcomeId": "1", "isBuy": true,
        "price": "100", "amount": "100", "clientOrderId": "c",
    }))
    .execute(&pool)
    .await
    .expect("insert place");

    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp($3::bigint), $4, $4,
                   'OrderBook.OrderCancelled', '{}'::jsonb, $5)"#,
    )
    .bind(&msg_id_cancel)
    .bind(format!("5f80{msg_id_cancel:0>28}"))
    .bind(cancel_seconds)
    .bind(&orderbook_addr)
    .bind(json!({ "orderId": order_id }))
    .execute(&pool)
    .await
    .expect("insert cancel");

    repo.reproject_pending(1000).await.expect("reproject");

    let chain_updated_ms: i64 = sqlx::query_scalar(
        r#"select (extract(epoch from chain_updated_at) * 1000)::bigint
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read ts");

    assert_eq!(chain_updated_ms, cancel_seconds * 1000);
}

#[tokio::test]
async fn orderplaced_fill_cancel_pipeline_reports_partial_executed_qty() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());
    let read_model = PostgresReadModelRepository::new(pool.clone());

    let test = "reproj_partial_cancel_orders";
    let orderbook_addr = format!("0:{test}_book");
    let pmp = format!("0:{test}_pmp");
    let symbol = format!("{test}_YES");
    let owner_pn = format!("0:{test}_owner");
    let order_id = "23";
    let msg_id_place = format!("{test}-a-place-msg");
    let msg_id_fill = format!("{test}-b-fill-msg");
    let msg_id_cancel = format!("{test}-c-cancel-msg");
    let msg_id_confirm = format!("{test}-d-confirm-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_confirm.as_str()),
            ("delete from market_outcomes where symbol = $1", symbol.as_str()),
            ("delete from markets where pmp_address = $1", pmp.as_str()),
        ],
    )
    .await;

    insert_reconciled_market(&pool, &pmp, &symbol, &orderbook_addr).await;
    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        &json!({
            "orderId": order_id,
            "outcomeId": "1",
            "isBuy": true,
            "price": "100",
            "amount": "10000000",
            "clientOrderId": "partial-cancel",
        }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        &json!({ "orderId": order_id, "filledAmount": "3000000", "isTaker": false }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_cancel,
        &orderbook_addr,
        "OrderBook.OrderCancelled",
        &json!({ "orderId": order_id }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_confirm,
        &owner_pn,
        "PrivateNote.OrderPlacedConfirmed",
        &json!({ "orderBook": orderbook_addr, "orderId": order_id }),
    )
    .await;

    indexer.reproject_pending(1000).await.expect("reproject");

    let page = read_model
        .list_orders(&OrdersQuery {
            owner_pn_address: owner_pn,
            market: None,
            status: OrderStatusFilter::All,
            limit: OrdersLimit::from_const(100),
            cursor: None,
        })
        .await
        .expect("list_orders");

    assert_eq!(page.orders.len(), 1);
    let order = &page.orders[0];
    assert_eq!(order.status().as_str(), "CANCELED");
    assert_eq!(order.orig_qty(), "10.00");
    assert_eq!(
        order.executed_qty(),
        "3.00",
        "executedQty must reflect the fill before cancellation, not the canceled remainder"
    );
    assert!(page.next_cursor.is_none());
}

/// Pins the "filled wins" race semantics promised by
/// docs/tech-specs/write-api.md §Response. The chain contract should
/// keep this case off the wire (`_doCancel` returns without emitting
/// `OrderCancelled` once the order is gone from the book), but the
/// projector must also fail closed: if both events do reach the
/// indexer, the terminal status surfaced to clients is `FILLED`, not
/// `CANCELED`.
#[tokio::test]
async fn orderplaced_full_fill_then_cancel_keeps_filled_status() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());
    let read_model = PostgresReadModelRepository::new(pool.clone());

    let test = "reproj_full_fill_then_cancel";
    let orderbook_addr = format!("0:{test}_book");
    let pmp = format!("0:{test}_pmp");
    let symbol = format!("{test}_YES");
    let owner_pn = format!("0:{test}_owner");
    let order_id = "24";
    let msg_id_place = format!("{test}-a-place-msg");
    let msg_id_fill = format!("{test}-b-fill-msg");
    let msg_id_cancel = format!("{test}-c-cancel-msg");
    let msg_id_confirm = format!("{test}-d-confirm-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_confirm.as_str()),
            ("delete from market_outcomes where symbol = $1", symbol.as_str()),
            ("delete from markets where pmp_address = $1", pmp.as_str()),
        ],
    )
    .await;

    insert_reconciled_market(&pool, &pmp, &symbol, &orderbook_addr).await;
    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        &json!({
            "orderId": order_id,
            "outcomeId": "1",
            "isBuy": true,
            "price": "100",
            "amount": "10000000",
            "clientOrderId": "full-fill-then-cancel",
        }),
    )
    .await;
    // Full fill — amount_remaining hits 0 and status flips to FILLED.
    insert_raw(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        &json!({ "orderId": order_id, "filledAmount": "10000000", "isTaker": false }),
    )
    .await;
    // OrderCancelled arrives after the full fill — must NOT regress
    // status to CANCELLED.
    insert_raw(
        &pool,
        &msg_id_cancel,
        &orderbook_addr,
        "OrderBook.OrderCancelled",
        &json!({ "orderId": order_id }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_confirm,
        &owner_pn,
        "PrivateNote.OrderPlacedConfirmed",
        &json!({ "orderBook": orderbook_addr, "orderId": order_id }),
    )
    .await;

    indexer.reproject_pending(1000).await.expect("reproject");

    let page = read_model
        .list_orders(&OrdersQuery {
            owner_pn_address: owner_pn,
            market: None,
            status: OrderStatusFilter::All,
            limit: OrdersLimit::from_const(100),
            cursor: None,
        })
        .await
        .expect("list_orders");

    assert_eq!(page.orders.len(), 1);
    let order = &page.orders[0];
    assert_eq!(
        order.status().as_str(),
        "FILLED",
        "filled wins: a cancel arriving after a full fill must not demote the row to CANCELED"
    );
    assert_eq!(order.orig_qty(), "10.00");
    assert_eq!(order.executed_qty(), "10.00", "fully filled");
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn orderplaced_cancel_then_fill_keeps_canceled_status_and_remainder() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());
    let read_model = PostgresReadModelRepository::new(pool.clone());

    let test = "reproj_orderplaced_cancel_then_fill";
    let pmp = format!("0:{test}_pmp");
    let symbol = format!("{test}_YES");
    let orderbook_addr = format!("0:{test}_book");
    let owner_pn = format!("0:{test}_owner");
    let order_id = "51";
    let msg_id_place = format!("{test}-aplace-msg");
    let msg_id_cancel = format!("{test}-bcancel-msg");
    let msg_id_fill = format!("{test}-cfill-msg");
    let msg_id_confirm = format!("{test}-dconfirm-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_confirm.as_str()),
            ("delete from market_outcomes where symbol = $1", symbol.as_str()),
            ("delete from markets where pmp_address = $1", pmp.as_str()),
        ],
    )
    .await;

    insert_reconciled_market(&pool, &pmp, &symbol, &orderbook_addr).await;
    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        &json!({
            "orderId": order_id,
            "outcomeId": "1",
            "isBuy": true,
            "price": "100",
            "amount": "10000000",
            "clientOrderId": "cancel-then-fill",
        }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_cancel,
        &orderbook_addr,
        "OrderBook.OrderCancelled",
        &json!({ "orderId": order_id }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        &json!({ "orderId": order_id, "filledAmount": "10000000", "isTaker": false }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_id_confirm,
        &owner_pn,
        "PrivateNote.OrderPlacedConfirmed",
        &json!({ "orderBook": orderbook_addr, "orderId": order_id }),
    )
    .await;

    indexer.reproject_pending(1000).await.expect("reproject");

    let page = read_model
        .list_orders(&OrdersQuery {
            owner_pn_address: owner_pn,
            market: None,
            status: OrderStatusFilter::All,
            limit: OrdersLimit::from_const(100),
            cursor: None,
        })
        .await
        .expect("list_orders");

    assert_eq!(page.orders.len(), 1);
    let order = &page.orders[0];
    assert_eq!(order.status().as_str(), "CANCELED");
    assert_eq!(order.orig_qty(), "10.00");
    assert_eq!(
        order.executed_qty(),
        "0.00",
        "a stale fill after cancellation must not erase the canceled remainder"
    );
    assert!(page.next_cursor.is_none());

    // executed_qty is `greatest(amount_initial - amount_remaining, 0)` in
    // SQL, so a co-mutation of `amount_initial` (kept = 10_000_000 atoms) and
    // `amount_remaining` (zeroed by a stale fill) would still render
    // executed_qty = 0. Assert the stored `amount_remaining` directly
    // to pin the storage truth (raw chain atoms, not display units).
    let amount_remaining: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read amount_remaining");
    assert_eq!(
        amount_remaining, "10000000",
        "OrderCancelled before OrderFilled preserves the unfilled remainder",
    );
}

#[tokio::test]
async fn orderfilled_on_filled_row_preserves_remainder() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());

    let test = "reproj_fill_filled_guard";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_fill = format!("{test}-fill-msg");
    let order_id = "77";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
        ],
    )
    .await;

    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 1000::numeric,
                   300::numeric, 55::numeric, '0:filled-owner', 'FILLED',
                   '5f80000000000000000000000000000001',
                   '5f80000000000000000000000000000001',
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert filled row");

    // Event timestamp strictly later than the seeded chain_updated_at
    // (1_700_000_000s) so the unconditional `greatest(...)` form would
    // move chain_updated_at forward. The CASE in apply_order_filled must
    // hold it at the seed value because the row is already FILLED.
    insert_raw_with_ts(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        1_700_001_000,
        &json!({ "orderId": order_id, "filledAmount": "100", "isTaker": false }),
    )
    .await;

    indexer.reproject_pending(1000).await.expect("reproject");

    let (status, amount_remaining, chain_updated_ms, last_chain_order): (
        String,
        String,
        i64,
        String,
    ) = sqlx::query_as(
        r#"select status,
                      amount_remaining::text,
                      (extract(epoch from chain_updated_at) * 1000)::bigint,
                      last_chain_order
                 from live_orders where orderbook_address = $1"#,
    )
    .bind(&orderbook_addr)
    .fetch_one(&pool)
    .await
    .expect("read row");

    assert_eq!(status, "FILLED");
    assert_eq!(amount_remaining, "300", "stale fill must not mutate terminal remainder");
    assert_eq!(
        chain_updated_ms, 1_700_000_000_000,
        "stale fill on terminal row must not move public updateTime",
    );
    // last_chain_order is gated by the same terminal CASE — a stale
    // fill must not move /depth lastUpdateId for the pair either.
    assert_eq!(
        last_chain_order, "5f80000000000000000000000000000001",
        "stale fill on terminal row must not advance last_chain_order",
    );
}

#[tokio::test]
async fn ordercancelled_does_not_rewrite_rejected_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());

    let test = "reproj_cancel_rejected_guard";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_cancel = format!("{test}-cancel-msg");
    let order_id = "0";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
        ],
    )
    .await;

    sqlx::query("alter table live_orders drop constraint if exists live_orders_status_check")
        .execute(&pool)
        .await
        .expect("drop status check for future-status fixture");
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 1000::numeric,
                   0::numeric, 55::numeric, '0:rejected-owner', 'REJECTED',
                   '5f80000000000000000000000000000001',
                   '5f80000000000000000000000000000001',
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert rejected row");

    // Event timestamp strictly later than the seeded chain_updated_at so
    // a regression to an unconditional `greatest(...)` on chain_updated_at
    // would move the public updateTime; the CASE must keep it pinned to
    // the seed value because the row is REJECTED.
    insert_raw_with_ts(
        &pool,
        &msg_id_cancel,
        &orderbook_addr,
        "OrderBook.OrderCancelled",
        1_700_001_000,
        &json!({ "orderId": order_id }),
    )
    .await;

    let result = indexer.reproject_pending(1000).await;

    // Capture the read result without panicking — the CHECK constraint must
    // be restored before any assertion fires, or a failure here would leak
    // a constraint-less table into every subsequent test in the suite.
    let row_result: Result<(String, i64, String), sqlx::Error> = sqlx::query_as(
        r#"select status,
                  (extract(epoch from chain_updated_at) * 1000)::bigint,
                  last_chain_order
             from live_orders where orderbook_address = $1"#,
    )
    .bind(&orderbook_addr)
    .fetch_one(&pool)
    .await;

    sqlx::query(
        "update live_orders set status = 'OPEN' where orderbook_address = $1 and status = 'REJECTED'",
    )
    .bind(&orderbook_addr)
    .execute(&pool)
    .await
    .expect("restore row status before constraint");
    sqlx::query(
        r#"alter table live_orders
             add constraint live_orders_status_check
             check (status in ('OPEN', 'FILLED', 'CANCELLED'))"#,
    )
    .execute(&pool)
    .await
    .expect("restore status check");

    let (status, chain_updated_ms, last_chain_order) =
        row_result.expect("read status + chain_updated_at + last_chain_order");
    result.expect("reproject");
    assert_eq!(status, "REJECTED", "cancel projector must preserve future rejected rows");
    assert_eq!(
        chain_updated_ms, 1_700_000_000_000,
        "stale cancel on terminal row must not move public updateTime",
    );
    assert_eq!(
        last_chain_order, "5f80000000000000000000000000000001",
        "stale cancel on terminal row must not advance last_chain_order",
    );
}

/// A duplicate `OrderCancelled` arriving on an already-`CANCELLED`
/// row must be a true no-op — status was already correct, and the
/// row's `last_chain_order` / `chain_updated_at` must NOT advance via
/// `greatest()`, since that would move `/depth lastUpdateId` and
/// `/orders updateTime` for an event the public state ignores. The
/// existing `apply_order_filled` terminal-row guard already gates on
/// `('FILLED', 'CANCELLED', 'REJECTED')`; this test pins the symmetric
/// behaviour for `apply_order_cancelled`.
#[tokio::test]
async fn ordercancelled_is_noop_on_already_canceled_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());

    let test = "reproj_cancel_canceled_guard";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_cancel = format!("{test}-cancel-msg");
    let order_id = "80";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_cancel.as_str()),
        ],
    )
    .await;

    // Seed a CANCELLED row with a known last_chain_order and
    // chain_updated_at. The duplicate cancel event below carries a
    // strictly-later chain timestamp and chain_order; the four
    // mutation columns must all be held.
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 1000::numeric,
                   400::numeric, 9::numeric, '0:canceled-owner', 'CANCELLED',
                   '5f80000000000000000000000000000080',
                   '5f80000000000000000000000000000080',
                   to_timestamp(1700000000), to_timestamp(1700000500))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("seed CANCELLED row");

    // Strictly-later raw event timestamp + chain_order. Without the
    // 'CANCELLED' added to the terminal set, both last_chain_order
    // and chain_updated_at would advance via greatest().
    insert_raw_with_ts(
        &pool,
        &msg_id_cancel,
        &orderbook_addr,
        "OrderBook.OrderCancelled",
        1_700_001_000,
        &json!({ "orderId": order_id }),
    )
    .await;

    indexer.reproject_pending(1000).await.expect("reproject");

    let (status, amount_remaining, chain_updated_ms, last_chain_order): (
        String,
        String,
        i64,
        String,
    ) = sqlx::query_as(
        r#"select status,
                      amount_remaining::text,
                      (extract(epoch from chain_updated_at) * 1000)::bigint,
                      last_chain_order
                 from live_orders where orderbook_address = $1"#,
    )
    .bind(&orderbook_addr)
    .fetch_one(&pool)
    .await
    .expect("read row");

    assert_eq!(status, "CANCELLED");
    assert_eq!(amount_remaining, "400", "amount_remaining preserved");
    assert_eq!(
        chain_updated_ms, 1_700_000_500_000,
        "duplicate cancel on CANCELLED row must not move public updateTime",
    );
    assert_eq!(
        last_chain_order, "5f80000000000000000000000000000080",
        "duplicate cancel on CANCELLED row must not advance last_chain_order",
    );
}

#[tokio::test]
async fn oracle_event_list_deployed_persists_description() {
    let Some(pool) = setup().await else { return };

    let oracle_addr = "0:oracles_desc_test_oracle";
    let oel_addr = "0:oracles_desc_test_evlist";
    let msg_id = "oracles_desc_test_msg";

    // Clean slate.
    sqlx::query("delete from oracle_event_lists where address = $1")
        .bind(oel_addr)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_addr)
        .execute(&pool)
        .await
        .unwrap();

    // Parent oracle must exist or the projector defers.
    sqlx::query(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ('oracles-desc-test', $1, 'oracles-desc-deploy', '0xff')"#,
    )
    .bind(oracle_addr)
    .execute(&pool)
    .await
    .unwrap();

    let event = dodex_infrastructure::decoder::DecodedEvent {
        contract_kind: "Oracle",
        event_name: "OracleEventListDeployed".to_string(),
        event_type: "Oracle.OracleEventListDeployed".to_string(),
        value: serde_json::json!({
            "eventListAddress": oel_addr,
            "index": "0",
            "description": "Election markets verified by ElectionOracle."
        }),
    };
    let node = dodex_infrastructure::graphql::EventNode {
        msg_id: msg_id.to_string(),
        src: Some(oracle_addr.to_string()),
        msg_chain_order: None,
        src_dapp_id: None,
        dst: None,
        body: None,
        created_at: None,
    };

    let mut tx = pool.begin().await.unwrap();
    let outcome = dodex_infrastructure::projectors::project_event(&mut tx, &event, &node)
        .await
        .expect("project");
    tx.commit().await.unwrap();
    assert_eq!(outcome, dodex_infrastructure::projectors::ProjectionOutcome::Applied);

    let desc: Option<String> =
        sqlx::query_scalar("select description from oracle_event_lists where address = $1")
            .bind(oel_addr)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(desc.as_deref(), Some("Election markets verified by ElectionOracle."));
}

/// An oracle may deploy the same event list twice — first bare, then again once
/// a description is set. Each emission carries its own msg_id, so the upsert has
/// to reconcile on `address`; keying it on msg_id turns the second emission into
/// a plain insert that trips `oracle_event_lists_address_key` and pins the raw
/// event pending forever.
#[tokio::test]
async fn oracle_event_list_redeployed_updates_row_instead_of_colliding() {
    let Some(pool) = setup().await else { return };

    let oracle_addr = "0:oel_redeploy_test_oracle";
    let oel_addr = "0:oel_redeploy_test_evlist";

    // Clean slate.
    sqlx::query("delete from oracle_event_lists where address = $1")
        .bind(oel_addr)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_addr)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ('oel-redeploy-test', $1, 'oel-redeploy-deploy', '0xff')"#,
    )
    .bind(oracle_addr)
    .execute(&pool)
    .await
    .unwrap();

    let deploy = |msg_id: &str, description: &str| {
        let event = dodex_infrastructure::decoder::DecodedEvent {
            contract_kind: "Oracle",
            event_name: "OracleEventListDeployed".to_string(),
            event_type: "Oracle.OracleEventListDeployed".to_string(),
            value: serde_json::json!({
                "eventListAddress": oel_addr,
                "index": "0",
                "description": description,
            }),
        };
        let node = dodex_infrastructure::graphql::EventNode {
            msg_id: msg_id.to_string(),
            src: Some(oracle_addr.to_string()),
            msg_chain_order: None,
            src_dapp_id: None,
            dst: None,
            body: None,
            created_at: None,
        };
        (event, node)
    };

    let project = |msg_id: &'static str, description: &'static str| {
        let pool = pool.clone();
        async move {
            let (event, node) = deploy(msg_id, description);
            let mut tx = pool.begin().await.unwrap();
            let outcome = dodex_infrastructure::projectors::project_event(&mut tx, &event, &node)
                .await
                .unwrap_or_else(|err| panic!("project {msg_id}: {err:?}"));
            tx.commit().await.unwrap();
            outcome
        }
    };

    // Bare deploy, then a re-deploy of the same list carrying the description.
    assert_eq!(
        project("oel-redeploy-msg-bare", "").await,
        dodex_infrastructure::projectors::ProjectionOutcome::Applied,
    );
    assert_eq!(
        project("oel-redeploy-msg-named", "Football").await,
        dodex_infrastructure::projectors::ProjectionOutcome::Applied,
        "a second deploy of the same list address must upsert, not collide",
    );

    // A later bare re-emission must not erase the description already on record.
    assert_eq!(
        project("oel-redeploy-msg-bare-again", "").await,
        dodex_infrastructure::projectors::ProjectionOutcome::Applied,
    );

    let (rows, description, msg_id): (i64, String, String) = sqlx::query_as(
        r#"select count(*) over (), description, msg_id
             from oracle_event_lists where address = $1"#,
    )
    .bind(oel_addr)
    .fetch_one(&pool)
    .await
    .expect("exactly one event list row for the address");

    assert_eq!(rows, 1, "re-deploys must reconcile onto the single row keyed by address");
    assert_eq!(description, "Football", "the description-bearing deploy wins over a bare one");
    assert_eq!(
        msg_id, "oel-redeploy-msg-bare",
        "msg_id stays the message that first created the row",
    );
}

#[tokio::test]
async fn orderfilled_does_not_rewrite_rejected_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let indexer = IndexerRepository::new(pool.clone());

    let test = "reproj_fill_rejected_guard";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_fill = format!("{test}-fill-msg");
    let order_id = "0";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
        ],
    )
    .await;

    sqlx::query("alter table live_orders drop constraint if exists live_orders_status_check")
        .execute(&pool)
        .await
        .expect("drop status check for future-status fixture");
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 1000::numeric,
                   0::numeric, 55::numeric, '0:rejected-owner', 'REJECTED',
                   '5f80000000000000000000000000000001',
                   '5f80000000000000000000000000000001',
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert rejected row");

    insert_raw(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        &json!({ "orderId": order_id, "filledAmount": "100", "isTaker": false }),
    )
    .await;

    let result = indexer.reproject_pending(1000).await;

    let status_result: Result<String, sqlx::Error> =
        sqlx::query_scalar("select status from live_orders where orderbook_address = $1")
            .bind(&orderbook_addr)
            .fetch_one(&pool)
            .await;

    sqlx::query(
        "update live_orders set status = 'OPEN' where orderbook_address = $1 and status = 'REJECTED'",
    )
    .bind(&orderbook_addr)
    .execute(&pool)
    .await
    .expect("restore row status before constraint");
    sqlx::query(
        r#"alter table live_orders
             add constraint live_orders_status_check
             check (status in ('OPEN', 'FILLED', 'CANCELLED'))"#,
    )
    .execute(&pool)
    .await
    .expect("restore status check");

    let status = status_result.expect("read status");
    result.expect("reproject");
    assert_eq!(status, "REJECTED", "fill projector must preserve future rejected rows");
}

#[tokio::test]
async fn orderplaced_confirmed_is_idempotent_when_already_attributed() {
    // The PN-confirm projector must not overwrite an existing owner_pn_address
    // (defence against a second confirmation event with a different src landing
    // on the same orderbook+orderId after a reprojection sweep).
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_confirmed_idempotent";
    let original_owner = format!("0:{test}_owner");
    let other_owner = format!("0:{test}_other");
    let orderbook_addr = format!("0:{test}_book");
    let order_id = "88";
    let msg_id = format!("{test}-confirm-msg");

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id.as_str()),
        ],
    )
    .await;

    // Pre-create the row with the original owner already attached.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                status, last_chain_order, placed_chain_order,
                chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1, true, 100::numeric,
                   100::numeric, 100::numeric, $3,
                   'OPEN', '5f800000000000000088', '5f800000000000000088',
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .bind(&original_owner)
    .execute(&pool)
    .await
    .expect("insert pre-attributed row");

    // The PN confirmation event claims a different owner.
    insert_raw(
        &pool,
        &msg_id,
        &other_owner,
        "PrivateNote.OrderPlacedConfirmed",
        &json!({ "orderBook": orderbook_addr, "orderId": order_id }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "Applied (no-op) outcome must stamp processed_at"
    );

    let owner: String = sqlx::query_scalar(
        "select owner_pn_address from live_orders
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read owner");
    assert_eq!(owner, original_owner, "owner must not change once attributed");
}

#[tokio::test]
async fn orderplaced_replay_refused_on_terminal_row() {
    // Pins the ON CONFLICT WHERE guard at apply_order_placed: a stale
    // OrderPlaced replay landing on a row that is already FILLED /
    // CANCELLED / REJECTED is treated as a no-op so an isolated
    // partial replay cannot demote the public status back to OPEN.
    // The wipe-and-reproject path documented in
    // docs/migrations/orders-cancel-remainder-cutover.md is unaffected
    // because step 2 (`delete`) ensures the row does not yet exist when
    // OrderPlaced lands.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_replay_refused_on_terminal";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_place = format!("{test}-place-msg");
    let order_id = "77";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
        ],
    )
    .await;

    // Seed a terminal row directly. amount_remaining=0 and status=FILLED
    // mimic a fully-filled order whose lifecycle was previously projected.
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 50::numeric,
                   0::numeric, 9::numeric, '0:terminal-owner', 'FILLED',
                   '5f80000000000000000000000000000077',
                   '5f80000000000000000000000000000077',
                   to_timestamp(1700000000), to_timestamp(1700000500))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("seed FILLED row");

    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": "9",
        }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    let (status, amount_remaining): (String, String) = sqlx::query_as(
        r#"select status, amount_remaining::text
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read terminal row");

    assert_eq!(status, "FILLED", "WHERE-guarded ON CONFLICT preserves terminal status");
    assert_eq!(
        amount_remaining, "0",
        "WHERE-guarded ON CONFLICT preserves terminal amount_remaining"
    );
}

/// A sentinel-shape row (`status='OPEN'` with `amount_remaining=0`,
/// produced by an operator edit or a legacy projector) must not be
/// auto-healed to FILLED by an incoming `OrderFilled`. The status CASE
/// in `apply_order_filled` short-circuits when `lo.amount_remaining =
/// 0`, so the row stays OPEN+0 — invisible to /orders via the
/// `fully_filled` guard in `order_from_row` — and a non-zero
/// `filled_amount` cannot fabricate a full fill that the chain never
/// emitted.
#[tokio::test]
async fn orderfilled_does_not_auto_heal_sentinel_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderfilled_sentinel_row";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_fill = format!("{test}-fill-msg");
    let order_id = "79";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_fill.as_str()),
        ],
    )
    .await;

    // Sentinel row: status='OPEN' AND amount_remaining=0. Pre-fix, any
    // positive filled_amount would satisfy `0 - x <= 0` in the status
    // CASE and flip status to FILLED, surfacing the row as a fake
    // fully-filled order with executed_qty = amount_initial.
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 100::numeric,
                   0::numeric, 9::numeric, '0:sentinel-owner', 'OPEN',
                   '5f80000000000000000000000000000079',
                   '5f80000000000000000000000000000079',
                   to_timestamp(1700000000), to_timestamp(1700000500))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("seed sentinel row");

    insert_raw(
        &pool,
        &msg_id_fill,
        &orderbook_addr,
        "OrderBook.OrderFilled",
        &json!({ "orderId": order_id, "filledAmount": "60", "isTaker": false }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    let (status, amount_remaining): (String, String) = sqlx::query_as(
        r#"select status, amount_remaining::text
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read sentinel row");

    assert_eq!(status, "OPEN", "sentinel row must not auto-heal to FILLED");
    assert_eq!(amount_remaining, "0", "sentinel row's amount_remaining stays clamped at 0",);
}

/// The ON CONFLICT WHERE guard also refuses to overwrite a
/// partially-filled OPEN row. Without the `amount_remaining =
/// amount_initial` conjunct, replaying `OrderPlaced` alone against a
/// partial-fill row would silently reset `amount_remaining` back to
/// `amount_initial` and erase the fill history.
#[tokio::test]
async fn orderplaced_replay_refused_on_partial_fill_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_replay_refused_on_partial_fill";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_place = format!("{test}-place-msg");
    let order_id = "78";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
        ],
    )
    .await;

    // Seed a partial-fill OPEN row directly: amount_initial=100,
    // amount_remaining=40 (i.e., 60 already filled). status stays OPEN
    // because the order is not yet fully filled.
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 100::numeric,
                   40::numeric, 9::numeric, '0:partial-fill-owner', 'OPEN',
                   '5f80000000000000000000000000000078',
                   '5f80000000000000000000000000000078',
                   to_timestamp(1700000000), to_timestamp(1700000500))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("seed partial-fill row");

    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "100", "clientOrderId": "9",
        }),
    )
    .await;

    repo.reproject_pending(1000).await.expect("reproject");

    let (status, amount_remaining): (String, String) = sqlx::query_as(
        r#"select status, amount_remaining::text
             from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read partial-fill row");

    assert_eq!(status, "OPEN", "partial-fill row stays OPEN");
    assert_eq!(
        amount_remaining, "40",
        "ON CONFLICT must not overwrite amount_remaining on a partial-fill row",
    );
}

/// `apply_order_placed_confirmed` must refuse a confirmation event
/// whose `src` is empty. Without the explicit empty-string filter,
/// `as_deref().context(...)` would only catch `None`, an empty `src`
/// would bind `owner_pn_address = ""` into `live_orders`, and the row
/// would be unreachable from any `/orders` query
/// (`owner_pn_address = $caller_pn` never matches) or
/// `resolve_for_cancel` (same predicate). The reproject loop reports
/// the event as failed and the existing OrderBook row keeps its
/// NULL owner — recoverable by a well-formed replay.
#[tokio::test]
async fn orderplaced_confirmed_rejects_empty_src() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_confirmed_empty_src";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_confirm = format!("{test}-confirm-msg");
    let order_id = "81";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_confirm.as_str()),
        ],
    )
    .await;

    // Seed an OrderBook-produced row with NULL owner (typical state
    // before the PrivateNote confirmation arrives).
    sqlx::query(
        r#"insert into live_orders
             (orderbook_address, order_id, outcome_id, is_buy, price, amount_initial,
              amount_remaining, client_order_id, owner_pn_address, status,
              placed_chain_order, last_chain_order, chain_created_at, chain_updated_at)
           values ($1, $2::numeric, 1::numeric, true, 100::numeric, 1000::numeric,
                   1000::numeric, 9::numeric, NULL, 'OPEN',
                   '5f80000000000000000000000000000081',
                   '5f80000000000000000000000000000081',
                   to_timestamp(1700000000), to_timestamp(1700000000))"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("seed unattributed OrderBook row");

    // Raw event with empty src_address — the bug path.
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000500), '', '',
                   'PrivateNote.OrderPlacedConfirmed', '{}'::jsonb, $3)"#,
    )
    .bind(&msg_id_confirm)
    .bind(format!("5f80{msg_id_confirm:0>28}"))
    .bind(json!({ "orderBook": orderbook_addr, "orderId": order_id }))
    .execute(&pool)
    .await
    .expect("insert raw event with empty src");

    let stats = repo.reproject_pending(1000).await.expect("reproject");

    let owner: Option<String> = sqlx::query_scalar(
        r#"select owner_pn_address from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read row owner");

    // Purge BEFORE asserts so an assertion panic still leaves the DB
    // clean. Failed events stay `processed_at IS NULL`, and any leftover
    // here would surface as a phantom `stats.failed` count in the next
    // sibling test.
    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_confirm.as_str()),
        ],
    )
    .await;

    assert_eq!(
        stats.failed, 1,
        "empty-src OrderPlacedConfirmed must be reported as a failed projection",
    );
    assert!(
        owner.is_none(),
        "owner_pn_address must remain NULL — empty string is not a legal attribution",
    );
}

/// `apply_order_placed` accepts `clientOrderId` only as a string (or
/// `None` / `null`). A non-string JSON payload is schema drift, not a
/// legitimate "no clientOrderId" — the four-arm match must reject the
/// event so the user-correlatable id never silently lands NULL. The
/// reproject loop reports it as failed and the live_orders row is
/// never created.
#[tokio::test]
async fn orderplaced_rejects_non_string_client_order_id() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_orderplaced_non_string_coid";
    let orderbook_addr = format!("0:{test}_book");
    let msg_id_place = format!("{test}-place-msg");
    let order_id = "88";

    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
        ],
    )
    .await;

    insert_raw(
        &pool,
        &msg_id_place,
        &orderbook_addr,
        "OrderBook.OrderPlaced",
        // clientOrderId as integer 12345 — schema drift, not absent.
        &json!({
            "orderId": order_id, "outcomeId": "1", "isBuy": true,
            "price": "100", "amount": "50", "clientOrderId": 12345,
        }),
    )
    .await;

    let stats = repo.reproject_pending(1000).await.expect("reproject");

    let row_count: i64 = sqlx::query_scalar(
        r#"select count(*) from live_orders
            where orderbook_address = $1 and order_id = $2::numeric"#,
    )
    .bind(&orderbook_addr)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("count live_orders");

    // Purge BEFORE asserts so an assertion panic still leaves the DB
    // clean. Failed events stay `processed_at IS NULL`, and any leftover
    // here would surface as a phantom `stats.failed` count in the next
    // sibling test.
    purge(
        &pool,
        &[
            ("delete from live_orders where orderbook_address = $1", orderbook_addr.as_str()),
            ("delete from raw_events where msg_id = $1", msg_id_place.as_str()),
        ],
    )
    .await;

    assert_eq!(stats.failed, 1, "non-string clientOrderId must be reported as a failed projection");
    assert_eq!(row_count, 0, "no live_orders row may materialise when clientOrderId is non-string",);
}

/// On a taker-side OrderFilled (`isTaker = true`) the projector writes exactly
/// one `trades` row. `trade_id` is the event's chain_order; `price`/`qty` come
/// from the event (`clearingPrice`/`filledAmount`), while `outcome_id` and
/// direction are recovered from the order's `live_orders` row — the on-chain
/// event (`OrderFilled` in contracts/dex/OrderBook.sol) carries neither field. The parent is a SELL, so a taker fill means the
/// buyer is the maker => `is_buyer_maker = true`.
#[tokio::test]
async fn taker_orderfilled_writes_one_trade_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_taker_trade";
    let book = format!("0:{test}_book");
    let order_id = "42";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    // Parent order price (5000) differs from the fill's clearingPrice (6150)
    // so the assertion proves the trade price is the clearing price, not the
    // resting order's price.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 7, false, 5000::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;
    repo.reproject_pending(1000).await.expect("reproject");

    let row: (String, i32, bool, String, String, bool) = sqlx::query_as(
        "select trade_id, outcome_id, is_buyer_maker, price::text, qty::text, \
                chain_time is not null \
           from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("exactly one trade row");
    assert_eq!(row.0, chain_order, "trade_id is the taker event's chain_order");
    assert_eq!(row.1, 7, "outcome_id recovered from the order row");
    assert!(row.2, "SELL taker => buyer is the maker => is_buyer_maker = true");
    assert_eq!(row.3, "6150", "price comes from clearingPrice, not the order");
    assert_eq!(row.4, "30", "qty comes from filledAmount");
    assert!(row.5, "chain_time populated from the event");

    let count: i64 = sqlx::query_scalar("select count(*) from trades where orderbook_address = $1")
        .bind(&book)
        .fetch_one(&pool)
        .await
        .expect("count trades");
    assert_eq!(count, 1, "exactly one trade per taker fill");

    purge(&pool, cleanup).await;
}

/// A maker-side OrderFilled (`isTaker = false`) decrements its `live_orders`
/// row but writes no `trades` row, so a match is never double-counted.
#[tokio::test]
async fn maker_orderfilled_writes_no_trade_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_maker_no_trade";
    let book = format!("0:{test}_book");
    let order_id = "43";
    let msg_id = format!("{test}-fill-msg");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    // No clearingPrice on purpose: only the taker-side trade insert decodes
    // it, so its absence here is load-bearing — a refactor that hoists the
    // decode out of the taker branch fails this test.
    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "isTaker": false,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;
    repo.reproject_pending(1000).await.expect("reproject");

    let trade_count: i64 =
        sqlx::query_scalar("select count(*) from trades where orderbook_address = $1")
            .bind(&book)
            .fetch_one(&pool)
            .await
            .expect("count trades");
    assert_eq!(trade_count, 0, "maker-side fill writes no trades row");

    // The maker path still mutates live_orders: amount_remaining 100 - 30 = 70.
    let remaining: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders \
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders");
    assert_eq!(remaining, "70", "maker fill still decrements amount_remaining");

    purge(&pool, cleanup).await;
}

/// Re-projecting the same taker OrderFilled must not duplicate the trade or
/// mutate it, exercised in two distinct replay shapes:
///
/// * pass 2 — DIVERGENT payload (different clearingPrice): the conflict arm's
///   WHERE guard skips the row entirely, so nothing changes (and the
///   projector error!-logs the divergence). A widening of the conflict arm
///   to a full upsert fails the price assert.
/// * pass 3 — MATCHING payload with a shifted timestamp: the guard passes,
///   the arm fires, and the coalesce keeps the FIRST chain_time. A flip of
///   `coalesce(trades.chain_time, excluded.chain_time)` to last-write-wins
///   fails the chain_time assert. (The live_orders twin of this pin is
///   orderplaced_chain_created_at_is_first_write_wins; the guard-skip-on-NULL
///   twin is divergent_replay_never_heals_null_chain_time.)
///
/// The live_orders side is asserted too: OrderBook fill arms are deliberately
/// NOT replay-idempotent (`reproject_pending`'s doc: a replayed OrderFilled
/// re-subtracts `filledAmount`), so each pass drains another 30 from the
/// resting order. That is why an operator must never clear `processed_at` on
/// a fill whose order is still live — pinned here so the corruption mode
/// stays visible instead of hiding behind trade-only asserts.
#[tokio::test]
async fn taker_trade_insert_is_idempotent_on_replay() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_taker_idempotent";
    let book = format!("0:{test}_book");
    let order_id = "55";
    let msg_id = format!("{test}-fill-msg");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   1000::numeric, 1000::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;
    repo.reproject_pending(1000).await.expect("reproject pass 1");

    // Pass 2: replay the same trade_id with a DIVERGENT payload — a different
    // clearing price and a later chain timestamp. The WHERE guard on the
    // conflict arm sees price diverge and skips the row entirely.
    sqlx::query(
        r#"update raw_events
              set processed_at = null,
                  created_at_chain = to_timestamp(1700000999),
                  decoded = jsonb_set(decoded, '{clearingPrice}', '"9999"')
            where msg_id = $1"#,
    )
    .bind(&msg_id)
    .execute(&pool)
    .await
    .expect("reset processed_at with mutated payload");
    repo.reproject_pending(1000).await.expect("reproject pass 2");

    let (count, any_buyer_maker, price, chain_time_ms): (i64, bool, String, i64) = sqlx::query_as(
        "select count(*), bool_or(is_buyer_maker), min(price::text), \
                min((extract(epoch from chain_time) * 1000)::bigint) \
           from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("count trades");
    assert_eq!(count, 1, "replaying the same OrderFilled must not duplicate the trade");
    assert_eq!(price, "6150", "a divergent replay must not clobber the first-write price");
    assert_eq!(
        chain_time_ms, 1_700_000_000_000,
        "a divergent replay is guard-skipped and must leave chain_time untouched"
    );
    // The parent order is a BUY, so its taker fill means the maker sold: the
    // buyer is NOT the maker. Complements the SELL-taker => true direction in
    // taker_orderfilled_writes_one_trade_row.
    assert!(!any_buyer_maker, "BUY taker => is_buyer_maker = false");

    // Pass 3: restore the original clearingPrice but keep the shifted
    // timestamp — a MATCHING replay. The guard passes and the conflict arm
    // fires; the coalesce must still keep the FIRST chain_time.
    sqlx::query(
        r#"update raw_events
              set processed_at = null,
                  decoded = jsonb_set(decoded, '{clearingPrice}', '"6150"')
            where msg_id = $1"#,
    )
    .bind(&msg_id)
    .execute(&pool)
    .await
    .expect("reset processed_at with restored payload");
    repo.reproject_pending(1000).await.expect("reproject pass 3");

    let (count, chain_time_ms): (i64, i64) = sqlx::query_as(
        "select count(*), min((extract(epoch from chain_time) * 1000)::bigint) \
           from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("read trades after matching replay");
    assert_eq!(count, 1, "a matching replay must not duplicate the trade either");
    assert_eq!(
        chain_time_ms, 1_700_000_000_000,
        "a matching replay fires the conflict arm, and the coalesce must keep \
         the first chain_time (first-write-wins, not last-write-wins)"
    );

    // The fill arm is not replay-safe: each pass subtracted another 30 from
    // the still-OPEN order (1000 - 3 * 30). Deliberate pin of the documented
    // corruption mode, not an endorsement — see the test doc above.
    let remaining: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders \
          where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders");
    assert_eq!(remaining, "910", "every replayed fill re-subtracts on a non-terminal order");

    purge(&pool, cleanup).await;
}

/// A taker OrderFilled observed before its parent OrderPlaced defers without
/// writing a trade, and the replay — once the live_orders row exists — writes
/// exactly one. The tape's whole write path rides on this deferral mechanism:
/// a regression here means silently missing trades, not an error.
#[tokio::test]
async fn deferred_taker_orderfilled_writes_trade_on_replay() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_deferred_taker_trade";
    let book = format!("0:{test}_book");
    let order_id = "88";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;

    // Pass 1: no parent row -> Deferred, and no trade may leak out early.
    repo.reproject_pending(1000).await.expect("reproject pass 1");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "deferred taker OrderFilled must keep processed_at null"
    );
    let early: i64 = sqlx::query_scalar("select count(*) from trades where orderbook_address = $1")
        .bind(&book)
        .fetch_one(&pool)
        .await
        .expect("count trades pass 1");
    assert_eq!(early, 0, "no trade row may exist before the parent OrderPlaced lands");

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 3, false, 5000::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    // Pass 2: parent exists -> the replayed fill writes exactly one trade.
    repo.reproject_pending(1000).await.expect("reproject pass 2");
    assert!(processed_at_is_set(&pool, &msg_id).await, "replay must stamp processed_at");

    let rows: Vec<(String, i32, bool)> = sqlx::query_as(
        "select trade_id, outcome_id, is_buyer_maker from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_all(&pool)
    .await
    .expect("read trades");
    assert_eq!(rows.len(), 1, "replay writes exactly one trade row");
    assert_eq!(rows[0].0, chain_order, "trade_id is the taker event's chain_order");
    assert_eq!(rows[0].1, 3, "outcome_id recovered from the replayed parent row");
    assert!(rows[0].2, "SELL parent => taker fill => buyer is the maker");

    purge(&pool, cleanup).await;
}

/// One taker order crossing N makers emits N taker-side OrderFilled events
/// (distinct chain_orders) and therefore N trades with N distinct trade_ids.
#[tokio::test]
async fn one_taker_over_n_makers_yields_n_distinct_trades() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_taker_n_fills";
    let book = format!("0:{test}_book");
    let order_id = "99";

    let mut cleanup: Vec<(String, String)> = vec![
        ("delete from trades where orderbook_address = $1".into(), book.clone()),
        ("delete from live_orders where orderbook_address = $1".into(), book.clone()),
    ];
    for i in 0..3 {
        cleanup
            .push(("delete from raw_events where msg_id = $1".into(), format!("{test}-fill-{i}")));
    }
    let cleanup_refs: Vec<(&str, &str)> =
        cleanup.iter().map(|(s, k)| (s.as_str(), k.as_str())).collect();
    purge(&pool, &cleanup_refs).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   1000::numeric, 1000::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    for i in 0..3 {
        let msg_id = format!("{test}-fill-{i}");
        let decoded = json!({
            "orderId": order_id,
            "filledAmount": "10",
            "clearingPrice": "6150",
            "isTaker": true,
        });
        insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;
    }
    repo.reproject_pending(1000).await.expect("reproject");

    let (count, distinct): (i64, i64) = sqlx::query_as(
        "select count(*), count(distinct trade_id) from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("count trades");
    assert_eq!(count, 3, "one taker over 3 makers yields 3 trades");
    assert_eq!(distinct, 3, "each fill has its own chain_order as trade_id");

    purge(&pool, &cleanup_refs).await;
}

/// An `isTaker` that is missing, JSON-null, or not a bool (ABI drift, decode
/// regression) must surface as a failed projection — not collapse into the
/// maker path, which would silently drop the public trade with no log and,
/// once `processed_at` is stamped, no replay able to heal it. The ABI
/// declares `isTaker` on every OrderFilled, so absence is the same drift as
/// a wrong type. Mirrors the loud failure on a malformed `isBuy` in
/// OrderPlaced.
#[tokio::test]
async fn malformed_is_taker_fails_projection_loudly() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_malformed_istaker";
    let book = format!("0:{test}_book");
    let order_id = "77";
    // One event per drift shape: wrong type, JSON null, absent field.
    let variants: &[(&str, serde_json::Value)] = &[
        (
            "string",
            json!({"orderId": order_id, "filledAmount": "30",
                          "clearingPrice": "6150", "isTaker": "yes"}),
        ),
        (
            "null",
            json!({"orderId": order_id, "filledAmount": "30",
                        "clearingPrice": "6150", "isTaker": null}),
        ),
        (
            "absent",
            json!({"orderId": order_id, "filledAmount": "30",
                          "clearingPrice": "6150"}),
        ),
    ];
    let msg_ids: Vec<String> = variants.iter().map(|(k, _)| format!("{test}-{k}-msg")).collect();

    let mut cleanup: Vec<(String, String)> = vec![
        ("delete from trades where orderbook_address = $1".into(), book.clone()),
        ("delete from live_orders where orderbook_address = $1".into(), book.clone()),
    ];
    for msg_id in &msg_ids {
        cleanup.push(("delete from raw_events where msg_id = $1".into(), msg_id.clone()));
    }
    let cleanup_refs: Vec<(&str, &str)> =
        cleanup.iter().map(|(s, k)| (s.as_str(), k.as_str())).collect();
    purge(&pool, &cleanup_refs).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    for ((_, decoded), msg_id) in variants.iter().zip(&msg_ids) {
        insert_raw(&pool, msg_id, &book, "OrderBook.OrderFilled", decoded).await;
    }

    let stats = repo.reproject_pending(1000).await.expect("reproject");
    assert_eq!(
        stats.failed, 3,
        "missing / null / non-bool isTaker must each be reported as a failed projection"
    );
    for ((kind, _), msg_id) in variants.iter().zip(&msg_ids) {
        assert!(
            !processed_at_is_set(&pool, msg_id).await,
            "isTaker={kind}: a failed projection must keep processed_at null \
             so the repaired payload can replay"
        );
    }

    let trade_count: i64 =
        sqlx::query_scalar("select count(*) from trades where orderbook_address = $1")
            .bind(&book)
            .fetch_one(&pool)
            .await
            .expect("count trades");
    assert_eq!(trade_count, 0, "no trade row may materialise from a malformed event");

    purge(&pool, &cleanup_refs).await;
}

/// Inserts a pending `raw_events` row with an explicit `chain_order`, so a test
/// can place rows at controlled positions and bound a reproject to exactly its
/// own rows — isolating it from whatever else the shared CI database holds.
async fn insert_raw_at(
    pool: &PgPool,
    msg_id: &str,
    chain_order: &str,
    src: &str,
    event_type: &str,
    decoded: &serde_json::Value,
) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000000), $3, $3, $4, '{}'::jsonb, $5)"#,
    )
    .bind(msg_id)
    .bind(chain_order)
    .bind(src)
    .bind(event_type)
    .bind(decoded)
    .execute(pool)
    .await
    .expect("insert raw_events at chain_order");
}

/// Inserts an OPEN parent order so a subsequent OrderFilled reaches the body
/// parse (and can fail there) instead of deferring on a missing parent.
async fn insert_open_parent(pool: &PgPool, book: &str, order_id: &str) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(book)
    .bind(order_id)
    .execute(pool)
    .await
    .expect("insert parent live_orders");
}

/// A clean row then a poison row in one batch: the optimistic pass applies the
/// clean row, errors on the poison, rolls the whole transaction back, and the
/// savepointed replay re-applies the clean row and leaves the poison pending.
/// Asserts the branch actually taken (the fallback log marker), the recomputed
/// high-water mark, exact outcome counts, and that the rolled-back fast pass did
/// not double-warn the clean Unknown row.
///
/// `current_thread` keeps the reproject future on this thread so the
/// thread-local capture subscriber sees its events. The rows carry explicit
/// chain_orders that sort above the `insert_raw` "5f80…" space, and the reproject
/// is bounded to that range, so exactly these two rows are drained regardless of
/// what else the shared database holds (deterministic stats and high-water mark).
#[tokio::test(flavor = "current_thread")]
async fn fast_path_falls_back_and_isolates_poison_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let book = "0:reproj_fastfb_book";
    let order_id = "91";
    let probe_type = "Test.FastfbCleanProbe";
    let clean_msg = "reproj_fastfb_clean-msg";
    let poison_msg = "reproj_fastfb_poison-msg";
    let after = "zzzz_reproj_fastfb_0";
    let clean_chain = "zzzz_reproj_fastfb_1_clean";
    let poison_chain = "zzzz_reproj_fastfb_2_poison";

    let cleanup: Vec<(&str, &str)> = vec![
        ("delete from live_orders where orderbook_address = $1", book),
        ("delete from raw_events where msg_id = $1", clean_msg),
        ("delete from raw_events where msg_id = $1", poison_msg),
    ];
    purge(&pool, &cleanup).await;

    insert_open_parent(&pool, book, order_id).await;
    insert_raw_at(&pool, clean_msg, clean_chain, "0:reproj_fastfb_src", probe_type, &json!({}))
        .await;
    insert_raw_at(
        &pool,
        poison_msg,
        poison_chain,
        book,
        "OrderBook.OrderFilled",
        &json!({"orderId": order_id, "filledAmount": "30", "clearingPrice": "6150"}),
    )
    .await;

    let events = Arc::new(StdMutex::new(Vec::new()));
    let stats = {
        let subscriber =
            tracing_subscriber::registry().with(CaptureLayer { events: events.clone() });
        let _sub = tracing::subscriber::set_default(subscriber);
        repo.reproject_pending_from(1000, Some(after), Some(poison_chain)).await.expect("reproject")
    };

    // Exactly the two in-range rows are drained, so the counts are deterministic.
    assert_eq!(stats.scanned, 2, "only the two in-range rows are drained");
    assert_eq!(stats.applied, 0);
    assert_eq!(stats.unknown, 1, "the clean Unknown row");
    assert_eq!(stats.deferred, 0);
    assert_eq!(stats.failed, 1, "the poison row");
    // The savepointed branch recomputes the high-water mark; the drain loop's
    // forward floor depends on it. Poison sorts last in the bounded range.
    assert_eq!(stats.max_chain_order.as_deref(), Some(poison_chain));
    assert!(processed_at_is_set(&pool, clean_msg).await, "clean row marked despite the fallback");
    assert!(!processed_at_is_set(&pool, poison_msg).await, "poison row stays pending");
    assert_eq!(repo.projection_fallback_count(), 1, "the batch fell back exactly once");

    let captured: Vec<CapturedEvent> = events.lock().unwrap().clone();
    // The branch actually taken: the optimistic pass logged its fallback.
    assert!(
        captured.iter().any(|e| e.message.contains("per-row savepoints")),
        "the optimistic pass must log its fallback (proves the fast path ran and aborted)"
    );
    // A rolled-back fast pass must not pre-warn the clean Unknown row: exactly one
    // warning, on the normal target (the savepointed replay owns it).
    let unknown_warnings: Vec<&CapturedEvent> = captured
        .iter()
        .filter(|e| e.event_type == probe_type && e.message.contains("no handler for event type"))
        .collect();
    assert_eq!(unknown_warnings.len(), 1, "exactly one unknown warning, got {unknown_warnings:?}");
    assert_ne!(unknown_warnings[0].target, dodex_logging::EVENT_NOISE_TARGET);
    assert!(unknown_warnings[0].message.contains("first sighting"));

    purge(&pool, &cleanup).await;
}

/// A fully clean batch commits on the optimistic fast path with no fallback:
/// the fallback log marker is absent and the freshly-computed stats are exact.
#[tokio::test(flavor = "current_thread")]
async fn fast_path_clean_batch_commits_without_fallback() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let probe_type = "Test.FastfbCleanOnlyProbe";
    let a_msg = "reproj_fastfb_clean_a-msg";
    let b_msg = "reproj_fastfb_clean_b-msg";
    let after = "zzzy_reproj_fastfb_0";
    let a_chain = "zzzy_reproj_fastfb_1";
    let b_chain = "zzzy_reproj_fastfb_2";

    let cleanup: Vec<(&str, &str)> = vec![
        ("delete from raw_events where msg_id = $1", a_msg),
        ("delete from raw_events where msg_id = $1", b_msg),
    ];
    purge(&pool, &cleanup).await;

    insert_raw_at(&pool, a_msg, a_chain, "0:reproj_fastfb_co_src", probe_type, &json!({})).await;
    insert_raw_at(&pool, b_msg, b_chain, "0:reproj_fastfb_co_src", probe_type, &json!({})).await;

    let events = Arc::new(StdMutex::new(Vec::new()));
    let stats = {
        let subscriber =
            tracing_subscriber::registry().with(CaptureLayer { events: events.clone() });
        let _sub = tracing::subscriber::set_default(subscriber);
        repo.reproject_pending_from(1000, Some(after), Some(b_chain)).await.expect("reproject")
    };

    assert_eq!(stats.scanned, 2);
    assert_eq!(stats.unknown, 2, "both rows are unhandled Unknown");
    assert_eq!(stats.applied, 0);
    assert_eq!(stats.deferred, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.max_chain_order.as_deref(), Some(b_chain));
    assert!(processed_at_is_set(&pool, a_msg).await);
    assert!(processed_at_is_set(&pool, b_msg).await);
    assert_eq!(repo.projection_fallback_count(), 0, "a clean batch must not fall back");

    let captured: Vec<CapturedEvent> = events.lock().unwrap().clone();
    assert!(
        !captured.iter().any(|e| e.message.contains("per-row savepoints")),
        "a clean batch must commit on the fast path with no savepoint fallback, got {captured:?}"
    );

    purge(&pool, &cleanup).await;
}

/// The poison row sorts FIRST: the optimistic pass aborts immediately with
/// nothing applied (empty mark set), rolls back, and the savepointed replay
/// applies the trailing clean row and leaves the poison pending. Covers the
/// rollback-from-scratch branch, distinct from a poison after applied rows.
#[tokio::test(flavor = "current_thread")]
async fn fast_path_first_row_poison_falls_back() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let book = "0:reproj_fastfb_first_book";
    let order_id = "73";
    let probe_type = "Test.FastfbFirstProbe";
    let poison_msg = "reproj_fastfb_first_poison-msg";
    let clean_msg = "reproj_fastfb_first_clean-msg";
    let after = "zzzx_reproj_fastfb_0";
    let poison_chain = "zzzx_reproj_fastfb_1_poison";
    let clean_chain = "zzzx_reproj_fastfb_2_clean";

    let cleanup: Vec<(&str, &str)> = vec![
        ("delete from live_orders where orderbook_address = $1", book),
        ("delete from raw_events where msg_id = $1", poison_msg),
        ("delete from raw_events where msg_id = $1", clean_msg),
    ];
    purge(&pool, &cleanup).await;

    insert_open_parent(&pool, book, order_id).await;
    insert_raw_at(
        &pool,
        poison_msg,
        poison_chain,
        book,
        "OrderBook.OrderFilled",
        &json!({"orderId": order_id, "filledAmount": "30", "clearingPrice": "6150"}),
    )
    .await;
    insert_raw_at(
        &pool,
        clean_msg,
        clean_chain,
        "0:reproj_fastfb_first_src",
        probe_type,
        &json!({}),
    )
    .await;

    let events = Arc::new(StdMutex::new(Vec::new()));
    let stats = {
        let subscriber =
            tracing_subscriber::registry().with(CaptureLayer { events: events.clone() });
        let _sub = tracing::subscriber::set_default(subscriber);
        repo.reproject_pending_from(1000, Some(after), Some(clean_chain)).await.expect("reproject")
    };

    assert_eq!(stats.scanned, 2);
    assert_eq!(stats.unknown, 1, "the trailing clean row");
    assert_eq!(stats.failed, 1, "the leading poison row");
    assert!(!processed_at_is_set(&pool, poison_msg).await, "poison stays pending");
    assert!(
        processed_at_is_set(&pool, clean_msg).await,
        "the trailing clean row is applied by the fallback"
    );
    assert_eq!(repo.projection_fallback_count(), 1, "the batch fell back exactly once");

    let captured: Vec<CapturedEvent> = events.lock().unwrap().clone();
    assert!(
        captured.iter().any(|e| e.message.contains("per-row savepoints")),
        "the optimistic pass must fall back even when the first row poisons"
    );

    purge(&pool, &cleanup).await;
}

/// The poison row mutates `live_orders` and *then* fails — a taker OrderFilled
/// missing `clearingPrice`: the `amount_remaining` UPDATE applies, then the trade
/// insert fails. This proves the optimistic rollback actually reverts a
/// partially-applied transaction, which the missing-`isTaker` poison (it fails
/// before any write) cannot. The clean row still commits, the poison stays
/// pending with its mutation reverted, and the fallback counter accumulates
/// across retries (and the revert holds on each).
#[tokio::test]
async fn fast_path_rolls_back_partial_mutation() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let book = "0:reproj_pmut_book";
    let order_id = "91";
    let probe_type = "Test.PmutCleanProbe";
    let clean_msg = "reproj_pmut_clean-msg";
    let poison_msg = "reproj_pmut_poison-msg";
    let after = "zzzw_reproj_pmut_0";
    let clean_chain = "zzzw_reproj_pmut_1_clean";
    let poison_chain = "zzzw_reproj_pmut_2_poison";

    let cleanup: Vec<(&str, &str)> = vec![
        ("delete from trades where orderbook_address = $1", book),
        ("delete from live_orders where orderbook_address = $1", book),
        ("delete from raw_events where msg_id = $1", clean_msg),
        ("delete from raw_events where msg_id = $1", poison_msg),
    ];
    purge(&pool, &cleanup).await;

    insert_open_parent(&pool, book, order_id).await;
    insert_raw_at(&pool, clean_msg, clean_chain, "0:reproj_pmut_src", probe_type, &json!({})).await;
    // Taker fill with no clearingPrice: the amount_remaining UPDATE applies, then
    // the trade insert fails, so the projector's whole apply must revert.
    insert_raw_at(
        &pool,
        poison_msg,
        poison_chain,
        book,
        "OrderBook.OrderFilled",
        &json!({"orderId": order_id, "filledAmount": "30", "isTaker": true}),
    )
    .await;

    let stats = repo
        .reproject_pending_from(1000, Some(after), Some(poison_chain))
        .await
        .expect("reproject");
    assert_eq!(stats.scanned, 2);
    assert_eq!(stats.unknown, 1, "the clean row");
    assert_eq!(stats.failed, 1, "the mutating poison");
    assert!(processed_at_is_set(&pool, clean_msg).await, "clean row committed");
    assert!(!processed_at_is_set(&pool, poison_msg).await, "poison stays pending");
    assert_eq!(repo.projection_fallback_count(), 1, "one fallback so far");

    // The poison's amount_remaining UPDATE must not survive the optimistic
    // rollback — a regression that committed the fast transaction's partial
    // writes on the error path would leave 70 here.
    let remaining: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders \
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders");
    assert_eq!(remaining, "100", "the rolled-back optimistic batch must not persist the mutation");

    // A second pass falls back again (the poison is still pending): the counter
    // accumulates past 1 — the property the metric depends on — and the mutation
    // still must not leak.
    repo.reproject_pending_from(1000, Some(after), Some(poison_chain)).await.expect("reproject 2");
    assert_eq!(
        repo.projection_fallback_count(),
        2,
        "the fallback counter accumulates across passes"
    );
    let remaining_again: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders \
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders again");
    assert_eq!(remaining_again, "100", "the mutation stays reverted across retries");

    purge(&pool, &cleanup).await;
}

/// A taker OrderFilled with no `clearingPrice` fails the projection
/// atomically: the live_orders mutation issued earlier in the same
/// transaction rolls back with the trade insert, no trade row appears, and
/// `processed_at` stays null so a fixed payload can replay. A partial apply
/// would decrement the order while writing no trade — a silent divergence
/// between /api/v1/prediction/orders and /api/v1/prediction/trades.
#[tokio::test]
async fn taker_orderfilled_without_clearing_price_rolls_back_atomically() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_no_clearing_price";
    let book = format!("0:{test}_book");
    let order_id = "91";
    let msg_id = format!("{test}-fill-msg");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   100::numeric, 100::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "isTaker": true,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;

    let stats = repo.reproject_pending(1000).await.expect("reproject");
    assert_eq!(stats.failed, 1, "missing clearingPrice on a taker fill is a failed projection");
    assert!(
        !processed_at_is_set(&pool, &msg_id).await,
        "a failed projection must keep processed_at null so a fixed payload can replay it"
    );

    let remaining: String = sqlx::query_scalar(
        "select amount_remaining::text from live_orders \
              where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders");
    assert_eq!(remaining, "100", "the live_orders mutation must roll back with the trade insert");

    let trade_count: i64 =
        sqlx::query_scalar("select count(*) from trades where orderbook_address = $1")
            .bind(&book)
            .fetch_one(&pool)
            .await
            .expect("count trades");
    assert_eq!(trade_count, 0, "no trade row may survive the rolled-back transaction");

    purge(&pool, cleanup).await;
}

/// A taker OrderFilled whose raw event carries no chain timestamp still
/// records the trade, but with `chain_time = NULL`: the /api/v1/prediction/trades read
/// query filters `chain_time IS NOT NULL`, so the row is invisible to the
/// public tape until the timestamp is healed. The projection itself applies —
/// `processed_at` is stamped and live_orders advances.
///
/// The heal phase models the only replay-safe recovery: the fill here
/// terminalises its order (30 of 30), so re-running `apply_order_filled` is
/// held off live_orders by the terminal CASE guards while the trades conflict
/// arm coalesces the NULL chain_time. For an order that is still live the
/// fill arm is NOT replay-idempotent (it would re-subtract `filledAmount` —
/// pinned in taker_trade_insert_is_idempotent_on_replay), which is why the
/// documented recovery for that case is a direct UPDATE of the trades row,
/// never a `processed_at` reset.
#[tokio::test]
async fn taker_orderfilled_without_chain_time_writes_hidden_trade_row() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_taker_null_chain_time";
    let book = format!("0:{test}_book");
    let order_id = "92";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    // amount_initial == filledAmount: the fill terminalises the order, which
    // is what makes the heal-phase replay below safe for live_orders.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   30::numeric, 30::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, null, $3, $3, 'OrderBook.OrderFilled', '{}'::jsonb, $4)"#,
    )
    .bind(&msg_id)
    .bind(&chain_order)
    .bind(&book)
    .bind(&decoded)
    .execute(&pool)
    .await
    .expect("insert raw_events without chain timestamp");

    repo.reproject_pending(1000).await.expect("reproject");
    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "a missing chain timestamp must not block the projection"
    );

    let row: (String, bool) = sqlx::query_as(
        "select trade_id, chain_time is null from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("exactly one trade row");
    assert_eq!(row.0, chain_order, "trade_id is the taker event's chain_order");
    assert!(row.1, "the trade lands with NULL chain_time, hidden from the tape read");

    // Heal phase — safe here ONLY because the order is now terminal: replay
    // re-runs the whole apply_order_filled, and on a terminal row the CASE
    // guards hold live_orders while the trades conflict arm coalesces the
    // NULL chain_time. Fractional seconds also pin sub-second precision
    // through to_timestamp.
    sqlx::query(
        "update raw_events set created_at_chain = to_timestamp(1700000000.5), \
                               processed_at = null \
          where msg_id = $1",
    )
    .bind(&msg_id)
    .execute(&pool)
    .await
    .expect("repair raw event timestamp");
    repo.reproject_pending(1000).await.expect("reproject heal pass");

    let healed: Vec<(i64,)> = sqlx::query_as(
        "select (extract(epoch from chain_time) * 1000)::bigint \
           from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_all(&pool)
    .await
    .expect("read healed trade");
    assert_eq!(healed.len(), 1, "healing must update the row, not duplicate it");
    assert_eq!(
        healed[0].0, 1_700_000_000_500,
        "replay after repairing raw_events must heal chain_time (sub-second preserved)"
    );

    // The terminal CASE guards held the live_orders row through the replay:
    // no double-subtraction, status and remainder untouched.
    let order: (String, String) = sqlx::query_as(
        "select status, amount_remaining::text from live_orders \
          where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders after heal");
    assert_eq!(order.0, "FILLED", "heal replay must not touch a terminal order's status");
    assert_eq!(order.1, "0", "heal replay must not re-subtract the fill");

    purge(&pool, cleanup).await;
}

/// A taker OrderFilled against an already-terminal order: the SQL CASE guards
/// hold every live_orders mutation (and warn loudly), but the tape insert is
/// deliberately not gated on the prior status — the chain emitted the fill,
/// and the public tape mirrors chain events one-to-one. Pinned so a future
/// change to the terminal guard cannot silently change what the tape records.
#[tokio::test]
async fn taker_orderfilled_on_terminal_order_still_records_trade() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_terminal_taker_trade";
    let book = format!("0:{test}_book");
    let order_id = "94";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    // Terminal prior row: FILLED with 0 remaining.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   100::numeric, 0::numeric, 'FILLED',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert terminal live_orders row");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    insert_raw(&pool, &msg_id, &book, "OrderBook.OrderFilled", &decoded).await;

    repo.reproject_pending(1000).await.expect("reproject");
    assert!(processed_at_is_set(&pool, &msg_id).await, "the event still projects as Applied");

    let trade: (String, String) =
        sqlx::query_as("select trade_id, qty::text from trades where orderbook_address = $1")
            .bind(&book)
            .fetch_one(&pool)
            .await
            .expect("post-terminal taker fill still lands on the tape");
    assert_eq!(trade.0, chain_order);
    assert_eq!(trade.1, "30");

    let row: (String, String, String) = sqlx::query_as(
        "select status, amount_remaining::text, last_chain_order from live_orders \
          where orderbook_address = $1 and order_id = $2::numeric",
    )
    .bind(&book)
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .expect("read live_orders");
    assert_eq!(row.0, "FILLED", "terminal status is held");
    assert_eq!(row.1, "0", "amount_remaining is held");
    assert_eq!(row.2, "5f800000000000000000", "last_chain_order is held");

    purge(&pool, cleanup).await;
}

/// The WHERE guard's only observable delta over the bare coalesce: a
/// DIVERGENT replay must not heal a NULL chain_time. Without the guard, a
/// replay whose immutables drifted (here: clearingPrice) would still coalesce
/// its timestamp into the hidden row — quietly blessing a row whose recorded
/// values no longer match the event that produced it. The divergence is
/// Applied + error!-logged, not a stuck projection. The parent order is
/// terminalised by the first fill, so the replay leaves live_orders alone.
#[tokio::test]
async fn divergent_replay_never_heals_null_chain_time() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_divergent_no_heal";
    let book = format!("0:{test}_book");
    let order_id = "96";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   30::numeric, 30::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, null, $3, $3, 'OrderBook.OrderFilled', '{}'::jsonb, $4)"#,
    )
    .bind(&msg_id)
    .bind(&chain_order)
    .bind(&book)
    .bind(&decoded)
    .execute(&pool)
    .await
    .expect("insert raw_events without chain timestamp");

    repo.reproject_pending(1000).await.expect("reproject pass 1");

    // Replay with a parseable timestamp AND a divergent clearingPrice: the
    // guard must refuse the row wholesale, timestamp included.
    sqlx::query(
        r#"update raw_events
              set processed_at = null,
                  created_at_chain = to_timestamp(1700000000.5),
                  decoded = jsonb_set(decoded, '{clearingPrice}', '"9999"')
            where msg_id = $1"#,
    )
    .bind(&msg_id)
    .execute(&pool)
    .await
    .expect("reset processed_at with divergent payload");
    repo.reproject_pending(1000).await.expect("reproject pass 2");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "a divergent replay is Applied (and error!-logged), not a stuck projection"
    );

    let row: (String, bool) = sqlx::query_as(
        "select price::text, chain_time is null from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("exactly one trade row");
    assert_eq!(row.0, "6150", "the divergent price must not reach the row");
    assert!(
        row.1,
        "a divergent replay must not heal the NULL chain_time — the guard \
         refuses the row wholesale, timestamp included"
    );

    purge(&pool, cleanup).await;
}

/// Sibling of `divergent_replay_never_heals_null_chain_time` for the `qty`
/// column of the conflict guard. That test diverges on `clearingPrice`
/// (→ `price`); this one keeps the price matching and diverges only on
/// `filledAmount` (→ `qty`). The guard checks five immutable columns, but
/// the price-divergence tests would still pass a regression that narrowed
/// it to drop `qty`/`outcome_id`/`is_buyer_maker` — only a non-price
/// divergence pins those. With `qty` in the guard the replay is refused
/// wholesale (chain_time stays NULL); a guard missing `qty` would see the
/// matching price, fire the conflict arm, and coalesce the timestamp in.
#[tokio::test]
async fn divergent_qty_replay_never_heals_null_chain_time() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_divergent_qty_no_heal";
    let book = format!("0:{test}_book");
    let order_id = "97";
    let msg_id = format!("{test}-fill-msg");
    let chain_order = format!("5f80{msg_id:0>28}");

    let cleanup: &[(&str, &str)] = &[
        ("delete from trades where orderbook_address = $1", book.as_str()),
        ("delete from live_orders where orderbook_address = $1", book.as_str()),
        ("delete from raw_events where msg_id = $1", msg_id.as_str()),
    ];
    purge(&pool, cleanup).await;

    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, $2::numeric, 1, true, 6150::numeric,
                   30::numeric, 30::numeric, 'OPEN',
                   '5f800000000000000000', '5f800000000000000000')"#,
    )
    .bind(&book)
    .bind(order_id)
    .execute(&pool)
    .await
    .expect("insert parent live_orders");

    let decoded = json!({
        "orderId": order_id,
        "filledAmount": "30",
        "clearingPrice": "6150",
        "isTaker": true,
    });
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded)
           values ($1, $2, null, $3, $3, 'OrderBook.OrderFilled', '{}'::jsonb, $4)"#,
    )
    .bind(&msg_id)
    .bind(&chain_order)
    .bind(&book)
    .bind(&decoded)
    .execute(&pool)
    .await
    .expect("insert raw_events without chain timestamp");

    repo.reproject_pending(1000).await.expect("reproject pass 1");

    // Replay with a parseable timestamp and a MATCHING clearingPrice but a
    // divergent filledAmount: only the `qty` immutable drifts. The guard must
    // still refuse the row wholesale, timestamp included.
    sqlx::query(
        r#"update raw_events
              set processed_at = null,
                  created_at_chain = to_timestamp(1700000000.5),
                  decoded = jsonb_set(decoded, '{filledAmount}', '"31"')
            where msg_id = $1"#,
    )
    .bind(&msg_id)
    .execute(&pool)
    .await
    .expect("reset processed_at with divergent qty payload");
    repo.reproject_pending(1000).await.expect("reproject pass 2");

    assert!(
        processed_at_is_set(&pool, &msg_id).await,
        "a divergent replay is Applied (and error!-logged), not a stuck projection"
    );

    let (count, qty, time_is_null): (i64, String, bool) = sqlx::query_as(
        "select count(*), min(qty::text), bool_and(chain_time is null) \
           from trades where orderbook_address = $1",
    )
    .bind(&book)
    .fetch_one(&pool)
    .await
    .expect("read trades after divergent qty replay");
    assert_eq!(count, 1, "a divergent-qty replay must not duplicate the trade");
    assert_eq!(qty, "30", "the divergent qty must not reach the row");
    assert!(
        time_is_null,
        "a qty-divergent replay must not heal the NULL chain_time — a guard \
         that dropped `qty` would see the matching price and coalesce it in"
    );

    purge(&pool, cleanup).await;
}

#[tokio::test]
async fn count_pending_projection_counts_only_unprojected_typed_decoded_rows() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "count_pending";
    let pending = format!("{test}-pending");
    let processed = format!("{test}-processed");
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", pending.as_str()),
            ("delete from raw_events where msg_id = $1", processed.as_str()),
        ],
    )
    .await;

    let before = repo.count_pending_projection().await.expect("count before");

    // One pending decodable+typed row.
    insert_raw(&pool, &pending, "0:count_pending_src", "RootOracle.OracleDeployed", &json!({}))
        .await;
    // One already-processed row — must NOT be counted.
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, src_address, dst_address,
                event_type, body_json, decoded, processed_at)
           values ($1, $2, to_timestamp(1700000000), $3, $3,
                   'RootOracle.OracleDeployed', '{}'::jsonb, '{}'::jsonb, now())"#,
    )
    .bind(&processed)
    .bind(format!("5f80{processed:0>28}"))
    .bind("0:count_pending_src")
    .execute(&pool)
    .await
    .expect("insert processed row");

    let after = repo.count_pending_projection().await.expect("count after");
    assert_eq!(after - before, 1, "only the unprojected typed+decoded row adds to the backlog");

    // Purge before returning: `pending` is typed + decoded + processed_at NULL,
    // so it satisfies reproject_pending's filter. Left behind in the shared test
    // DB, a later reproject_pending(1000) would select it, fail on the empty
    // payload, and add a phantom stats.failed — breaking the exact failure-count
    // assertions elsewhere in this suite.
    purge(
        &pool,
        &[
            ("delete from raw_events where msg_id = $1", pending.as_str()),
            ("delete from raw_events where msg_id = $1", processed.as_str()),
        ],
    )
    .await;
}

#[tokio::test]
async fn reproject_pending_marks_a_whole_batch_processed() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_batch_mark";
    let addr_a = format!("0:{test}_a");
    let addr_b = format!("0:{test}_b");
    let msg_a = format!("{test}-a");
    let msg_b = format!("{test}-b");
    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", addr_a.as_str()),
            ("delete from oracles where address = $1", addr_b.as_str()),
            ("delete from raw_events where msg_id = $1", msg_a.as_str()),
            ("delete from raw_events where msg_id = $1", msg_b.as_str()),
        ],
    )
    .await;

    insert_raw(
        &pool,
        &msg_a,
        &addr_a,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": addr_a, "pubkey": "0x00", "name": format!("{test}-a") }),
    )
    .await;
    insert_raw(
        &pool,
        &msg_b,
        &addr_b,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": addr_b, "pubkey": "0x00", "name": format!("{test}-b") }),
    )
    .await;

    let stats = repo.reproject_pending(1000).await.expect("reproject");
    assert!(stats.applied >= 2, "both rows apply in one batch");

    assert!(processed_at_is_set(&pool, &msg_a).await, "row A marked processed");
    assert!(processed_at_is_set(&pool, &msg_b).await, "row B marked processed");

    // Purge the rows and the oracles they projected so the test leaves the
    // shared DB as it found it.
    purge(
        &pool,
        &[
            ("delete from oracles where address = $1", addr_a.as_str()),
            ("delete from oracles where address = $1", addr_b.as_str()),
            ("delete from raw_events where msg_id = $1", msg_a.as_str()),
            ("delete from raw_events where msg_id = $1", msg_b.as_str()),
        ],
    )
    .await;
}

#[tokio::test]
async fn reproject_pending_from_honors_after_and_until_bounds() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "reproj_range";
    let a1 = format!("0:{test}_a1");
    let a2 = format!("0:{test}_a2");
    let a3 = format!("0:{test}_a3");
    let m1 = format!("{test}-1");
    let m2 = format!("{test}-2");
    let m3 = format!("{test}-3");
    let cleanup = [
        ("delete from oracles where address = $1", a1.as_str()),
        ("delete from oracles where address = $1", a2.as_str()),
        ("delete from oracles where address = $1", a3.as_str()),
        ("delete from raw_events where msg_id = $1", m1.as_str()),
        ("delete from raw_events where msg_id = $1", m2.as_str()),
        ("delete from raw_events where msg_id = $1", m3.as_str()),
    ];
    purge(&pool, &cleanup).await;

    insert_raw(
        &pool,
        &m1,
        &a1,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": a1, "pubkey": "0x00", "name": m1 }),
    )
    .await;
    insert_raw(
        &pool,
        &m2,
        &a2,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": a2, "pubkey": "0x00", "name": m2 }),
    )
    .await;
    insert_raw(
        &pool,
        &m3,
        &a3,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": a3, "pubkey": "0x00", "name": m3 }),
    )
    .await;

    // insert_raw derives chain_order as `5f80{msg_id:0>28}`. after = row 1's value,
    // until = row 2's value, so only row 2 (after < chain_order <= until) is eligible:
    // row 1 is excluded by `>`, row 3 by `<=`.
    let after = format!("5f80{m1:0>28}");
    let until = format!("5f80{m2:0>28}");
    let stats = repo
        .reproject_pending_from(1000, Some(&after), Some(&until))
        .await
        .expect("reproject_from");

    assert!(!processed_at_is_set(&pool, &m1).await, "row at/before `after` must be skipped");
    assert!(processed_at_is_set(&pool, &m2).await, "row inside (after, until] must be projected");
    assert!(!processed_at_is_set(&pool, &m3).await, "row above `until` must be skipped");
    assert_eq!(
        stats.max_chain_order.as_deref(),
        Some(until.as_str()),
        "max_chain_order must be the highest chain_order read in the batch"
    );

    purge(&pool, &cleanup).await;
}

#[tokio::test]
async fn has_pending_above_and_max_pending_chain_order_respect_eligibility() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let barrier_stream = "reproj_pending_endpoints_barrier";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(barrier_stream);

    let test = "reproj_pending_endpoints";
    let addr = format!("0:{test}_oracle");

    // Three eligible rows with known chain_orders (derived from msg_id by insert_raw).
    let m1 = format!("{test}-msg-1");
    let m2 = format!("{test}-msg-2");
    let m3 = format!("{test}-msg-3");
    // Ineligible rows: processed_at set, event_type NULL, decoded NULL.
    let m_proc = format!("{test}-msg-proc");
    let m_null_type = format!("{test}-msg-nulltype");
    let m_null_dec = format!("{test}-msg-nulldec");

    let cleanup = [
        ("delete from oracles where address = $1", addr.as_str()),
        ("delete from raw_events where msg_id = $1", m1.as_str()),
        ("delete from raw_events where msg_id = $1", m2.as_str()),
        ("delete from raw_events where msg_id = $1", m3.as_str()),
        ("delete from raw_events where msg_id = $1", m_proc.as_str()),
        ("delete from raw_events where msg_id = $1", m_null_type.as_str()),
        ("delete from raw_events where msg_id = $1", m_null_dec.as_str()),
    ];
    purge(&pool, &cleanup).await;

    // Insert three eligible rows.
    insert_raw(
        &pool,
        &m1,
        &addr,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": format!("0:{test}_o1"), "pubkey": "0x01", "name": "n1" }),
    )
    .await;
    insert_raw(
        &pool,
        &m2,
        &addr,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": format!("0:{test}_o2"), "pubkey": "0x02", "name": "n2" }),
    )
    .await;
    insert_raw(
        &pool,
        &m3,
        &addr,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": format!("0:{test}_o3"), "pubkey": "0x03", "name": "n3" }),
    )
    .await;

    // Insert an already-processed row (highest chain_order among all inserts,
    // so if the predicate were broken it would show up as max).
    // chain_order = 5f80{msg_id:0>28}; m_proc > m3 lexicographically so its
    // chain_order is above m3's — confirming the filter ignores it.
    insert_raw(
        &pool,
        &m_proc,
        &addr,
        "RootOracle.OracleDeployed",
        &json!({ "oracle": format!("0:{test}_oproc"), "pubkey": "0x04", "name": "nproc" }),
    )
    .await;
    sqlx::query("update raw_events set processed_at = now() where msg_id = $1")
        .bind(&m_proc)
        .execute(&pool)
        .await
        .expect("mark processed");

    // Insert a row with event_type IS NULL.
    sqlx::query(
        r#"insert into raw_events (msg_id, chain_order, created_at_chain, src_address,
               dst_address, event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000000), $3, $3,
                   null, '{}'::jsonb, '{}'::jsonb)"#,
    )
    .bind(&m_null_type)
    .bind(format!("5f80{m_null_type:0>28}"))
    .bind(&addr)
    .execute(&pool)
    .await
    .expect("insert null-type row");

    // Insert a row with decoded IS NULL.
    sqlx::query(
        r#"insert into raw_events (msg_id, chain_order, created_at_chain, src_address,
               dst_address, event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000000), $3, $3,
                   'RootOracle.OracleDeployed', '{}'::jsonb, null)"#,
    )
    .bind(&m_null_dec)
    .bind(format!("5f80{m_null_dec:0>28}"))
    .bind(&addr)
    .execute(&pool)
    .await
    .expect("insert null-decoded row");

    // chain_orders for the three eligible rows (derived by insert_raw formula):
    let co1 = format!("5f80{m1:0>28}");
    let co2 = format!("5f80{m2:0>28}");
    let co3 = format!("5f80{m3:0>28}");

    repo.set_capture_barrier(Some(&co2), false).await.expect("set projection barrier");

    // max_pending_chain_order returns the highest eligible chain_order (m3's).
    let max = repo.max_pending_chain_order().await.expect("max_pending_chain_order");
    assert_eq!(
        max.as_deref(),
        Some(co3.as_str()),
        "max_pending_chain_order must return the highest eligible chain_order; \
         processed/null-type/null-decoded rows must not count"
    );

    // has_pending_above(co2) is true: m3 is eligible and above co2.
    assert!(
        repo.has_pending_above(&co2).await.expect("has_pending_above co2"),
        "has_pending_above must be true when an eligible row exists above the threshold"
    );
    // has_pending_above(co3) is false: no eligible row above the highest.
    assert!(
        !repo.has_pending_above(&co3).await.expect("has_pending_above co3"),
        "has_pending_above must be false at or above the highest eligible chain_order"
    );
    // has_pending_above(co1) is true: m2 and m3 are both above co1.
    assert!(
        repo.has_pending_above(&co1).await.expect("has_pending_above co1"),
        "has_pending_above must be true when multiple eligible rows exist above threshold"
    );

    let projectable =
        repo.max_projectable_chain_order().await.expect("max_projectable_chain_order");
    assert_eq!(
        projectable.as_deref(),
        Some(co2.as_str()),
        "the aggregate barrier must hold back an eligible row from the faster capture stream"
    );
    assert!(
        repo.has_projectable_pending_above(&co1).await.expect("has_projectable_pending_above co1"),
        "the row inside the barrier remains immediately projectable"
    );
    assert!(
        !repo.has_projectable_pending_above(&co2).await.expect("has_projectable_pending_above co2"),
        "the row beyond the barrier must not make the projection loop spin"
    );

    repo.set_capture_barrier(None, false).await.expect("clear projection barrier");
    assert_eq!(
        repo.max_projectable_chain_order().await.expect("null barrier"),
        None,
        "a null barrier blocks projection until both capture streams establish progress"
    );

    purge(&pool, &cleanup).await;
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(barrier_stream)
        .execute(&pool)
        .await
        .expect("cleanup barrier");
}

#[tokio::test]
async fn run_reprojection_loop_drains_pending_and_retries_deferred() {
    // Tests that the loop drains seeded pending rows end-to-end and that the
    // timer-gated retry pass re-attempts deferred rows once their dependency
    // arrives.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let barrier_stream = "reproj_loop_orch_barrier";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(barrier_stream);
    repo.set_capture_barrier(Some("zzzzzzzz"), true).await.expect("set projection barrier");

    let test = "reproj_loop_orch";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let evlist_addr = format!("0:{test}_evlist");
    let msg_oracle = format!("{test}-oracle-msg");
    let msg_child = format!("{test}-child-msg");

    let cleanup = [
        ("delete from oracle_event_lists where address = $1", evlist_addr.as_str()),
        ("delete from oracles where address = $1", oracle_addr.as_str()),
        ("delete from raw_events where msg_id = $1", msg_oracle.as_str()),
        ("delete from raw_events where msg_id = $1", msg_child.as_str()),
    ];
    purge(&pool, &cleanup).await;

    // Seed the parent oracle raw event (will Apply immediately).
    insert_raw(
        &pool,
        &msg_oracle,
        &oracle_addr,
        "RootOracle.OracleDeployed",
        &json!({
            "oracle": oracle_addr,
            "pubkey": "0x0000000000000000000000000000000000000000000000000000000000001234",
            "name": oracle_name,
        }),
    )
    .await;

    // Seed the child event list — its oracle parent is not yet in `oracles`,
    // so the first pass will Defer it.
    insert_raw(
        &pool,
        &msg_child,
        &oracle_addr,
        "Oracle.OracleEventListDeployed",
        &json!({
            "eventListAddress": evlist_addr,
            "index": "1",
            "description": "Loop orch test event list",
        }),
    )
    .await;

    // Start the reprojection loop with a 50ms idle interval.
    let h = tokio::spawn(repo.clone().run_reprojection_loop(Duration::from_millis(50), 1000));

    // Poll up to 3 seconds for the oracle row (parent) to be applied.
    let oracle_applied = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if processed_at_is_set(&pool, &msg_oracle).await {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert!(oracle_applied, "loop must drain the oracle row within 3s");

    // The oracle row is now Applied → the `oracles` table has the row.
    // The child event list was Deferred on the first pass. Poll up to 3s
    // for the retry pass to re-attempt it and find the parent now present.
    let child_applied = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if processed_at_is_set(&pool, &msg_child).await {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert!(
        child_applied,
        "loop retry pass must re-attempt the deferred child and apply it once the parent exists"
    );

    h.abort();
    let _ = h.await; // drive the aborted task to completion before purging its rows
    purge(&pool, &cleanup).await;
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(barrier_stream)
        .execute(&pool)
        .await
        .expect("cleanup barrier");
}

#[tokio::test]
async fn identical_chain_order_both_rows_eventually_drain() {
    // Verifies that two pending rows sharing the same chain_order are both
    // eventually drained. With batch_size=1, the first forward pass takes row A
    // and advances `after` to the shared chain_order; the next forward SELECT
    // (`chain_order > after`) then excludes row B, stranding it. The loop's
    // front-rewinding retry pass (after = None, `processed_at is null` filter)
    // then recovers B. This exercises the actual boundary path: forward-pass
    // stranding followed by retry-pass recovery.
    //
    // chain_order is globally unique by gateway design, so real duplicates are
    // not anticipated on the write path; this test asserts the recovery
    // mechanism as defense-in-depth.
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let barrier_stream = "reproj_dup_chain_order_barrier";
    let repo = IndexerRepository::new(pool.clone()).with_capture_stream(barrier_stream);
    repo.set_capture_barrier(Some("zzzzzzzz"), true).await.expect("set projection barrier");

    let test = "reproj_dup_chain_order";
    let addr_a = format!("0:{test}_a");
    let addr_b = format!("0:{test}_b");
    let msg_a = format!("{test}-msg-a");
    let msg_b = format!("{test}-msg-b");
    // Both rows share the SAME chain_order, deliberately violating the normal
    // uniqueness guarantee to probe the keyset boundary.
    let shared_chain_order = "5f80reproj_dup_chain_order000000";

    let cleanup = [
        ("delete from oracles where address = $1", addr_a.as_str()),
        ("delete from oracles where address = $1", addr_b.as_str()),
        ("delete from raw_events where msg_id = $1", msg_a.as_str()),
        ("delete from raw_events where msg_id = $1", msg_b.as_str()),
    ];
    purge(&pool, &cleanup).await;

    // Insert both rows with the same chain_order.
    sqlx::query(
        r#"insert into raw_events (msg_id, chain_order, created_at_chain, src_address,
               dst_address, event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000000), $3, $3,
                   'RootOracle.OracleDeployed', '{}'::jsonb, $4)"#,
    )
    .bind(&msg_a)
    .bind(shared_chain_order)
    .bind(&addr_a)
    .bind(json!({ "oracle": addr_a, "pubkey": "0x01", "name": format!("{test}-a") }))
    .execute(&pool)
    .await
    .expect("insert row A");

    sqlx::query(
        r#"insert into raw_events (msg_id, chain_order, created_at_chain, src_address,
               dst_address, event_type, body_json, decoded)
           values ($1, $2, to_timestamp(1700000000), $3, $3,
                   'RootOracle.OracleDeployed', '{}'::jsonb, $4)"#,
    )
    .bind(&msg_b)
    .bind(shared_chain_order)
    .bind(&addr_b)
    .bind(json!({ "oracle": addr_b, "pubkey": "0x02", "name": format!("{test}-b") }))
    .execute(&pool)
    .await
    .expect("insert row B");

    // Drive through the loop with batch_size=1 and a short idle interval.
    // batch_size=1 forces the keyset boundary: the first forward pass takes
    // row A (advances `after` to the shared chain_order), the next forward
    // SELECT excludes row B, and the timer-gated retry pass (rewind to front)
    // recovers B.
    let h = tokio::spawn(repo.clone().run_reprojection_loop(Duration::from_millis(50), 1));

    // Poll up to 5 seconds for BOTH rows to drain.
    let both_applied = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let a = processed_at_is_set(&pool, &msg_a).await;
            let b = processed_at_is_set(&pool, &msg_b).await;
            if a && b {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    h.abort();
    let _ = h.await; // drive the aborted task to completion before purging its rows

    assert!(
        both_applied,
        "both rows with identical chain_order must eventually drain via the loop's \
         front-rewinding retry pass"
    );

    purge(&pool, &cleanup).await;
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(barrier_stream)
        .execute(&pool)
        .await
        .expect("cleanup barrier");
}

#[tokio::test]
async fn projection_lag_seconds_empty_queue_is_zero() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    // projection_lag_seconds and count_pending_projection are global over ALL
    // eligible pending rows, not scoped to this test, and run as separate reads
    // on a shared DB (REPROJECTION_LOCK is in-process only, so it does not cross
    // nextest's process-per-test). Read the count on both sides of the lag read
    // and assert the empty-queue==0 contract only when the queue is observably
    // empty across both — then it was empty during the lag read too. Otherwise a
    // concurrent row makes the snapshots disagree and we skip rather than flake.
    let pending_before = repo.count_pending_projection().await.expect("count_pending_projection");
    let lag = repo.projection_lag_seconds().await.expect("projection_lag_seconds");
    let pending_after = repo.count_pending_projection().await.expect("count_pending_projection");
    if pending_before == 0 && pending_after == 0 {
        assert_eq!(lag, 0, "empty eligible queue must return 0");
    }
}

#[tokio::test]
async fn projection_lag_seconds_pending_row_has_positive_lag() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let test = "metrics_lag_pending";
    let src = format!("0:{test}_src");
    let msg_id = format!("{test}-msg");
    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id.as_str())]).await;

    // insert_raw uses created_at_chain = to_timestamp(1_700_000_000)
    // which is far in the past, so lag will be large and positive.
    insert_raw(&pool, &msg_id, &src, "Nullifier.VoucherGenerated", &serde_json::json!({})).await;

    let lag = repo.projection_lag_seconds().await.expect("projection_lag_seconds");
    // On the shared DB another nextest process may consume this eligible row via
    // reproject_* before the lag read. processed_at is monotonic (NULL -> set),
    // so if the row is still pending afterward it was pending during the read and
    // the global min therefore included its old timestamp -> lag > 0. If it was
    // consumed, skip rather than flake on a spurious 0.
    if !processed_at_is_set(&pool, &msg_id).await {
        assert!(
            lag > 0,
            "pending row with old created_at_chain must produce positive lag, got {lag}"
        );
    }

    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id.as_str())]).await;
}

#[tokio::test]
async fn projection_lag_seconds_null_chain_time_falls_back_to_ingest_time() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let msg_id = "metrics_lag_null_chain-msg";
    let src = "0:metrics_lag_null_chain_src";
    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id)]).await;

    // Eligible pending row whose gateway created_at was unparseable, so chain
    // time is NULL, but whose ingest time (created_at) is far in the past. A
    // bare min(created_at_chain) would be NULL and report 0 lag, hiding the
    // stale row; the coalesce fallback to created_at must surface it.
    sqlx::query(
        r#"insert into raw_events
               (msg_id, chain_order, created_at_chain, created_at, src_address,
                dst_address, event_type, body_json, decoded)
           values ($1, $2, NULL, to_timestamp($3), $4, $4,
                   'Nullifier.VoucherGenerated', '{}'::jsonb, '{}'::jsonb)"#,
    )
    .bind(msg_id)
    .bind(format!("5f80{msg_id:0>28}"))
    .bind(1_700_000_000_f64)
    .bind(src)
    .execute(&pool)
    .await
    .expect("insert null-chain raw_events");

    let lag = repo.projection_lag_seconds().await.expect("projection_lag_seconds");
    // Same shared-DB isolation as the positive-lag test (processed_at monotonic):
    // assert only while the row is still pending, so its created_at fallback was
    // in the global min -> lag > 0.
    if !processed_at_is_set(&pool, msg_id).await {
        assert!(lag > 0, "NULL chain time must fall back to ingest age, got {lag}");
    }

    purge(&pool, &[("delete from raw_events where msg_id = $1", msg_id)]).await;
}

#[tokio::test]
async fn cursor_age_seconds_nonexistent_stream_is_none() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let stream = "metrics_cursor_age_nonexistent_stream";
    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("purge cursor");

    let age = repo.cursor_age_seconds(stream).await.expect("cursor_age_seconds");
    assert!(age.is_none(), "non-existent stream must return None");
}

#[tokio::test]
async fn cursor_age_seconds_known_stream_is_small() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    let stream = "metrics_cursor_age_known_stream";

    // Upsert a cursor row with updated_at = now().
    sqlx::query(
        r#"insert into indexer_cursors (stream_name, cursor, updated_at)
           values ($1, 'test-cursor', now())
           on conflict (stream_name)
           do update set cursor = excluded.cursor, updated_at = now()"#,
    )
    .bind(stream)
    .execute(&pool)
    .await
    .expect("upsert cursor");

    let age = repo.cursor_age_seconds(stream).await.expect("cursor_age_seconds");
    assert!(age.is_some(), "known stream must return Some");
    let age = age.unwrap();
    assert!((0..10).contains(&age), "cursor just updated must have age < 10s, got {age}");

    sqlx::query("delete from indexer_cursors where stream_name = $1")
        .bind(stream)
        .execute(&pool)
        .await
        .expect("cleanup cursor");
}

#[tokio::test]
async fn pool_connection_stats_is_callable_and_sane() {
    let _guard = REPROJECTION_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());

    // Issue a query to ensure at least one connection exists.
    let _: i32 = sqlx::query_scalar("select 1").fetch_one(&pool).await.expect("warmup query");

    let (in_use, idle) = repo.pool_connection_stats();
    // Pool state is live and read non-atomically (size()/num_idle() are separate
    // reads, and other test binaries share the pool), so exact counts are not
    // assertable without flaking. After the warmup query at least one connection
    // exists, so the total is positive — the strongest non-flaky check.
    assert!(
        in_use + idle >= 1,
        "expected >=1 connection after warmup, got in_use={in_use} idle={idle}"
    );
}
