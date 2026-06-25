// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Sixth indexer loop: reconciles InferenceOrderBook state off-chain.
// Queue A (discovery): fill params/constants/price, sweep phantoms behind idle+at-head
// gates, stamp last_reconciled_at (visibility) only on a clean bounded-sweep cycle.
// Queue B (refresh, Task 10): re-price + sweep phantoms on a separate cadence.

use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use serde_json::json;
use serde_json::Value;
use sqlx::PgPool;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::decoder::Decoder;
use crate::graphql::GraphqlClient;
use crate::projectors::uint_field_to_decimal;
use crate::tvm_runner::run_getter;
use crate::tvm_runner::tvm_exit_code;

const BATCH_SIZE: i64 = 16;
const SWEEP_BATCH_N: i64 = 50;
const FAILURE_BACKOFF_INTERVAL_SQL: &str = "5 minutes";
const ERR_NO_LIQUIDITY: i32 = 334;
const EVENTS_STREAM_NAME: &str = "blockchain_events";
const KIND: &str = "InferenceOrderBook";
const QUOTE_TOKEN_TYPE_SHELL: i32 = 2;
const PRICE_PRECISION: i32 = 9;
const QUANTITY_PRECISION: i32 = 0;
const TICK_SIZE: &str = "0.000000001";
const STEP_SIZE: &str = "1";
const MIN_NOTIONAL: &str = "0.000000001";

#[derive(Debug, Default, Clone, Copy)]
pub struct InferenceReconcileStats {
    pub scanned: u64,
    pub reconciled: u64,
    pub waiting_gates: u64,
    pub refreshed: u64,
    pub skipped: u64,
    pub failed: u64,
}

/// Test seam over the off-chain getter call. Production wraps `run_getter`; tests
/// inject a mock so the reconciler's getter-driven orchestration (gates, sweep,
/// ERR_NO_LIQUIDITY → NULL, getParams fill) is covered deterministically without a
/// live chain. `run_getter` is synchronous (TVM emulation), so this trait is too.
pub trait OrderBookGetter: Send + Sync {
    fn call(&self, boc: &str, name: &str, args: &Value) -> anyhow::Result<Value>;
}

/// Production getter: executes the getter on the decoder's InferenceOrderBook contract.
pub struct DecoderGetter {
    decoder: Decoder,
}

impl OrderBookGetter for DecoderGetter {
    fn call(&self, boc: &str, name: &str, args: &Value) -> anyhow::Result<Value> {
        let contract = self
            .decoder
            .contract(KIND)
            .ok_or_else(|| anyhow!("InferenceOrderBook abi missing"))?;
        run_getter(contract, boc, name, args)
    }
}

pub struct InferenceReconciler {
    pool: PgPool,
    graphql: GraphqlClient,
    getter: std::sync::Arc<dyn OrderBookGetter>,
    reference_price_refresh: Duration,
    sweep_interval: Duration,
    // OPEN orders checked per sweep tick; SWEEP_BATCH_N in prod, smaller in tests.
    sweep_batch_n: i64,
    events_stream: String,
}

#[derive(sqlx::FromRow)]
struct BookRow {
    orderbook_address: String,
    model_hash: Option<String>,
    reference_price_at: Option<chrono::DateTime<chrono::Utc>>,
}

enum DiscoveryOutcome {
    Stamped,
    WaitingGates,
    NoBoc,
}

pub enum SweepStep {
    GatesFailed,
    Continued,
    CycleComplete,
}

impl InferenceReconciler {
    pub fn new(
        pool: PgPool,
        graphql: GraphqlClient,
        decoder: Decoder,
        reference_price_refresh: Duration,
        sweep_interval: Duration,
    ) -> Self {
        Self::with_getter(
            pool,
            graphql,
            std::sync::Arc::new(DecoderGetter { decoder }),
            reference_price_refresh,
            sweep_interval,
        )
    }

    pub fn with_getter(
        pool: PgPool,
        graphql: GraphqlClient,
        getter: std::sync::Arc<dyn OrderBookGetter>,
        reference_price_refresh: Duration,
        sweep_interval: Duration,
    ) -> Self {
        Self {
            pool,
            graphql,
            getter,
            reference_price_refresh,
            sweep_interval,
            sweep_batch_n: SWEEP_BATCH_N,
            events_stream: EVENTS_STREAM_NAME.to_string(),
        }
    }

    pub fn with_sweep_batch_n(mut self, n: i64) -> Self {
        self.sweep_batch_n = n;
        self
    }

    pub fn with_events_stream(mut self, s: impl Into<String>) -> Self {
        self.events_stream = s.into();
        self
    }

