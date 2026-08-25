// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the inference reconciler path.
// Gated on TEST_DATABASE_URL: unset → skip.
//
//   cargo test -p dodex-infrastructure --test inference_reconciler

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
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
async fn at_head_round_trips() {
    let Some(pool) = setup().await else { return };
    let repo = IndexerRepository::new(pool.clone());
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head) values ('t_at_head','c',true)
         on conflict (stream_name) do update set at_head = excluded.at_head",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(repo.at_head("t_at_head").await.unwrap());
    sqlx::query("update indexer_cursors set at_head=false where stream_name='t_at_head'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!repo.at_head("t_at_head").await.unwrap());
    // Missing stream ⇒ not at head.
    assert!(!repo.at_head("t_missing_stream").await.unwrap());
}

// ---- discovery / sweep helpers ----

use dodex_infrastructure::inference_reconciler::InferenceReconciler;

async fn seed_market(pool: &sqlx::PgPool, ob: &str, reconciled: bool) {
    sqlx::query("delete from inference_orders where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address=$1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain, last_reconciled_at)
                 values ($1, to_timestamp(1700000000), case when $2 then now() else null end)",
    )
    .bind(ob)
    .bind(reconciled)
    .execute(pool)
    .await
    .unwrap();
}

async fn open_order(pool: &sqlx::PgPool, ob: &str, id: i64) {
    sqlx::query(
        "insert into inference_orders (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining, status, last_chain_order)
                 values ($1,$2,true,1,5,5,'OPEN','co')",
    )
    .bind(ob)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// An OPEN subscription row (`is_subscription = true`) — the spec §6 sweep
/// target: a long-lived subscription that only ever emitted `Refunded`, so the
/// event stream never closes it and the phantom sweep must.
async fn open_subscription_order(pool: &sqlx::PgPool, ob: &str, id: i64) {
    sqlx::query(
        "insert into inference_orders (orderbook_address, order_id, is_buy, is_subscription, price, amount_initial, amount_remaining, status, last_chain_order)
                 values ($1,$2,true,true,1,5,5,'OPEN','co')",
    )
    .bind(ob)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// An OPEN SELL with neither TokenContract nor deadline — what the projector leaves behind
/// when a fill arrives before the placement it belongs to, and the only row shape that
/// closes the read gate.
async fn open_sell_order(pool: &sqlx::PgPool, ob: &str, id: i64) {
    sqlx::query(
        "insert into inference_orders (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining, status, last_chain_order)
                 values ($1,$2,false,1,5,5,'OPEN','co')",
    )
    .bind(ob)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// A BUY MARKET/IOC taker that matched partially: still OPEN with ticks left, because the
/// chain refunded the remainder without emitting an order-id-bearing event.
async fn partially_filled_buy(pool: &sqlx::PgPool, ob: &str, id: i64) {
    sqlx::query(
        "insert into inference_orders (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining, status, last_chain_order)
                 values ($1,$2,true,1,5,2,'OPEN','co')",
    )
    .bind(ob)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// The zero address an ABI decodes for an unset `address` field — what a BUY placement
/// and a BUY order's getter carry in place of a TokenContract.
const ZERO_ADDRESS: &str = "0:0000000000000000000000000000000000000000000000000000000000000000";

// ---- discovery / sweep DB-side tests ----

#[tokio::test]
async fn pending_events_gate_detects_unprojected_rows() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_pending_gate";
    seed_market(&pool, ob, false).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let r = InferenceReconciler::for_test(pool.clone());
    assert!(!r.pending_events_exist(ob).await.unwrap(), "no rows ⇒ gate open");
    // An unprojected decoded inference event for this book ⇒ gate closed.
    sqlx::query(
        "insert into raw_events (msg_id, chain_order, src_address, event_type, body_json, decoded, processed_at)
                 values ('pg-1','co', $1, 'InferenceOrderBook.Filled','{}'::jsonb,'{}'::jsonb, null)",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    assert!(r.pending_events_exist(ob).await.unwrap(), "unprojected row ⇒ gate closed");
    // A processed row does NOT close the gate.
    sqlx::query("update raw_events set processed_at=now() where msg_id='pg-1'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!r.pending_events_exist(ob).await.unwrap());
    // Fourth state — the narrowing conjuncts (IX-REC-15). An undecodable row
    // (event_type NULL, decoded NULL, processed_at NULL) must NOT close the
    // sweep gate: it can never be projected, so it would wedge the sweep
    // forever (the rationale documented at inference_read_repo.rs and
    // indexer.md). The READ gate deliberately counts exactly such rows — its
    // half of the asymmetry is pinned by
    // gate_refuses_while_any_message_for_the_book_is_unprojected
    // (inference_orders_repo.rs); this is the sweep half of IX-REC-28.
    sqlx::query(
        "insert into raw_events (msg_id, chain_order, src_address, event_type, body_json, decoded, processed_at)
                 values ('pg-undecodable','co-u', $1, null,'{}'::jsonb, null, null)",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !r.pending_events_exist(ob).await.unwrap(),
        "an undecodable row must not close the sweep gate — it can never be projected"
    );
    // Unlike pg-1 (left processed, harmless), this row is pending forever and
    // its fresh created_at would land the book in the e2e observer's
    // inference_books_with_events_since window — delete it, not just this
    // test's next run's purge.
    sqlx::query("delete from raw_events where msg_id='pg-undecodable'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn provisional_cancel_marks_swept() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_prov_cancel";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    let r = InferenceReconciler::for_test(pool.clone());
    r.provisional_cancel(ob, &["1".to_string()]).await.unwrap();
    let (status, swept_not_null): (String, bool) = sqlx::query_as(
        "select status, swept_at is not null from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), swept_not_null), ("CANCELLED", true));
}

#[tokio::test]
async fn discovery_stamp_is_optimistic_blocks_on_override() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_opt_stamp";
    let r = InferenceReconciler::for_test(pool.clone());
    let reconciled_at = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "select last_reconciled_at from inference_markets where orderbook_address='0:t_opt_stamp'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    // (1) Mid-cycle override: read prev_cursor=5/seq=0, then an override resets cursor=NULL + bumps seq.
    seed_market(&pool, ob, false).await;
    sqlx::query(
        "update inference_markets set sweep_cursor=5, sweep_cycle_max=10, sweep_override_seq=0 where orderbook_address=$1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "update inference_markets set sweep_cursor=null, sweep_override_seq=1 where orderbook_address=$1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, Some("5"), "10", None, true, true, 0).await.unwrap();
    assert!(!stamped, "mid-cycle override ⇒ stamp blocked");
    assert!(reconciled_at(pool.clone()).await.is_none());

    // (2) FIRST-TICK override (prev_cursor=NULL): the cursor-CAS alone cannot see a
    //     reset-to-NULL, but the override bumped the seq from 0→1, so the stamp is blocked.
    seed_market(&pool, ob, false).await;
    sqlx::query(
        "update inference_markets set sweep_cursor=null, sweep_cycle_max=10, sweep_override_seq=1 where orderbook_address=$1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, None, "10", None, true, true, 0).await.unwrap();
    assert!(
        !stamped,
        "first-tick (NULL prev) override must still block the stamp via the seq guard"
    );
    assert!(reconciled_at(pool.clone()).await.is_none());

    // (3) Clean completion (cursor + seq unchanged) ⇒ stamps.
    seed_market(&pool, ob, false).await;
    sqlx::query(
        "update inference_markets set sweep_cursor=5, sweep_cycle_max=10, sweep_override_seq=0 where orderbook_address=$1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, Some("5"), "10", None, true, true, 0).await.unwrap();
    assert!(stamped);
    assert!(reconciled_at(pool.clone()).await.is_some());
}

// ---- getter-seam sweep tests ----

use dodex_infrastructure::inference_reconciler::DiscoveryOutcome;
use dodex_infrastructure::inference_reconciler::OrderBookGetter;
use dodex_infrastructure::inference_reconciler::SlotClaim;
use dodex_infrastructure::inference_reconciler::SweepStep;
use dodex_infrastructure::tvm_runner::TvmGetterError;
use serde_json::json;
use serde_json::Value;

struct FnGetter<F>(F);
impl<F: Fn(&str, &Value) -> anyhow::Result<Value> + Send + Sync> OrderBookGetter for FnGetter<F> {
    fn call(&self, _boc: &str, name: &str, args: &Value) -> anyhow::Result<Value> {
        (self.0)(name, args)
    }
}

// The at-head gate reads a single shared `indexer_cursors` row keyed by the
// fixed `blockchain_events` stream name, so tests that flip `at_head` clobber
// each other under the default parallel runner. Serialize them through one
// process-wide lock; each acquires it for its whole body.
static AT_HEAD_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn set_at_head(pool: &sqlx::PgPool, v: bool) {
    sqlx::query(
        "insert into indexer_cursors (stream_name, cursor, at_head) values ('test_inference_at_head_stream','c',$1)
                 on conflict (stream_name) do update set at_head = excluded.at_head",
    )
    .bind(v)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn fill_params_writes_model_hash_and_constants() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_fillparams";
    seed_market(&pool, ob, false).await;
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getParams" => Ok(json!({"modelHash":"0x2a","platformFeeBps":"0x64"})), // 42, 100
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    r.fill_params(ob, "boc").await.unwrap();
    let (mh, fee, qt, pp): (Option<String>, Option<i32>, Option<i32>, Option<i32>) =
        sqlx::query_as(
            "select model_hash::text, platform_fee_bps, quote_token_type, price_precision from inference_markets where orderbook_address=$1",
        )
        .bind(ob)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((mh.as_deref(), fee, qt, pp), (Some("42"), Some(100), Some(2), Some(9)));
}

// `fill_params` also captures model identity from the `getModelName` getter. The
// model `version` (3rd part) lands in `model_version`, NOT the `version` column —
// that one holds the contract version (getVersion) for slot-supersede resolution.
async fn identity_cols(
    pool: &sqlx::PgPool,
    ob: &str,
) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    sqlx::query_as(
        "select model_ref, producer, model_name, model_version from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(pool)
    .await
    .unwrap()
}

// Each identity test gets a DISTINCT `model_hash` so they never contend for the
// single non-superseded slot in the shared test DB (`inference_markets_model_hash_idx`).
fn identity_getter(
    model_hash_hex: &'static str,
    model_name_value0: serde_json::Value,
) -> std::sync::Arc<impl OrderBookGetter> {
    std::sync::Arc::new(FnGetter(move |name: &str, _a: &Value| match name {
        "getParams" => Ok(json!({ "modelHash": model_hash_hex, "platformFeeBps": "0x64" })),
        "getModelName" => Ok(json!({ "value0": model_name_value0.clone() })),
        _ => Ok(json!({})),
    }))
}

#[tokio::test]
async fn fill_params_captures_three_part_model_identity() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_identity_3part";
    seed_market(&pool, ob, false).await;
    let g = identity_getter("0xf0001", json!("qwen--qwen2.5-32b--instruct"));
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .fill_params(ob, "boc")
        .await
        .unwrap();
    let (mref, producer, mname, mver) = identity_cols(&pool, ob).await;
    assert_eq!(mref.as_deref(), Some("qwen--qwen2.5-32b--instruct"));
    assert_eq!(producer.as_deref(), Some("qwen"));
    assert_eq!(mname.as_deref(), Some("qwen2.5-32b"));
    assert_eq!(mver.as_deref(), Some("instruct"));
}

#[tokio::test]
async fn fill_params_non_three_part_name_keeps_only_ref() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_identity_freeform";
    seed_market(&pool, ob, false).await;
    let g = identity_getter("0xf0002", json!("freeform name"));
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .fill_params(ob, "boc")
        .await
        .unwrap();
    let (mref, producer, mname, mver) = identity_cols(&pool, ob).await;
    assert_eq!(mref.as_deref(), Some("freeform name"));
    assert!(producer.is_none() && mname.is_none() && mver.is_none());
}

#[tokio::test]
async fn fill_params_empty_name_leaves_identity_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_identity_empty";
    seed_market(&pool, ob, false).await;
    let g = identity_getter("0xf0003", json!(""));
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .fill_params(ob, "boc")
        .await
        .unwrap();
    let (mref, producer, mname, mver) = identity_cols(&pool, ob).await;
    assert!(mref.is_none() && producer.is_none() && mname.is_none() && mver.is_none());
}

#[tokio::test]
async fn refresh_price_err_no_liquidity_is_null_success_writes() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_refprice";
    seed_market(&pool, ob, true).await;
    // Dry book ⇒ ERR_NO_LIQUIDITY (334) ⇒ price NULL but reference_price_at stamped.
    let dry = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getWeeklyMedianPrice" {
            Err(anyhow::Error::new(TvmGetterError {
                exit_code: 334,
                message: "ERR_NO_LIQUIDITY".into(),
            }))
        } else {
            Ok(json!({}))
        }
    }));
    InferenceReconciler::for_test_with_getter(pool.clone(), dry)
        .refresh_price(ob, "boc")
        .await
        .unwrap();
    let (price, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select reference_price::text, reference_price_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(price.is_none() && at.is_some(), "ERR_NO_LIQUIDITY ⇒ NULL price, stamped at");
    // Liquid book ⇒ value0 written.
    let liq = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getWeeklyMedianPrice" {
            Ok(json!({"value0":"0x3e8"})) // 1000
        } else {
            Ok(json!({}))
        }
    }));
    InferenceReconciler::for_test_with_getter(pool.clone(), liq)
        .refresh_price(ob, "boc")
        .await
        .unwrap();
    let price: Option<String> = sqlx::query_scalar(
        "select reference_price::text from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(price.as_deref(), Some("1000"));
}

