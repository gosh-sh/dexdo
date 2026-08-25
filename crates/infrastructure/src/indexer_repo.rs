// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use sqlx::Acquire;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::decoder::DecodeOutcome;
use crate::decoder::DecodedEvent;
use crate::decoder::Decoder;
use crate::graphql::EventEdge;
use crate::graphql::EventNode;
use crate::projectors;
use crate::projectors::ProjectionOutcome;

/// The capture loop's cursor stream name. The orphan dead-letter reads its
/// `at_head` flag to avoid dropping an orphan whose parent may still be ahead of
/// the cursor during a backfill (see `is_dead_letterable_orphan`).
pub const CAPTURE_STREAM: &str = "blockchain_events";
pub const DAPP_CAPTURE_STREAM: &str = "blockchain_events_dex_dapp";
pub const ROOT_PN_CAPTURE_STREAM: &str = "blockchain_events_root_pn";

#[derive(Debug, Clone)]
pub struct IndexerRepository {
    pool: PgPool,
    /// Event types already seen with no projector handler this process. The
    /// first sighting of a type is logged at the normal target (stdout + main
    /// log) as the operator's signal that a deployed contract emits something
    /// the indexer does not yet handle; every later repeat is diverted to the
    /// noise log. Shared via `Arc` across all clones. The projection loop
    /// (`run_reprojection_loop`) is the sole emitter of unknown-type sightings;
    /// "first" is process-global across the loop's passes. (The metrics-refresh
    /// clone shares the `Arc` too but never records sightings.) Bounded by the
    /// decoder's ABI event vocabulary, so it cannot grow without limit.
    seen_unknown_event_types: Arc<Mutex<HashSet<String>>>,
    /// Running count of projection batches that aborted the optimistic
    /// (savepoint-free) pass and replayed with per-row savepoints. Shared via
    /// `Arc` across all clones, so the projection loop's increments are visible
    /// to the metrics-refresh clone, which polls it for `indexer_projection_fallbacks`.
    /// A steadily climbing rate means projector errors are routinely dropping
    /// the fast path — a throughput regression the backlog/lag gauges only show
    /// as a symptom.
    projection_fallbacks: Arc<AtomicU64>,
    /// How long an inference `Filled`/`OrderCancelled` row may remain `Deferred`
    /// (measured from its ingest timestamp `raw_events.created_at`) before it is
    /// treated as a permanent orphan and dead-lettered. Applies ONLY to inference
    /// event types (`InferenceOrderBook.*`); DEX deferral is unaffected.
    inference_orphan_cutoff: std::time::Duration,
    /// Running count of inference orphan rows dead-lettered (marked processed
    /// without an order-table write) because their parent `OrderPlaced` never
    /// arrived within the cutoff window. Shared across clones via `Arc` — same
    /// pattern as `projection_fallbacks`.
    inference_orphans_dropped: Arc<AtomicU64>,
    /// Running count of event bodies the decoder attempted but failed to decode
    /// (`decode_output`/`detokenize` error, or an unparseable cell). These are
    /// stored undecoded (`event_type`/`decoded` NULL) and skipped by projection
    /// — byte-identical at rest to an unknown/ambiguous id, which is NOT counted
    /// here. A non-zero rate means ABI drift or malformed bodies for an otherwise
    /// known event. Shared across clones via `Arc`, like `projection_fallbacks`.
    decode_errors: Arc<AtomicU64>,
    /// Running count of decoded rows that matched no projector arm
    /// (`ProjectionOutcome::Unknown`). `Unknown` marks the row processed and never
    /// retries it, and the `warn!` beside it is demoted to the noise target after
    /// the first sighting of each type (`warn_unknown`), so without this counter a
    /// new contract event is decoded, dropped and leaves no trace anywhere an
    /// operator looks: backlog 0, decode_errors 0, observer green. Distinct
    /// from `decode_errors` and `decode_ambiguous_collisions` in the way that
    /// matters most — those rows keep their payload and stay replayable, these do
    /// not. Shared across clones via `Arc`, like `decode_errors`.
    unknown_events: Arc<AtomicU64>,
    /// Running count of event bodies left undecoded because their id collides
    /// across ABIs and no `dst` route disambiguated it (the `AmbiguousCollision`
    /// decode outcome). Distinct from a benign unknown id and from a hard decode
    /// error. Unreachable today: the collision `decoder.rs` names as REAL is
    /// `ContractDeployed`, declared byte-identically by `RootModel` and
    /// `TokenContract`, and both sides carry a mandatory `dst` route. A non-zero
    /// value means a new colliding ABI was added without a route — alert on it.
    /// Shared across clones via `Arc`.
    decode_ambiguous_collisions: Arc<AtomicU64>,
    /// Running count of hard inference reconcile failures (`Err` outcomes from
    /// discovery or refresh — not `NoBoc`, which is a benign skip). Bumped by the
    /// `InferenceReconciler` through a cloned handle and polled here for
    /// `indexer_inference_reconcile_failures`. Shared across clones via `Arc`,
    /// like `inference_orphans_dropped`.
    inference_reconcile_failures: Arc<AtomicU64>,
    /// Capture cursor stream whose `at_head` gates the inference orphan
    /// dead-letter (an orphan is only dropped once capture has drained to the
    /// chain tip, so a parent that is merely still-ahead-in-backfill is not
    /// mistaken for a permanently dropped one). Defaults to `CAPTURE_STREAM`;
    /// overridable in tests via `with_capture_stream` so they need not race the
    /// shared live cursor row.
    capture_stream: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PagePersistResult {
    pub inserted: u64,
    pub skipped: u64,
    pub decoded: u64,
    pub undecoded: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ReprojectionStats {
    pub scanned: u64,
    pub applied: u64,
    pub deferred: u64,
    pub unknown: u64,
    pub failed: u64,
    /// Highest `chain_order` read in the batch (rows are ordered asc, so the
    /// last row's). `None` for an empty batch. Drives the drain loop's cursor.
    /// It is a keyset cursor folded into the stats struct (returned alongside
    /// the counts to avoid a second return value), not an outcome counter.
    pub max_chain_order: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingRow {
    id: i64,
    msg_id: String,
    chain_order: String,
    src_address: Option<String>,
    dst_address: Option<String>,
    event_type: Option<String>,
    decoded: Option<Value>,
    ts: Option<f64>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// What [`IndexerRepository::dead_letter_verdict`] says about a deferred row: not
/// admitted, or admitted with the repair path that applies to it. Carrying the
/// path in the verdict is the point — see that function for the drift a bare
/// `bool` allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadLetterVerdict {
    /// Stays Deferred: the type is not dead-letterable, the row has not reached
    /// the cutoff, or capture is not at head.
    NotDeadLetterable,
    /// An `InferenceOrderBook.*` depth update — the book's own repair runs before
    /// the row is dropped.
    Book,
    /// `OracleEventList.RangeEventAdded` — it annotates a row it does not create,
    /// so there is no partial state to correct, only a loss to name.
    RangeEvent,
}

impl IndexerRepository {
    /// Shared subquery for "addresses that emitted at least one event inside the
    /// run window". Shared by `inference_books_with_events_since` and
    /// `inference_anchored_books_since`: their window predicate must be
    /// identical, or the anchor and the diagnostic would be talking about
    /// different runs.
    ///
    /// Written as `in (subquery)` rather than a per-book `exists(...)` for the
    /// query plan: `exists` with `src_address = m.orderbook_address` is covered
    /// by NO existing index — both `src_address` indices are partial on
    /// `processed_at is null` (`raw_events_pending_src_idx`,
    /// `raw_events_unprocessed_src_idx`), and processed rows are exactly what a
    /// run window needs (at the tail of a run they are the majority). This way
    /// the window is scanned once via `raw_events_created_at_idx` (migration
    /// 0007) instead of once per book on every poll.
    const EVENTS_IN_WINDOW: &'static str = r#"select e.src_address from raw_events e
                       where e.created_at >= to_timestamp($1::double precision)
                         and e.src_address is not null"#;
    /// Shared `count(*) filter (...)` projection for the three inference-market
    /// lifecycle buckets, in `(discovering, visible, failing)` order. Both
    /// `inference_market_state_counts` (whole-table) and
    /// `inference_market_state_counts_for` (scoped to a caller-supplied address
    /// set) below build from this one constant so the production count and the
    /// scoped query a test exercises can never drift — edit the bucket
    /// predicates here only.
    const MARKET_STATE_COUNTS_SELECT: &'static str = r#"
        count(*) filter (where last_reconciled_at is null
                           and last_reconcile_failed_at is null
                           and superseded_at is null) as discovering,
        count(*) filter (where last_reconciled_at is not null
                           and superseded_at is null) as visible,
        count(*) filter (where last_reconciled_at is null
                           and last_reconcile_failed_at is not null
                           and superseded_at is null) as failing"#;
    /// Shared WHERE-clause fragment for "this book carries no verdict": not
    /// visible, not superseded, and not failing **with a reason**. The first
    /// three states are exactly `MARKET_STATE_COUNTS_SELECT`'s
    /// `discovering`/`visible`/`failing` buckets; the one difference is that a
    /// failure stamp without text does not count as failing here, because
    /// IX-SEQ-10 asks for the reason and the observer cannot read the pod's logs.
    const NO_VERDICT_WHERE: &'static str = r#"m.last_reconciled_at is null
                  and m.superseded_at is null
                  and (m.last_reconcile_failed_at is null or m.last_reconcile_error is null)"#;
    /// Shared `count(*) filter (...)` projection for the four inference-order
    /// status buckets, in `(open, filled, cancelled, expired)` order. Shared by
    /// `inference_order_status_counts` (whole-table) and
    /// `inference_order_status_counts_for` (scoped) for the same anti-drift
    /// reason as `MARKET_STATE_COUNTS_SELECT`.
    const ORDER_STATUS_COUNTS_SELECT: &'static str = r#"
        count(*) filter (where status = 'OPEN') as open,
        count(*) filter (where status = 'FILLED') as filled,
        count(*) filter (where status = 'CANCELLED') as cancelled,
        count(*) filter (where status = 'EXPIRED') as expired"#;
    /// Shared WHERE-clause fragment for "a `raw_events` row the projection loop
    /// will pick up": unprocessed, typed and decoded. `count_pending_projection`
    /// (whole-table), `projection_lag_seconds` (age of the oldest such row) and
    /// `pending_projection_since` (the run-window breakdown the e2e observer
    /// reads) all build from this one constant — edit the predicate here only,
    /// never duplicate it.
    const PENDING_PROJECTION_WHERE: &'static str = r#"processed_at is null
                  and event_type is not null
                  and decoded is not null"#;
    /// Shared SELECT projection for the two staleness ages, in
    /// `(price_lag, sweep_lag)` order. Shared by `inference_staleness_seconds`
    /// (whole-table) and `inference_staleness_seconds_for` (scoped) for the
    /// same anti-drift reason as `ORDER_STATUS_COUNTS_SELECT`.
    const STALENESS_SELECT: &'static str = r#"
        extract(epoch from now() - min(reference_price_at))::bigint as price_lag,
        extract(epoch from now() - min(last_swept_at))::bigint as sweep_lag"#;
    /// Shared WHERE-clause fragment for "a row the projection loop will NOT pick
    /// up": untyped or undecoded. A deliberate complement to
    /// [`Self::PENDING_PROJECTION_WHERE`], not a variant of it: a growing count
    /// here diagnoses ABI drift (IX-CAP-05), but an event from a contract we do
    /// not know is not our failure, so the observer prints these rather than
    /// failing on them. `count_undecodable_since` (whole-table) and
    /// `undecodable_addresses_since` (scoped) both build from it.
    const UNDECODABLE_WHERE: &'static str = r#"processed_at is null
                  and (event_type is null or decoded is null)"#;
    /// Shared WHERE-clause fragment for the "wedged inference book" predicate:
    /// visible, not superseded, and with at least one still-unprocessed
    /// `raw_events` row under the book's `orderbook_address`. Both
    /// `inference_wedged_books_count` (whole-table) and
    /// `inference_wedged_book_addresses` (scoped to a caller-supplied address
    /// set) below build their query from this one constant, so the whole-table
    /// production count and the scoped query a test exercises can never drift
    /// apart — edit the predicate here only, never duplicate it.
    /// The event-type prefix [`Self::dex_capture_progress_since`] is taken over.
    /// `OrderBook.` is the prediction-market order book
    /// (`contracts/dex/OrderBook.sol`) — deliberately NOT `InferenceOrderBook.`,
    /// the airegistry book the inference anchor already covers. The distinction
    /// is load-bearing: matched as a LIKE prefix it anchors at the start, so
    /// `InferenceOrderBook.*` cannot satisfy the DEX anchor. Widening this to a
    /// contains-match would make inference traffic answer for the DEX side and
    /// quietly void the anchor; `observer_queries.rs` pins that it does not.
    const DEX_ANCHOR_EVENT_PREFIX: &'static str = "OrderBook.";
    const WEDGED_BOOKS_WHERE: &'static str = r#"m.last_reconciled_at is not null
                  and m.superseded_at is null
                  and exists(
                      select 1 from raw_events e
                       where e.src_address = m.orderbook_address
                         and e.processed_at is null)"#;

    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            seen_unknown_event_types: Arc::new(Mutex::new(HashSet::new())),
            projection_fallbacks: Arc::new(AtomicU64::new(0)),
            inference_orphan_cutoff: std::time::Duration::from_millis(1_800_000), /* default; overridden in main */
            inference_orphans_dropped: Arc::new(AtomicU64::new(0)),
            decode_errors: Arc::new(AtomicU64::new(0)),
            unknown_events: Arc::new(AtomicU64::new(0)),
            decode_ambiguous_collisions: Arc::new(AtomicU64::new(0)),
            inference_reconcile_failures: Arc::new(AtomicU64::new(0)),
            capture_stream: CAPTURE_STREAM.to_string(),
        }
    }

    pub fn with_inference_orphan_cutoff(mut self, cutoff: std::time::Duration) -> Self {
        self.inference_orphan_cutoff = cutoff;
        self
    }

    /// Override the capture cursor stream whose `at_head` gates orphan
    /// dead-lettering. Tests use a unique stream so they do not race the shared
    /// `blockchain_events` cursor row.
    pub fn with_capture_stream(mut self, stream: impl Into<String>) -> Self {
        self.capture_stream = stream.into();
        self
    }

    /// Running total of inference orphan rows that exceeded the cutoff and were
    /// dead-lettered (marked processed with no order-table write). Polled by the
    /// metrics-refresh loop for `indexer_inference_orphans_dropped`.
    pub fn inference_orphans_dropped_count(&self) -> u64 {
        self.inference_orphans_dropped.load(Ordering::Relaxed)
    }

    /// Running total of event bodies that failed to decode and were stored
    /// undecoded. Polled by the metrics-refresh loop for `indexer_decode_errors`.
    pub fn decode_errors_count(&self) -> u64 {
        self.decode_errors.load(Ordering::Relaxed)
    }

    /// Running total of decoded rows dropped by the `Unknown` arm. Polled by the
    /// metrics-refresh loop for `indexer_unknown_events`.
    pub fn unknown_events_count(&self) -> u64 {
        self.unknown_events.load(Ordering::Relaxed)
    }

    /// Running total of event bodies left undecoded due to an ambiguous event-id
    /// collision with no `dst` route. Polled by the metrics-refresh loop for
    /// `indexer_decode_ambiguous_collisions`.
    pub fn decode_ambiguous_collisions_count(&self) -> u64 {
        self.decode_ambiguous_collisions.load(Ordering::Relaxed)
    }

    /// Running total of hard inference reconcile failures. Polled by the
    /// metrics-refresh loop for `indexer_inference_reconcile_failures`.
    pub fn inference_reconcile_failures_count(&self) -> u64 {
        self.inference_reconcile_failures.load(Ordering::Relaxed)
    }

    /// A write-handle to the shared inference-reconcile-failure counter, for the
    /// `InferenceReconciler` to bump. The handle and `inference_reconcile_failures_count`
    /// read and write the same atomic, so a reconciler bump is visible to the
    /// metrics-refresh poll.
    pub fn inference_reconcile_failures_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.inference_reconcile_failures)
    }

    /// Returns `true` the first time `event_type` is seen as projector-unknown
    /// this process, `false` on every later sighting. Used to surface a novel
    /// event type once (loudly) without flooding the main log on every repeat.
    fn first_unknown_sighting(&self, event_type: &str) -> bool {
        // The guard is held only across the in-memory insert (no await), so a
        // poisoned lock can only mean a prior panic while holding it — recover
        // the set rather than cascading the panic into the ingest loop.
        let mut seen =
            self.seen_unknown_event_types.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.insert(event_type.to_string())
    }

    /// Counts `raw_events` rows grouped by `event_type`, restricted to the
    /// given types. Types with zero matching rows are omitted from the result;
    /// the caller defaults them to 0. Backs the indexer's DB-derived metric
    /// counters — cheap thanks to `raw_events_event_type_idx`.
    pub async fn count_events_by_type(
        &self,
        event_types: &[&str],
    ) -> anyhow::Result<Vec<(String, i64)>> {
        let types: Vec<String> = event_types.iter().map(|s| s.to_string()).collect();
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"select event_type, count(*)
                 from raw_events
                where event_type = any($1)
                group by event_type"#,
        )
        .bind(types.as_slice())
        .fetch_all(&self.pool)
        .await
        .context("count raw_events by event_type")?;
        Ok(rows)
    }

    pub async fn load_cursor(&self, stream_name: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("select cursor from indexer_cursors where stream_name = $1")
                .bind(stream_name)
                .fetch_optional(&self.pool)
                .await
                .context("select indexer_cursors")?;
        Ok(row.and_then(|(c,)| c))
    }

    /// Prepares the aggregate capture row before the projector starts. An
    /// existing aggregate cursor is preserved only when every source stream has
    /// already been initialized by the dual-stream capture loop. This makes the
    /// first deployment discard the old global-scan cursor instead of treating
    /// it as a valid cross-stream projection barrier.
    pub async fn initialize_capture_barrier(&self, source_streams: &[&str]) -> anyhow::Result<()> {
        let source_count: i64 =
            sqlx::query_scalar("select count(*) from indexer_cursors where stream_name = any($1)")
                .bind(source_streams)
                .fetch_one(&self.pool)
                .await
                .context("count initialized capture source streams")?;
        let cursor = if source_count == source_streams.len() as i64 {
            self.load_cursor(&self.capture_stream).await?
        } else {
            None
        };
        self.set_capture_barrier(cursor.as_deref(), false).await
    }

    /// Replaces the aggregate projection barrier after both source streams have
    /// completed a successful drain. Unlike `persist_page`, a null cursor must
    /// replace the previous value: it means no chain-order prefix is known to be
    /// complete yet, so projecting through the old cursor would be unsafe.
    pub async fn set_capture_barrier(
        &self,
        cursor: Option<&str>,
        at_head: bool,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"insert into indexer_cursors (stream_name, cursor, at_head, updated_at)
               values ($1, $2, $3, now())
               on conflict (stream_name) do update
                 set cursor = excluded.cursor,
                     at_head = excluded.at_head,
                     updated_at = now()"#,
        )
        .bind(&self.capture_stream)
        .bind(cursor)
        .bind(at_head)
        .execute(&self.pool)
        .await
        .context("upsert aggregate capture barrier")?;
        Ok(())
    }

    /// Returns `true` when the capture loop's most recent drain for `stream_name`
    /// reached the head of the blockchain (i.e. `has_next_page` was `false`).
    /// Returns `false` if the stream is unknown (no cursor row yet). Used by the
    /// inference reconciler as sweep catch-up gate (i).
    pub async fn at_head(&self, stream_name: &str) -> anyhow::Result<bool> {
        let row: Option<(bool,)> =
            sqlx::query_as("select at_head from indexer_cursors where stream_name = $1")
                .bind(stream_name)
                .fetch_optional(&self.pool)
                .await
                .context("select indexer_cursors.at_head")?;
        Ok(row.map(|(b,)| b).unwrap_or(false))
    }

    pub async fn persist_page(
        &self,
        stream_name: &str,
        edges: &[EventEdge],
        end_cursor: Option<&str>,
        decoder: &Decoder,
        at_head: bool,
    ) -> anyhow::Result<PagePersistResult> {
        let mut result = PagePersistResult::default();

        // Build column vectors for one bulk insert. Capture decodes (so the
        // row lands with `decoded` populated for the projection loop) but does
        // NOT project — projection is run_reprojection_loop's job. De-dup by
        // msg_id within the page so a repeated edge does not have to be
        // absorbed by `on conflict` and the inserted/skipped counts stay honest.
        let mut seen: HashSet<&str> = HashSet::with_capacity(edges.len());
        let mut msg_ids: Vec<String> = Vec::with_capacity(edges.len());
        let mut chain_orders: Vec<String> = Vec::with_capacity(edges.len());
        let mut created_ats: Vec<Option<f64>> = Vec::with_capacity(edges.len());
        let mut src_addresses: Vec<Option<String>> = Vec::with_capacity(edges.len());
        let mut dst_addresses: Vec<Option<String>> = Vec::with_capacity(edges.len());
        let mut event_types: Vec<Option<String>> = Vec::with_capacity(edges.len());
        let mut body_texts: Vec<String> = Vec::with_capacity(edges.len());
        let mut decoded_texts: Vec<Option<String>> = Vec::with_capacity(edges.len());

        for edge in edges {
            // `chain_order` is the projection-ordering key. The GraphQL gateway
            // promises it on every message edge; an event without it is unusable
            // (the projection SQL orders by `chain_order` and the column is NOT
            // NULL). Drop the edge with a warning rather than synthesise a key.
            let Some(chain_order) = edge.node.msg_chain_order.as_deref() else {
                result.undecoded += 1;
                warn!(
                    msg_id = %edge.node.msg_id,
                    "GraphQL event edge missing msg_chain_order; dropping row"
                );
                continue;
            };
            if !seen.insert(edge.node.msg_id.as_str()) {
                continue;
            }

            let decoded = try_decode(
                self,
                decoder,
                &edge.node.msg_id,
                edge.node.body.as_ref(),
                edge.node.dst.as_deref(),
            );
            if decoded.is_some() {
                result.decoded += 1;
            } else {
                result.undecoded += 1;
            }

            let created_at_chain = parse_unix_seconds(edge.node.created_at.as_ref());
            if should_warn_unparseable_created_at(edge.node.created_at.as_ref(), created_at_chain) {
                warn!(
                    msg_id = %edge.node.msg_id,
                    chain_order,
                    created_at = ?edge.node.created_at,
                    "GraphQL event edge has unparseable created_at; storing raw_events.created_at_chain as NULL"
                );
            }

            msg_ids.push(edge.node.msg_id.clone());
            chain_orders.push(chain_order.to_string());
            created_ats.push(created_at_chain);
            src_addresses.push(edge.node.src.clone());
            dst_addresses.push(edge.node.dst.clone());
            event_types.push(decoded.as_ref().map(|d| d.event_type.clone()));
            // body_json is NOT NULL; an absent body stores jsonb 'null'
            // ("null"::jsonb), exactly as the prior per-row path did.
            body_texts
                .push(edge.node.body.as_ref().map_or_else(|| "null".to_string(), Value::to_string));
            decoded_texts.push(decoded.as_ref().map(|d| d.value.to_string()));
        }

        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await.context("begin tx")?;

        if !msg_ids.is_empty() {
            // One prepared statement regardless of page size. The jsonb columns
            // are passed as text[] and cast with `::jsonb` so we never depend on
            // sqlx encoding an array of jsonb values directly.
            let inserted = sqlx::query(
                r#"insert into raw_events
                       (msg_id, chain_order, created_at_chain, src_address,
                        dst_address, event_type, body_json, decoded)
                   select msg_id, chain_order, to_timestamp(created_f8),
                          src_address, dst_address, event_type,
                          body_text::jsonb, decoded_text::jsonb
                     from unnest($1::text[], $2::text[], $3::double precision[],
                                 $4::text[], $5::text[], $6::text[],
                                 $7::text[], $8::text[])
                          as t(msg_id, chain_order, created_f8, src_address,
                               dst_address, event_type, body_text, decoded_text)
                   on conflict (msg_id) do nothing"#,
            )
            .bind(msg_ids.as_slice())
            .bind(chain_orders.as_slice())
            .bind(created_ats.as_slice())
            .bind(src_addresses.as_slice())
            .bind(dst_addresses.as_slice())
            .bind(event_types.as_slice())
            .bind(body_texts.as_slice())
            .bind(decoded_texts.as_slice())
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!(
                    "bulk insert raw_events chain_orders {:?}..{:?}",
                    chain_orders.first(),
                    chain_orders.last()
                )
            })?
            .rows_affected();

            result.inserted = inserted;
            result.skipped = msg_ids.len() as u64 - inserted;
        }

        sqlx::query(
            r#"insert into indexer_cursors (stream_name, cursor, at_head, updated_at)
               values ($1, $2, $3, now())
               on conflict (stream_name) do update
                 set cursor = coalesce(excluded.cursor, indexer_cursors.cursor),
                     at_head = excluded.at_head,
                     updated_at = now()"#,
        )
        .bind(stream_name)
        .bind(end_cursor)
        .bind(at_head)
        .execute(&mut *tx)
        .await
        .context("upsert indexer_cursors")?;

        tx.commit().await.context("commit tx")?;
        Ok(result)
    }

    /// Replays decoded-but-unprojected `raw_events` through the projector in
    /// `chain_order` order, considering only rows in the range
    /// `after_chain_order < chain_order <= until_chain_order` (either bound
    /// `None` = unbounded on that side). The drain loop passes the previous
    /// batch's high-water mark as `after` and the cycle ceiling as `until`, so
    /// each pending row is attempted at most once per cycle and the cycle is
    /// bounded to rows that existed at its start. `for update skip locked` + a
    /// single transaction keep concurrent workers from applying a non-idempotent
    /// projector twice.
    ///
    /// The batch is drained optimistically with no per-row savepoint (each
    /// applied row then costs only its projector statements, not an extra
    /// SAVEPOINT/RELEASE round-trip pair). If a projector errors, the optimistic
    /// transaction is rolled back untouched and the same range is replayed with
    /// per-row savepoints, which applies the clean rows and leaves the failing
    /// row pending — the same outcome as savepointing every row, paid for only
    /// when a failure actually occurs. The fallback is paid only on passes that
    /// hit a projector error — in practice the retry pass re-attempting a stuck
    /// row, though a newly captured row that errors on first sight also drops its
    /// forward pass to the fallback; a clean forward drain stays on the fast path.
    pub async fn reproject_pending_from(
        &self,
        batch_size: u32,
        after_chain_order: Option<&str>,
        until_chain_order: Option<&str>,
    ) -> anyhow::Result<ReprojectionStats> {
        // Gate orphan dead-lettering on the capture stream reaching head. While
        // capture is still backfilling, a missing parent may simply not be
        // ingested yet, so we must not declare it permanently dropped. On a read
        // error, default to `false` (skip drops this pass) — never abort the
        // batch, which may still apply non-orphan rows; the next pass retries.
        let capture_at_head = self.at_head(&self.capture_stream).await.unwrap_or_else(|err| {
            debug!(?err, stream = %self.capture_stream, "at_head read failed; deferring orphan drops");
            false
        });
        match self
            .reproject_batch_fast(batch_size, after_chain_order, until_chain_order, capture_at_head)
            .await?
        {
            Some(stats) => Ok(stats),
            None => {
                self.reproject_batch_savepointed(
                    batch_size,
                    after_chain_order,
                    until_chain_order,
                    capture_at_head,
                )
                .await
            }
        }
    }

    /// Optimistic drain: applies the whole batch in one transaction with no
    /// per-row savepoint. Returns `Ok(Some(stats))` on a clean pass (`failed` is
    /// always 0); returns `Ok(None)` if any projector errors, after rolling the
    /// transaction back so nothing is committed — the caller then replays the
    /// range via `reproject_batch_savepointed`.
    async fn reproject_batch_fast(
        &self,
        batch_size: u32,
        after_chain_order: Option<&str>,
        until_chain_order: Option<&str>,
        capture_at_head: bool,
    ) -> anyhow::Result<Option<ReprojectionStats>> {
        let mut tx: Transaction<'_, Postgres> =
            self.pool.begin().await.context("reproject(fast) tx begin")?;
        let rows =
            Self::fetch_pending_batch(&mut tx, batch_size, after_chain_order, until_chain_order)
                .await?;

        // Rows are ordered asc, so the last one carries the high-water chain_order.
        let mut stats = ReprojectionStats {
            max_chain_order: rows.last().map(|r| r.chain_order.clone()),
            ..Default::default()
        };
        let mut to_mark: Vec<i64> = Vec::new();
        // Unknown-type warnings are collected and emitted only after the batch
        // commits — `warn_unknown`'s log line and its first-sighting set mutation
        // both survive a rollback, so firing them mid-pass would double-warn the
        // same row once a later Err forces the savepointed replay.
        let mut unknown_warnings: Vec<(String, String)> = Vec::new();
        // Same reason, same discipline, for the two counters this pass owns: an
        // `AtomicU64` bumped inside the transaction survives its rollback, and the
        // savepointed replay re-processes the very same rows. Bumped mid-pass, one
        // dropped orphan would be reported as two the moment any LATER row in the
        // batch errored — and `indexer_inference_orphans_dropped` carries an alert
        // whose threshold is zero, so the over-count is not cosmetic.
        let mut orphans_dropped = 0u64;

        for row in rows {
            stats.scanned += 1;
            let Some((event, node)) = pending_row_to_inputs(&row) else {
                continue;
            };
            match projectors::project_event(&mut tx, &event, &node).await {
                Ok(ProjectionOutcome::Applied) => {
                    to_mark.push(row.id);
                    stats.applied += 1;
                }
                Ok(ProjectionOutcome::Deferred) => {
                    let verdict = Self::dead_letter_verdict(
                        &row,
                        self.inference_orphan_cutoff,
                        capture_at_head,
                    );
                    if verdict != DeadLetterVerdict::NotDeadLetterable {
                        // Repair the present resting leg(s) before dropping the row
                        // whose parent will never arrive (the repair emits the warn
                        // naming the data consequence). A repair DB error aborts this
                        // optimistic batch like a projector error; the savepointed
                        // replay then isolates the row.
                        let repaired = match verdict {
                            DeadLetterVerdict::Book => {
                                crate::inference_projectors::repair_expired_inference_orphan(
                                    &mut tx, &event, &node,
                                )
                                .await
                                .map(|_| ())
                            }
                            DeadLetterVerdict::RangeEvent => {
                                // Nothing in the read model to repair:
                                // `RangeEventAdded` annotates a row it does not
                                // create, so with the parent gone there is no
                                // partial state to correct. The loss is still
                                // named — a silent drop is indistinguishable from
                                // a row that was never captured at all.
                                warn!(
                                    msg_id = %row.msg_id,
                                    event_type = ?event.event_type,
                                    "orphan past cutoff dead-lettered: the parent never arrived — it lies outside the captured history, or its sibling EventAdded row is captured but not projected; the range-to-book linkage is lost for this event"
                                );
                                Ok(())
                            }
                            DeadLetterVerdict::NotDeadLetterable => unreachable!(
                                "the verdict gates this branch: a row that is not \
                                 dead-letterable never reaches it"
                            ),
                        };
                        match repaired {
                            Ok(()) => {
                                orphans_dropped += 1;
                                to_mark.push(row.id); // mark processed so it stops looping
                                stats.applied += 1;
                            }
                            Err(err) => {
                                self.projection_fallbacks.fetch_add(1, Ordering::Relaxed);
                                let rollback_error = tx.rollback().await.err();
                                warn!(
                                    msg_id = %row.msg_id,
                                    event_type = ?event.event_type,
                                    ?err,
                                    ?rollback_error,
                                    "expired-orphan repair errored in optimistic batch; falling back to per-row savepoints"
                                );
                                return Ok(None);
                            }
                        }
                    } else {
                        stats.deferred += 1;
                    }
                }
                Ok(ProjectionOutcome::Unknown) => {
                    unknown_warnings.push((row.msg_id.clone(), event.event_type.clone()));
                    to_mark.push(row.id);
                    stats.unknown += 1;
                }
                Err(err) => {
                    // The optimistic transaction is now aborted; discard it and
                    // let the caller replay this range with per-row savepoints,
                    // which applies the clean rows and isolates this one.
                    self.projection_fallbacks.fetch_add(1, Ordering::Relaxed);
                    let rollback_error = tx.rollback().await.err();
                    // A deterministic projector error (e.g. a missing field) is not
                    // a sqlx error: the savepointed replay re-attempts the row and
                    // emits the single authoritative `warn`, so log at debug to
                    // avoid double-warning. A DB-layer error (sqlx) or a failed
                    // rollback is instead a transient/health signal the replay may
                    // silently recover from on a fresh connection — surface it.
                    if err.downcast_ref::<sqlx::Error>().is_some() || rollback_error.is_some() {
                        warn!(
                            msg_id = %row.msg_id,
                            event_type = ?event.event_type,
                            ?err,
                            ?rollback_error,
                            "optimistic projection batch hit a DB-layer error; falling back to per-row savepoints"
                        );
                    } else {
                        debug!(
                            msg_id = %row.msg_id,
                            event_type = ?event.event_type,
                            ?err,
                            "optimistic projection batch errored; falling back to per-row savepoints"
                        );
                    }
                    return Ok(None);
                }
            }
        }

        Self::mark_processed(&mut tx, &to_mark).await?;
        tx.commit().await.context("reproject(fast) tx commit")?;
        // Durably committed — now (and only now) emit the unknown-type warnings,
        // record first-sightings, and bump the counters, in chain_order. A crash
        // between the commit and here loses only log lines (and the first-sighting
        // dedup) and under-counts by one batch; before the commit it would have
        // over-counted every batch that fell back, which is the worse direction for
        // a counter an alert reads.
        self.unknown_events.fetch_add(unknown_warnings.len() as u64, Ordering::Relaxed);
        self.inference_orphans_dropped.fetch_add(orphans_dropped, Ordering::Relaxed);
        for (msg_id, event_type) in &unknown_warnings {
            self.warn_unknown(msg_id, event_type);
        }
        Ok(Some(stats))
    }

    /// Pessimistic drain: wraps each row in a savepoint so one failing projector
    /// rolls back only its own row and the rest of the batch still commits. Used
    /// as the fallback when `reproject_batch_fast` hits an error.
    async fn reproject_batch_savepointed(
        &self,
        batch_size: u32,
        after_chain_order: Option<&str>,
        until_chain_order: Option<&str>,
        capture_at_head: bool,
    ) -> anyhow::Result<ReprojectionStats> {
        let mut tx: Transaction<'_, Postgres> =
            self.pool.begin().await.context("reproject tx begin")?;
        let rows =
            Self::fetch_pending_batch(&mut tx, batch_size, after_chain_order, until_chain_order)
                .await?;

        // Rows are ordered asc, so the last one carries the high-water chain_order.
        let mut stats = ReprojectionStats {
            max_chain_order: rows.last().map(|r| r.chain_order.clone()),
            ..Default::default()
        };
        let mut to_mark: Vec<i64> = Vec::new();
        // Buffered until the outer commit, for the same reason as in the fast path:
        // a savepoint release is not a commit, and neither an atomic nor a log line
        // rolls back with the transaction. `warn_unknown` in particular mutates the
        // first-sighting set, so firing it before a commit that then fails would
        // spend the one loud warning on a pass whose rows stayed pending — every
        // later sighting of that type is demoted to the noise target.
        let mut orphans_dropped = 0u64;
        let mut unknown_warnings: Vec<(String, String)> = Vec::new();

        for row in rows {
            stats.scanned += 1;
            let Some((event, node)) = pending_row_to_inputs(&row) else {
                continue;
            };

            let mut sp = tx.begin().await.context("reproject savepoint begin")?;
            let outcome = projectors::project_event(&mut sp, &event, &node).await;
            match outcome {
                Ok(ProjectionOutcome::Applied) => {
                    sp.commit().await.context("reproject savepoint release")?;
                    to_mark.push(row.id);
                    stats.applied += 1;
                }
                Ok(ProjectionOutcome::Deferred) => {
                    let verdict = Self::dead_letter_verdict(
                        &row,
                        self.inference_orphan_cutoff,
                        capture_at_head,
                    );
                    if verdict != DeadLetterVerdict::NotDeadLetterable {
                        // Repair the present leg(s) inside this row's savepoint, then
                        // release it; a repair error rolls back only this row.
                        let repaired = match verdict {
                            DeadLetterVerdict::Book => {
                                crate::inference_projectors::repair_expired_inference_orphan(
                                    &mut sp, &event, &node,
                                )
                                .await
                                .map(|_| ())
                            }
                            DeadLetterVerdict::RangeEvent => {
                                warn!(
                                    msg_id = %row.msg_id,
                                    event_type = ?event.event_type,
                                    "orphan past cutoff dead-lettered: the parent never arrived — it lies outside the captured history, or its sibling EventAdded row is captured but not projected; the range-to-book linkage is lost for this event"
                                );
                                Ok(())
                            }
                            DeadLetterVerdict::NotDeadLetterable => unreachable!(
                                "the verdict gates this branch: a row that is not \
                                 dead-letterable never reaches it"
                            ),
                        };
                        match repaired {
                            Ok(()) => {
                                sp.commit().await.context("reproject savepoint release")?;
                                orphans_dropped += 1;
                                to_mark.push(row.id); // mark processed so it stops looping
                                stats.applied += 1;
                            }
                            Err(err) => {
                                drop(sp);
                                stats.failed += 1;
                                warn!(
                                    msg_id = %row.msg_id,
                                    event_type = ?event.event_type,
                                    ?err,
                                    "expired-orphan repair failed; raw event still pending, savepoint rolled back"
                                );
                            }
                        }
                    } else {
                        // Not an expired orphan: release the savepoint so the seed
                        // skeleton written during project_event survives.
                        sp.commit().await.context("reproject savepoint release")?;
                        stats.deferred += 1;
                    }
                }
                Ok(ProjectionOutcome::Unknown) => {
                    sp.commit().await.context("reproject savepoint release")?;
                    unknown_warnings.push((row.msg_id.clone(), event.event_type.clone()));
                    to_mark.push(row.id);
                    stats.unknown += 1;
                }
                Err(err) => {
                    drop(sp);
                    stats.failed += 1;
                    warn!(
                        msg_id = %row.msg_id,
                        event_type = ?event.event_type,
                        ?err,
                        "reprojection failed; raw event still pending, savepoint rolled back"
                    );
                }
            }
        }

        Self::mark_processed(&mut tx, &to_mark).await?;
        tx.commit().await.context("reproject tx commit")?;
        self.unknown_events.fetch_add(unknown_warnings.len() as u64, Ordering::Relaxed);
        self.inference_orphans_dropped.fetch_add(orphans_dropped, Ordering::Relaxed);
        for (msg_id, event_type) in &unknown_warnings {
            self.warn_unknown(msg_id, event_type);
        }
        Ok(stats)
    }

    /// What the dead-letter rule says about a deferred row — and, for a row it
    /// admits, WHICH repair path applies.
    ///
    /// The dead letter is an ALLOW-LIST decision, not a property of deferral in
    /// general: the cutoff asserts "this parent will never arrive", which is a claim
    /// about a specific parent. `projectors.rs` defers in fourteen places and nearly
    /// all of them wait for something that legitimately arrives later — `PMPDeployed`
    /// waits for its `token_type` to show up in `ref_tokens`, and `TimingsSet`,
    /// `PoolsFrozen`, `Resolved` and the cancellation events wait for their own
    /// `PMPDeployed`. At the 30-minute production cutoff (`indexer.yaml.j2`),
    /// dead-lettering those would kill a market silently and permanently: the row is
    /// marked processed and never re-asked (IX-FAIL-06).
    ///
    /// A row is admitted when its type is dead-letterable, its **ingest** age
    /// (`now() - raw_events.created_at`) exceeds the configured cutoff, AND the
    /// capture stream has drained to the chain tip (`capture_at_head`). The
    /// `at_head` requirement keeps a parent that is merely still-ahead in an
    /// in-progress backfill from being mistaken for one that was permanently
    /// dropped at capture — "the parent will never arrive" is only declared once
    /// capture has reached head.
    ///
    /// Returned instead of a bare `bool` on purpose. Both call sites used to
    /// re-derive the repair path with a second, independent
    /// `starts_with("InferenceOrderBook.")`, so a new entry admitted by the first
    /// check would fall into the second's "nothing to repair" branch — silently,
    /// and precisely for the type the allow-list was extended to protect. One
    /// decision, taken once, removes that class of drift.
    fn dead_letter_verdict(
        row: &PendingRow,
        cutoff: std::time::Duration,
        capture_at_head: bool,
    ) -> DeadLetterVerdict {
        if !capture_at_head {
            return DeadLetterVerdict::NotDeadLetterable;
        }
        let aged =
            (chrono::Utc::now() - row.created_at).to_std().map(|age| age > cutoff).unwrap_or(false);
        if !aged {
            return DeadLetterVerdict::NotDeadLetterable;
        }
        // The two arms are the two entries of the old allow-list, and they carry
        // different promises. `InferenceOrderBook.` is a PREFIX on purpose: every
        // event the book can defer is a depth update whose parent `OrderPlaced`
        // lies outside the captured history, so a future book event inherits the
        // same verdict rather than becoming pending forever.
        // `OracleEventList.RangeEventAdded` is a FULL name on purpose: its sibling
        // `OracleEventList.EventAdded` must NOT be dead-letterable — that parent
        // arrives legitimately later, and dropping it would kill a market
        // silently. Adding a full name here is a narrow decision; adding a prefix
        // authorises every present and future event of that contract.
        match row.event_type.as_deref() {
            Some(t) if t.starts_with("InferenceOrderBook.") => DeadLetterVerdict::Book,
            Some("OracleEventList.RangeEventAdded") => DeadLetterVerdict::RangeEvent,
            _ => DeadLetterVerdict::NotDeadLetterable,
        }
    }

    /// The unknown-event warning: normal target on the first sighting of an
    /// event type, noise target thereafter.
    fn warn_unknown(&self, msg_id: &str, event_type: &str) {
        if self.first_unknown_sighting(event_type) {
            warn!(
                msg_id = %msg_id,
                event_type = %event_type,
                "reprojection has no handler for event type; marking processed and advancing (first sighting — later repeats go to the noise log)"
            );
        } else {
            warn!(
                target: dodex_logging::EVENT_NOISE_TARGET,
                msg_id = %msg_id,
                event_type = %event_type,
                "reprojection has no handler for event type; marking processed and advancing"
            );
        }
    }

    /// Keyset SELECT shared by both drain strategies: pending, typed, decoded
    /// rows in `(after, until]`, oldest first, row-locked with `skip locked` so
    /// a concurrent worker never double-applies.
    async fn fetch_pending_batch(
        tx: &mut Transaction<'_, Postgres>,
        batch_size: u32,
        after_chain_order: Option<&str>,
        until_chain_order: Option<&str>,
    ) -> anyhow::Result<Vec<PendingRow>> {
        sqlx::query_as(
            r#"select id,
                      msg_id,
                      chain_order,
                      src_address,
                      dst_address,
                      event_type,
                      decoded,
                      extract(epoch from created_at_chain)::double precision as ts,
                      created_at
                 from raw_events
                where processed_at is null
                  and event_type is not null
                  and decoded is not null
                  and ($2::text is null or chain_order > $2::text)
                  and ($3::text is null or chain_order <= $3::text)
                order by chain_order asc
                limit $1
                for update skip locked"#,
        )
        .bind(i64::from(batch_size))
        .bind(after_chain_order)
        .bind(until_chain_order)
        .fetch_all(&mut **tx)
        .await
        .context("select pending raw_events")
    }

    /// Batch-stamp `processed_at` for the rows the projector consumed this pass.
    async fn mark_processed(tx: &mut Transaction<'_, Postgres>, ids: &[i64]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let marked = sqlx::query(
            r#"update raw_events
                  set processed_at = now()
                where id = any($1)
                  and processed_at is null"#,
        )
        .bind(ids)
        .execute(&mut **tx)
        .await
        .context("batch mark raw_events.processed_at")?
        .rows_affected();
        if marked != ids.len() as u64 {
            warn!(
                expected = ids.len(),
                actual = marked,
                "batch-mark stamped fewer rows than projected: another writer may have set \
                 processed_at concurrently — single-consumer assumption may be violated"
            );
        }
        Ok(())
    }

    /// Projects the whole pending queue from the front, unbounded. Thin wrapper
    /// over `reproject_pending_from`; kept for tests and any single-shot caller.
    pub async fn reproject_pending(&self, batch_size: u32) -> anyhow::Result<ReprojectionStats> {
        self.reproject_pending_from(batch_size, None, None).await
    }

    /// Number of `raw_events` rows waiting for the projection loop — the
    /// backlog gauge. Predicate matches `reproject_pending_from`'s SELECT so the
    /// count reflects exactly what the loop will pick up. Cheap thanks to
    /// `raw_events_pending_chain_order_idx`.
    pub async fn count_pending_projection(&self) -> anyhow::Result<i64> {
        let sql =
            format!("select count(*) from raw_events where {}", Self::PENDING_PROJECTION_WHERE);
        let count: i64 = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .context("count pending projection")?;
        Ok(count)
    }

    /// Wall-clock age in seconds of the oldest eligible-but-unprojected raw_events
    /// row — how stale the read-model is. 0 when the projection queue is empty.
    /// Eligibility matches `count_pending_projection`. Chain time
    /// (`created_at_chain`) is nullable — the gateway may omit or send an
    /// unparseable `created_at` — so each row falls back to its non-null ingest
    /// time (`created_at`); a min over `coalesce(created_at_chain, created_at)`
    /// is therefore never NULL for a non-empty queue, so the gauge can't report
    /// 0 lag while pending work exists. Preferred over `now() - max(processed_at)`,
    /// which under-reports lag while the loop is busy projecting old rows.
    pub async fn projection_lag_seconds(&self) -> anyhow::Result<i64> {
        let sql = format!(
            "select extract(epoch from now() - min(coalesce(created_at_chain, created_at)))::bigint \
               from raw_events where {}",
            Self::PENDING_PROJECTION_WHERE
        );
        let secs: Option<i64> = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .context("projection lag seconds")?;
        Ok(secs.unwrap_or(0))
    }

    /// Seconds since the capture cursor for `stream_name` last advanced. `None`
    /// when no cursor row exists yet (capture not started). Small here while
    /// `projection_lag_seconds` is large is the "capture healthy, projection
    /// behind" signature.
    pub async fn cursor_age_seconds(&self, stream_name: &str) -> anyhow::Result<Option<i64>> {
        let secs: Option<i64> = sqlx::query_scalar(
            r#"select extract(epoch from now() - updated_at)::bigint
                 from indexer_cursors where stream_name = $1"#,
        )
        .bind(stream_name)
        .fetch_optional(&self.pool)
        .await
        .context("cursor age seconds")?;
        Ok(secs)
    }

    /// Inference order-book markets grouped by lifecycle state, backing
    /// `indexer_inference_markets`. Returns `(discovering, visible, failing)`:
    /// `discovering` is a seeded skeleton not yet visible and not currently
    /// failing; `visible` has `last_reconciled_at` stamped and is served by the
    /// API; `failing` is still invisible but the reconciler recorded a failure
    /// — the bucket that surfaces an ABI-drift book or a never-deployed /
    /// wrong-dApp address that would otherwise accrue `reconcile_attempts` with
    /// nothing in metrics. The three buckets partition the table.
    pub async fn inference_market_state_counts(&self) -> anyhow::Result<(i64, i64, i64)> {
        let sql = format!("select {} from inference_markets", Self::MARKET_STATE_COUNTS_SELECT);
        let row: (i64, i64, i64) = sqlx::query_as(&sql)
            .fetch_one(&self.pool)
            .await
            .context("inference market state counts")?;
        Ok(row)
    }

    /// `inference_market_state_counts` restricted to the `orderbook_address`es in
    /// `scope`, running the identical `MARKET_STATE_COUNTS_SELECT` bucket
    /// predicates. Exists so a test can assert exact per-bucket counts over just
    /// the rows it seeded — immune to a concurrent writer elsewhere in the shared
    /// test DB, which a whole-table delta cannot exclude.
    pub async fn inference_market_state_counts_for(
        &self,
        scope: &[String],
    ) -> anyhow::Result<(i64, i64, i64)> {
        let sql = format!(
            "select {} from inference_markets where orderbook_address = any($1)",
            Self::MARKET_STATE_COUNTS_SELECT
        );
        let row: (i64, i64, i64) = sqlx::query_as(&sql)
            .bind(scope)
            .fetch_one(&self.pool)
            .await
            .context("inference market state counts (scoped)")?;
        Ok(row)
    }

    /// Worst-case data staleness in seconds across visible inference markets,
    /// backing `indexer_inference_reference_price_lag_seconds` and
    /// `indexer_inference_sweep_lag_seconds`. Each value is
    /// `now() - min(ts)` over books with `last_reconciled_at` set — the oldest
    /// timestamp yields the largest age. Visibility implies both
    /// `reference_price_at` and `last_swept_at` are stamped (discovery refreshes
    /// the price and completes a sweep cycle before stamping
    /// `last_reconciled_at`), so `min` never skips a visible book. Returns
    /// `(reference_price_lag, sweep_lag)`, each 0 when no book is visible yet.
    /// Both values come from one query to keep it to a single round-trip.
    pub async fn inference_staleness_seconds(&self) -> anyhow::Result<(i64, i64)> {
        let sql = format!(
            "select {} from inference_markets where last_reconciled_at is not null",
            Self::STALENESS_SELECT
        );
        let row: (Option<i64>, Option<i64>) = sqlx::query_as(&sql)
            .fetch_one(&self.pool)
            .await
            .context("inference staleness seconds")?;
        Ok((row.0.unwrap_or(0), row.1.unwrap_or(0)))
    }

    /// `inference_staleness_seconds` restricted to the `orderbook_address`es in
    /// `scope`, running the identical `STALENESS_SELECT` age expressions.
    /// Test-scoping counterpart, same rationale as
    /// `inference_market_state_counts_for`. The `unwrap_or(0)` empty-set
    /// behaviour is shared too: a scope with no visible rows reads as zero lag.
    pub async fn inference_staleness_seconds_for(
        &self,
        scope: &[String],
    ) -> anyhow::Result<(i64, i64)> {
        let sql = format!(
            "select {} from inference_markets \
             where last_reconciled_at is not null and orderbook_address = any($1)",
            Self::STALENESS_SELECT
        );
        let row: (Option<i64>, Option<i64>) = sqlx::query_as(&sql)
            .bind(scope)
            .fetch_one(&self.pool)
            .await
            .context("inference staleness seconds (scoped)")?;
        Ok((row.0.unwrap_or(0), row.1.unwrap_or(0)))
    }

    /// Resting inference orders grouped by status, backing
    /// `indexer_inference_orders`. Returns `(open, filled, cancelled, expired)`
    /// — the four values of the `inference_orders.status` check constraint (see
    /// `migrations/0002_inference_order_expired.sql`). The four buckets
    /// partition the table.
    pub async fn inference_order_status_counts(&self) -> anyhow::Result<(i64, i64, i64, i64)> {
        let sql = format!("select {} from inference_orders", Self::ORDER_STATUS_COUNTS_SELECT);
        let row: (i64, i64, i64, i64) = sqlx::query_as(&sql)
            .fetch_one(&self.pool)
            .await
            .context("inference order status counts")?;
        Ok(row)
    }

    /// `inference_order_status_counts` restricted to the `orderbook_address`es in
    /// `scope`, running the identical `ORDER_STATUS_COUNTS_SELECT` bucket
    /// predicates. Test-scoping counterpart, same rationale as
    /// `inference_market_state_counts_for`.
    pub async fn inference_order_status_counts_for(
        &self,
        scope: &[String],
    ) -> anyhow::Result<(i64, i64, i64, i64)> {
        let sql = format!(
            "select {} from inference_orders where orderbook_address = any($1)",
            Self::ORDER_STATUS_COUNTS_SELECT
        );
        let row: (i64, i64, i64, i64) = sqlx::query_as(&sql)
            .bind(scope)
            .fetch_one(&self.pool)
            .await
            .context("inference order status counts (scoped)")?;
        Ok(row)
    }

    /// Visible inference order-book markets currently wedged by an unprojected
    /// `raw_events` row under their address, backing `indexer_inference_wedged_books`.
    /// Mirrors the read gate's arm-2 (`inference_read_repo::build_snapshot_query`):
    /// a book is "wedged" when it is visible AND has at least one `raw_events` row with
    /// `src_address = orderbook_address and processed_at is null` — the same
    /// predicate that trips `MarketInconsistent` (503) for a `tokenContract` query that
    /// scopes live SELLs (`side` not BUY and a status set including LIVE); a `note`
    /// filter, a `side=BUY` query, or a status set without LIVE against the same book
    /// still returns 200. The gate spells visibility as `last_reconciled_at is not null` alone; the
    /// `superseded_at is null` clause below is belt-and-suspenders (superseding always
    /// nulls `last_reconciled_at` in the same update, so it is redundant) and matches
    /// the sibling `visible` metric bucket — do not expect to find it in the gate's SQL.
    /// An ABI-lagging contract upgrade can wedge a book here indefinitely with no
    /// other symptom, so this is the signal an operator alerts on rather than
    /// reading 503s as a silent, unexplained outage. The correlated `EXISTS` rides
    /// `raw_events_unprocessed_src_idx` the same way the gate's probe does. Runs
    /// `WEDGED_BOOKS_WHERE` above, whole-table.
    pub async fn inference_wedged_books_count(&self) -> anyhow::Result<i64> {
        let sql =
            format!("select count(*) from inference_markets m where {}", Self::WEDGED_BOOKS_WHERE);
        let count: i64 = sqlx::query_scalar(&sql)
            .fetch_one(&self.pool)
            .await
            .context("inference wedged books count")?;
        Ok(count)
    }

    /// The `orderbook_address`es among `scope` that satisfy the same
    /// wedged-book predicate as `inference_wedged_books_count` above: both
    /// build from `WEDGED_BOOKS_WHERE`, the identical wedged-book predicate, and
    /// must stay in lockstep; this method additionally filters on
    /// `orderbook_address = any($1)`. Ordered by address for a deterministic result. Exists
    /// so a test can pin exactly which of several seeded addresses the
    /// predicate selects, scoped to those addresses so a concurrent writer
    /// elsewhere in the shared test DB cannot perturb the result — the
    /// whole-table count above can only report a delta, which cannot make that
    /// distinction.
    pub async fn inference_wedged_book_addresses(
        &self,
        scope: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let sql = format!(
            "select m.orderbook_address from inference_markets m \
             where m.orderbook_address = any($1) and {} \
             order by m.orderbook_address",
            Self::WEDGED_BOOKS_WHERE
        );
        let addrs: Vec<String> = sqlx::query_scalar(&sql)
            .bind(scope)
            .fetch_all(&self.pool)
            .await
            .context("inference wedged book addresses")?;
        Ok(addrs)
    }

    /// Breakdown of the unprojected backlog by event type, over rows **ingested**
    /// inside the run window. Predicate is `PENDING_PROJECTION_WHERE`, the same
    /// one `count_pending_projection` uses; only the window is added.
    ///
    /// The window keys on ingest time, not chain time, so that both it and the
    /// column it is compared against come from the same clock (the Postgres of
    /// the host the indexer runs on) — the CI runner's clock offset never enters
    /// the comparison.
    pub async fn pending_projection_since(&self, since: i64) -> anyhow::Result<Vec<(String, i64)>> {
        let sql = format!(
            "select event_type, count(*) from raw_events \
              where {} and created_at >= to_timestamp($1::double precision) \
              group by event_type order by event_type",
            Self::PENDING_PROJECTION_WHERE
        );
        let rows: Vec<(String, i64)> = sqlx::query_as(&sql)
            .bind(since as f64)
            .fetch_all(&self.pool)
            .await
            .context("pending projection since")?;
        Ok(rows)
    }

    /// How many rows ingested inside the window the projection loop will NOT pick
    /// up. This is the variant the observer calls: an undecodable row can come
    /// from any contract, and scoping it by address would mean "count some of
    /// them".
    pub async fn count_undecodable_since(&self, since: i64) -> anyhow::Result<i64> {
        let sql = format!(
            "select count(*) from raw_events \
              where {} and created_at >= to_timestamp($1::double precision)",
            Self::UNDECODABLE_WHERE
        );
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(since as f64)
            .fetch_one(&self.pool)
            .await
            .context("count undecodable since")?;
        Ok(count)
    }

    /// The addresses among `scope` carrying such a row inside the window. Exists
    /// for exactly what `inference_wedged_book_addresses` exists for: a test has
    /// to assert WHICH rows the predicate selected, and a whole-table count can
    /// only report a delta — which, in the shared test DB, breaks on an unrelated
    /// writer (`capture.rs` inserts an undecodable row and then purges it, moving
    /// two successive reads by different amounts).
    ///
    /// `distinct` is required, and not as decoration. The analogy with
    /// `inference_wedged_book_addresses` only half holds: that one selects from
    /// `inference_markets`, where the address is unique and the result is a set by
    /// construction, whereas `raw_events` yields a row per EVENT. Without
    /// `distinct` this would return a bag, and a second undecodable event under
    /// the same address would break an `assert_eq!` for a reason that has nothing
    /// to do with the window.
    pub async fn undecodable_addresses_since(
        &self,
        since: i64,
        scope: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let sql = format!(
            "select distinct src_address from raw_events \
              where {} and created_at >= to_timestamp($1::double precision) \
                and src_address = any($2) \
              order by src_address",
            Self::UNDECODABLE_WHERE
        );
        let addrs: Vec<String> = sqlx::query_scalar(&sql)
            .bind(since as f64)
            .bind(scope)
            .fetch_all(&self.pool)
            .await
            .context("undecodable addresses since")?;
        Ok(addrs)
    }

    /// Books that had at least one `raw_events` row under their address inside the
    /// run window. This is IX-SEQ-10's "book with events", narrowed to the current
    /// run: the stand's database outlives pipelines, and a book abandoned by a
    /// cancelled run would otherwise fail the next one for a foreign reason.
    pub async fn inference_books_with_events_since(
        &self,
        since: i64,
    ) -> anyhow::Result<Vec<String>> {
        let sql = format!(
            "select m.orderbook_address from inference_markets m \
              where m.orderbook_address in ({}) \
              order by m.orderbook_address",
            Self::EVENTS_IN_WINDOW
        );
        let addrs: Vec<String> = sqlx::query_scalar(&sql)
            .bind(since as f64)
            .fetch_all(&self.pool)
            .await
            .context("inference books with events since")?;
        Ok(addrs)
    }

    /// The addresses among `scope` carrying no verdict. The scope is mandatory:
    /// the observer feeds this the current run's books, and a whole-table variant
    /// would mean "fail the run over someone else's leftovers".
    pub async fn inference_books_without_verdict(
        &self,
        scope: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let sql = format!(
            "select m.orderbook_address from inference_markets m \
              where m.orderbook_address = any($1) and {} \
              order by m.orderbook_address",
            Self::NO_VERDICT_WHERE
        );
        let addrs: Vec<String> = sqlx::query_scalar(&sql)
            .bind(scope)
            .fetch_all(&self.pool)
            .await
            .context("inference books without verdict")?;
        Ok(addrs)
    }

    /// `(address, reason)` for the failing books among `scope`. The observer
    /// prints these even when it passes: `NoBoc` is a benign outcome that also
    /// stamps a failure, and without the distribution of reasons "failing with a
    /// reason" reads stricter than it actually is.
    ///
    /// Deliberately WIDER than `MARKET_STATE_COUNTS_SELECT`'s `failing` bucket:
    /// it carries no `last_reconciled_at is null`. A book that became visible and
    /// only then started failing is the most alarming class there is, and it is
    /// the one the gauge cannot show — the bucket counts it as `visible`. It
    /// reaches this list because `stamp_failure` writes the mark without touching
    /// `last_reconciled_at`, while the visibility stamp
    /// (`advance_sweep_and_maybe_stamp`) clears the mark in the same UPDATE, so
    /// "failed, then recovered through discovery" is already absent here.
    ///
    /// For a VISIBLE book the mark means "the most recent refresh failed", which is
    /// what makes this predicate honest without further clauses. Three writers keep
    /// it that way: the visibility stamp clears it (`advance_sweep_and_maybe_stamp`),
    /// a refresh pass that completes clears it (`clear_failure`, at the tail of
    /// `refresh_against_boc`), and `stamp_failure` sets it. A book that failed and
    /// recovered therefore leaves this list on its next clean pass, while one that
    /// broke after becoming visible stays in it.
    ///
    /// For a DISCOVERING book it still means "failed at least once since seeding":
    /// Queue A clears nothing, so a book that failed and now keeps missing its sweep
    /// gates stays named here until the visibility stamp lands. That is the weaker
    /// reading, and it is the right one to keep — a book stuck in discovery is worth
    /// naming whether its last tick failed or merely made no progress.
    ///
    /// `NoBoc` remains a failure on purpose: the account is not on chain yet, and
    /// the observer prints the reason text so a routine `NoBoc` reads as what it
    /// is rather than as an outage.
    pub async fn inference_failing_books(
        &self,
        scope: &[String],
    ) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"select m.orderbook_address, m.last_reconcile_error
                 from inference_markets m
                where m.orderbook_address = any($1)
                  and m.last_reconcile_failed_at is not null
                  and m.last_reconcile_error is not null
                  and m.superseded_at is null
                order by m.orderbook_address"#,
        )
        .bind(scope)
        .fetch_all(&self.pool)
        .await
        .context("inference failing books")?;
        Ok(rows)
    }

    /// Visible books carrying at least one projected order AND at least one event
    /// ingested inside the run window — IX-SEQ-11's positive anchor. It does NOT
    /// prove the book is the one a particular scenario deployed (scenario
    /// addresses are recorded nowhere a database-tail step could read them), and
    /// it does not prove the order itself was projected during this run.
    pub async fn inference_anchored_books_since(&self, since: i64) -> anyhow::Result<Vec<String>> {
        let sql = format!(
            "select distinct m.orderbook_address \
               from inference_markets m \
               join inference_orders o on o.orderbook_address = m.orderbook_address \
              where m.last_reconciled_at is not null \
                and m.superseded_at is null \
                and m.orderbook_address in ({}) \
              order by m.orderbook_address",
            Self::EVENTS_IN_WINDOW
        );
        let addrs: Vec<String> = sqlx::query_scalar(&sql)
            .bind(since as f64)
            .fetch_all(&self.pool)
            .await
            .context("inference anchored books since")?;
        Ok(addrs)
    }

    /// Per-type progress of DEX order-book events INGESTED in the window:
    /// `(event_type, captured, projected)`.
    ///
    /// The DEX counterpart of [`Self::inference_anchored_books_since`]. The two
    /// cover the two halves of `config::SCOPED_EVENT_IDS` — `contracts/dex` and
    /// `contracts/airegistry` — and the split is the reason this exists: one
    /// ingest scope feeds both sides, so an edit that drops the DEX ids while
    /// keeping the inference ones leaves the inference anchor green.
    ///
    /// One query serves both the anchor's assertion and the line it prints, so
    /// what is claimed and what is shown cannot drift apart.
    ///
    /// There is no `decoded` counter, and its absence is the point. `persist_page`
    /// fills `event_type` and `decoded` from one and the same `Option`, so a row
    /// is typed exactly when it is decoded — and a row whose decode failed carries
    /// no type at all, which is why it cannot match a prefix and never appears
    /// here. A `decoded` column would therefore always equal `captured` while
    /// suggesting the anchor covers decode failures. It does not:
    /// [`Self::count_undecodable_since`] does, and the anchor's failure message
    /// sends the reader there.
    ///
    /// Unlike [`Self::PENDING_PROJECTION_WHERE`] and its neighbours this is not
    /// built from a shared constant. Those are shared because a production gauge
    /// reads the same predicate and IX-MET-03 requires the two to match; this one
    /// backs no gauge, so there is no second definition to drift away from.
    pub async fn dex_capture_progress_since(
        &self,
        since: i64,
    ) -> anyhow::Result<Vec<(String, i64, i64)>> {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            r#"select event_type,
                      count(*) as captured,
                      count(*) filter (where processed_at is not null) as projected
                 from raw_events
                where created_at >= to_timestamp($1::double precision)
                  and event_type like $2
                group by event_type
                order by event_type"#,
        )
        .bind(since as f64)
        .bind(format!("{}%", Self::DEX_ANCHOR_EVENT_PREFIX))
        .fetch_all(&self.pool)
        .await
        .context("dex capture progress since")?;
        Ok(rows)
    }

    /// (in_use, idle) sqlx pool connections — cheap in-memory reads, no DB query.
    /// `size()` is total (in_use + idle); `num_idle()` is idle.
    pub fn pool_connection_stats(&self) -> (u64, u64) {
        let size = u64::from(self.pool.size());
        let idle = self.pool.num_idle() as u64;
        (size.saturating_sub(idle), idle)
    }

    /// Running total of projection batches that fell back from the optimistic
    /// pass to per-row savepoints (process-wide, since startup). Polled by the
    /// metrics-refresh loop for `indexer_projection_fallbacks`.
    pub fn projection_fallback_count(&self) -> u64 {
        self.projection_fallbacks.load(Ordering::Relaxed)
    }

    /// Highest pending `chain_order` right now, or `None` when the queue is
    /// empty. The drain loop snapshots this as each cycle's ceiling so the cycle
    /// is bounded to rows that existed at its start and terminates even under
    /// sustained ingest. Cheap — a backward scan endpoint of
    /// `raw_events_pending_chain_order_idx`.
    pub async fn max_pending_chain_order(&self) -> anyhow::Result<Option<String>> {
        let max: Option<String> = sqlx::query_scalar(
            r#"select max(chain_order) from raw_events
                where processed_at is null
                  and event_type is not null
                  and decoded is not null"#,
        )
        .fetch_one(&self.pool)
        .await
        .context("max pending chain_order")?;
        Ok(max)
    }

    /// Highest pending row that is safe to project through the aggregate
    /// capture barrier. A missing/null barrier yields `None` even when pending
    /// rows exist: one source stream has not established its ordered prefix yet.
    pub async fn max_projectable_chain_order(&self) -> anyhow::Result<Option<String>> {
        let max: Option<String> = sqlx::query_scalar(
            r#"select max(chain_order) from raw_events
                where processed_at is null
                  and event_type is not null
                  and decoded is not null
                  and chain_order <= (
                      select cursor from indexer_cursors where stream_name = $1
                  )"#,
        )
        .bind(&self.capture_stream)
        .fetch_one(&self.pool)
        .await
        .context("max pending chain_order through capture barrier")?;
        Ok(max)
    }

    /// Whether any pending row exists with `chain_order` above the argument. The
    /// drain loop calls this after a cycle to decide whether to idle: rows at or
    /// below the just-drained ceiling are stuck (Deferred/failed, already
    /// attempted this cycle), so the loop sleeps only when NO new rows have
    /// arrived above it — it never idles while applicable work is queued. The
    /// `>` comparison is done in SQL so it matches the column's Postgres
    /// collation (the same ordering `reproject_pending_from` filters on).
    pub async fn has_pending_above(&self, chain_order: &str) -> anyhow::Result<bool> {
        let exists: bool = sqlx::query_scalar(
            r#"select exists(
                   select 1 from raw_events
                    where processed_at is null
                      and event_type is not null
                      and decoded is not null
                      and chain_order > $1)"#,
        )
        .bind(chain_order)
        .fetch_one(&self.pool)
        .await
        .context("has pending above chain_order")?;
        Ok(exists)
    }

    /// Whether newly captured work exists above `chain_order` but still inside
    /// the current aggregate capture barrier. Rows beyond the barrier belong to
    /// a source stream that has run ahead and must not make the projector spin.
    pub async fn has_projectable_pending_above(&self, chain_order: &str) -> anyhow::Result<bool> {
        let exists: bool = sqlx::query_scalar(
            r#"select exists(
                   select 1 from raw_events
                    where processed_at is null
                      and event_type is not null
                      and decoded is not null
                      and chain_order > $1
                      and chain_order <= (
                          select cursor from indexer_cursors where stream_name = $2
                      ))"#,
        )
        .bind(chain_order)
        .bind(&self.capture_stream)
        .fetch_one(&self.pool)
        .await
        .context("has pending above chain_order through capture barrier")?;
        Ok(exists)
    }

    /// Hot loop, runs forever until cancelled. The sole projector. It keeps a
    /// forward `floor` (high-water chain_order already attempted) and a retry
    /// timer; each pass snapshots a `ceiling` (highest pending chain_order now)
    /// and drains the bounded range `(after, ceiling]` batch by batch.
    ///  - Forward pass (default): `after` = floor, so it drains only newly
    ///    captured rows above the floor and never re-touches the stuck
    ///    Deferred/failed rows below it; the floor then advances to the ceiling.
    ///  - Retry pass: `after` = None (front), re-attempting the stuck rows too,
    ///    rate-limited to once per `idle_interval` — so a permanently stuck row
    ///    is re-tried/re-logged on the polling cadence, not the drain cadence.
    ///
    /// The ceiling bounds every pass so it terminates under sustained ingest; the
    /// retry timer fires every idle_interval regardless of ingest (Deferred rows
    /// retried within ~one interval); the post-pass sleep is conditional on
    /// `has_pending_above`, so the projector never idles with work queued.
    /// `idle_interval` is wired to polling_interval_ms.
    pub async fn run_reprojection_loop(self, idle_interval: Duration, batch_size: u32) {
        let mut floor: Option<String> = None;
        let mut last_retry = tokio::time::Instant::now();
        let mut force_retry = true; // first pass rewinds to the front

        loop {
            // Forward passes resume above the floor; a rate-limited retry pass
            // rewinds to the front to re-attempt the stuck set below the floor.
            let (mut after, reset_timer) =
                next_pass_start(force_retry, last_retry.elapsed(), idle_interval, &floor);
            force_retry = false;
            if reset_timer {
                last_retry = tokio::time::Instant::now();
            }

            // Ceiling = highest pending chain_order inside the synchronized
            // capture barrier. Rows from a faster source stream stay pending
            // until the slower stream proves the preceding range complete.
            let ceiling = match self.max_projectable_chain_order().await {
                Ok(Some(c)) => c,
                Ok(None) => {
                    tokio::time::sleep(idle_interval).await;
                    continue;
                }
                Err(err) => {
                    error!(?err, "projection ceiling query failed");
                    tokio::time::sleep(idle_interval).await;
                    continue;
                }
            };

            let mut drained_clean = true;
            loop {
                match self
                    .reproject_pending_from(batch_size, after.as_deref(), Some(&ceiling))
                    .await
                {
                    Ok(stats) => {
                        if stats.scanned > 0 {
                            info!(
                                scanned = stats.scanned,
                                applied = stats.applied,
                                deferred = stats.deferred,
                                unknown = stats.unknown,
                                failed = stats.failed,
                                "projection sweep"
                            );
                        }
                        // Not a full batch -> the bounded range (after, ceiling]
                        // is drained; stop.
                        if stats.scanned < u64::from(batch_size) {
                            break;
                        }
                        // Full batch -> advance past the highest chain_order read.
                        // A full batch always has a max; if absent, stop.
                        match stats.max_chain_order {
                            Some(co) => after = Some(co),
                            None => break,
                        }
                    }
                    Err(err) => {
                        error!(?err, "projection sweep failed");
                        drained_clean = false;
                        break;
                    }
                }
            }
            // Advances floor only after a clean drain; on a sweep error the floor
            // is left so the next forward pass re-covers the range.
            if drained_clean {
                floor = Some(ceiling.clone());
            }

            // Backlog gauge: rows still pending after the pass (Deferred waiting
            // on a parent, or rows that arrived above the ceiling). Info only when
            // non-zero to avoid an idle flood.
            match self.count_pending_projection().await {
                Ok(backlog) if backlog > 0 => info!(backlog, "projection backlog"),
                Ok(backlog) => debug!(backlog, "projection backlog (drained)"),
                Err(err) => warn!(?err, "projection backlog gauge failed"),
            }

            // Idle only if no new rows arrived above the ceiling. If they did, run
            // the next pass immediately so the projector never idles with
            // applicable work queued. The retry timer still fires on schedule.
            match self.has_projectable_pending_above(&ceiling).await {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(idle_interval).await,
                Err(err) => {
                    warn!(?err, "projection has-pending-above check failed; idling");
                    tokio::time::sleep(idle_interval).await;
                }
            }
        }
    }
}

