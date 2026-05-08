// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Reconciler for fields that live on the `OracleEventList` contract state but
// are NOT carried by the `EventAdded` event. Today the API spec exposes
// `event.description` (docs/api-spec.md §Event), which is `oracle_events.describe`
// in the read-model — populated only via the OEL `_events` getter. This
// reconciler walks OEL contracts that still have at least one child
// `oracle_events` row with `describe is null` and fills the metadata in.
// Independent task spawned from `services/indexer/src/main.rs`, mirrors the
// shape of `MarketReconciler` (`reconciler.rs`).

use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Transaction;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::decoder::Decoder;
use crate::graphql::GraphqlClient;
use crate::projectors::uint256_hex_to_decimal;
use crate::tvm_runner::run_getter;

const BATCH_SIZE: i64 = 16;
const OEL_KIND: &str = "OracleEventList";
// Cooldown window after a failed OEL reconcile attempt — same anti-starvation
// logic as `MarketReconciler::FAILURE_BACKOFF_INTERVAL_SQL`. Keeps a few
// permanently broken `_events` getters from blocking the rest of the queue.
const FAILURE_BACKOFF_INTERVAL_SQL: &str = "5 minutes";

#[derive(Debug, Default, Clone, Copy)]
pub struct OelReconcileStats {
    pub scanned: u64,
    pub reconciled: u64,
    pub skipped: u64,
    pub failed: u64,
    pub events_filled: u64,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingOel {
    id: i64,
    address: String,
}

pub struct OracleEventListReconciler {
    pool: PgPool,
    graphql: GraphqlClient,
    decoder: Decoder,
}

impl OracleEventListReconciler {
    pub fn new(pool: PgPool, graphql: GraphqlClient, decoder: Decoder) -> Self {
        Self { pool, graphql, decoder }
    }