    pub fn for_test(pool: PgPool) -> Self {
        Self::new(
            pool,
            GraphqlClient::new("http://127.0.0.1:0/graphql", Duration::from_secs(1)).unwrap(),
            Decoder::new().unwrap(),
            Duration::from_secs(3600),
            Duration::from_secs(30),
        )
        .with_events_stream("test_inference_at_head_stream")
    }

    pub fn for_test_with_getter(
        pool: PgPool,
        getter: std::sync::Arc<dyn OrderBookGetter>,
    ) -> Self {
        Self::with_getter(
            pool,
            GraphqlClient::new("http://127.0.0.1:0/graphql", Duration::from_secs(1)).unwrap(),
            getter,
            Duration::from_secs(3600),
            Duration::from_secs(30),
        )
        .with_events_stream("test_inference_at_head_stream")
    }

    pub async fn run_loop(self, interval: Duration) {
        loop {
            match self.run_once().await {
                Ok(s) if s.scanned > 0 => info!(
                    scanned = s.scanned,
                    reconciled = s.reconciled,
                    waiting_gates = s.waiting_gates,
                    refreshed = s.refreshed,
                    skipped = s.skipped,
                    failed = s.failed,
                    "inference reconciler tick"
                ),
                Ok(_) => debug!("inference reconciler tick (idle)"),
                Err(err) => error!(?err, "inference reconciler sweep failed"),
            }
            tokio::time::sleep(interval).await;
        }
    }

    pub async fn run_once(&self) -> anyhow::Result<InferenceReconcileStats> {
        let mut stats = InferenceReconcileStats::default();
        // Queue A — discovery.
        let pending: Vec<BookRow> = sqlx::query_as(&format!(
            r#"select orderbook_address, model_hash::text as model_hash, reference_price_at
                 from inference_markets
                where last_reconciled_at is null
                  and (last_reconcile_failed_at is null
                       or last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
                order by last_reconcile_failed_at nulls first, id asc
                limit $1"#
        ))
        .bind(BATCH_SIZE)
        .fetch_all(&self.pool)
        .await
        .context("select discovery candidates")?;

        for book in &pending {
            stats.scanned += 1;
            match self.reconcile_discovery(book).await {
                Ok(DiscoveryOutcome::Stamped) => stats.reconciled += 1,
                Ok(DiscoveryOutcome::WaitingGates) => stats.waiting_gates += 1,
                Ok(DiscoveryOutcome::NoBoc) => {
                    stats.skipped += 1;
                    self.stamp_failure(&book.orderbook_address).await.ok();
                }
                Err(err) => {
                    stats.failed += 1;
                    warn!(ob = %book.orderbook_address, ?err, "discovery failed");
                    self.stamp_failure(&book.orderbook_address).await.ok();
                }
            }
        }
        // Queue B — refresh: added in Task 10.
        Ok(stats)
    }

    async fn reconcile_discovery(&self, book: &BookRow) -> anyhow::Result<DiscoveryOutcome> {
        let ob = &book.orderbook_address;
        let Some(boc) = self.graphql.fetch_account_boc(ob).await.context("fetch boc")? else {
            return Ok(DiscoveryOutcome::NoBoc);
        };
        if book.model_hash.is_none() {
            self.fill_params(ob, &boc).await?;
        }
        if book.reference_price_at.is_none() {
            self.refresh_price(ob, &boc).await?;
        }
        match self.run_sweep_step(ob, &boc, true).await? {
            SweepStep::GatesFailed | SweepStep::Continued => Ok(DiscoveryOutcome::WaitingGates),
            SweepStep::CycleComplete => Ok(DiscoveryOutcome::Stamped),
        }
    }

    fn call_getter(&self, boc: &str, name: &str, args: &Value) -> anyhow::Result<Value> {
        self.getter.call(boc, name, args)
    }

    pub async fn fill_params(&self, ob: &str, boc: &str) -> anyhow::Result<()> {
        let params = self.call_getter(boc, "getParams", &json!({})).context("getParams")?;
        let model_hash = uint_field_to_decimal(&params, "modelHash")?;
        let fee: i32 = uint_field_to_decimal(&params, "platformFeeBps")?
            .parse()
            .context("platformFeeBps")?;
        self.write_params(ob, &model_hash, fee).await
    }

    pub async fn refresh_price(&self, ob: &str, boc: &str) -> anyhow::Result<()> {
        match self.call_getter(boc, "getWeeklyMedianPrice", &json!({})) {
            Ok(v) => {
                let price = uint_field_to_decimal(&v, "value0")?;
                sqlx::query(
                    "update inference_markets set reference_price=$2::numeric, reference_price_at=now(), updated_at=now() where orderbook_address=$1",
                )
                .bind(ob)
                .bind(&price)
                .execute(&self.pool)
                .await
                .context("write reference_price")?;
            }
            Err(e) if tvm_exit_code(&e) == Some(ERR_NO_LIQUIDITY) => {
                // Dry book — normal outcome. Stamp reference_price_at so discovery
                // doesn't refetch every tick; price stays NULL.
                sqlx::query(
                    "update inference_markets set reference_price=null, reference_price_at=now(), updated_at=now() where orderbook_address=$1",
                )
                .bind(ob)
                .execute(&self.pool)
                .await
                .context("write null reference_price")?;
            }
            Err(e) => return Err(e).context("getWeeklyMedianPrice"),
        }
        Ok(())
    }

    async fn write_params(&self, ob: &str, model_hash: &str, fee: i32) -> anyhow::Result<()> {
        sqlx::query(
            r#"update inference_markets
                  set model_hash=$2::numeric, platform_fee_bps=$3,
                      quote_token_type=$4, price_precision=$5, quantity_precision=$6,
                      tick_size=$7, step_size=$8, min_notional=$9, updated_at=now()
                where orderbook_address=$1"#,
        )
        .bind(ob)
        .bind(model_hash)
        .bind(fee)
        .bind(QUOTE_TOKEN_TYPE_SHELL)
        .bind(PRICE_PRECISION)
        .bind(QUANTITY_PRECISION)
        .bind(TICK_SIZE)
        .bind(STEP_SIZE)
        .bind(MIN_NOTIONAL)
        .execute(&self.pool)
        .await
        .context("write inference params")?;
        Ok(())
    }

    async fn at_head(&self) -> anyhow::Result<bool> {
        let row: Option<(bool,)> =
            sqlx::query_as("select at_head from indexer_cursors where stream_name=$1")
                .bind(self.events_stream.as_str())
                .fetch_optional(&self.pool)
                .await
                .context("read at_head")?;
        Ok(row.map(|(b,)| b).unwrap_or(false))
    }

    pub async fn pending_events_exist(&self, ob: &str) -> anyhow::Result<bool> {
        let exists: bool = sqlx::query_scalar(
            r#"select exists(select 1 from raw_events
                 where src_address=$1 and processed_at is null
                   and event_type is not null and decoded is not null)"#,
        )
        .bind(ob)
        .fetch_one(&self.pool)
        .await
        .context("pending events probe")?;
        Ok(exists)
    }