#[tokio::test]
async fn sweep_idle_gate_blocks_when_queue_nonempty() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_idlegate";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0":"0x1"})),
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    assert!(matches!(r.run_sweep_step(ob, "boc", false).await.unwrap(), SweepStep::GatesFailed));
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "idle gate failed ⇒ no cancel");
}

#[tokio::test]
async fn sweep_cancels_empty_order_when_gates_open() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sweepcancel";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0":"0x0"})),
        "getStats" => Ok(json!({"nextOrderId":"0xa"})), // boundary = 10
        "getOrder" => Ok(json!({"note":"0:0","amount":"0x0","isBuy":true})), // empty ⇒ phantom
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    let _ = r.run_sweep_step(ob, "boc", false).await.unwrap();
    let (status, swept): (String, bool) = sqlx::query_as(
        "select status, swept_at is not null from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), swept),
        ("CANCELLED", true),
        "empty getOrder ⇒ provisional sweep-cancel"
    );
}

struct RecGetter {
    queue: i64,
    next_id: i64,
    empty: std::collections::HashSet<String>,
    calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
}

impl RecGetter {
    fn new(queue: i64, next_id: i64, empty: &[&str]) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            queue,
            next_id,
            empty: empty.iter().map(|s| s.to_string()).collect(),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn order_ids(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == "getOrder")
            .filter_map(|(_, id)| id.clone())
            .collect()
    }
}

impl OrderBookGetter for RecGetter {
    fn call(&self, _boc: &str, name: &str, args: &Value) -> anyhow::Result<Value> {
        let id = args.get("id").and_then(|v| v.as_str()).map(String::from);
        self.calls.lock().unwrap().push((name.to_string(), id.clone()));
        Ok(match name {
            "getQueueSize" => json!({"value0": format!("0x{:x}", self.queue)}),
            "getStats" => json!({"nextOrderId": format!("0x{:x}", self.next_id)}),
            "getOrder" => {
                let i = id.unwrap();
                if self.empty.contains(&i) {
                    json!({"amount":"0x0","note":"0:0","isBuy":true})
                } else {
                    json!({"amount":"0xa","note":"0:n","isBuy":true})
                }
            }
            "getWeeklyMedianPrice" => json!({"value0":"0x0"}),
            "getParams" => json!({"modelHash":"0x1","platformFeeBps":"0x0"}),
            _ => json!({}),
        })
    }
}

