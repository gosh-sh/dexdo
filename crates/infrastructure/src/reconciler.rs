// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// MarketReconciler walks markets that lack post-discovery state (read from
// PMP.getDetails / getOrderBookAddress) and fills them in. Driven on a
// separate tokio task at indexer.reconciliation_interval_ms cadence.

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
const PMP_KIND: &str = "PMP";
// Cooldown window after a failed reconcile attempt. A market that just failed
// is excluded from the candidate set for this long, so a few permanently
// broken contracts cannot keep starving newer pending rows behind them. The
// value is intentionally several reconciler ticks long but short enough that
// transient failures (graphql timeouts, brief node hiccups) recover within a
// minute or two.
const FAILURE_BACKOFF_INTERVAL_SQL: &str = "5 minutes";

#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileStats {
    pub scanned: u64,
    pub reconciled: u64,
    pub skipped: u64,
    pub failed: u64,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingMarket {
    id: i64,
    pmp_address: String,
}

pub struct MarketReconciler {
    pool: PgPool,
    graphql: GraphqlClient,
    decoder: Decoder,
}

impl MarketReconciler {
    pub fn new(pool: PgPool, graphql: GraphqlClient, decoder: Decoder) -> Self {
        Self { pool, graphql, decoder }
    }

    /// Single sweep: pulls a batch of unreconciled markets, calls the getters,
    /// updates `markets` and rebuilds `market_outcomes`. Runs each market in
    /// its own transaction so a single broken contract does not block siblings.
    pub async fn run_once(&self) -> Result<ReconcileStats> {
        let mut stats = ReconcileStats::default();
        // Two anti-starvation rules baked into the SELECT:
        //   1. Filter out rows whose last failure is still inside the backoff
        //      window — they will not be retried this tick at all.
        //   2. Order never-failed rows ahead of cooled-down failed rows
        //      (`nulls first`). Within each group, oldest id first.
        let pending: Vec<PendingMarket> = sqlx::query_as(&format!(
            r#"select id, pmp_address from markets
               where last_reconciled_at is null
                 and (last_reconcile_failed_at is null
                      or last_reconcile_failed_at < now() - interval '{FAILURE_BACKOFF_INTERVAL_SQL}')
               order by last_reconcile_failed_at nulls first, id asc
               limit $1"#
        ))
        .bind(BATCH_SIZE)
        .fetch_all(&self.pool)
        .await
        .context("select pending markets")?;

        for market in pending {
            stats.scanned += 1;
            match self.reconcile_one(&market).await {
                Ok(true) => stats.reconciled += 1,
                Ok(false) => stats.skipped += 1,
                Err(err) => {
                    stats.failed += 1;
                    warn!(market_id = market.id, pmp_address = %market.pmp_address, ?err, "reconcile failed");
                    if let Err(stamp_err) = self.stamp_failure(market.id).await {
                        warn!(
                            market_id = market.id,
                            ?stamp_err,
                            "failed to stamp reconcile failure marker"
                        );
                    }
                }
            }
        }

        Ok(stats)
    }

    async fn stamp_failure(&self, market_id: i64) -> Result<()> {
        sqlx::query(
            r#"update markets
                  set last_reconcile_failed_at = now(),
                      reconcile_attempts = reconcile_attempts + 1
                where id = $1"#,
        )
        .bind(market_id)
        .execute(&self.pool)
        .await
        .context("stamp reconcile failure")?;
        Ok(())
    }

