// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Sixth indexer loop: reconciles InferenceOrderBook state off-chain.
// Queue A (discovery): fill params/constants/price, sweep phantoms behind idle+at-head
// gates, stamp last_reconciled_at (visibility) only on a clean bounded-sweep cycle.
// Queue B (refresh): re-price + sweep phantoms on a separate cadence.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
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
    pub superseded: u64,
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
        let contract =
            self.decoder.contract(KIND).ok_or_else(|| anyhow!("InferenceOrderBook abi missing"))?;
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
    // Shared with IndexerRepository; bumped per hard reconcile failure for the
    // indexer_inference_reconcile_failures counter. Defaults to a standalone
    // atomic so tests and standalone construction work without wiring.
    reconcile_failures: Arc<AtomicU64>,
}

#[derive(sqlx::FromRow)]
pub struct BookRow {
    pub orderbook_address: String,
    model_hash: Option<String>,
    reference_price_at: Option<chrono::DateTime<chrono::Utc>>,
    last_swept_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Stamped,
    WaitingGates,
    NoBoc,
    Superseded,
}

pub enum SlotClaim {
    Claimed,
    Superseded,
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
            reconcile_failures: Arc::new(AtomicU64::new(0)),
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

    /// Share the inference-reconcile-failure counter with `IndexerRepository`,
    /// so per-book `Err` outcomes are visible to the metrics-refresh poll.
    pub fn with_failure_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.reconcile_failures = counter;
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

    pub fn for_test_with_getter(pool: PgPool, getter: std::sync::Arc<dyn OrderBookGetter>) -> Self {
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
                    superseded = s.superseded,
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
        let pending = self.select_discovery_candidates(BATCH_SIZE).await?;

        for book in &pending {
            stats.scanned += 1;
            match self.reconcile_discovery(book).await {
                Ok(DiscoveryOutcome::Stamped) => stats.reconciled += 1,
                Ok(DiscoveryOutcome::WaitingGates) => stats.waiting_gates += 1,
                Ok(DiscoveryOutcome::Superseded) => stats.superseded += 1,
                Ok(DiscoveryOutcome::NoBoc) => {
                    stats.skipped += 1;
                    self.stamp_failure_logged(&book.orderbook_address).await;
                }
                Err(err) => {
                    stats.failed += 1;
                    self.reconcile_failures.fetch_add(1, Ordering::Relaxed);
                    warn!(ob = %book.orderbook_address, ?err, "discovery failed");
                    self.stamp_failure_logged(&book.orderbook_address).await;
                }
            }
        }
        // Queue B — refresh (price + phantom sweep on separate cadences).
        let refresh = self.select_refresh_books().await?;
        for book in &refresh {
            stats.scanned += 1;
            match self.reconcile_refresh(book).await {
                Ok(DiscoveryOutcome::NoBoc) => {
                    stats.skipped += 1;
                    self.stamp_failure_logged(&book.orderbook_address).await;
                }
                Ok(_) => stats.refreshed += 1,
                Err(err) => {
                    stats.failed += 1;
                    self.reconcile_failures.fetch_add(1, Ordering::Relaxed);
                    warn!(ob = %book.orderbook_address, ?err, "refresh failed");
                    self.stamp_failure_logged(&book.orderbook_address).await;
                }
            }
        }
        Ok(stats)
    }

    async fn reconcile_discovery(&self, book: &BookRow) -> anyhow::Result<DiscoveryOutcome> {
        let ob = &book.orderbook_address;
        let Some(boc) = self.graphql.fetch_account_boc(ob).await.context("fetch boc")? else {
            return Ok(DiscoveryOutcome::NoBoc);
        };
        self.reconcile_discovery_with_boc(
            ob,
            &boc,
            book.model_hash.is_none(),
            book.reference_price_at.is_none(),
        )
        .await
    }

    /// Discovery composition given an already-fetched BOC: fill params/constants
    /// (`needs_params`), refresh the reference price (`needs_price`), run one
    /// discovery sweep tick, and report whether the visibility stamp landed. Split
    /// from `reconcile_discovery`'s GraphQL BOC fetch so the composed path is
    /// exercisable through the off-chain getter seam. The two `needs_*` flags carry
    /// the freshly-selected row state so a re-fill is skipped once a column is set.
    pub async fn reconcile_discovery_with_boc(
        &self,
        ob: &str,
        boc: &str,
        needs_params: bool,
        needs_price: bool,
    ) -> anyhow::Result<DiscoveryOutcome> {
        if needs_params {
            if let SlotClaim::Superseded = self.fill_params(ob, boc).await? {
                return Ok(DiscoveryOutcome::Superseded);
            }
        }
        if needs_price {
            self.refresh_price(ob, boc).await?;
        }
        match self.run_sweep_step(ob, boc, true).await? {
            SweepStep::GatesFailed | SweepStep::Continued => Ok(DiscoveryOutcome::WaitingGates),
            SweepStep::CycleComplete => Ok(DiscoveryOutcome::Stamped),
        }
    }