async fn r_run(pool: &sqlx::PgPool, g: std::sync::Arc<RecGetter>, ob: &str) {
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .run_sweep_step(ob, "boc", false)
        .await
        .unwrap();
}

#[tokio::test]
async fn discovery_sweep_bounded_advances_across_ticks_resumes_after_restart_stamps_on_completion()
{
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_bounded";
    seed_market(&pool, ob, false).await;
    for id in [1, 2, 3] {
        open_order(&pool, ob, id).await;
    }
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // batch N=2, boundary nextOrderId=4, no empties (rows stay OPEN, cursor just advances).
    let g1 = RecGetter::new(0, 4, &[]);
    let r1 =
        InferenceReconciler::for_test_with_getter(pool.clone(), g1.clone()).with_sweep_batch_n(2);
    // Tick 1: batch [1,2] == N ⇒ Continued; cursor=2, cycle_max=4; NOT stamped.
    assert!(matches!(r1.run_sweep_step(ob, "boc", true).await.unwrap(), SweepStep::Continued));
    assert_eq!(g1.order_ids(), vec!["1", "2"], "at most N getOrder per tick, lowest ids first");
    let (cur, cmax, rec): (
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "select sweep_cursor::text, sweep_cycle_max::text, last_reconciled_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((cur.as_deref(), cmax.as_deref(), rec.is_none()), (Some("2"), Some("4"), true));

    // RESTART: a FRESH reconciler resumes from the persisted cursor.
    let g2 = RecGetter::new(0, 4, &[]);
    let r2 =
        InferenceReconciler::for_test_with_getter(pool.clone(), g2.clone()).with_sweep_batch_n(2);
    assert!(matches!(r2.run_sweep_step(ob, "boc", true).await.unwrap(), SweepStep::CycleComplete));
    assert_eq!(g2.order_ids(), vec!["3"], "resume from cursor 2 — ids 1,2 NOT re-probed");
    let (cur, rec): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "select sweep_cursor::text, last_reconciled_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        cur.is_none() && rec.is_some(),
        "cycle complete ⇒ cursor reset NULL + visibility stamped only now"
    );
}

#[tokio::test]
async fn sweep_wraps_against_fixed_boundary_not_live_next_order_id() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_wrap";
    seed_market(&pool, ob, true).await; // reconciled ⇒ Queue B sweep (discovery=false)
    for id in [1, 2, 10] {
        open_order(&pool, ob, id).await;
    }
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Cycle 1: snapshot boundary = nextOrderId = 3 ⇒ id=10 (>3) is excluded THIS cycle.
    let g1 = RecGetter::new(0, 3, &[]); // default batch 50 ⇒ one batch covers (−1,3)
    let _ = r_run(&pool, g1.clone(), ob).await;
    assert_eq!(
        g1.order_ids(),
        vec!["1", "2"],
        "id minted past the snapshot boundary is NOT probed this cycle"
    );
    // Cycle complete ⇒ cursor reset; next cycle re-snapshots a higher boundary ⇒ id=10 now probed.
    let g2 = RecGetter::new(0, 11, &[]);
    let _ = r_run(&pool, g2.clone(), ob).await;
    assert!(
        g2.order_ids().contains(&"10".to_string()),
        "id=10 re-probed in the next cycle (low ids also re-scanned)"
    );
}

#[tokio::test]
async fn sweep_does_not_probe_the_next_unassigned_order_id() {
    let ob = "0:t_sweep_bound";
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_order(&pool, ob, 5).await; // id == getStats().nextOrderId
                                    // queue idle; nextOrderId = 5; id "5" would report amount == 0 and be cancelled.
    let g = RecGetter::new(0, 5, &["5"]);

    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    r.run_sweep_step(ob, "boc", false).await.unwrap();

    assert!(g.order_ids().is_empty(), "the next unassigned id must never be probed");
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=5",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "the boundary id must not be probed against a possibly stale BOC");
}

#[tokio::test]
async fn sweep_does_not_probe_ids_a_rolled_back_account_state_cannot_answer_for() {
    let ob = "0:t_sweep_rollback";
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_order(&pool, ob, 7).await;
    // Mid-cycle: the stored boundary came from an older, further-ahead account state.
    sqlx::query("update inference_markets set sweep_cursor=1, sweep_cycle_max=20 where orderbook_address=$1")
        .bind(ob).execute(&pool).await.unwrap();

    // This BOC has rolled back: it has not assigned id 7 yet.
    let g = RecGetter::new(0, 5, &["7"]);
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    r.run_sweep_step(ob, "boc", false).await.unwrap();

    assert!(g.order_ids().is_empty(), "a rolled-back state cannot answer for id 7");
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=7",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status, "OPEN",
        "a stored cycle boundary must not authorize probing a rolled-back state"
    );
}

/// The clamp above must not be read as "the cycle finished". A short batch means the
/// *bound* was reached, and when the BOC clamps the bound below `cycle_max` the ids in
/// `[boc_next_order_id, cycle_max)` were never looked at. Reporting `CycleComplete` there
/// resets the cursor and — under discovery — stamps `last_reconciled_at` for a book the
/// sweep only half-inspected.
#[tokio::test]
async fn a_batch_clamped_by_a_rolled_back_boc_does_not_complete_the_cycle() {
    let ob = "0:t_sweep_clamped_cycle";
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    seed_market(&pool, ob, /* reconciled= */ false).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_order(&pool, ob, 2).await; // below the clamp: probed, alive
    open_order(&pool, ob, 7).await; // above the clamp: never probed
    sqlx::query("update inference_markets set sweep_cursor=1, sweep_cycle_max=20 where orderbook_address=$1")
        .bind(ob).execute(&pool).await.unwrap();

    let g = RecGetter::new(0, 5, &[]); // nextOrderId=5 ⇒ effective bound 5 < cycle_max 20
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    let step = r.run_sweep_step(ob, "boc", /* discovery= */ true).await.unwrap();

    assert!(matches!(step, SweepStep::Continued), "a clamped batch has not finished the cycle");
    assert_eq!(g.order_ids(), vec!["2".to_string()], "only ids below the clamp are probed");

    let (cursor, cycle_max, reconciled): (Option<String>, Option<String>, Option<i64>) =
        sqlx::query_as(
            "select sweep_cursor::text, sweep_cycle_max::text,
                               extract(epoch from last_reconciled_at)::bigint
                          from inference_markets where orderbook_address=$1",
        )
        .bind(ob)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cursor.as_deref(), Some("2"), "the cursor keeps the progress it made");
    assert_eq!(cycle_max.as_deref(), Some("20"), "the cycle keeps its own boundary");
    assert!(reconciled.is_none(), "an unfinished cycle must not stamp visibility");
}

/// An empty clamped batch (every id below the clamp is already swept) must not rewind the
/// cycle to its start either — `ids.last()` is `None` there, and a bare `None` cursor is
/// how a *completed* cycle is spelled.
#[tokio::test]
async fn an_empty_clamped_batch_leaves_the_cursor_where_it_was() {
    let ob = "0:t_sweep_clamped_empty";
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_order(&pool, ob, 7).await; // above the clamp
    sqlx::query("update inference_markets set sweep_cursor=4, sweep_cycle_max=20 where orderbook_address=$1")
        .bind(ob).execute(&pool).await.unwrap();

    let g = RecGetter::new(0, 5, &[]);
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    assert!(matches!(r.run_sweep_step(ob, "boc", false).await.unwrap(), SweepStep::Continued));

    let cursor: Option<String> = sqlx::query_scalar(
        "select sweep_cursor::text from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursor.as_deref(), Some("4"), "an empty clamped batch must not reset the cursor");
}

#[tokio::test]
async fn sweep_at_head_false_gate_blocks_even_when_idle_and_no_pending() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_athead_gate";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    set_at_head(&pool, false).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let g = RecGetter::new(0, 5, &[]);
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    assert!(
        matches!(r.run_sweep_step(ob, "boc", false).await.unwrap(), SweepStep::GatesFailed),
        "at_head=false ⇒ no sweep EVEN IF idle and pending-empty"
    );
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    let lsw: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select last_swept_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), lsw.is_none()),
        ("OPEN", true),
        "gate miss ⇒ no cancel, last_swept_at untouched"
    );
}

