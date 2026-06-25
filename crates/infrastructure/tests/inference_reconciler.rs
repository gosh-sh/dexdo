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

// ---- Task 9 helpers ----

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

// ---- Task 9 DB-side tests ----

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
    let stamped = r
        .advance_sweep_and_maybe_stamp(ob, Some("5"), "10", None, true, true, 0)
        .await
        .unwrap();
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
    let stamped = r
        .advance_sweep_and_maybe_stamp(ob, None, "10", None, true, true, 0)
        .await
        .unwrap();
    assert!(!stamped, "first-tick (NULL prev) override must still block the stamp via the seq guard");
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
    let stamped = r
        .advance_sweep_and_maybe_stamp(ob, Some("5"), "10", None, true, true, 0)
        .await
        .unwrap();
    assert!(stamped);
    assert!(reconciled_at(pool.clone()).await.is_some());
}

// ---- Task 9 getter-seam tests ----

use dodex_infrastructure::inference_reconciler::OrderBookGetter;
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

#[tokio::test]
async fn refresh_price_err_no_liquidity_is_null_success_writes() {
    let Some(pool) = setup().await else { return };
    let ob = "0:t_refprice";
    seed_market(&pool, ob, true).await;
    // Dry book ⇒ ERR_NO_LIQUIDITY (334) ⇒ price NULL but reference_price_at stamped.
    let dry = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getWeeklyMedianPrice" {
            Err(anyhow::Error::new(TvmGetterError { exit_code: 334, message: "ERR_NO_LIQUIDITY".into() }))
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
    assert_eq!((status.as_str(), swept), ("CANCELLED", true), "empty getOrder ⇒ provisional sweep-cancel");
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
async fn discovery_sweep_bounded_advances_across_ticks_resumes_after_restart_stamps_on_completion() {
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
    let g1 = RecGetter::new(0, 3, &[]); // default batch 50 ⇒ one batch covers (−1,3]
    let _ = r_run(&pool, g1.clone(), ob).await;
    assert_eq!(g1.order_ids(), vec!["1", "2"], "id minted past the snapshot boundary is NOT probed this cycle");
    // Cycle complete ⇒ cursor reset; next cycle re-snapshots a higher boundary ⇒ id=10 now probed.
    let g2 = RecGetter::new(0, 11, &[]);
    let _ = r_run(&pool, g2.clone(), ob).await;
    assert!(
        g2.order_ids().contains(&"10".to_string()),
        "id=10 re-probed in the next cycle (low ids also re-scanned)"
    );
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
    assert!(at.is_none(), "a real failure must NOT stamp reference_price_at (unlike ERR_NO_LIQUIDITY)");
    // fill_params likewise surfaces a getter error.
    let boom_params = std::sync::Arc::new(FnGetter(|name: &str, _a: &Value| {
        if name == "getParams" {
            Err(anyhow::anyhow!("getter boom"))
        } else {
            Ok(json!({}))
        }
    }));
    assert!(
        InferenceReconciler::for_test_with_getter(pool.clone(), boom_params)
            .fill_params(ob, "boc")
            .await
            .is_err()
    );
}
