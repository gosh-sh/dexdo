// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use anyhow::Context;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;

use crate::graphql::EventEdge;

#[derive(Debug, Clone)]
pub struct IndexerRepository {
    pool: PgPool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct PagePersistResult {
    pub inserted: u64,
    pub skipped: u64,
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
    ) -> anyhow::Result<PagePersistResult> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await.context("begin tx")?;
        let mut result = PagePersistResult::default();

        for edge in edges {
            let body = edge.node.body.clone().unwrap_or(Value::Null);
            let affected = sqlx::query(
                r#"insert into raw_events
                       (msg_id, created_at_chain, src_address, dst_address, event_type, body_json)
                   values ($1, to_timestamp($2), $3, $4, $5, $6)
                   on conflict (msg_id) do nothing"#,
            )
            .bind(&edge.node.msg_id)
            .bind(parse_unix_seconds(edge.node.created_at.as_ref()))
            .bind(edge.node.src.as_deref())
            .bind(edge.node.dst.as_deref())
            .bind::<Option<&str>>(None)
            .bind(body)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("insert raw_events msg_id={}", edge.node.msg_id))?
            .rows_affected();

            if affected == 0 {
                result.skipped += 1;
            } else {
                result.inserted += affected;
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

fn parse_unix_seconds(value: Option<&Value>) -> Option<f64> {
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