/// Reconstructs the projector inputs from a stored `raw_events` row. Returns
/// `None` when the row is not eligible for reprojection (e.g. event_type is
/// missing, which the SQL filter already rejects but defensive code keeps the
/// invariant local). `contract_kind` / `event_name` / `body` are unused on
/// the projection path — only `event_type`, `value`, `msg_id`, `src` and
/// `created_at` are read by `projectors::project_event`.
fn pending_row_to_inputs(row: &PendingRow) -> Option<(DecodedEvent, EventNode)> {
    let event_type = row.event_type.clone()?;
    let value = row.decoded.clone().unwrap_or(Value::Null);

    let event = DecodedEvent { contract_kind: "", event_name: String::new(), event_type, value };
    let node = EventNode {
        msg_id: row.msg_id.clone(),
        msg_chain_order: Some(row.chain_order.clone()),
        src: row.src_address.clone(),
        src_dapp_id: None,
        dst: row.dst_address.clone(),
        body: None,
        created_at: row.ts.and_then(serde_json::Number::from_f64).map(Value::Number),
    };
    Some((event, node))
}

fn try_decode(
    repo: &IndexerRepository,
    decoder: &Decoder,
    msg_id: &str,
    body: Option<&Value>,
    dst: Option<&str>,
) -> Option<DecodedEvent> {
    let body_str = body.and_then(Value::as_str)?;
    match decoder.decode_event_body(body_str, dst) {
        Ok(DecodeOutcome::Decoded(d)) => Some(d),
        // Benign: a contract emitted an id the indexer does not index. Silent.
        Ok(DecodeOutcome::UnknownId) => None,
        Ok(DecodeOutcome::AmbiguousCollision { event_id }) => {
            // A colliding id with no dst route — left undecoded (never first-ABI).
            // Count it so a new-colliding-ABI-without-route regression is alertable
            // (distinct from benign unknown-id noise), and route the repeat warn
            // through the noise-dedup so a regression flood does not drown the main
            // log. Keyed on a synthetic type so it reuses the no-handler dedup set.
            repo.decode_ambiguous_collisions.fetch_add(1, Ordering::Relaxed);
            let key = format!("ambiguous_collision:{event_id}");
            if repo.first_unknown_sighting(&key) {
                warn!(
                    msg_id,
                    event_id,
                    "ambiguous event_id with no dst route; left undecoded (first sighting — repeats go to the noise log). A new colliding ABI likely needs a dst route in the decoder."
                );
            } else {
                warn!(
                    target: dodex_logging::EVENT_NOISE_TARGET,
                    msg_id,
                    event_id,
                    "ambiguous event_id with no dst route; left undecoded"
                );
            }
            None
        }
        Err(err) => {
            // Hard decode failure of a body the gateway delivered — distinct from
            // an unknown/ambiguous id. Count it so ABI drift or a malformed cell
            // on a known event is observable, not just a single warn line. The
            // row is still stored undecoded and skipped by projection (and is
            // invisible to the sweep's `decoded IS NOT NULL` pending-events gate),
            // so the counter is the only durable signal.
            repo.decode_errors.fetch_add(1, Ordering::Relaxed);
            warn!(msg_id, ?err, "decode body failed");
            None
        }
    }
    .inspect(|d| debug!(msg_id, event_type = %d.event_type, "decoded event"))
}