    pub async fn run_sweep_step(
        &self,
        ob: &str,
        boc: &str,
        discovery: bool,
    ) -> anyhow::Result<SweepStep> {
        // Gate 1: idle (no in-flight queue continuation).
        let qsize: i64 =
            uint_field_to_decimal(&self.call_getter(boc, "getQueueSize", &json!({}))?, "value0")?
                .parse()
                .context("getQueueSize parse")?;
        if qsize > 0 {
            return Ok(SweepStep::GatesFailed);
        }
        // Gate 2: capture at head.
        if !self.at_head().await? {
            return Ok(SweepStep::GatesFailed);
        }
        // Gate 3: no unprojected raw_events for this book.
        if self.pending_events_exist(ob).await? {
            return Ok(SweepStep::GatesFailed);
        }

        // Re-read sweep state FRESH: the queue SELECT may be stale, and a concurrent
        // override could have reset the cursor / bumped the seq since. `override_seq`
        // captured here gates the discovery stamp (catches a same-tick override even
        // when prev_cursor is NULL, which a cursor-only CAS cannot).
        let Some((prev_cursor, existing_cycle_max, override_seq)): Option<(
            Option<String>,
            Option<String>,
            i64,
        )> = sqlx::query_as(
            "select sweep_cursor::text, sweep_cycle_max::text, sweep_override_seq from inference_markets where orderbook_address=$1",
        )
        .bind(ob)
        .fetch_optional(&self.pool)
        .await
        .context("read sweep state")?
        else {
            return Ok(SweepStep::GatesFailed);
        };

        // New cycle (cursor NULL) ⇒ snapshot boundary = getStats().nextOrderId.
        let cycle_max: String = match existing_cycle_max {
            Some(m) if prev_cursor.is_some() => m,
            _ => uint_field_to_decimal(
                &self.call_getter(boc, "getStats", &json!({}))?,
                "nextOrderId",
            )?,
        };

        // Next N OPEN orders in (prev_cursor, cycle_max].
        let lower = prev_cursor.clone().unwrap_or_else(|| "-1".to_string());
        let ids: Vec<(String,)> = sqlx::query_as(
            r#"select order_id::text from inference_orders
                where orderbook_address=$1 and status='OPEN'
                  and order_id > $2::numeric and order_id <= $3::numeric
                order by order_id asc limit $4"#,
        )
        .bind(ob)
        .bind(&lower)
        .bind(&cycle_max)
        .bind(self.sweep_batch_n)
        .fetch_all(&self.pool)
        .await
        .context("select sweep batch")?;

        // Probe each via getOrder; an empty order (amount==0) is a phantom.
        let mut to_cancel: Vec<String> = Vec::new();
        for (id,) in &ids {
            let o = self.call_getter(boc, "getOrder", &json!({ "id": id }))?;
            let amount = uint_field_to_decimal(&o, "amount")?;
            if amount == "0" {
                to_cancel.push(id.clone());
            }
        }
        if !to_cancel.is_empty() {
            self.provisional_cancel(ob, &to_cancel).await?;
        }

        let cycle_complete = (ids.len() as i64) < self.sweep_batch_n;
        let new_cursor: Option<String> =
            if cycle_complete { None } else { ids.last().map(|(id,)| id.clone()) };

        if discovery {
            let stamped = self
                .advance_sweep_and_maybe_stamp(
                    ob,
                    prev_cursor.as_deref(),
                    &cycle_max,
                    new_cursor.as_deref(),
                    cycle_complete,
                    true,
                    override_seq,
                )
                .await?;
            Ok(if stamped { SweepStep::CycleComplete } else { SweepStep::Continued })
        } else {
            self.advance_sweep_and_maybe_stamp(
                ob,
                prev_cursor.as_deref(),
                &cycle_max,
                new_cursor.as_deref(),
                cycle_complete,
                false,
                override_seq,
            )
            .await?;
            Ok(if cycle_complete { SweepStep::CycleComplete } else { SweepStep::Continued })
        }
    }