    /// Single sweep: pulls OELs that have at least one child event with
    /// `describe is null`, runs `_events` per OEL, and fills the metadata
    /// for matching `oracle_events` rows. Each OEL runs in its own
    /// transaction so one broken contract does not block its siblings.
    pub async fn run_once(&self) -> Result<OelReconcileStats> {
        let mut stats = OelReconcileStats::default();
        // Anti-starvation: same shape as `MarketReconciler::run_once`. Skip
        // OELs whose last failure is still inside the backoff window, and
        // among the rest run never-failed first, then cooled-down failures.
        let pending: Vec<PendingOel> = sqlx::query_as(&format!(
            r#"select oel.id, oel.address
                 from oracle_event_lists oel
                where (oel.last_reconcile_failed_at is null
                       or oel.last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
                  and exists (
                      select 1 from oracle_events oe
                       where oe.eventlist_id = oel.id and oe.describe is null
                  )
                order by oel.last_reconcile_failed_at nulls first, oel.id asc
                limit $1"#
        ))
        .bind(BATCH_SIZE)
        .fetch_all(&self.pool)
        .await
        .context("select pending oracle event lists")?;

        for oel in pending {
            stats.scanned += 1;
            match self.reconcile_one(&oel).await {
                Ok(filled) => {
                    if filled > 0 {
                        stats.reconciled += 1;
                        stats.events_filled += filled;
                    } else {
                        stats.skipped += 1;
                    }
                    if let Err(clear_err) = self.clear_failure(oel.id).await {
                        warn!(
                            oel_id = oel.id,
                            ?clear_err,
                            "failed to clear oel reconcile failure marker"
                        );
                    }
                }
                Err(err) => {
                    stats.failed += 1;
                    warn!(oel_id = oel.id, oel_address = %oel.address, ?err, "oel reconcile failed");
                    if let Err(stamp_err) = self.stamp_failure(oel.id).await {
                        warn!(
                            oel_id = oel.id,
                            ?stamp_err,
                            "failed to stamp oel reconcile failure marker"
                        );
                    }
                }
            }
        }

        Ok(stats)
    }

    async fn stamp_failure(&self, oel_id: i64) -> Result<()> {
        sqlx::query(
            r#"update oracle_event_lists
                  set last_reconcile_failed_at = now(),
                      reconcile_attempts = reconcile_attempts + 1
                where id = $1"#,
        )
        .bind(oel_id)
        .execute(&self.pool)
        .await
        .context("stamp oel reconcile failure")?;
        Ok(())
    }

    async fn clear_failure(&self, oel_id: i64) -> Result<()> {
        sqlx::query(
            r#"update oracle_event_lists
                  set last_reconcile_failed_at = null
                where id = $1
                  and last_reconcile_failed_at is not null"#,
        )
        .bind(oel_id)
        .execute(&self.pool)
        .await
        .context("clear oel reconcile failure")?;
        Ok(())
    }

    /// Hot loop, runs forever until cancelled.
    pub async fn run_loop(self, interval: Duration) {
        loop {
            match self.run_once().await {
                Ok(stats) if stats.scanned > 0 => {
                    info!(
                        scanned = stats.scanned,
                        reconciled = stats.reconciled,
                        skipped = stats.skipped,
                        failed = stats.failed,
                        events_filled = stats.events_filled,
                        "oel reconciler tick"
                    );
                }
                Ok(_) => debug!("oel reconciler tick (idle)"),
                Err(err) => error!(?err, "oel reconciler sweep failed"),
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn reconcile_one(&self, oel: &PendingOel) -> Result<u64> {
        let Some(account_boc) =
            self.graphql.fetch_account_boc(&oel.address).await.context("fetch oel account boc")?
        else {
            debug!(oel_address = %oel.address, "no oel account boc available yet");
            return Ok(0);
        };

        let oel_contract = self
            .decoder
            .contract(OEL_KIND)
            .ok_or_else(|| anyhow!("OracleEventList abi missing in decoder"))?;

        let raw = run_getter(oel_contract, &account_boc, "_events", &json!({}))
            .with_context(|| format!("_events getter for {}", oel.address))?;

        let items = parse_events_map(&raw)
            .with_context(|| format!("parse _events for {}", oel.address))?;

        if items.is_empty() {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await.context("oel reconcile tx begin")?;
        let mut filled = 0u64;
        for item in &items {
            filled += apply_event_metadata(&mut tx, oel.id, item).await?;
        }
        tx.commit().await.context("oel reconcile tx commit")?;

        Ok(filled)
    }
}

/// Decoded view of one entry of the `OracleEventList._events` map. Only the
/// fields we surface to the read-model are kept; everything else is ignored.
#[derive(Debug, Clone)]
pub(crate) struct EventMetadata {
    pub event_id_decimal: String,
    pub describe: Option<String>,
    pub trust_addr: Option<String>,
}

pub(crate) fn parse_events_map(raw: &Value) -> Result<Vec<EventMetadata>> {
    // The getter detokenizes to `{"_events": {"<hex>": {...}, ...}}`.
    let map = raw
        .get("_events")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("_events response missing object under `_events`"))?;

    let mut out = Vec::with_capacity(map.len());
    for (event_id_hex, tuple) in map {
        let event_id_decimal = uint256_hex_to_decimal(event_id_hex).with_context(|| {
            format!("convert _events key `{event_id_hex}` from uint256 hex to decimal")
        })?;
        let describe = tuple
            .get("describe")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // `optional(uint256)` detokenizes to a string when set, JSON null when
        // absent. Treat empty strings as absent too — defensive, matches how
        // `describe` is handled.
        let trust_addr = tuple
            .get("trustAddr")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        out.push(EventMetadata { event_id_decimal, describe, trust_addr });
    }
    Ok(out)
}

async fn apply_event_metadata(
    tx: &mut Transaction<'_, Postgres>,
    eventlist_id: i64,
    item: &EventMetadata,
) -> Result<u64> {
    if item.describe.is_none() && item.trust_addr.is_none() {
        return Ok(0);
    }
    // `coalesce(existing, $new)` semantics: never overwrite a value already
    // recorded. The on-chain map is the only source of truth for these fields,
    // and once set they do not change — events are immutable once added.
    let updated = sqlx::query(
        r#"update oracle_events
              set describe = coalesce(describe, $1),
                  trust_addr = coalesce(trust_addr, $2),
                  updated_at = now()
            where eventlist_id = $3
              and internal_id_in_eventlist = $4::numeric
              and (describe is null or trust_addr is null)"#,
    )
    .bind(item.describe.as_deref())
    .bind(item.trust_addr.as_deref())
    .bind(eventlist_id)
    .bind(&item.event_id_decimal)
    .execute(&mut **tx)
    .await
    .with_context(|| {
        format!(
            "update oracle_events.describe for eventlist_id={eventlist_id} event_id={}",
            item.event_id_decimal
        )
    })?
    .rows_affected();

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter_response(events: Vec<(&str, Value)>) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in events {
            map.insert(k.to_string(), v);
        }
        json!({ "_events": Value::Object(map) })
    }

    #[test]
    fn parse_events_map_extracts_describe_and_trust_addr() {
        let raw = getter_response(vec![(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            json!({
                "eventName": "Election",
                "oracleFee": "100",
                "deadline": "1710000000",
                "describe": "Will candidate X win?",
                "outcomeNames": {},
                "count": "0",
                "trustAddr": "0xabcdef0000000000000000000000000000000000000000000000000000000000",
            }),
        )]);

        let items = parse_events_map(&raw).expect("parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id_decimal, "1");
        assert_eq!(items[0].describe.as_deref(), Some("Will candidate X win?"));
        assert_eq!(
            items[0].trust_addr.as_deref(),
            Some("0xabcdef0000000000000000000000000000000000000000000000000000000000")
        );
    }

    #[test]
    fn parse_events_map_treats_empty_strings_as_absent() {
        let raw = getter_response(vec![(
            "0x000000000000000000000000000000000000000000000000000000000000000a",
            json!({
                "eventName": "X",
                "oracleFee": "0",
                "deadline": "0",
                "describe": "",
                "outcomeNames": {},
                "count": "0",
                "trustAddr": "",
            }),
        )]);

        let items = parse_events_map(&raw).expect("parse");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id_decimal, "10");
        assert!(items[0].describe.is_none());
        assert!(items[0].trust_addr.is_none());
    }

    #[test]
    fn parse_events_map_handles_null_optional() {
        let raw = getter_response(vec![(
            "0x0000000000000000000000000000000000000000000000000000000000000005",
            json!({
                "eventName": "X",
                "oracleFee": "0",
                "deadline": "0",
                "describe": "Sample",
                "outcomeNames": {},
                "count": "0",
                "trustAddr": Value::Null,
            }),
        )]);

        let items = parse_events_map(&raw).expect("parse");
        assert!(items[0].trust_addr.is_none());
        assert_eq!(items[0].describe.as_deref(), Some("Sample"));
    }

    #[test]
    fn parse_events_map_rejects_missing_root() {
        let err = parse_events_map(&json!({"other": {}})).unwrap_err();
        assert!(err.to_string().contains("_events"));
    }

    #[test]
    fn parse_events_map_rejects_bad_hex_key() {
        let raw = getter_response(vec![(
            "not-hex",
            json!({
                "eventName": "X", "oracleFee": "0", "deadline": "0",
                "describe": "Sample", "outcomeNames": {}, "count": "0",
                "trustAddr": Value::Null,
            }),
        )]);
        let err = parse_events_map(&raw).unwrap_err();
        assert!(err.to_string().contains("uint256"));
    }
}
