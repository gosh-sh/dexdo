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

/// Outcome of attempting to reconcile a single market. Distinguishes the
/// "BOC not yet available on the node" path from "reconciled successfully":
/// a missing BOC must trip the failure-backoff so the row drops off the
/// front of the next sweep, otherwise a pending market stuck on
/// `account_boc = null` would starve every later row in the queue.
enum MarketReconcileOutcome {
    Reconciled,
    NoBoc,
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
        // A row enters the queue when it has never been reconciled
        // (`last_reconciled_at is null`). The first successful pass stamps the
        // deterministic `orderbook_address` from `PMP.getOrderBookAddress()`
        // and the row drops out of the queue permanently — the getter result
        // is stable, so there's no later re-queue trigger.
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
                Ok(MarketReconcileOutcome::Reconciled) => stats.reconciled += 1,
                Ok(MarketReconcileOutcome::NoBoc) => {
                    stats.skipped += 1;
                    // Push the row to the back of the queue: without this it
                    // would keep selecting first under `nulls first, id asc`
                    // every tick and starve every later pending market.
                    if let Err(stamp_err) = self.stamp_failure(market.id).await {
                        warn!(
                            market_id = market.id,
                            ?stamp_err,
                            "failed to stamp reconcile backoff for missing BOC"
                        );
                    }
                }
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

    async fn reconcile_one(&self, market: &PendingMarket) -> Result<MarketReconcileOutcome> {
        let Some(account_boc) = self
            .graphql
            .fetch_account_boc(&market.pmp_address)
            .await
            .context("fetch account boc")?
        else {
            debug!(pmp_address = %market.pmp_address, "no account boc available yet");
            return Ok(MarketReconcileOutcome::NoBoc);
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

        Ok(MarketReconcileOutcome::Reconciled)
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
    // Timings (`stakeStart`/`stakeEnd`/`resultStart`/`resultEnd`) are
    // intentionally NOT read from `getDetails()` here. On a pre-`TimingsSet`
    // PMP the getter returns contract defaults (zeros), which used to land in
    // the row and make `derive_status` flip straight to AWAITING_FREEZE —
    // violating docs/tech-specs/read-api.md §Status derivation ("status ==
    // PENDING implies timings == null") and the PENDING definition
    // ("EventConfirmed received; no TimingsSet yet"). The `apply_timings_set`
    // projector (projectors.rs:332-363) is the sole writer of those columns;
    // until it fires, the row stays NULL-timings → PENDING.
    let orderbook_address = field_str(orderbook, "orderBookAddress")?;

    // When the reconciler is the first to observe a cancellation (event lost
    // or not yet replayed) we also stamp `cancelled_at` so the API can populate
    // `terminal.at`. Coalesce-style: if the cancellation-event projector has
    // already set a chain-derived timestamp, keep it — `now()` is only the
    // fallback for the "event missed entirely" path. Conversely, if the chain
    // says the market is no longer cancelled we leave `cancelled_at` alone:
    // we never erase a previously-recorded cancellation timestamp.
    // `orderbook_address` is written unconditionally — `getOrderBookAddress()`
    // is deterministic and returns the precomputed address even pre-freeze
    // (contracts/dex/PMP.sol:1360). The migration-0014 CHECK constraint pins the
    // invariant `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL`
    // so an empty getter result fails the pass instead of producing a hidden
    // half-state.
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
                  orderbook_address = $6
            where id = $7"#,
    )
    .bind(market_name)
    .bind(&oracle_list_hash_decimal)
    .bind(approved)
    .bind(is_cancelled)
    .bind(num_outcomes)
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

    // Pull both display precisions (driving tick/step) and the trading-rule
    // fields from ref_tokens. `min_notional` is stored as a raw uint scaled by
    // 10^decimals (the asset's on-chain decimals, *not* `quantity_precision`,
    // which is a coarser display knob — for USDC decimals=6 / qtyPrec=2, raw
    // 1_000_000 must render as "1.000000" = 1 USDC, not "10000.00").
    let token_params: Option<(i32, i32, i32, String)> = sqlx::query_as(
        r#"select rt.price_precision,
                  rt.quantity_precision,
                  rt.decimals,
                  rt.min_notional::text
             from markets m
             join ref_tokens rt on rt.token_type = m.token_type
            where m.id = $1"#,
    )
    .bind(market_id)
    .fetch_optional(&mut **tx)
    .await
    .context("select token params")?;

    let Some((price_precision, quantity_precision, token_decimals, min_notional_raw)) =
        token_params
    else {
        return Err(anyhow!("ref_tokens lookup returned no row for market id {market_id}"));
    };

    // Trading-parameter bridge from ref_tokens to API representation. tick_size
    // and step_size are render-precision exponents; min_notional comes per-token
    // from ref_tokens and gets scaled to a DECIMAL string here so the API can
    // bind it straight to the response.
    let tick_size = power_of_ten_neg(price_precision as u32);
    let step_size = power_of_ten_neg(quantity_precision as u32);
    let min_notional = scale_uint_to_decimal(&min_notional_raw, token_decimals.max(0) as u32);
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
                    tick_size, step_size, min_notional, updated_at)
               values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
               on conflict (pmp_address, outcome_id) do update
                   set outcome_name = excluded.outcome_name,
                       symbol = excluded.symbol,
                       price_precision = excluded.price_precision,
                       quantity_precision = excluded.quantity_precision,
                       tick_size = excluded.tick_size,
                       step_size = excluded.step_size,
                       min_notional = excluded.min_notional,
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

/// Render a raw decimal-integer string as a fixed-point DECIMAL with `scale`
/// digits after the dot. Pure string arithmetic — works for arbitrarily large
/// `numeric(78,0)` values from `ref_tokens.min_notional` without going through
/// floats. Examples: `("1000000", 6) -> "1.000000"`, `("1234", 6) -> "0.001234"`,
/// `("0", 6) -> "0.000000"`, `("42", 0) -> "42"`.
fn scale_uint_to_decimal(raw: &str, scale: u32) -> String {
    if scale == 0 {
        return raw.to_string();
    }
    let p = scale as usize;
    if raw.len() <= p {
        let zeros = "0".repeat(p - raw.len());
        format!("0.{zeros}{raw}")
    } else {
        let split = raw.len() - p;
        format!("{}.{}", &raw[..split], &raw[split..])
    }
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

    #[test]
    fn scale_uint_to_decimal_matches_ref_tokens_seed() {
        // ref_tokens seed values from the initial schema migration.
        // These pin the API-facing min_notional contract: clients use it to
        // validate orders, so a regression here is a public-API regression.
        assert_eq!(scale_uint_to_decimal("1000000", 6), "1.000000"); // USDC
        assert_eq!(scale_uint_to_decimal("10000000000", 9), "10.000000000"); // NACKL
        assert_eq!(scale_uint_to_decimal("100000000000", 9), "100.000000000"); // SHELL
    }

    #[test]
    fn scale_uint_to_decimal_handles_edge_cases() {
        assert_eq!(scale_uint_to_decimal("0", 6), "0.000000");
        assert_eq!(scale_uint_to_decimal("1", 6), "0.000001");
        assert_eq!(scale_uint_to_decimal("123", 6), "0.000123");
        // scale = 0: pass-through.
        assert_eq!(scale_uint_to_decimal("42", 0), "42");
        // raw longer than scale: split, no leading zeros.
        assert_eq!(scale_uint_to_decimal("1234567", 6), "1.234567");
    }
}