    pub async fn provisional_cancel(&self, ob: &str, ids: &[String]) -> anyhow::Result<()> {
        sqlx::query(
            r#"update inference_orders set status='CANCELLED', swept_at=now(), updated_at=now()
                where orderbook_address=$1 and order_id = any($2::numeric[]) and status='OPEN'"#,
        )
        .bind(ob)
        .bind(ids)
        .execute(&self.pool)
        .await
        .context("provisional cancel")?;
        Ok(())
    }

    /// Advance the sweep cursor and (discovery + clean completion) stamp visibility.
    /// For discovery, the write is OPTIMISTIC: it lands only if BOTH `sweep_cursor` is
    /// unchanged from `prev_cursor` AND `sweep_override_seq` is unchanged from
    /// `override_seq`. The seq guard closes the first-tick hole: when `prev_cursor` is
    /// NULL, an override that resets the cursor to NULL is invisible to the cursor-CAS,
    /// but it bumped the seq, so the guard fails and the stamp is blocked.
    /// Returns whether `last_reconciled_at` was stamped.
    pub async fn advance_sweep_and_maybe_stamp(
        &self,
        ob: &str,
        prev_cursor: Option<&str>,
        cycle_max: &str,
        new_cursor: Option<&str>,
        cycle_complete: bool,
        discovery: bool,
        override_seq: i64,
    ) -> anyhow::Result<bool> {
        let stamp = discovery && cycle_complete;
        let res = sqlx::query(
            r#"update inference_markets
                  set sweep_cycle_max = $2::numeric,
                      sweep_cursor = $3::numeric,
                      last_swept_at = now(),
                      last_reconciled_at = case when $5 then now() else last_reconciled_at end,
                      last_reconcile_failed_at = case when $5 then null else last_reconcile_failed_at end,
                      updated_at = now()
                where orderbook_address = $1
                  and ($6 = false
                       or (sweep_cursor is not distinct from $4::numeric
                           and sweep_override_seq = $7))"#,
        )
        .bind(ob)
        .bind(cycle_max)
        .bind(new_cursor)
        .bind(prev_cursor)
        .bind(stamp)
        .bind(discovery)
        .bind(override_seq)
        .execute(&self.pool)
        .await
        .context("advance sweep / stamp")?;
        Ok(stamp && res.rows_affected() == 1)
    }

    pub async fn stamp_failure(&self, ob: &str) -> anyhow::Result<()> {
        sqlx::query(
            "update inference_markets set last_reconcile_failed_at=now(), reconcile_attempts=reconcile_attempts+1 where orderbook_address=$1",
        )
        .bind(ob)
        .execute(&self.pool)
        .await
        .context("stamp inference reconcile failure")?;
        Ok(())
    }
}
