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

use crate::decoder::DecodedEvent;
use crate::decoder::Decoder;
use crate::graphql::EventEdge;
use crate::graphql::EventNode;
use crate::projectors;
use crate::projectors::ProjectionOutcome;

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
}

impl IndexerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            seen_unknown_event_types: Arc::new(Mutex::new(HashSet::new())),
            projection_fallbacks: Arc::new(AtomicU64::new(0)),
        }
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

    pub async fn persist_page(
        &self,
        stream_name: &str,
        edges: &[EventEdge],
        end_cursor: Option<&str>,
        decoder: &Decoder,
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

            let decoded = try_decode(decoder, &edge.node.msg_id, edge.node.body.as_ref(), edge.node.dst.as_deref());
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

        if let Some(cursor) = end_cursor {
            sqlx::query(
                r#"insert into indexer_cursors (stream_name, cursor, updated_at)
                   values ($1, $2, now())
                   on conflict (stream_name)
                   do update set cursor = excluded.cursor, updated_at = now()"#,
            )
            .bind(stream_name)
            .bind(cursor)
            .execute(&mut *tx)
            .await
            .context("upsert indexer_cursors")?;
        }

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
        match self.reproject_batch_fast(batch_size, after_chain_order, until_chain_order).await? {
            Some(stats) => Ok(stats),
            None => {
                self.reproject_batch_savepointed(batch_size, after_chain_order, until_chain_order)
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
                    stats.deferred += 1;
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
        // Durably committed — now (and only now) emit the unknown-type warnings
        // and record first-sightings, in chain_order. A crash between the commit
        // and this loop loses only log lines (and the first-sighting dedup),
        // never projection state: the rows are already marked processed.
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
                    sp.commit().await.context("reproject savepoint release")?;
                    stats.deferred += 1;
                }
                Ok(ProjectionOutcome::Unknown) => {
                    self.warn_unknown(&row.msg_id, &event.event_type);
                    sp.commit().await.context("reproject savepoint release")?;
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
        Ok(stats)
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
                      extract(epoch from created_at_chain)::double precision as ts
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
        let count: i64 = sqlx::query_scalar(
            r#"select count(*) from raw_events
                where processed_at is null
                  and event_type is not null
                  and decoded is not null"#,
        )
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
        let secs: Option<i64> = sqlx::query_scalar(
            r#"select extract(epoch from now() - min(coalesce(created_at_chain, created_at)))::bigint
                 from raw_events
                where processed_at is null
                  and event_type is not null
                  and decoded is not null"#,
        )
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
            let retry = force_retry || last_retry.elapsed() >= idle_interval;
            force_retry = false;
            // Forward passes resume above the floor; a rate-limited retry pass
            // rewinds to the front to re-attempt the stuck set below the floor.
            let mut after: Option<String> = if retry {
                last_retry = tokio::time::Instant::now();
                None
            } else {
                floor.clone()
            };

            // Ceiling = highest pending chain_order now; bounds this pass so it
            // terminates even while capture keeps appending above it.
            let ceiling = match self.max_pending_chain_order().await {
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
            match self.has_pending_above(&ceiling).await {
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

fn try_decode(decoder: &Decoder, msg_id: &str, body: Option<&Value>, dst: Option<&str>) -> Option<DecodedEvent> {
    let body_str = body?.as_str()?;
    match decoder.decode_event_body(body_str, dst) {
        Ok(decoded) => decoded,
        Err(err) => {
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

#[cfg(test)]
mod tests {
    use super::*;

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