#[tokio::test]
async fn real_getter_failure_surfaces_as_err_not_silent_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_realfail";
    seed_market(&pool, ob, true).await;
    // A non-ERR_NO_LIQUIDITY getter error must propagate as Err, NOT be swallowed.
    let boom_price = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getWeeklyMedianPrice" {
            Err(anyhow::Error::new(TvmGetterError { exit_code: 999, message: "boom".into() }))
        } else {
            Ok(json!({}))
        }
    }));
    assert!(
        InferenceReconciler::for_test_with_getter(pool.clone(), boom_price)
            .refresh_price(ob, "boc")
            .await
            .is_err(),
        "non-334 getter error must be Err"
    );
    let at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select reference_price_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        at.is_none(),
        "a real failure must NOT stamp reference_price_at (unlike ERR_NO_LIQUIDITY)"
    );
    // fill_params likewise surfaces a getter error.
    let boom_params = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getParams" {
            Err(anyhow::anyhow!("getter boom"))
        } else {
            Ok(json!({}))
        }
    }));
    assert!(InferenceReconciler::for_test_with_getter(pool.clone(), boom_params)
        .fill_params(ob, "boc")
        .await
        .is_err());
}

#[tokio::test]
async fn discovery_composes_params_price_phantom_cancel_then_stamps_visibility() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_disc_compose";
    // Freshly seeded: last_reconciled_at / model_hash / reference_price_at all NULL.
    seed_market(&pool, ob, false).await;
    open_order(&pool, ob, 1).await; // resting OPEN row that the book no longer holds
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // One getter drives the whole composition: idle queue, params/price present,
    // a low boundary so the single OPEN id completes the cycle in one tick, and an
    // empty getOrder so id 1 is a phantom the sweep cancels.
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0": "0x0"})),
        // model_hash is UNIQUE across markets — use a value no other test writes.
        "getParams" => Ok(json!({"modelHash": "0x2329", "platformFeeBps": "0x1e"})), // 9001, 30
        "getWeeklyMedianPrice" => Ok(json!({"value0": "0x64"})),                     // 100
        "getStats" => Ok(json!({"nextOrderId": "0xa"})),                             // boundary 10
        "getOrder" => Ok(json!({"note": "0:0", "amount": "0x0", "isBuy": true})), /* empty ⇒ phantom */
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);

    let outcome = r.reconcile_discovery_with_boc(ob, "boc", true, true).await.unwrap();
    assert_eq!(outcome, DiscoveryOutcome::Stamped, "clean one-tick cycle must stamp visibility");

    // Every composed step landed: params + constants, reference price, phantom
    // cancelled, and the visibility stamp — the "never visible with phantom
    // liquidity" guarantee end-to-end.
    let (model_hash, fee, price, reconciled): (
        Option<String>,
        i32,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "select model_hash::text, platform_fee_bps, reference_price::text, last_reconciled_at
           from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(model_hash.as_deref(), Some("9001"), "fill_params wrote model_hash");
    assert_eq!(fee, 30, "fill_params wrote platform_fee_bps");
    assert_eq!(price.as_deref(), Some("100"), "refresh_price wrote reference_price");
    assert!(reconciled.is_some(), "completed clean cycle stamps last_reconciled_at");

    let (status, swept): (String, bool) = sqlx::query_as(
        "select status, swept_at is not null from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), swept),
        ("CANCELLED", true),
        "phantom swept-cancelled in the same pass"
    );
}

#[tokio::test]
async fn discovery_with_busy_queue_fills_params_but_leaves_book_invisible() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_disc_busy";
    seed_market(&pool, ob, false).await;
    open_order(&pool, ob, 1).await;
    // at_head true and no pending events, so the ONLY failing gate is the busy queue.
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0": "0x1"})), // in-flight queue ⇒ idle gate fails
        // model_hash is UNIQUE across markets — distinct from every other test.
        "getParams" => Ok(json!({"modelHash": "0x232a", "platformFeeBps": "0x1e"})), // 9002
        "getWeeklyMedianPrice" => Ok(json!({"value0": "0x64"})),
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);

    let outcome = r.reconcile_discovery_with_boc(ob, "boc", true, true).await.unwrap();
    assert_eq!(outcome, DiscoveryOutcome::WaitingGates, "busy snapshot must not stamp");

    // Params/price are filled (they precede the sweep), but the book stays invisible
    // and the order is untouched until a clean sweep cycle can run.
    let (model_hash, reconciled): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as(
            "select model_hash::text, last_reconciled_at from inference_markets where orderbook_address=$1",
        )
        .bind(ob)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(model_hash.as_deref(), Some("9002"), "params fill before the sweep gate");
    assert!(reconciled.is_none(), "busy queue ⇒ last_reconciled_at stays NULL");
    let status: String = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "OPEN", "no sweep ran ⇒ order untouched");
}

// ---- Queue B refresh tests ----