pub(crate) fn parse_unix_seconds(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(n) = value.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = value.as_u64() {
        return Some(n as f64);
    }
    if let Some(n) = value.as_f64() {
        return n.is_finite().then_some(n);
    }
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.parse::<i64>() {
            return Some(n as f64);
        }
        // is_finite: "inf"/"NaN" parse as f64 but are not timestamps —
        // to_timestamp('infinity') would pass IS NOT NULL read filters and
        // crash the epoch ::bigint cast, 500ing the whole page.
        if let Ok(n) = s.parse::<f64>() {
            return n.is_finite().then_some(n);
        }
    }
    None
}

fn should_warn_unparseable_created_at(value: Option<&Value>, parsed: Option<f64>) -> bool {
    value.is_some() && parsed.is_none()
}

/// The pass-mode decision of `run_reprojection_loop`, extracted pure so a unit
/// can pin it without a clock or a DB (`elapsed` is the caller's
/// `last_retry.elapsed()`). Returns `(after, reset_timer)`: `after` is the
/// pass's lower bound — the floor on a forward pass, `None` (the front) on a
/// retry — and `reset_timer` is true exactly on the retry choice, never on a
/// forward pass. This is the IX-FAIL-01 negative half: a regression passing
/// `None` on every pass (or resetting the timer on forward passes, starving
/// the retry) is caught here deterministically, where a wall-clock lower-bound
/// assert on the live loop would flake — the timer's phase resets at the START
/// of a retry pass, so a slow pass legally retries "too early" relative to any
/// post-pass synchronization point.
fn next_pass_start(
    force_retry: bool,
    elapsed: Duration,
    idle_interval: Duration,
    floor: &Option<String>,
) -> (Option<String>, bool) {
    if force_retry || elapsed >= idle_interval {
        (None, true)
    } else {
        (floor.clone(), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PendingRow` with only the fields the verdict reads.
    fn pending_row_for_test(
        event_type: &str,
        created_at: chrono::DateTime<chrono::Utc>,
    ) -> PendingRow {
        PendingRow {
            id: 1,
            msg_id: "m".to_string(),
            chain_order: "co".to_string(),
            src_address: None,
            dst_address: None,
            event_type: Some(event_type.to_string()),
            decoded: None,
            ts: None,
            created_at,
        }
    }

    /// The verdict names the repair path, not just admission — which is the whole
    /// reason it replaced a `bool`. Admission and repair used to be two independent
    /// `starts_with` checks, so a type the first admitted could reach the second's
    /// "nothing to repair" branch silently.
    #[test]
    fn dead_letter_verdict_names_the_repair_path_not_just_admission() {
        let cutoff = std::time::Duration::from_secs(1);
        let old = chrono::Utc::now() - chrono::Duration::seconds(3600);

        let book = pending_row_for_test("InferenceOrderBook.InferenceFilled", old);
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&book, cutoff, true),
            DeadLetterVerdict::Book
        );
        // The prefix covers a type nobody has written a projector for yet: that is
        // what "prefix on purpose" means, and it must hold for a future event too.
        let future_book = pending_row_for_test("InferenceOrderBook.SomethingNewInV5", old);
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&future_book, cutoff, true),
            DeadLetterVerdict::Book
        );

        let range = pending_row_for_test("OracleEventList.RangeEventAdded", old);
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&range, cutoff, true),
            DeadLetterVerdict::RangeEvent
        );
        // The sibling from the SAME contract must not be admitted: its parent
        // arrives legitimately later, and dead-lettering it kills a market
        // silently. This is the assert that makes "full name, not prefix" real.
        let sibling = pending_row_for_test("OracleEventList.EventAdded", old);
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&sibling, cutoff, true),
            DeadLetterVerdict::NotDeadLetterable
        );

        // Not at head: nothing is dead-letterable, whatever the type — a parent
        // still ahead in a backfill is not a parent that will never arrive.
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&book, cutoff, false),
            DeadLetterVerdict::NotDeadLetterable
        );
        // Fresh: the cutoff refuses it, not the type.
        let fresh = pending_row_for_test("InferenceOrderBook.InferenceFilled", chrono::Utc::now());
        assert_eq!(
            IndexerRepository::dead_letter_verdict(&fresh, cutoff, true),
            DeadLetterVerdict::NotDeadLetterable
        );
    }

    #[test]
    fn parses_integer_unix_seconds() {
        assert_eq!(
            parse_unix_seconds(Some(&Value::from(1_710_000_000_i64))),
            Some(1_710_000_000.0)
        );
    }

    #[test]
    fn parses_string_unix_seconds() {
        assert_eq!(parse_unix_seconds(Some(&Value::from("1710000000"))), Some(1_710_000_000.0));
    }

    #[test]
    fn parses_float_unix_seconds() {
        assert_eq!(
            parse_unix_seconds(Some(&Value::from(1_710_000_000.5_f64))),
            Some(1_710_000_000.5)
        );
    }

    #[test]
    fn handles_missing_value() {
        assert_eq!(parse_unix_seconds(None), None);
        assert_eq!(parse_unix_seconds(Some(&Value::Null)), None);
    }

    // IX-FAIL-01 is closed by a triple: (1) these units pin the loop's CHOICE
    // of pass mode; (2) reproject_pending_from_honors_after_and_until_bounds
    // (tests/reprojection.rs) pins that the method honors the bound it is
    // given; (3) the loop test's upper bound (deferred_retry_rides_the_idle
    // _interval_timer, same file) pins that a timer retry actually reaches a
    // stuck row. No wall-clock lower bound is asserted anywhere — see
    // `next_pass_start`'s doc for why such an assert flakes on correct code.

    #[test]
    fn next_pass_start_resumes_above_the_floor_on_a_forward_pass() {
        let floor = Some("5f80co".to_string());
        let (after, reset) =
            next_pass_start(false, Duration::from_millis(299), Duration::from_millis(300), &floor);
        assert_eq!(after, floor, "a forward pass must resume above the floor");
        assert!(!reset, "the retry timer must NOT reset on a forward pass");
    }

    #[test]
    fn next_pass_start_rewinds_to_the_front_when_the_timer_expires() {
        let floor = Some("5f80co".to_string());
        let (after, reset) =
            next_pass_start(false, Duration::from_millis(300), Duration::from_millis(300), &floor);
        assert_eq!(after, None, "an expired timer must rewind to the front");
        assert!(reset, "the retry choice must reset the timer");
    }

    #[test]
    fn next_pass_start_rewinds_on_force_retry_regardless_of_elapsed() {
        let floor = Some("5f80co".to_string());
        let (after, reset) =
            next_pass_start(true, Duration::ZERO, Duration::from_millis(300), &floor);
        assert_eq!(after, None, "force_retry (the first pass) must read from the front");
        assert!(reset, "force_retry is a retry choice and must reset the timer");
    }

    #[test]
    fn next_pass_start_with_no_floor_still_only_resets_on_retry() {
        // A forward pass over an empty floor also reads from the front, but the
        // timer reset must stay attached to the retry CHOICE, not to the
        // resulting bound — resetting here would starve the rate-limited retry.
        let (after, reset) =
            next_pass_start(false, Duration::from_millis(1), Duration::from_millis(300), &None);
        assert_eq!(after, None);
        assert!(!reset, "a forward pass over an empty floor is not a retry");
    }

    #[test]
    fn warns_only_for_present_unparseable_created_at() {
        assert!(should_warn_unparseable_created_at(Some(&Value::from("NaN")), None));
        assert!(should_warn_unparseable_created_at(Some(&Value::from("not-a-time")), None));
        assert!(should_warn_unparseable_created_at(Some(&Value::Null), None));

        assert!(!should_warn_unparseable_created_at(None, None));
        assert!(!should_warn_unparseable_created_at(
            Some(&Value::from("1710000000")),
            Some(1_710_000_000.0)
        ));
    }

    /// A non-finite string parses as f64 but is not a timestamp: letting it
    /// through lands `to_timestamp('infinity')` in chain-time columns, which
    /// passes `IS NOT NULL` read filters and then blows up the epoch
    /// `::bigint` cast — a permanent 500 for the whole page.
    #[test]
    fn rejects_non_finite_values() {
        for bad in ["inf", "-inf", "Infinity", "NaN", "nan"] {
            assert_eq!(parse_unix_seconds(Some(&Value::from(bad))), None, "string {bad:?}");
        }
    }

    /// The main-vs-noise split for unhandled events rides entirely on this
    /// boolean: `true` routes the first sighting to the operator-visible log,
    /// `false` diverts repeats to the noise log. Pin the direction (true once
    /// per type, false thereafter) and the per-type independence, so a flipped
    /// return — which would flood the main log with the repeats this guard
    /// exists to suppress — fails here rather than only in production.
    #[tokio::test]
    async fn first_unknown_sighting_is_true_once_per_type() {
        // A lazy pool never opens a connection unless a query runs;
        // `first_unknown_sighting` only touches the in-memory set, so this
        // needs no database — only a Tokio context for the pool to build in.
        let pool = PgPool::connect_lazy("postgres://unused/unused").expect("lazy pool");
        let repo = IndexerRepository::new(pool);

        assert!(repo.first_unknown_sighting("OrderBook.NovelEvent"), "first sighting is true");
        assert!(!repo.first_unknown_sighting("OrderBook.NovelEvent"), "repeat is false");
        assert!(!repo.first_unknown_sighting("OrderBook.NovelEvent"), "still false");

        // Tracking is per-type: a different type is still "first" even after
        // another type has been seen.
        assert!(repo.first_unknown_sighting("PMP.OtherNovelEvent"), "distinct type is first");
        assert!(!repo.first_unknown_sighting("PMP.OtherNovelEvent"), "its repeat is false");
        assert!(!repo.first_unknown_sighting("OrderBook.NovelEvent"), "earlier type stays seen");
    }

    fn pending_row_with(
        event_type: Option<&str>,
        decoded: Option<Value>,
        ts: Option<f64>,
    ) -> PendingRow {
        PendingRow {
            id: 42,
            msg_id: "msg-42".to_string(),
            chain_order: "5f8000000000000003".to_string(),
            src_address: Some("0:src".to_string()),
            dst_address: Some("0:dst".to_string()),
            event_type: event_type.map(str::to_string),
            decoded,
            ts,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn pending_row_to_inputs_maps_full_payload() {
        let decoded = serde_json::json!({"oracle": "0:abc", "name": "n"});
        let row = pending_row_with(
            Some("RootOracle.OracleDeployed"),
            Some(decoded.clone()),
            Some(1_700_000_000.5),
        );

        let (event, node) = pending_row_to_inputs(&row).expect("inputs");

        assert_eq!(event.event_type, "RootOracle.OracleDeployed");
        assert_eq!(event.contract_kind, "");
        assert!(event.event_name.is_empty());
        assert_eq!(event.value, decoded);

        assert_eq!(node.msg_id, "msg-42");
        assert_eq!(node.src.as_deref(), Some("0:src"));
        assert_eq!(node.dst.as_deref(), Some("0:dst"));
        assert!(node.src_dapp_id.is_none());
        assert!(node.body.is_none());
        // Round-trips through serde_json::Number, which preserves f64.
        assert_eq!(node.created_at.as_ref().and_then(Value::as_f64), Some(1_700_000_000.5));
    }

    #[test]
    fn pending_row_to_inputs_returns_none_without_event_type() {
        let row = pending_row_with(None, Some(serde_json::json!({})), Some(1.0));
        assert!(pending_row_to_inputs(&row).is_none());
    }

    #[test]
    fn pending_row_to_inputs_defaults_decoded_to_null_value() {
        let row = pending_row_with(Some("X.Y"), None, None);
        let (event, node) = pending_row_to_inputs(&row).expect("inputs");
        assert_eq!(event.value, Value::Null);
        assert!(node.created_at.is_none());
    }

    #[test]
    fn pending_row_to_inputs_drops_non_finite_ts() {
        // serde_json::Number::from_f64 returns None for NaN / +-inf, which
        // matches the projector's expectation of a finite Unix-seconds value.
        let row_nan = pending_row_with(Some("X.Y"), None, Some(f64::NAN));
        let row_inf = pending_row_with(Some("X.Y"), None, Some(f64::INFINITY));
        assert!(pending_row_to_inputs(&row_nan).unwrap().1.created_at.is_none());
        assert!(pending_row_to_inputs(&row_inf).unwrap().1.created_at.is_none());
    }

    #[test]
    fn pending_row_to_inputs_propagates_nullable_addresses() {
        let mut row = pending_row_with(Some("X.Y"), None, None);
        row.src_address = None;
        row.dst_address = None;
        let (_, node) = pending_row_to_inputs(&row).expect("inputs");
        assert!(node.src.is_none());
        assert!(node.dst.is_none());
    }
}