    fn call_getter(&self, boc: &str, name: &str, args: &Value) -> anyhow::Result<Value> {
        self.getter.call(boc, name, args)
    }

    /// Read the contract's self-reported version via the `getVersion` getter,
    /// executed on the already-fetched account BOC (no extra network round-trip).
    /// Getter FAILURES propagate (like `getParams`): a transient getVersion error
    /// fails this discovery tick and is retried, so it is never silently misread as
    /// "version unknown → lowest" and used to retire a book. A success that carries
    /// no `value0` is `Ok(None)` — destructive only on an actual model-slot
    /// collision, which `claim_model_slot` refuses (see Task 4).
    fn fetch_version(&self, boc: &str) -> anyhow::Result<Option<String>> {
        let v = self.call_getter(boc, "getVersion", &serde_json::json!({})).context("getVersion")?;
        Ok(version_from_getter(&v))
    }

    pub async fn fill_params(&self, ob: &str, boc: &str) -> anyhow::Result<SlotClaim> {
        let params = self.call_getter(boc, "getParams", &json!({})).context("getParams")?;
        let model_hash = uint_field_to_decimal(&params, "modelHash")?;
        let fee: i32 =
            uint_field_to_decimal(&params, "platformFeeBps")?.parse().context("platformFeeBps")?;
        let version = self.fetch_version(boc)?;
        self.claim_model_slot(ob, &model_hash, version.as_deref(), fee).await
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

    /// Resolve the `model_hash` slot for `ob` against the partial unique index.
    /// One book per model is an on-chain invariant (single `_modelHash` static);
    /// duplicates only exist across code versions (redeploys). The higher
    /// `getVersion` is canonical: it claims the slot, releasing and retiring any
    /// lower-version holder. A book that loses is retired via `superseded_at`
    /// instead of failing forever on the unique violation.
    async fn claim_model_slot(
        &self,
        ob: &str,
        model_hash: &str,
        version: Option<&str>,
        fee: i32,
    ) -> anyhow::Result<SlotClaim> {
        let mut tx = self.pool.begin().await.context("begin model-slot tx")?;

        let incumbent: Option<(String, Option<String>)> = sqlx::query_as(
            "select orderbook_address, version from inference_markets
              where model_hash = $1::numeric and superseded_at is null
              for update",
        )
        .bind(model_hash)
        .fetch_optional(&mut *tx)
        .await
        .context("select model-slot incumbent")?;

        if let Some((incumbent_ob, incumbent_version)) = &incumbent {
            if incumbent_ob != ob {
                let incoming = parse_semver(version);
                // Refuse a destructive slot decision on an unknown/unparseable incoming
                // version — it could be the real higher-version replacement. Fail
                // discovery (the book retries on the next tick) rather than retiring it.
                // (Dropping `tx` here rolls back the FOR UPDATE select.)
                if incoming.is_none() {
                    return Err(anyhow!(
                        "model-slot conflict for {ob}: incoming version unknown ({version:?})"
                    ));
                }
                if incoming > parse_semver(incumbent_version.as_deref()) {
                    // Incoming wins: free + retire + hide the incumbent.
                    sqlx::query(
                        "update inference_markets
                            set model_hash = null, last_reconciled_at = null,
                                superseded_at = now(), updated_at = now()
                          where orderbook_address = $1",
                    )
                    .bind(incumbent_ob)
                    .execute(&mut *tx)
                    .await
                    .context("release superseded incumbent slot")?;
                } else {
                    // Incoming is a known, lower-or-equal version: retire it. Clear
                    // model_hash + last_reconciled_at — same as the incumbent-supersede
                    // branch — so a superseded row never holds a slot or stays visible.
                    // Both are already NULL in the current discovery flow (the retire
                    // branch is only reached when the snapshot model_hash was NULL), but
                    // clearing them unconditionally keeps "superseded ⇒ hidden, slot-free"
                    // robust to future changes. Record the fetched version for audit.
                    sqlx::query(
                        "update inference_markets
                            set version = $2, model_hash = null, last_reconciled_at = null,
                                superseded_at = now(), updated_at = now()
                          where orderbook_address = $1",
                    )
                    .bind(ob)
                    .bind(version)
                    .execute(&mut *tx)
                    .await
                    .context("retire stale duplicate book")?;
                    tx.commit().await.context("commit retire")?;
                    return Ok(SlotClaim::Superseded);
                }
            }
        }

        sqlx::query(
            r#"update inference_markets
                  set model_hash=$2::numeric, platform_fee_bps=$3, version=$10,
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
        .bind(version)
        .execute(&mut *tx)
        .await
        .context("write inference params")?;
        tx.commit().await.context("commit claim")?;
        Ok(SlotClaim::Claimed)
    }

    pub async fn select_discovery_candidates(&self, limit: i64) -> anyhow::Result<Vec<BookRow>> {
        sqlx::query_as(&format!(
            r#"select orderbook_address, model_hash::text as model_hash, reference_price_at, last_swept_at
                 from inference_markets
                where last_reconciled_at is null
                  and superseded_at is null
                  and (last_reconcile_failed_at is null
                       or last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
                order by last_reconcile_failed_at nulls first, id asc
                limit $1"#
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("select discovery candidates")
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
    #[allow(clippy::too_many_arguments)]
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
                  and superseded_at is null
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

    /// Stamps the reconcile-failure backoff, logging — not swallowing — a write
    /// error. If the backoff stamp itself fails, the book keeps re-entering the
    /// queue every tick with no cooldown, so a silent drop would hide a book
    /// spinning without backoff. Used by `run_once` where the outcome is already
    /// a failure and there is nothing further to propagate.
    async fn stamp_failure_logged(&self, ob: &str) {
        if let Err(e) = self.stamp_failure(ob).await {
            warn!(ob = %ob, ?e, "failed to stamp inference reconcile backoff");
        }
    }

    /// Single source for the Queue B refresh SELECT, used by `run_once`. A book
    /// is due when its reference price is stale, or its sweep cadence elapsed and
    /// it still has OPEN rows; failed books are held out for the backoff window.
    async fn select_refresh_books(&self) -> anyhow::Result<Vec<BookRow>> {
        let rows: Vec<BookRow> = sqlx::query_as(&format!(
            r#"select orderbook_address, model_hash::text as model_hash, reference_price_at, last_swept_at
                 from inference_markets m
                where last_reconciled_at is not null
                  and (last_reconcile_failed_at is null
                       or last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
                  and (
                        (reference_price_at is null or reference_price_at < now() - make_interval(secs => $2))
                     or ( (last_swept_at is null or last_swept_at < now() - make_interval(secs => $3))
                          and exists (select 1 from inference_orders o
                                       where o.orderbook_address = m.orderbook_address and o.status='OPEN') )
                      )
                order by reference_price_at nulls first
                limit $1"#
        ))
        .bind(BATCH_SIZE)
        .bind(self.reference_price_refresh.as_secs_f64())
        .bind(self.sweep_interval.as_secs_f64())
        .fetch_all(&self.pool)
        .await
        .context("select refresh candidates")?;
        Ok(rows)
    }

    /// Test-facing view of the Queue B selection: the addresses `run_once` would
    /// refresh this tick. Wraps the same `select_refresh_books` query the loop
    /// runs, so the selection test exercises the production query, not a copy.
    pub async fn select_refresh_candidates(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.select_refresh_books().await?.into_iter().map(|r| r.orderbook_address).collect())
    }

    fn price_due(&self, book: &BookRow) -> bool {
        match book.reference_price_at {
            None => true,
            Some(at) => (chrono::Utc::now() - at)
                .to_std()
                .map(|a| a > self.reference_price_refresh)
                .unwrap_or(true),
        }
    }

    fn sweep_due_by_time(&self, book: &BookRow) -> bool {
        match book.last_swept_at {
            None => true,
            Some(at) => {
                (chrono::Utc::now() - at).to_std().map(|a| a > self.sweep_interval).unwrap_or(true)
            }
        }
    }

    async fn has_open_orders(&self, ob: &str) -> anyhow::Result<bool> {
        let exists: bool = sqlx::query_scalar(
            "select exists(select 1 from inference_orders where orderbook_address=$1 and status='OPEN')",
        )
        .bind(ob)
        .fetch_one(&self.pool)
        .await
        .context("has_open_orders probe")?;
        Ok(exists)
    }

    async fn reconcile_refresh(&self, book: &BookRow) -> anyhow::Result<DiscoveryOutcome> {
        let ob = &book.orderbook_address;
        let Some(boc) = self.graphql.fetch_account_boc(ob).await.context("fetch boc (refresh)")?
        else {
            return Ok(DiscoveryOutcome::NoBoc);
        };
        self.refresh_against_boc(book, &boc).await
    }

    /// Execute one refresh pass for a selected book against a fetched BOC:
    /// re-price iff the price cadence is due, and run one phantom-sweep step iff
    /// the sweep cadence is due AND the book has an OPEN row. The two cadences
    /// are decoupled — neither gates the other. Split out of `reconcile_refresh`
    /// (which fetches the BOC) so tests can inject a BOC + getter.
    async fn refresh_against_boc(
        &self,
        book: &BookRow,
        boc: &str,
    ) -> anyhow::Result<DiscoveryOutcome> {
        let ob = &book.orderbook_address;
        if self.price_due(book) {
            self.refresh_price(ob, boc).await?;
        }
        // Phantom sweep is due ONLY when the cadence is stale AND the book actually has an
        // OPEN row (spec). A book selected solely for a price refresh (stale/NULL last_swept_at
        // but no OPEN rows) must NOT enter run_sweep_step — that would spend getQueueSize/getStats
        // getters and stamp last_swept_at for no work.
        if self.sweep_due_by_time(book) && self.has_open_orders(ob).await? {
            // run_sweep_step self-gates (idle + at-head + no-pending); on a gate miss it
            // does not touch last_swept_at, so the book stays sweep-due next tick.
            let _ = self.run_sweep_step(ob, boc, /* discovery= */ false).await?;
        }
        Ok(DiscoveryOutcome::Stamped) // "handled" — reuse the enum; mapped to `refreshed` in run_once
    }

    /// Test seam mirroring `reconcile_discovery_with_boc` for the refresh path:
    /// selects the current `BookRow` for `ob` (so a test controls the cadence
    /// timestamps via the seeded row) and runs `refresh_against_boc` with an
    /// injected BOC — exercising the price-vs-sweep cadence gating without the
    /// (test-unreachable) GraphQL BOC fetch.
    pub async fn reconcile_refresh_with_boc(
        &self,
        ob: &str,
        boc: &str,
    ) -> anyhow::Result<DiscoveryOutcome> {
        let book: BookRow = sqlx::query_as(
            r#"select orderbook_address, model_hash::text as model_hash, reference_price_at, last_swept_at
                 from inference_markets where orderbook_address = $1"#,
        )
        .bind(ob)
        .fetch_one(&self.pool)
        .await
        .context("select book for refresh-with-boc")?;
        self.refresh_against_boc(&book, boc).await
    }
}

/// Parse a dotted `major.minor.patch` version. `None` (or any unparseable
/// value) sorts below every real version under `Option` ordering, so an
/// unversioned book always loses a slot contest to a versioned one.
#[allow(dead_code)]
fn parse_semver(v: Option<&str>) -> Option<(u64, u64, u64)> {
    let v = v?;
    let mut it = v.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Version string from a `getVersion` getter result (`value0`).
#[allow(dead_code)]
fn version_from_getter(v: &serde_json::Value) -> Option<String> {
    v.get("value0").and_then(|x| x.as_str()).map(|s| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_orders_versions() {
        assert_eq!(parse_semver(Some("4.0.14")), Some((4, 0, 14)));
        assert!(parse_semver(Some("4.0.14")) > parse_semver(Some("4.0.11")));
        assert!(parse_semver(Some("4.0.9")) < parse_semver(Some("4.0.14"))); // numeric, not lexical
        // legacy/unknown sorts below every real version
        assert!(parse_semver(None) < parse_semver(Some("4.0.10")));
        assert_eq!(parse_semver(Some("garbage")), None);
    }

    #[test]
    fn version_from_getter_reads_value0() {
        let v = serde_json::json!({"value0": "4.0.14", "value1": "InferenceOrderBook"});
        assert_eq!(version_from_getter(&v).as_deref(), Some("4.0.14"));
        assert_eq!(version_from_getter(&serde_json::json!({})), None);
    }
}
