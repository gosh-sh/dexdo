// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

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
        Self { pool }
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
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await.context("begin tx")?;
        let mut result = PagePersistResult::default();

        for edge in edges {
            // `chain_order` is the projection-ordering key (migration 0016).
            // The GraphQL gateway promises it on every message edge; an event
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
            if decoded.is_some() {
                result.decoded += 1;
            } else {
                result.undecoded += 1;
            }

            let event_type = decoded.as_ref().map(|d| d.event_type.clone());
            let decoded_value = decoded.as_ref().map(|d| d.value.clone());

            let affected = sqlx::query(
                r#"insert into raw_events
                       (msg_id, chain_order, created_at_chain, src_address,
                        dst_address, event_type, body_json, decoded)
                   values ($1, $2, to_timestamp($3), $4, $5, $6, $7, $8)
                   on conflict (msg_id) do nothing"#,
            )
            .bind(&edge.node.msg_id)
            .bind(chain_order)
            .bind(parse_unix_seconds(edge.node.created_at.as_ref()))
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
        return Some(n);
    }
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.parse::<i64>() {
            return Some(n as f64);
        }
        if let Ok(n) = s.parse::<f64>() {
            return Some(n);
        }
    }
    None
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