#[tokio::test]
async fn refresh_selects_price_due_sweep_due_and_skips_fresh() {
    let Some(pool) = setup().await else { return };
    let r = InferenceReconciler::for_test(pool.clone()); // refresh=3600s, sweep=30s

    // (a) price-due: reconciled, reference_price_at stale (2h old), no open rows.
    let a = "0:t_qb_price";
    seed_market(&pool, a, true).await;
    sqlx::query("update inference_markets set reference_price_at = now() - interval '2 hours', last_swept_at = now() where orderbook_address=$1")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();

    // (b) sweep-due: reconciled, fresh price, has OPEN row, last_swept_at stale (1 min old).
    let b = "0:t_qb_sweep";
    seed_market(&pool, b, true).await;
    open_order(&pool, b, 1).await;
    sqlx::query("update inference_markets set reference_price_at = now(), last_swept_at = now() - interval '1 minute' where orderbook_address=$1")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();

    // (c) never-swept: reconciled, fresh price, has OPEN row, last_swept_at NULL ⇒ sweep-due (IS NULL arm).
    let c = "0:t_qb_nullswept";
    seed_market(&pool, c, true).await;
    open_order(&pool, c, 1).await;
    sqlx::query("update inference_markets set reference_price_at = now(), last_swept_at = null where orderbook_address=$1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();

    // (d) fresh everything: reconciled, fresh price, fresh sweep ⇒ NOT selected.
    let d = "0:t_qb_fresh";
    seed_market(&pool, d, true).await;
    open_order(&pool, d, 1).await;
    sqlx::query("update inference_markets set reference_price_at = now(), last_swept_at = now() where orderbook_address=$1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();

    // Scope the selection to just these four books so concurrent never-priced
    // visible markets elsewhere in the shared test DB cannot crowd the
    // `reference_price_at nulls first` LIMIT window and evict our candidates.
    let scope = vec![a.to_string(), b.to_string(), c.to_string(), d.to_string()];
    let picked = r.select_refresh_candidates_within(&scope).await.unwrap();
    assert!(picked.contains(&a.to_string()), "price-due must be selected");
    assert!(picked.contains(&b.to_string()), "sweep-due must be selected");
    assert!(picked.contains(&c.to_string()), "never-swept (last_swept_at NULL) must be selected");
    assert!(!picked.contains(&d.to_string()), "fresh book must NOT be selected");
}

// The reconciler bumps a write-handle; metrics-refresh reads the count getter.
// Both must address the same atomic or the metric would always report 0.
#[tokio::test]
async fn reconcile_failures_handle_shares_state_with_count() {
    let Some(pool) = setup().await else { return };
    let repo = dodex_infrastructure::indexer_repo::IndexerRepository::new(pool);
    let handle = repo.inference_reconcile_failures_handle();
    assert_eq!(repo.inference_reconcile_failures_count(), 0);
    handle.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(repo.inference_reconcile_failures_count(), 3);
}

// run_once bumps the shared counter by exactly stats.failed. Deterministic
// under concurrency: each IndexerRepository instance owns its own counter Arc,
// so only this reconciler writes it. The seeded discovery book guarantees the
// for_test reconciler (unreachable GraphQL) records >= 1 failure; concurrent
// discovery rows on the shared DB only add more, and the equality to
// stats.failed still holds.
#[tokio::test]
async fn run_once_counts_hard_failures_into_shared_counter() {
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let Some(pool) = setup().await else { return };
    let repo = dodex_infrastructure::indexer_repo::IndexerRepository::new(pool.clone());
    let reconciler =
        dodex_infrastructure::inference_reconciler::InferenceReconciler::for_test(pool.clone())
            .with_failure_counter(repo.inference_reconcile_failures_handle());

    // A discovery book (never reconciled, never failed) the reconciler will
    // scan and fail on BOC fetch. model_hash NULL avoids the UNIQUE partial index.
    let ob = "inf_recon_fail_test.book";
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("purge");
    sqlx::query("insert into inference_markets (orderbook_address) values ($1)")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("seed discovery book");

    let before = repo.inference_reconcile_failures_count();
    let stats = reconciler.run_once().await.expect("run_once");
    let after = repo.inference_reconcile_failures_count();

    assert!(stats.failed >= 1, "seeded book should fail BOC fetch");
    assert_eq!(after - before, stats.failed, "counter advances by exactly stats.failed");

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
async fn failure_stamps_backoff_and_excludes_from_reselection() {
    let Some(pool) = setup().await else { return };
    let r = InferenceReconciler::for_test(pool.clone()); // refresh=3600s

    let ob = "0:t_qb_backoff";
    seed_market(&pool, ob, true).await;
    // Price-due: stale reference_price_at so it qualifies for Queue B.
    sqlx::query("update inference_markets set reference_price_at = now() - interval '2 hours', last_reconcile_failed_at = null, reconcile_attempts = 0 where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Scope the selection to just this book so concurrent never-priced visible
    // markets elsewhere in the shared test DB cannot crowd it out of the
    // `nulls first` LIMIT window (see
    // refresh_selects_price_due_sweep_due_and_skips_fresh).
    let scope = vec![ob.to_string()];
    // Confirm selected before any failure.
    let before = r.select_refresh_candidates_within(&scope).await.unwrap();
    assert!(before.contains(&ob.to_string()), "price-due book must be selected before failure");

    // Stamp a failure.
    r.stamp_failure(ob, "probe: stamped by the attempts test").await.unwrap();

    // Within backoff window ⇒ must be excluded.
    let after = r.select_refresh_candidates_within(&scope).await.unwrap();
    assert!(
        !after.contains(&ob.to_string()),
        "within 5-min backoff ⇒ must be excluded from selection"
    );

    // Verify reconcile_attempts was incremented.
    let attempts: i32 = sqlx::query_scalar(
        "select reconcile_attempts from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1, "stamp_failure must increment reconcile_attempts");

    // Simulate backoff window expiry: set last_reconcile_failed_at > 5 min ago.
    sqlx::query("update inference_markets set last_reconcile_failed_at = now() - interval '6 minutes' where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // Must be selectable again.
    let after_expiry = r.select_refresh_candidates_within(&scope).await.unwrap();
    assert!(
        after_expiry.contains(&ob.to_string()),
        "after backoff window expires ⇒ must be selectable again"
    );
}

// ---- Queue B refresh EXECUTION layer (price-vs-sweep cadence decoupling) ----
// select_refresh_candidates (above) covers the SELECT; these drive the method
// that acts on a selected book, asserting each cadence gates independently.

#[tokio::test]
async fn refresh_sweeps_due_book_without_repricing_fresh_price() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_refresh_sweep_only";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    // Price FRESH (within the 3600s refresh interval) but sweep DUE (last swept
    // 1h ago, past the 30s sweep interval). Sentinel price proves no re-price.
    sqlx::query(
        "update inference_markets
            set reference_price = 12345, reference_price_at = now(),
                last_swept_at = now() - interval '1 hour'
          where orderbook_address = $1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // getOrder reports the order empty (phantom ⇒ swept); getWeeklyMedianPrice
    // returns a DIFFERENT price so a stray re-price would be detectable.
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0":"0x0"})),
        "getStats" => Ok(json!({"nextOrderId":"0xa"})),
        "getOrder" => Ok(json!({"amount":"0x0","note":"0:0","isBuy":true})),
        "getWeeklyMedianPrice" => Ok(json!({"value0":"0x270f"})), // 9999 — must NOT be written
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    r.reconcile_refresh_with_boc(ob, "boc").await.unwrap();

    let (status, swept, price): (String, bool, i64) = sqlx::query_as(
        "select o.status, o.swept_at is not null, m.reference_price::bigint
           from inference_orders o join inference_markets m using (orderbook_address)
          where o.orderbook_address=$1 and o.order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), swept), ("CANCELLED", true), "sweep-due ⇒ phantom swept");
    assert_eq!(price, 12345, "price fresh ⇒ reference_price NOT re-fetched");
}

#[tokio::test]
async fn refresh_reprices_due_price_without_sweeping_fresh_book() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_refresh_price_only";
    seed_market(&pool, ob, true).await;
    open_order(&pool, ob, 1).await;
    // Price DUE (2h old, past the 3600s interval) but sweep FRESH (just swept).
    sqlx::query(
        "update inference_markets
            set reference_price = 12345, reference_price_at = now() - interval '2 hours',
                last_swept_at = now()
          where orderbook_address = $1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // getWeeklyMedianPrice returns a new price; getOrder would report empty, but
    // the sweep must NOT run (sweep fresh) so the OPEN row must survive.
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0":"0x0"})),
        "getStats" => Ok(json!({"nextOrderId":"0xa"})),
        "getOrder" => Ok(json!({"amount":"0x0","note":"0:0","isBuy":true})),
        "getWeeklyMedianPrice" => Ok(json!({"value0":"0x270f"})), // 9999 — MUST be written
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    r.reconcile_refresh_with_boc(ob, "boc").await.unwrap();

    let (status, price): (String, i64) = sqlx::query_as(
        "select o.status, m.reference_price::bigint
           from inference_orders o join inference_markets m using (orderbook_address)
          where o.orderbook_address=$1 and o.order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(price, 9999, "price due ⇒ reference_price re-fetched");
    assert_eq!(status, "OPEN", "sweep fresh ⇒ phantom NOT swept this tick");
}

#[tokio::test]
async fn discovery_persists_version() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_version_persist";
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    seed_market(&pool, ob, false).await; // bare skeleton: model_hash NULL, not reconciled

    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        Ok(match name {
            "getParams" => json!({"modelHash": "0x30000", "platformFeeBps": "0x64"}), /* 196608, 100 */
            "getVersion" => json!({"value0": "4.0.14", "value1": "InferenceOrderBook"}),
            _ => json!({}),
        })
    }));
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .fill_params(ob, "boc")
        .await
        .unwrap();

    let version: Option<String> =
        sqlx::query_scalar("select version from inference_markets where orderbook_address = $1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version.as_deref(), Some("4.0.14"));
}

#[tokio::test]
async fn sweep_cancels_empty_subscription_order() {
    // Spec §6: a long-lived OPEN subscription that only ever emitted Refunded is
    // never closed by events; the phantom sweep must cancel it once empty on-chain.
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sub_sweep";
    seed_market(&pool, ob, true).await;
    open_subscription_order(&pool, ob, 1).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({"value0":"0x0"})),
        "getStats" => Ok(json!({"nextOrderId":"0xa"})),
        "getOrder" => Ok(json!({"amount":"0x0","note":"0:0","isBuy":true})), // empty ⇒ phantom
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    let _ = r.run_sweep_step(ob, "boc", false).await.unwrap();
    let (status, swept, is_sub): (String, bool, bool) = sqlx::query_as(
        "select status, swept_at is not null, is_subscription from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), swept, is_sub),
        ("CANCELLED", true, true),
        "empty subscription phantom is swept (status/swept set; still flagged subscription)"
    );
}

// ---- model-slot supersede tests ----

// Getter answering getParams (fixed model) + getVersion (given version).
fn model_getter(
    model_hash_hex: &'static str,
    version: &'static str,
) -> std::sync::Arc<FnGetter<impl Fn(&str, &Value) -> anyhow::Result<Value> + Send + Sync>> {
    std::sync::Arc::new(FnGetter(move |name: &str, _a: &Value| {
        Ok(match name {
            "getParams" => json!({"modelHash": model_hash_hex, "platformFeeBps": "0x64"}),
            "getVersion" => json!({"value0": version, "value1": "InferenceOrderBook"}),
            _ => json!({}),
        })
    }))
}

#[tokio::test]
async fn lower_version_duplicate_is_retired_not_claimed() {
    let Some(pool) = setup().await else { return };
    let (canonical, stale) = ("0:t_canon_lo", "0:t_stale_lo");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(vec![canonical.to_string(), stale.to_string()])
        .execute(&pool)
        .await
        .unwrap();
    // Incumbent holds model 0x10000 (65536) at v4.0.14, visible.
    sqlx::query(
        "insert into inference_markets (orderbook_address, model_hash, version, last_reconciled_at)
         values ($1, 65536, '4.0.14', now())",
    )
    .bind(canonical)
    .execute(&pool)
    .await
    .unwrap();
    // Seed the loser as PREVIOUSLY VISIBLE (last_reconciled_at set, model_hash NULL):
    // retirement must hide it again, matching the incumbent-supersede branch.
    seed_market(&pool, stale, true).await;

    let claim =
        InferenceReconciler::for_test_with_getter(pool.clone(), model_getter("0x10000", "4.0.12"))
            .fill_params(stale, "boc")
            .await
            .unwrap();
    assert!(matches!(claim, SlotClaim::Superseded));

    let (stale_superseded, stale_mh, stale_version, stale_visible): (bool, Option<String>, Option<String>, bool) =
        sqlx::query_as(
            "select superseded_at is not null, model_hash::text, version, last_reconciled_at is not null
               from inference_markets where orderbook_address = $1",
        ).bind(stale).fetch_one(&pool).await.unwrap();
    assert!(stale_superseded, "lower-version duplicate must be retired");
    assert_eq!(stale_mh, None, "lower-version duplicate must not claim the slot");
    assert_eq!(
        stale_version.as_deref(),
        Some("4.0.12"),
        "retired row records the fetched version for audit"
    );
    assert!(!stale_visible, "retiring a previously-visible loser clears last_reconciled_at");

    let (canon_mh, canon_visible): (Option<String>, bool) = sqlx::query_as(
        "select model_hash::text, last_reconciled_at is not null
           from inference_markets where orderbook_address = $1",
    )
    .bind(canonical)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(canon_mh.as_deref(), Some("65536"), "incumbent keeps the slot");
    assert!(canon_visible, "incumbent stays visible");
}

#[tokio::test]
async fn higher_version_supersedes_incumbent_and_swaps_slot() {
    let Some(pool) = setup().await else { return };
    let (old, new) = ("0:t_old_hi", "0:t_new_hi");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(vec![old.to_string(), new.to_string()])
        .execute(&pool)
        .await
        .unwrap();
    // Incumbent holds model 0x20000 (131072) at an OLD version, visible.
    sqlx::query(
        "insert into inference_markets (orderbook_address, model_hash, version, last_reconciled_at)
         values ($1, 131072, '4.0.11', now())",
    )
    .bind(old)
    .execute(&pool)
    .await
    .unwrap();
    seed_market(&pool, new, false).await;

    let claim =
        InferenceReconciler::for_test_with_getter(pool.clone(), model_getter("0x20000", "4.0.14"))
            .fill_params(new, "boc")
            .await
            .unwrap();
    assert!(matches!(claim, SlotClaim::Claimed));

    let (old_mh, old_superseded, old_visible): (Option<String>, bool, bool) = sqlx::query_as(
        "select model_hash::text, superseded_at is not null, last_reconciled_at is not null
           from inference_markets where orderbook_address = $1",
    )
    .bind(old)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_mh, None, "old incumbent releases the slot");
    assert!(old_superseded, "old incumbent retired");
    assert!(!old_visible, "old incumbent drops out of the visible set");

    let (new_mh, new_version): (Option<String>, Option<String>) = sqlx::query_as(
        "select model_hash::text, version from inference_markets where orderbook_address = $1",
    )
    .bind(new)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_mh.as_deref(), Some("131072"), "new book claims the slot");
    assert_eq!(new_version.as_deref(), Some("4.0.14"));
}