    /// Hot loop, runs forever until cancelled.
    pub async fn run_loop(self, interval: Duration) {
        loop {
            match self.run_once().await {
                Ok(stats) => {
                    if stats.scanned > 0 {
                        info!(
                            scanned = stats.scanned,
                            reconciled = stats.reconciled,
                            skipped = stats.skipped,
                            failed = stats.failed,
                            "market reconciler tick"
                        );
                    } else {
                        debug!("market reconciler tick (idle)");
                    }
                }
                Err(err) => error!(?err, "market reconciler sweep failed"),
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn reconcile_one(&self, market: &PendingMarket) -> Result<bool> {
        let Some(account_boc) = self
            .graphql
            .fetch_account_boc(&market.pmp_address)
            .await
            .context("fetch account boc")?
        else {
            debug!(pmp_address = %market.pmp_address, "no account boc available yet");
            return Ok(false);
        };

        let pmp =
            self.decoder.contract(PMP_KIND).ok_or_else(|| anyhow!("PMP abi missing in decoder"))?;

        let details = run_getter(pmp, &account_boc, "getDetails", &json!({}))
            .with_context(|| format!("getDetails for {}", market.pmp_address))?;

        let orderbook = run_getter(pmp, &account_boc, "getOrderBookAddress", &json!({}))
            .with_context(|| format!("getOrderBookAddress for {}", market.pmp_address))?;

        let mut tx = self.pool.begin().await.context("reconcile tx begin")?;
        write_market_state(&mut tx, market.id, &details, &orderbook).await?;
        write_market_outcomes(&mut tx, market.id, &market.pmp_address, &details).await?;
        sqlx::query(
            r#"update markets
                  set last_reconciled_at = now(),
                      updated_at = now(),
                      last_reconcile_failed_at = null
                where id = $1"#,
        )
        .bind(market.id)
        .execute(&mut *tx)
        .await
        .context("stamp last_reconciled_at")?;
        tx.commit().await.context("reconcile tx commit")?;

        Ok(true)
    }
}

async fn write_market_state(
    tx: &mut Transaction<'_, Postgres>,
    market_id: i64,
    details: &Value,
    orderbook: &Value,
) -> Result<()> {
    let market_name = field_str(details, "name")?;
    let oracle_list_hash_hex = field_str(details, "oracleListHash")?;
    let oracle_list_hash_decimal = uint256_hex_to_decimal(oracle_list_hash_hex)?;
    let approved = field_bool(details, "approved")?;
    let is_cancelled = field_bool(details, "isCancelled")?;
    // Strict parsing: a parse failure here means the detokenized getter output
    // disagrees with the ABI (or the ABI changed under us). The original code
    // fell back to 0 / NULL on parse error and then stamped the row reconciled,
    // which silently locked in incomplete data forever. Propagating the error
    // keeps the row pending and lets the failure-tracking path retry it.
    let num_outcomes: i32 =
        field_str(details, "numOutcomes")?.parse().context("parse numOutcomes")?;
    let stake_start: i64 = field_str(details, "stakeStart")?.parse().context("parse stakeStart")?;
    let stake_end: i64 = field_str(details, "stakeEnd")?.parse().context("parse stakeEnd")?;
    let result_start: i64 =
        field_str(details, "resultStart")?.parse().context("parse resultStart")?;
    let result_end: i64 = field_str(details, "resultEnd")?.parse().context("parse resultEnd")?;
    let orderbook_address = field_str(orderbook, "orderBookAddress")?;

    // When the reconciler is the first to observe a cancellation (event lost
    // or not yet replayed) we also stamp `cancelled_at` so the API can populate
    // `terminal.at`. Coalesce-style: if the cancellation-event projector has
    // already set a chain-derived timestamp, keep it — `now()` is only the
    // fallback for the "event missed entirely" path. Conversely, if the chain
    // says the market is no longer cancelled we leave `cancelled_at` alone:
    // we never erase a previously-recorded cancellation timestamp.
    sqlx::query(
        r#"update markets
              set market_id = $1,
                  name = $1,
                  oracle_list_hash = $2::numeric,
                  approved = $3,
                  is_cancelled = $4,
                  cancelled_at = case
                      when $4 and cancelled_at is null then extract(epoch from now())::bigint
                      else cancelled_at
                  end,
                  num_outcomes = $5,
                  stake_start = $6,
                  stake_end = $7,
                  result_start = $8,
                  result_end = $9,
                  orderbook_address = $10
            where id = $11"#,
    )
    .bind(market_name)
    .bind(&oracle_list_hash_decimal)
    .bind(approved)
    .bind(is_cancelled)
    .bind(num_outcomes)
    .bind(stake_start)
    .bind(stake_end)
    .bind(result_start)
    .bind(result_end)
    .bind(orderbook_address)
    .bind(market_id)
    .execute(&mut **tx)
    .await
    .context("update markets")?;

    Ok(())
}

async fn write_market_outcomes(
    tx: &mut Transaction<'_, Postgres>,
    market_id: i64,
    pmp_address: &str,
    details: &Value,
) -> Result<()> {
    let market_name = field_str(details, "name")?;
    let outcome_names =
        details.get("outcomeNames").ok_or_else(|| anyhow!("getDetails: missing outcomeNames"))?;
    // Detokenizer returns map(uint32, string) as a JSON object keyed by stringified uint.
    let map = outcome_names
        .as_object()
        .ok_or_else(|| anyhow!("getDetails.outcomeNames is not an object"))?;

    let token_params: Option<(i32, i32)> = sqlx::query_as(
        r#"select rt.price_precision, rt.quantity_precision
             from markets m
             join ref_tokens rt on rt.token_type = m.token_type
            where m.id = $1"#,
    )
    .bind(market_id)
    .fetch_optional(&mut **tx)
    .await
    .context("select token params")?;

    let Some((price_precision, quantity_precision)) = token_params else {
        return Err(anyhow!("ref_tokens lookup returned no row for market id {market_id}"));
    };

    // Trading-parameter bridge from ref_tokens to API representation. tick_size /
    // step_size come from precision exponents; min_notional and max_batch_size
    // are placeholders matching the stub until ref_tokens carries them in human
    // representation (see Stage 13 in the architecture plan).
    let tick_size = power_of_ten_neg(price_precision as u32);
    let step_size = power_of_ten_neg(quantity_precision as u32);
    let min_notional = "1".to_string();
    let max_batch_size: i32 = 5;

    for (outcome_id_str, outcome_name_value) in map {
        let outcome_id: i32 = outcome_id_str
            .parse()
            .with_context(|| format!("parse outcome_id `{outcome_id_str}`"))?;
        let outcome_name = outcome_name_value
            .as_str()
            .ok_or_else(|| anyhow!("outcomeNames[{outcome_id_str}] is not a string"))?;
        let symbol = format!("{market_name}-{outcome_name}");

        sqlx::query(
            r#"insert into market_outcomes
                   (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                    price_precision, quantity_precision,
                    tick_size, step_size, min_notional, max_batch_size, updated_at)
               values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
               on conflict (pmp_address, outcome_id) do update
                   set outcome_name = excluded.outcome_name,
                       symbol = excluded.symbol,
                       price_precision = excluded.price_precision,
                       quantity_precision = excluded.quantity_precision,
                       tick_size = excluded.tick_size,
                       step_size = excluded.step_size,
                       min_notional = excluded.min_notional,
                       max_batch_size = excluded.max_batch_size,
                       updated_at = now()"#,
        )
        .bind(market_id)
        .bind(pmp_address)
        .bind(outcome_id)
        .bind(outcome_name)
        .bind(&symbol)
        .bind(price_precision)
        .bind(quantity_precision)
        .bind(&tick_size)
        .bind(&step_size)
        .bind(&min_notional)
        .bind(max_batch_size)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upsert market_outcomes outcome_id={outcome_id}"))?;
    }

    Ok(())
}

fn field_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| anyhow!("missing field `{key}`"))
}

fn field_bool(value: &Value, key: &str) -> Result<bool> {
    value.get(key).and_then(Value::as_bool).ok_or_else(|| anyhow!("missing bool `{key}`"))
}

fn power_of_ten_neg(precision: u32) -> String {
    if precision == 0 {
        return "1".to_string();
    }
    let mut s = String::with_capacity(precision as usize + 2);
    s.push_str("0.");
    for _ in 1..precision {
        s.push('0');
    }
    s.push('1');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_of_ten_neg_renders_decimal() {
        assert_eq!(power_of_ten_neg(0), "1");
        assert_eq!(power_of_ten_neg(1), "0.1");
        assert_eq!(power_of_ten_neg(2), "0.01");
        assert_eq!(power_of_ten_neg(3), "0.001");
        assert_eq!(power_of_ten_neg(6), "0.000001");
    }
}
