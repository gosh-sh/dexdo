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

#[tokio::test]
async fn orderfilled_deferred_replays_after_orderplaced() {
    // Locks in the OrderBook deferred-replay contract: an OrderFilled that
    // arrives before its OrderPlaced must stay queued (processed_at = null),
    // and the next reprojection sweep — once the live_orders row exists —
    // must apply it. Without this, /api/v1/depth would inflate liquidity by
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
    .bind(json!({ "orderId": order_id, "filledAmount": "10" }))
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
        &json!({ "orderId": order_id, "filledAmount": "3000000" }),
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
        &json!({ "orderId": order_id, "filledAmount": "10000000" }),
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
        &json!({ "orderId": order_id, "filledAmount": "10000000" }),
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
        &json!({ "orderId": order_id, "filledAmount": "100" }),
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
        &json!({ "orderId": order_id, "filledAmount": "100" }),
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
        &json!({ "orderId": order_id, "filledAmount": "60" }),
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