#[tokio::test]
async fn select_discovery_candidates_excludes_superseded() {
    let Some(pool) = setup().await else { return };
    let (eligible, retired) = ("0:t_cand_eligible", "0:t_cand_retired");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(vec![eligible.to_string(), retired.to_string()])
        .execute(&pool)
        .await
        .unwrap();
    // Failed PAST the 5-minute backoff, NOT superseded -> must be selectable
    // (so the only thing that can exclude `retired` is the superseded_at guard).
    sqlx::query("insert into inference_markets (orderbook_address, last_reconcile_failed_at) values ($1, now() - interval '6 minutes')")
        .bind(eligible).execute(&pool).await.unwrap();
    sqlx::query("insert into inference_markets (orderbook_address, last_reconcile_failed_at, superseded_at) values ($1, now() - interval '6 minutes', now())")
        .bind(retired).execute(&pool).await.unwrap();

    let addrs: Vec<String> = InferenceReconciler::for_test(pool.clone())
        .select_discovery_candidates(i64::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.orderbook_address)
        .collect();
    assert!(addrs.iter().any(|a| a == eligible), "non-superseded failing book must be a candidate");
    assert!(!addrs.iter().any(|a| a == retired), "superseded book must be excluded from discovery");
}

#[tokio::test]
async fn superseded_row_is_not_stamped_visible() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_superseded_no_stamp";
    let r = InferenceReconciler::for_test(pool.clone());
    seed_market(&pool, ob, false).await;
    // Clean-completion CAS state that WOULD stamp (cursor + seq match), plus superseded_at.
    sqlx::query("update inference_markets set sweep_cursor=5, sweep_cycle_max=10, sweep_override_seq=0, superseded_at=now() where orderbook_address=$1")
        .bind(ob).execute(&pool).await.unwrap();

    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, Some("5"), "10", None, true, true, 0).await.unwrap();
    assert!(!stamped, "superseded row must not be stamped visible even on a clean completion");

    let reconciled: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "select last_reconciled_at from inference_markets where orderbook_address=$1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(reconciled.is_none(), "superseded row's last_reconciled_at stays NULL");
}

#[tokio::test]
async fn unknown_incoming_version_does_not_retire_on_conflict() {
    let Some(pool) = setup().await else { return };
    let (incumbent, incoming) = ("0:t_unk_incumbent", "0:t_unk_incoming");
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(vec![incumbent.to_string(), incoming.to_string()])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into inference_markets (orderbook_address, model_hash, version, last_reconciled_at) values ($1, 65537, '4.0.14', now())")
        .bind(incumbent).execute(&pool).await.unwrap();
    seed_market(&pool, incoming, false).await;

    // Same model (collision); getVersion carries no value0 -> unknown incoming version.
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        Ok(match name {
            "getParams" => json!({"modelHash": "0x10001", "platformFeeBps": "0x64"}), // 65537
            "getVersion" => json!({}),
            _ => json!({}),
        })
    }));
    let res = InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .fill_params(incoming, "boc")
        .await;
    assert!(
        res.is_err(),
        "unknown incoming version on a collision must fail discovery, not retire"
    );

    let incoming_superseded: bool = sqlx::query_scalar(
        "select superseded_at is not null from inference_markets where orderbook_address=$1",
    )
    .bind(incoming)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!incoming_superseded, "incoming with unknown version must NOT be retired");
}

// ---- sweep repairs token_contract / deadline from the getOrder probe ----

