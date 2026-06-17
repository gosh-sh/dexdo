// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashSet;
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
    /// noise log. Shared across the fetch / reprojection / metrics clones via
    /// `Arc`, so "first" is process-global. Bounded by the contract event
    /// vocabulary (a few dozen entries), so it does not grow without limit.
    seen_unknown_event_types: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PagePersistResult {
    pub inserted: u64,
    pub skipped: u64,
    pub decoded: u64,
    pub undecoded: u64,
    pub projected: u64,
    pub projection_deferred: u64,
    pub projection_failed: u64,
    pub type_ignored: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReprojectionStats {
    pub scanned: u64,
    pub applied: u64,
    pub deferred: u64,
    pub unknown: u64,
    pub failed: u64,
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
        Self { pool, seen_unknown_event_types: Arc::new(Mutex::new(HashSet::new())) }
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
        ignored_event_types: &HashSet<&str>,
    ) -> anyhow::Result<PagePersistResult> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await.context("begin tx")?;
        let mut result = PagePersistResult::default();

        for edge in edges {
            // `chain_order` is the projection-ordering key. The GraphQL gateway
            // promises it on every message edge; an event
            // without it is unusable here — the reproject SQL orders by
            // `chain_order` and would either misplace this row or fail on the
            // NOT NULL constraint. Drop the edge with a warning rather than
            // synthesise a fake key.
            let Some(chain_order) = edge.node.msg_chain_order.as_deref() else {
                result.undecoded += 1;
                warn!(
                    msg_id = %edge.node.msg_id,
                    "GraphQL event edge missing msg_chain_order; dropping row"
                );
                continue;
            };

            let body_value = edge.node.body.clone().unwrap_or(Value::Null);
            let decoded = try_decode(decoder, &edge.node.msg_id, edge.node.body.as_ref());
            // Drop configured no-op event types before they cost a raw_events
            // insert + projection + mark_processed. The page-level cursor
            // upsert after the loop still runs, so the cursor advances past
            // them. event_type is only known post-decode, which is why this
            // lives here and not in drain_events' src-based filter.
            if let Some(d) = decoded.as_ref()
                && ignored_event_types.contains(d.event_type.as_str())
            {
                result.type_ignored += 1;
                continue;
            }
            if decoded.is_some() {
                result.decoded += 1;
            } else {
                result.undecoded += 1;
            }

            let event_type = decoded.as_ref().map(|d| d.event_type.clone());
            let decoded_value = decoded.as_ref().map(|d| d.value.clone());
            let created_at_chain = parse_unix_seconds(edge.node.created_at.as_ref());
            if should_warn_unparseable_created_at(edge.node.created_at.as_ref(), created_at_chain) {
                warn!(
                    msg_id = %edge.node.msg_id,
                    chain_order,
                    created_at = ?edge.node.created_at,
                    "GraphQL event edge has unparseable created_at; storing raw_events.created_at_chain as NULL"
                );
            }

            let affected = sqlx::query(
                r#"insert into raw_events
                       (msg_id, chain_order, created_at_chain, src_address,
                        dst_address, event_type, body_json, decoded)
                   values ($1, $2, to_timestamp($3), $4, $5, $6, $7, $8)
                   on conflict (msg_id) do nothing"#,
            )
            .bind(&edge.node.msg_id)
            .bind(chain_order)
            .bind(created_at_chain)
            .bind(edge.node.src.as_deref())
            .bind(edge.node.dst.as_deref())
            .bind(event_type)
            .bind(body_value)
            .bind(decoded_value)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("insert raw_events msg_id={}", edge.node.msg_id))?
            .rows_affected();

            if affected == 0 {
                result.skipped += 1;
            } else {
                result.inserted += affected;
            }

            // Skip projection on conflict: either the row was already projected
            // (processed_at is set) or it is queued for retry — reproject_pending
            // will pick it up. Re-running the projector here is unsafe because
            // OrderBook arms are not idempotent (re-subtracted fills, OPEN reset
            // by a duplicate OrderPlaced).
            if affected == 0 {
                continue;
            }

            if let Some(decoded_event) = decoded.as_ref() {
                let mut sp = tx.begin().await.context("projector savepoint begin")?;
                let outcome = projectors::project_event(&mut sp, decoded_event, &edge.node).await;
                match outcome {
                    Ok(ProjectionOutcome::Applied) => {
                        sp.commit().await.context("projector savepoint release")?;
                        result.projected += 1;
                        mark_processed_by_msg_id(&mut tx, &edge.node.msg_id).await?;
                    }
                    Ok(ProjectionOutcome::Deferred) => {
                        // Leave processed_at null; the reprojection loop picks
                        // it up once the missing parent record materialises.
                        sp.commit().await.context("projector savepoint release")?;
                        result.projection_deferred += 1;
                    }
                    Ok(ProjectionOutcome::Unknown) => {
                        // The row is still marked processed so the cursor
                        // advances — blocking it would stall ingestion on
                        // every newly deployed contract emitting an event we
                        // don't yet teach. The first sighting of each unhandled
                        // type goes to the normal target (stdout + main log) so
                        // operators actually see the gap; later repeats are
                        // diverted to the noise log to avoid flooding it.
                        if self.first_unknown_sighting(&decoded_event.event_type) {
                            warn!(
                                msg_id = %edge.node.msg_id,
                                event_type = %decoded_event.event_type,
                                "projector has no handler for event type; marking processed and advancing cursor (first sighting — later repeats go to the noise log)"
                            );
                        } else {
                            warn!(
                                target: dodex_logging::EVENT_NOISE_TARGET,
                                msg_id = %edge.node.msg_id,
                                event_type = %decoded_event.event_type,
                                "projector has no handler for event type; marking processed and advancing cursor"
                            );
                        }
                        sp.commit().await.context("projector savepoint release")?;
                        mark_processed_by_msg_id(&mut tx, &edge.node.msg_id).await?;
                    }
                    Err(err) => {
                        drop(sp);
                        result.projection_failed += 1;
                        warn!(
                            msg_id = %edge.node.msg_id,
                            event_type = %decoded_event.event_type,
                            ?err,
                            "projector failed; raw event still persisted, savepoint rolled back"
                        );
                    }
                }
            }
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

    /// Replays decoded-but-unprojected `raw_events` through the projector.
    /// Picks rows where `processed_at is null` in chain-arrival order so a
    /// previously-deferred parent gets its first chance before children retry.
    /// Stored `decoded` jsonb is reused — bodies are not re-decoded.
    ///
    /// Uses `for update skip locked` and runs the whole batch inside a single
    /// transaction so concurrent reproject workers (or a parallel test
    /// harness) cannot pick up the same row and apply a non-idempotent
    /// projector twice — without the lock, an `OrderFilled` could subtract
    /// `filledAmount` from `live_orders` more than once.
    pub async fn reproject_pending(&self, batch_size: u32) -> anyhow::Result<ReprojectionStats> {
        let mut tx: Transaction<'_, Postgres> =
            self.pool.begin().await.context("reproject tx begin")?;

        let rows: Vec<PendingRow> = sqlx::query_as(
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
                order by chain_order asc
                limit $1
                for update skip locked"#,
        )
        .bind(i64::from(batch_size))
        .fetch_all(&mut *tx)
        .await
        .context("select pending raw_events")?;

        let mut stats = ReprojectionStats::default();
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
                    mark_processed_by_id(&mut tx, row.id).await?;
                    stats.applied += 1;
                }
                Ok(ProjectionOutcome::Deferred) => {
                    sp.commit().await.context("reproject savepoint release")?;
                    stats.deferred += 1;
                }
                Ok(ProjectionOutcome::Unknown) => {
                    // First sighting -> normal target so operators see the gap;
                    // repeats -> noise log. Shares the dedup set with the fetch
                    // loop, so a type already surfaced there stays quiet here.
                    if self.first_unknown_sighting(&event.event_type) {
                        warn!(
                            msg_id = %row.msg_id,
                            event_type = %event.event_type,
                            "reprojection has no handler for event type; marking processed and advancing (first sighting — later repeats go to the noise log)"
                        );
                    } else {
                        warn!(
                            target: dodex_logging::EVENT_NOISE_TARGET,
                            msg_id = %row.msg_id,
                            event_type = %event.event_type,
                            "reprojection has no handler for event type; marking processed and advancing"
                        );
                    }
                    sp.commit().await.context("reproject savepoint release")?;
                    mark_processed_by_id(&mut tx, row.id).await?;
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

        tx.commit().await.context("reproject tx commit")?;
        Ok(stats)
    }

    /// Hot loop, runs forever until cancelled.
    pub async fn run_reprojection_loop(self, interval: Duration, batch_size: u32) {
        loop {
            match self.reproject_pending(batch_size).await {
                Ok(stats) if stats.scanned > 0 => {
                    info!(
                        scanned = stats.scanned,
                        applied = stats.applied,
                        deferred = stats.deferred,
                        unknown = stats.unknown,
                        failed = stats.failed,
                        "reprojection sweep"
                    );
                }
                Ok(_) => debug!("reprojection sweep (idle)"),
                Err(err) => error!(?err, "reprojection sweep failed"),
            }
            tokio::time::sleep(interval).await;
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

async fn mark_processed_by_msg_id(
    tx: &mut Transaction<'_, Postgres>,
    msg_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"update raw_events
              set processed_at = now()
            where msg_id = $1
              and processed_at is null"#,
    )
    .bind(msg_id)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("mark raw_events.processed_at for msg_id={msg_id}"))?;
    Ok(())
}

async fn mark_processed_by_id(tx: &mut Transaction<'_, Postgres>, id: i64) -> anyhow::Result<()> {
    sqlx::query(
        r#"update raw_events
              set processed_at = now()
            where id = $1
              and processed_at is null"#,
    )
    .bind(id)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("mark raw_events.processed_at for id={id}"))?;
    Ok(())
}

fn try_decode(decoder: &Decoder, msg_id: &str, body: Option<&Value>) -> Option<DecodedEvent> {
    let body_str = body?.as_str()?;
    match decoder.decode_event_body(body_str) {
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
