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
use crate::inference_projectors::non_zero_address;
use crate::inference_projectors::non_zero_uint;
use crate::projectors::field_str;
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
        if needs_params && let SlotClaim::Superseded = self.fill_params(ob, boc).await? {
            return Ok(DiscoveryOutcome::Superseded);
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
        let v =
            self.call_getter(boc, "getVersion", &serde_json::json!({})).context("getVersion")?;
        Ok(version_from_getter(&v))
    }

    pub async fn fill_params(&self, ob: &str, boc: &str) -> anyhow::Result<SlotClaim> {
        let params = self.call_getter(boc, "getParams", &json!({})).context("getParams")?;
        let model_hash = uint_field_to_decimal(&params, "modelHash")?;
        let fee: i32 =
            uint_field_to_decimal(&params, "platformFeeBps")?.parse().context("platformFeeBps")?;
        // Model identity: the book carries only `model_hash`; `getModelName` is the
        // authoritative preimage. A getter FAILURE propagates (like getParams) so a
        // transient error retries rather than wiping a known name. A successful call
        // with no `value0` is an empty name → all-`None` identity (NOT an error).
        let model_name =
            self.call_getter(boc, "getModelName", &json!({})).context("getModelName")?;
        let identity = parse_model_ref(model_name_str(&model_name));
        let version = self.fetch_version(boc)?;
        self.claim_model_slot(ob, &model_hash, version.as_deref(), fee, &identity).await
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
        identity: &ModelIdentity,
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

        if let Some((incumbent_ob, incumbent_version)) = &incumbent
            && incumbent_ob != ob
        {
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

        // `version` ($10) is the CONTRACT version (getVersion). The model identity
        // from `getModelName` lands in the dedicated columns ($11..$14) — a flat SET
        // (the getter is authoritative and stable; a re-reconcile rewrites the same
        // value), so an empty/non-three-part name correctly NULLs producer/version.
        sqlx::query(
            r#"update inference_markets
                  set model_hash=$2::numeric, platform_fee_bps=$3, version=$10,
                      quote_token_type=$4, price_precision=$5, quantity_precision=$6,
                      tick_size=$7, step_size=$8, min_notional=$9,
                      model_ref=$11, producer=$12, model_name=$13, model_version=$14,
                      updated_at=now()
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
        .bind(&identity.model_ref)
        .bind(&identity.producer)
        .bind(&identity.name)
        .bind(&identity.version)
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

        // The account state this step probes. Read from THIS BOC on every step, not
        // replayed from the cycle's stored boundary: mid-cycle `cycle_max` describes the
        // BOC that opened the cycle, and a gateway serving a rolled-back state would
        // otherwise let us probe an id it has never assigned — reading amount == 0 for an
        // order that is alive.
        let boc_next_order_id =
            uint_field_to_decimal(&self.call_getter(boc, "getStats", &json!({}))?, "nextOrderId")?;

        // New cycle (cursor NULL) ⇒ snapshot boundary = this BOC's nextOrderId.
        let cycle_max: String = match existing_cycle_max {
            Some(m) if prev_cursor.is_some() => m,
            _ => boc_next_order_id.clone(),
        };

        // Bound exclusively, and never past what this BOC can answer for. `nextOrderId`
        // is the next UNASSIGNED id (`orderId = _nextOrderId++` on chain), and the BOC was
        // fetched before this step ran, so a placement projected since then can occupy
        // exactly that id while the BOC knows nothing about it. Probing it would read
        // amount == 0 and cancel a live order. The boundary id is swept next cycle,
        // against a fresh BOC.
        //
        let lower = prev_cursor.clone().unwrap_or_else(|| "-1".to_string());
        // The two `is null` flags ride along so the repair below can skip a row that needs
        // nothing. Asking the database per order instead would cost one round trip per open
        // SELL — at ~95 ms per round trip to the deployed database, a book with a few
        // hundred orders would spend minutes in a single step.
        let ids: Vec<(String, bool, bool)> = sqlx::query_as(
            r#"select order_id::text,
                      token_contract is null as tc_missing,
                      deadline is null as deadline_missing
                 from inference_orders
                where orderbook_address=$1 and status='OPEN'
                  and order_id > $2::numeric
                  and order_id < least($3::numeric, $4::numeric)
                order by order_id asc limit $5"#,
        )
        .bind(ob)
        .bind(&lower)
        .bind(&cycle_max)
        .bind(&boc_next_order_id)
        .bind(self.sweep_batch_n)
        .fetch_all(&self.pool)
        .await
        .context("select sweep batch")?;

        // Probe each via getOrder. amount == 0 is a phantom (placed but never rested, or
        // a taker remainder refunded on chain without an event). A live order is also an
        // opportunity: the getter carries tokenContract and deadline, so a row missing
        // either is repaired here at no extra chain cost. The two repairs are independent
        // — a BUY's TC is zero by design, and a subscription's deadline exists only on
        // chain.
        let mut to_cancel: Vec<String> = Vec::new();
        // Repairs are accumulated, not applied per order. One UPDATE per row would cost a
        // round trip each — and every subscription is born with a NULL deadline, so a fresh
        // book needs the whole batch repaired. At ~95 ms per round trip, a 50-row batch
        // would spend ~5 s here and stall every book behind it.
        let mut repairs: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        for (id, tc_missing, deadline_missing) in &ids {
            let o = self.call_getter(boc, "getOrder", &json!({ "id": id }))?;
            let amount = uint_field_to_decimal(&o, "amount")?;
            if amount == "0" {
                to_cancel.push(id.clone());
                continue;
            }
            // A healthy row needs nothing: it never reaches the database.
            if !tc_missing && !deadline_missing {
                continue;
            }
            let tc: Option<String> = if *tc_missing {
                match field_str(&o, "tokenContract") {
                    Ok(v) => non_zero_address(Some(v)).map(str::to_owned),
                    Err(e) => {
                        warn!(
                            orderbook_address = ob,
                            order_id = %id,
                            error = %e,
                            "getOrder tokenContract failed to decode during sweep repair; ABI drift?",
                        );
                        None
                    }
                }
            } else {
                None
            };
            let deadline: Option<String> = if *deadline_missing {
                match uint_field_to_decimal(&o, "deadline") {
                    Ok(v) => non_zero_uint(Some(v)),
                    Err(e) => {
                        warn!(
                            orderbook_address = ob,
                            order_id = %id,
                            error = %e,
                            "getOrder deadline failed to decode during sweep repair; ABI drift?",
                        );
                        None
                    }
                }
            } else {
                None
            };
            if tc.is_some() || deadline.is_some() {
                repairs.push((id.clone(), tc, deadline));
            }
        }
        if !to_cancel.is_empty() {
            self.provisional_cancel(ob, &to_cancel).await?;
        }
        let repaired = self.repair_order_fields(ob, &repairs).await?;
        if repaired > 0 {
            debug!(orderbook_address = ob, repaired, "sweep repaired inference order fields");
        }

        // Both are `uint128` on chain (the `_nextOrderId` counter, read via
        // `getStats().nextOrderId`). The DB columns are
        // `numeric(78, 0)`, so a corrupted `sweep_cycle_max` can exceed `u128` and this parse
        // fails the step. That is fail-loud, and the `.context` names the value rather than
        // guarding against the overflow.
        let clamped = boc_next_order_id.parse::<u128>().context("boc nextOrderId")?
            < cycle_max.parse::<u128>().context("cycle_max")?;
        if clamped {
            // Order ids only grow, so `boc_next_order_id < cycle_max` means the gateway served
            // an account state older than the one that opened this cycle. Silent otherwise: the
            // cycle cannot complete, so a book in discovery keeps answering -1121 on depth,
            // markets and this endpoint until a fresh BOC arrives. This is the only signal.
            warn!(
                orderbook_address = ob,
                boc_next_order_id = %boc_next_order_id,
                cycle_max = %cycle_max,
                "sweep bound clamped by a rolled-back account state; cycle cannot complete",
            );
        }
        // A short batch means the BOUND was reached — which is the cycle's boundary only
        // when the BOC did not clamp it. Under a clamp the ids in
        // [boc_next_order_id, cycle_max) were never probed, and calling that a completed
        // cycle would reset the cursor and, under discovery, stamp visibility for a book
        // this sweep only half-inspected.
        let cycle_complete = ((ids.len() as i64) < self.sweep_batch_n) && !clamped;
        // `or(prev_cursor)` keeps the cycle's progress when a clamped batch comes back
        // empty: `ids.last()` is None there, and a None cursor is how a COMPLETED cycle is
        // spelled — it would silently rewind the cycle to its start.
        let new_cursor: Option<String> = if cycle_complete {
            None
        } else {
            ids.last().map(|(id, _, _)| id.clone()).or_else(|| prev_cursor.clone())
        };

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

    /// Fill in `token_contract` / `deadline` for live orders from a chain getter, in one
    /// statement.
    ///
    /// Each column is applied only when the row is missing it AND the getter supplied one,
    /// so a row whose remaining NULL is intentional is never rewritten — a bare
    /// `IS NULL` predicate stays true forever on healthy rows (a BUY's TC, a resting SELL's
    /// deadline) and would churn `updated_at` every cycle. Returns the number of rows
    /// changed, for logging only.
    pub async fn repair_order_fields(
        &self,
        ob: &str,
        repairs: &[(String, Option<String>, Option<String>)],
    ) -> anyhow::Result<u64> {
        if repairs.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = repairs.iter().map(|(id, _, _)| id.clone()).collect();
        let tcs: Vec<Option<String>> = repairs.iter().map(|(_, tc, _)| tc.clone()).collect();
        let deadlines: Vec<Option<String>> = repairs.iter().map(|(_, _, d)| d.clone()).collect();

        // Every array binds as `text[]` — that is what sqlx sends for `Vec<String>` and
        // `Vec<Option<String>>` — and the two numeric columns are cast per element at the
        // point of use. Declaring the parameter `$2::numeric[]` instead would lean on
        // Postgres's array-level `text[] -> numeric[]` coercion, which is a different code
        // path for no gain here.
        let res = sqlx::query(
            r#"update inference_orders o
                  set token_contract = coalesce(o.token_contract, r.tc),
                      deadline = coalesce(o.deadline, r.deadline::numeric),
                      updated_at = now()
                 from unnest($2::text[], $3::text[], $4::text[]) as r(order_id, tc, deadline)
                where o.orderbook_address = $1
                  and o.order_id = r.order_id::numeric
                  and o.status = 'OPEN'
                  and (   (o.token_contract is null and r.tc is not null)
                       or (o.deadline is null and r.deadline is not null))"#,
        )
        .bind(ob)
        .bind(&ids)
        .bind(&tcs)
        .bind(&deadlines)
        .execute(&self.pool)
        .await
        .context("repair inference order fields")?;
        Ok(res.rows_affected())
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
        self.select_refresh_books_scoped(None).await
    }

    /// The Queue B refresh SELECT, optionally restricted to the `scope`
    /// addresses. Production passes `None` (whole-table, `LIMIT BATCH_SIZE`). A
    /// selection test passes `Some(&addrs)` so the `LIMIT` and the
    /// `reference_price_at nulls first` ordering apply within its own seeded
    /// books only — otherwise a concurrent writer's never-priced (NULL
    /// `reference_price_at`) visible markets, which sort first, can fill the
    /// window and evict the test's candidate. Both paths run the identical
    /// due-predicate SQL, so a scoped test still exercises the production query.
    async fn select_refresh_books_scoped(
        &self,
        scope: Option<&[String]>,
    ) -> anyhow::Result<Vec<BookRow>> {
        let scope_clause = if scope.is_some() { "and m.orderbook_address = any($4)" } else { "" };
        let sql = format!(
            r#"select orderbook_address, model_hash::text as model_hash, reference_price_at, last_swept_at
                 from inference_markets m
                where last_reconciled_at is not null
                  and superseded_at is null
                  and (last_reconcile_failed_at is null
                       or last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
                  and (
                        (reference_price_at is null or reference_price_at < now() - make_interval(secs => $2))
                     or ( (last_swept_at is null or last_swept_at < now() - make_interval(secs => $3))
                          and exists (select 1 from inference_orders o
                                       where o.orderbook_address = m.orderbook_address and o.status='OPEN') )
                      )
                  {scope_clause}
                order by reference_price_at nulls first
                limit $1"#
        );
        let mut query = sqlx::query_as::<sqlx::Postgres, BookRow>(&sql)
            .bind(BATCH_SIZE)
            .bind(self.reference_price_refresh.as_secs_f64())
            .bind(self.sweep_interval.as_secs_f64());
        if let Some(scope) = scope {
            query = query.bind(scope.to_vec());
        }
        let rows = query.fetch_all(&self.pool).await.context("select refresh candidates")?;
        Ok(rows)
    }

    /// Test-facing view of the Queue B selection: the addresses `run_once` would
    /// refresh this tick. Wraps the same `select_refresh_books` query the loop
    /// runs, so the selection test exercises the production query, not a copy.
    pub async fn select_refresh_candidates(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.select_refresh_books().await?.into_iter().map(|r| r.orderbook_address).collect())
    }

    /// `select_refresh_candidates` restricted to `scope`. A selection test uses
    /// this so the shared test DB's other never-priced visible books cannot crowd
    /// the `nulls first` `LIMIT` window and evict the test's own candidate; the
    /// due-predicate SQL is identical to the unscoped production path.
    pub async fn select_refresh_candidates_within(
        &self,
        scope: &[String],
    ) -> anyhow::Result<Vec<String>> {
        Ok(self
            .select_refresh_books_scoped(Some(scope))
            .await?
            .into_iter()
            .map(|r| r.orderbook_address)
            .collect())
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
fn parse_semver(v: Option<&str>) -> Option<(u64, u64, u64)> {
    let v = v?;
    let mut it = v.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Version string from a `getVersion` getter result (`value0`).
fn version_from_getter(v: &serde_json::Value) -> Option<String> {
    v.get("value0").and_then(|x| x.as_str()).map(|s| s.to_owned())
}

/// String payload of the `getModelName` getter (`value0`); empty when absent or
/// non-string, which [`parse_model_ref`] maps to an all-`None` identity.
fn model_name_str(v: &serde_json::Value) -> &str {
    v.get("value0").and_then(|x| x.as_str()).unwrap_or("")
}

/// Parsed components of the on-chain `modelName` (the `getModelName` getter).
/// `model_ref` is the trimmed name verbatim; `producer`/`name`/`version` are set
/// only on a clean `producer--model--version` (exactly three non-empty parts).
/// An empty name yields all `None` — the read API then falls back to `model_hash`.
/// Note: `version` here is the MODEL version (e.g. `instruct`), distinct from the
/// contract version (`getVersion`) the slot-supersede logic parses as semver.
#[derive(Debug, Default, Clone)]
pub struct ModelIdentity {
    pub model_ref: Option<String>,
    pub producer: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
}

/// Map the on-chain `modelName` to a [`ModelIdentity`]. See its doc for the rules.
pub(crate) fn parse_model_ref(model_name: &str) -> ModelIdentity {
    let trimmed = model_name.trim();
    if trimmed.is_empty() {
        return ModelIdentity::default();
    }
    let model_ref = Some(trimmed.to_string());
    let parts: Vec<&str> = trimmed.split("--").collect();
    if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
        ModelIdentity {
            model_ref,
            producer: Some(parts[0].to_string()),
            name: Some(parts[1].to_string()),
            version: Some(parts[2].to_string()),
        }
    } else {
        ModelIdentity { model_ref, ..Default::default() }
    }
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

#[cfg(test)]
mod parse_tests {
    use super::parse_model_ref;

    #[test]
    fn three_parts_fill_all() {
        let id = parse_model_ref("qwen--qwen2.5-32b--instruct");
        assert_eq!(id.model_ref.as_deref(), Some("qwen--qwen2.5-32b--instruct"));
        assert_eq!(id.producer.as_deref(), Some("qwen"));
        assert_eq!(id.name.as_deref(), Some("qwen2.5-32b"));
        assert_eq!(id.version.as_deref(), Some("instruct"));
    }

    #[test]
    fn non_three_parts_keep_only_ref() {
        let id = parse_model_ref("just-a-name");
        assert_eq!(id.model_ref.as_deref(), Some("just-a-name"));
        assert!(id.producer.is_none() && id.name.is_none() && id.version.is_none());

        let id2 = parse_model_ref("a--b"); // 2 parts
        assert_eq!(id2.model_ref.as_deref(), Some("a--b"));
        assert!(id2.producer.is_none());
    }

    #[test]
    fn empty_yields_all_none() {
        let id = parse_model_ref("   ");
        assert!(
            id.model_ref.is_none()
                && id.producer.is_none()
                && id.name.is_none()
                && id.version.is_none()
        );
    }
}
