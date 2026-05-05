// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use anyhow::Context;
use serde_json::Value;
use sqlx::Acquire;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::debug;
use tracing::warn;

use crate::decoder::DecodedEvent;
use crate::decoder::Decoder;
use crate::graphql::EventEdge;
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
                       (msg_id, created_at_chain, src_address, dst_address,
                        event_type, body_json, decoded)
                   values ($1, to_timestamp($2), $3, $4, $5, $6, $7)
                   on conflict (msg_id) do nothing"#,
            )
            .bind(&edge.node.msg_id)
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

            if let Some(decoded_event) = decoded.as_ref() {
                let mut sp = tx.begin().await.context("projector savepoint begin")?;
                let outcome = projectors::project_event(&mut sp, decoded_event, &edge.node).await;
                match outcome {
                    Ok(ProjectionOutcome::Applied) => {
                        sp.commit().await.context("projector savepoint release")?;
                        result.projected += 1;
                    }
                    Ok(ProjectionOutcome::Deferred) => {
                        sp.commit().await.context("projector savepoint release")?;
                        result.projection_deferred += 1;
                    }
                    Ok(ProjectionOutcome::Unknown) => {
                        sp.commit().await.context("projector savepoint release")?;
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
}