#[tokio::test]
async fn sweep_repairs_token_contract_and_deadline_independently() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sweep_repair";
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_sell_order(&pool, ob, 1).await; // SELL: real TC on chain, deadline 0
    open_order(&pool, ob, 2).await; // BUY:  zero TC on chain, real deadline

    let g = std::sync::Arc::new(FnGetter(|name: &str, a: &Value| match name {
        "getQueueSize" => Ok(json!({ "value0": "0" })),
        "getStats" => Ok(json!({ "nextOrderId": "9" })),
        "getOrder" => Ok(match a.get("id").and_then(|v| v.as_str()) {
            Some("1") => json!({ "amount": "5", "tokenContract": "0:deal", "deadline": "0" }),
            _ => json!({ "amount": "5", "tokenContract": ZERO_ADDRESS, "deadline": "1760003600" }),
        }),
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g.clone());
    r.run_sweep_step(ob, "boc", false).await.unwrap();

    let (tc, dl): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=1",
    ).bind(ob).fetch_one(&pool).await.unwrap();
    assert_eq!(tc.as_deref(), Some("0:deal"));
    assert!(dl.is_none(), "a zero deadline stays NULL");

    let (tc2, dl2): (Option<String>, Option<String>) = sqlx::query_as(
        "select token_contract, deadline::text from inference_orders where orderbook_address=$1 and order_id=2",
    ).bind(ob).fetch_one(&pool).await.unwrap();
    assert!(tc2.is_none(), "a BUY's zero TC stays NULL");
    assert_eq!(
        dl2.as_deref(),
        Some("1760003600"),
        "deadline repair must not depend on a TC being present"
    );

    // A second sweep must not rewrite a row whose remaining NULL is intentional.
    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    r.run_sweep_step(ob, "boc", false).await.unwrap();
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after, "a repaired row must not be rewritten on every cycle");
}

#[tokio::test]
async fn repair_conflates_a_sell_null_with_a_buy_null_when_the_getter_answers_zero() {
    // IX-REC-18, pinned deliberately (not fixed). A SELL with a NULL
    // token_contract whose getter answers the zero address is indistinguishable
    // from a BUY's intentional NULL: the repair consults the getter's normalized
    // answer, never the row's own is_buy (neither the candidate SELECT, nor the
    // needs-nothing skip, nor the UPDATE reads it). So the SELL gap stays — no
    // repair, no warn, no counter — and read-gate arm 1 (inference_read_repo.rs)
    // is what keeps serving MarketInconsistent for exactly this row. Changing
    // this is a reconciler-semantics decision, not a test's; the row's
    // normative "repair must close the SELL gap" half stays unimplemented and
    // the matrix keeps IX-REC-18 amber.
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sweep_conflation";
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_sell_order(&pool, ob, 1).await; // SELL with NULL TC — the gap repair SHOULD close
                                         // POSITIVE CONTROL. Every assert about order 1 below is a negative — the row
                                         // was not touched — and a sweep that never ran satisfies all of them: a
                                         // refused gate, a getter never called, an early return would each leave the
                                         // test green while proving nothing. Order 2 is a row the SAME pass MUST
                                         // repair, so a no-op pass turns this test red on the control instead of
                                         // passing it vacuously.
    open_sell_order(&pool, ob, 2).await;

    let g = std::sync::Arc::new(FnGetter(|name: &str, a: &Value| match name {
        "getQueueSize" => Ok(json!({ "value0": "0" })),
        "getStats" => Ok(json!({ "nextOrderId": "9" })),
        "getOrder" => Ok(match a.get("id").and_then(|v| v.as_str()) {
            // Order 1: the getter answers what a BUY would answer — zero TC, zero
            // deadline. This is the conflation under test.
            Some("1") => json!({ "amount": "5", "tokenContract": ZERO_ADDRESS, "deadline": "0" }),
            // Order 2: a real answer, so the repair has something to write.
            _ => json!({ "amount": "5", "tokenContract": "0:control-tc", "deadline": "0" }),
        }),
        _ => Ok(json!({})),
    }));
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);

    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    r.run_sweep_step(ob, "boc", false).await.unwrap();

    let (tc, dl, after): (Option<String>, Option<String>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "select token_contract, deadline::text, updated_at from inference_orders \
             where orderbook_address=$1 and order_id=1",
        )
        .bind(ob)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(tc.is_none(), "the SELL's NULL token_contract stays NULL — the conflation");
    assert!(dl.is_none(), "zero deadline normalizes to nothing to write");
    assert_eq!(before, after, "no repair happened: the row was not touched at all");

    // The control: the same pass repaired the row whose getter answered. Without
    // this the three negatives above are satisfied by a sweep that never ran.
    let control_tc: Option<String> = sqlx::query_scalar(
        "select token_contract from inference_orders where orderbook_address=$1 and order_id=2",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        control_tc.as_deref(),
        Some("0:control-tc"),
        "the sweep pass did not run at all — the negatives above prove nothing until this \
         control row is repaired"
    );
}

#[tokio::test]
async fn sweep_skips_the_repair_entirely_for_rows_that_need_nothing() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sweepnoop";
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    // A complete row: both columns already populated by the projector. It must be a SELL —
    // on chain a BUY always carries the zero TokenContract address, so a BUY row with a
    // non-zero TokenContract is chain-impossible and must not be used as a fixture here;
    // that would encode the same falsehood the read path depends on being false.
    open_sell_order(&pool, ob, 1).await;
    sqlx::query(
        "update inference_orders set token_contract='0:deal', deadline=1760003600 \
         where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getQueueSize" => Ok(json!({ "value0": "0" })),
        "getStats" => Ok(json!({ "nextOrderId": "9", "orderCount": "1" })),
        "getOrder" => {
            Ok(json!({ "amount": "5", "tokenContract": "0:deal", "deadline": "1760003600" }))
        }
        _ => Ok(json!({})),
    }));
    let rec = InferenceReconciler::for_test_with_getter(pool.clone(), g);

    let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    rec.run_sweep_step(ob, "boc", false).await.unwrap();
    let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "select updated_at from inference_orders where orderbook_address=$1 and order_id=1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert_eq!(before, after, "a complete row must not be written at all");
}

#[tokio::test]
async fn sweep_still_cancels_phantoms_and_partially_filled_buy_takers() {
    let Some(pool) = setup().await else { return };
    let _guard = AT_HEAD_GATE_LOCK.lock().await;
    let ob = "0:t_sweep_phantom";
    seed_market(&pool, ob, true).await;
    set_at_head(&pool, true).await;
    sqlx::query("delete from raw_events where src_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    open_sell_order(&pool, ob, 1).await; // a placement that never rested
    partially_filled_buy(&pool, ob, 2).await; // remainder refunded on chain, no event

    // nextOrderId = 9; both ids report amount == 0.
    let g = RecGetter::new(0, 9, &["1", "2"]);
    let r = InferenceReconciler::for_test_with_getter(pool.clone(), g);
    r.run_sweep_step(ob, "boc", false).await.unwrap();

    let statuses: Vec<String> = sqlx::query_scalar(
        "select status from inference_orders where orderbook_address=$1 order by order_id",
    )
    .bind(ob)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(statuses, vec!["CANCELLED", "CANCELLED"]);
}

#[tokio::test]
async fn a_clean_cycle_clears_a_failure_mark_left_by_an_earlier_one() {
    // The state this covers is the one the observer could not describe: a book that
    // went visible, failed once, and then recovered. Before this, nothing cleared
    // the mark for a visible book — `select_discovery_candidates` requires
    // `last_reconciled_at is null`, and both sites that null it set `superseded_at`
    // in the same UPDATE, so a visible row never returns to discovery. The mark
    // therefore meant "failed at least once, ever", and the observer's predicate
    // had to choose between naming recovered books and hiding broken ones. With the
    // clear on a clean cycle it means "the most recent cycle failed", and both
    // readings become correct.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_clear_failure";
    seed_market(&pool, ob, true).await;

    let r = InferenceReconciler::for_test(pool.clone());
    r.stamp_failure(ob, "getOrder reverted").await.unwrap();
    let (marked, text): (bool, Option<String>) = sqlx::query_as(
        "select last_reconcile_failed_at is not null, last_reconcile_error
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(marked && text.as_deref() == Some("getOrder reverted"), "precondition");

    r.clear_failure(ob).await.unwrap();

    let (still_marked, text_after, still_visible): (bool, Option<String>, bool) = sqlx::query_as(
        "select last_reconcile_failed_at is not null, last_reconcile_error,
                last_reconciled_at is not null
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!still_marked, "a clean cycle must clear the timestamp");
    assert!(text_after.is_none(), "the text goes with its timestamp — a text alone is stale");
    assert!(still_visible, "clearing a failure must not touch visibility");

    // Idempotent and quiet: an unmarked book is not rewritten, so a clean pass does
    // not churn the row of every book it touches.
    //
    // The witness is `xmin`, not `updated_at`. This schema has no triggers at all
    // (`grep 'create trigger' migrations/` is empty) and `clear_failure`'s UPDATE
    // does not set `updated_at`, so an `updated_at` comparison holds whatever the
    // `where` clause does — it would pass with the clause deleted, which is the one
    // thing it is supposed to catch. `xmin` is the transaction that wrote the live
    // row version, so it changes on ANY rewrite: drop `and last_reconcile_failed_at
    // is not null` and the UPDATE matches, writes a new version, and this goes red.
    let before: String =
        sqlx::query_scalar("select xmin::text from inference_markets where orderbook_address = $1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    r.clear_failure(ob).await.unwrap();
    let after: String =
        sqlx::query_scalar("select xmin::text from inference_markets where orderbook_address = $1")
            .bind(ob)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after, "clearing an unmarked book must not rewrite the row");

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_visibility_stamp_clears_the_failure_text_with_its_timestamp() {
    // The one path that can clear this text, and it had no test. `clear_failure` is
    // gated on `last_reconcile_failed_at is not null`, so a text the stamp leaves
    // behind is unreachable for the rest of the row's life — no later pass, in either
    // queue, can ever see it again.
    //
    // The seed is the ORDINARY cold start, not a contrived failure: a book's first
    // discovery tick routinely hits `NoBoc` (the account is not on chain yet) and
    // stamps both columns with NO_BOC_REASON. So before this clause the common
    // outcome was a perfectly healthy visible book answering "account BOC not served
    // yet" forever, and neither observer predicate fires on it — nothing goes red,
    // the column just lies to whoever reads it.
    let Some(pool) = setup().await else { return };
    let ob = "0:t_stamp_clears_text";
    let r = InferenceReconciler::for_test(pool.clone());

    seed_market(&pool, ob, false).await;
    r.stamp_failure(ob, InferenceReconciler::NO_BOC_REASON).await.unwrap();
    sqlx::query(
        "update inference_markets set sweep_cursor=null, sweep_cycle_max=10, sweep_override_seq=0
          where orderbook_address=$1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    // Not stamping yet: an incomplete cycle must leave BOTH columns exactly as they
    // are — otherwise the assertions below would pass on a clause that clears
    // unconditionally, which is a different bug in the same UPDATE.
    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, None, "10", Some("3"), false, true, 0).await.unwrap();
    assert!(!stamped, "an incomplete cycle does not stamp");
    let (marked, text): (bool, Option<String>) = sqlx::query_as(
        "select last_reconcile_failed_at is not null, last_reconcile_error
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(marked, "a non-stamping pass must leave the mark");
    assert_eq!(
        text.as_deref(),
        Some(InferenceReconciler::NO_BOC_REASON),
        "and must leave its text"
    );

    let stamped =
        r.advance_sweep_and_maybe_stamp(ob, Some("3"), "10", None, true, true, 0).await.unwrap();
    assert!(stamped, "a clean discovery cycle stamps visibility");

    let (visible, marked, text): (bool, bool, Option<String>) = sqlx::query_as(
        "select last_reconciled_at is not null, last_reconcile_failed_at is not null,
                last_reconcile_error
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(visible, "the book became visible");
    assert!(!marked, "the stamp clears the failure timestamp");
    assert!(
        text.is_none(),
        "and its text with it — this is the only writer that can, so a text surviving here \
         is orphaned permanently: clear_failure will not touch a row whose timestamp is \
         already NULL. Got {text:?}"
    );

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

/// The two tests below hold the CALL in place, not just the query. The test above
/// calls `clear_failure` directly, so deleting the only call site left the suite
/// green with nothing but an unused-function warning — verified by deleting it.
/// The call now lives at the tail of `refresh_against_boc`, which
/// `reconcile_refresh_with_boc` reaches; `run_once`'s `Ok(_)` arm never was
/// reachable here, because `reconcile_refresh` fetches the BOC over GraphQL.
#[tokio::test]
async fn a_refresh_pass_that_completes_clears_the_book_it_refreshed() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_refresh_clears";
    seed_market(&pool, ob, true).await;
    // Price DUE, sweep FRESH: the pass does real work (a re-price) and then falls
    // through to the clear, rather than clearing after doing nothing.
    sqlx::query(
        "update inference_markets
            set reference_price = 12345, reference_price_at = now() - interval '2 hours',
                last_swept_at = now(),
                last_reconcile_failed_at = now(), last_reconcile_error = 'getStats reverted'
          where orderbook_address = $1",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getWeeklyMedianPrice" => Ok(json!({"value0":"0x270f"})),
        _ => Ok(json!({})),
    }));
    InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .reconcile_refresh_with_boc(ob, "boc")
        .await
        .expect("a clean refresh must not error");

    let (marked, text, priced): (bool, Option<String>, Option<String>) = sqlx::query_as(
        "select last_reconcile_failed_at is not null, last_reconcile_error, reference_price::text
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(priced.as_deref(), Some("9999"), "the pass must have actually re-priced");
    assert!(!marked, "a completed refresh pass must clear the mark of the book it refreshed");
    assert!(text.is_none(), "the reason text goes with its timestamp");

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_refresh_pass_that_errors_leaves_the_mark_standing() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_refresh_keeps_mark";
    seed_market(&pool, ob, true).await;
    let stamped_at = "2020-01-01T00:00:00Z";
    sqlx::query(
        "update inference_markets
            set reference_price_at = now() - interval '2 hours', last_swept_at = now(),
                last_reconcile_failed_at = $2::timestamptz, last_reconcile_error = 'getStats reverted'
          where orderbook_address = $1",
    )
    .bind(ob)
    .bind(stamped_at)
    .execute(&pool)
    .await
    .unwrap();

    // The price refresh is the first thing the pass does and it propagates with `?`,
    // so the clear at the tail is never reached. Without that ordering the mark
    // would be cleared by a pass that failed — the exact false "recovered" the
    // observer's failing-books list must never show.
    let g = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| match name {
        "getWeeklyMedianPrice" => Err(anyhow::anyhow!("getter unavailable")),
        _ => Ok(json!({})),
    }));
    let outcome = InferenceReconciler::for_test_with_getter(pool.clone(), g)
        .reconcile_refresh_with_boc(ob, "boc")
        .await;
    assert!(outcome.is_err(), "a getter error must propagate, not be swallowed into a clean pass");

    let (marked, text): (bool, Option<String>) = sqlx::query_as(
        "select last_reconcile_failed_at is not null, last_reconcile_error
           from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(marked, "a failed pass must leave the mark standing");
    assert_eq!(
        text.as_deref(),
        Some("getStats reverted"),
        "and must leave the earlier reason readable"
    );

    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_reconcile_failure_records_its_reason() {
    // The observer is a DB-tail step and does not read logs. If the reason lives
    // only in the logs, "failing with a reason" (IX-SEQ-10) is checkable only as
    // "a stamp is present" — i.e. the matrix row would be closed by a weakened
    // version of itself, silently.
    let Some(pool) = setup().await else { return };
    let ob = "0:reconcile_err";
    seed_market(&pool, ob, false).await;

    let r = InferenceReconciler::for_test(pool.clone());
    r.stamp_failure(ob, "getModelName reverted: exit code 78").await.unwrap();

    let (failed_at, err): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) = sqlx::query_as(
        "select last_reconcile_failed_at, last_reconcile_error
               from inference_markets where orderbook_address = $1",
    )
    .bind(ob)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(failed_at.is_some(), "the failure timestamp must be stamped");
    assert_eq!(
        err.as_deref(),
        Some("getModelName reverted: exit code 78"),
        "the reason must sit next to the stamp, not only in the logs"
    );
}
