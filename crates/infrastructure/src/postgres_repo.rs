// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dodex_application::CancelBatchResolution;
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_application::OraclesRequest;
use dodex_application::OrderForCancel;
use dodex_application::OrderForCancelBatch;
use dodex_application::OrderStatusFilter;
use dodex_application::OrdersCursor;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
use dodex_application::QueryableOrderStatus;
use dodex_application::TradesLimit;
use dodex_domain::decimal_string_is_zero;
use dodex_domain::descale_pow10;
use dodex_domain::CancelReason;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketName;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::OracleEntry;
use dodex_domain::OracleEventEntry;
use dodex_domain::OracleEventListEntry;
use dodex_domain::OracleFee;
use dodex_domain::OracleListing;
use dodex_domain::OracleOutcome;
use dodex_domain::OraclesPage;
use dodex_domain::Order;
use dodex_domain::OrderIdentity;
use dodex_domain::OrderSide;
use dodex_domain::OrderStatus;
use dodex_domain::OrderType;
use dodex_domain::Outcome;
use dodex_domain::PriceLevel;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TerminalKind;
use dodex_domain::TimeInForce;
use dodex_domain::Timings;
use dodex_domain::Trade;
use dodex_domain::PRICE_BPS_DECIMALS;
use num_bigint::BigUint;
use sqlx::PgPool;
use tracing::error;
use tracing::warn;

use crate::projectors::uint256_hex_to_decimal;

#[derive(Debug, Clone)]
pub struct PostgresReadModelRepository {
    pool: PgPool,
}

impl PostgresReadModelRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }
}

#[derive(Debug, sqlx::FromRow)]
struct MarketRow {
    id: i64,
    pmp_address: String,
    orderbook_address: Option<String>,
    oracle_list_hash: Option<String>,
    market_name: Option<String>,
    token_type: i32,
    token_code: String,
    event_id: String,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    resolved_at: Option<i64>,
    resolved_outcome_id: Option<i32>,
    cancelled_at: Option<i64>,
    cancel_reason: Option<String>,
    is_cancelled: bool,
    // Seconds since epoch — exposed in the API response as `Market.created_at`.
    created_at_unix: i64,
    // Microseconds since epoch — internal sort/cursor key for `CreatedAtDesc`.
    // Sub-second precision avoids the keyset bug where two markets created in
    // the same second could be skipped or duplicated across page boundaries.
    created_at_micros: i64,
}

/// One row from the per-market oracle confirmation fetch. A PMP can be
/// confirmed by multiple `OracleEventList` contracts (see
/// `PrivateNote.PMPDeployed.oracleEventLists: address[]`), so each
/// `pmp_address` can produce N rows here. `event_name` / `event_description`
/// are derived from `eventId = hash(eventName, description, deadline,
/// outcomeNames)` and MUST be identical across all rows for the same
/// `pmp_address`; `aggregate_oracle_events` validates that and fails closed
/// (`MarketInconsistent`) on mismatch.
#[derive(Debug, sqlx::FromRow)]
struct OracleEventJoinRow {
    pmp_address: String,
    event_name: Option<String>,
    event_description: Option<String>,
    oracle_name: Option<String>,
    oracle_address: Option<String>,
    oracle_fee: Option<String>,
}

/// Aggregated oracle confirmation block for a single market. Built from
/// 0..N rows of `OracleEventJoinRow` sharing the same `pmp_address`.
#[derive(Debug, Default)]
struct OracleEventBlock {
    event_name: Option<String>,
    event_description: Option<String>,
    oracles: Vec<OracleEntry>,
}

#[derive(Debug, sqlx::FromRow)]
struct OutcomeRow {
    market_id_fk: i64,
    outcome_id: i32,
    outcome_name: String,
    symbol: String,
    price_precision: i32,
    quantity_precision: i32,
    tick_size: String,
    step_size: String,
    min_notional: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OracleHeadRow {
    id: i64,
    name: String,
    address: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OracleListEventRow {
    oracle_id: i64,
    list_index: Option<i64>,
    eventlist_address: String,
    eventlist_description: String,
    event_id: String,
    event_name: String,
    event_description: Option<String>,
    oracle_fee: Option<String>,
    deadline: i64,
    trust_addr: Option<String>,
    outcome_names_jsonb: serde_json::Value,
}

#[async_trait]
impl MarketReadRepository for PostgresReadModelRepository {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        match request {
            MarketsRequest::One { market_address, now } => {
                self.fetch_one(market_address, *now).await
            }
            MarketsRequest::Listing(listing) => self.fetch_listing(listing).await,
        }
    }

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        // markets.orderbook_address is nullable in the schema because the
        // `PMPDeployed` projector inserts a row before the reconciler runs. The
        // schema CHECK constraint pins
        // `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL`,
        // and the SQL below filters to reconciled rows — so a NULL (or blank)
        // address here is an invariant violation, not a legitimate empty-book
        // state. Decoded as `Option<String>` only to surface that violation as
        // a typed `MarketInconsistent` instead of a sqlx decode error.
        //
        // Pull (price|quantity)_precision from market_outcomes too: live_orders
        // stores raw uint256/uint128 integers as the contract emitted them, so
        // the API must scale by 10^-precision to honour the DECIMAL contract
        // in docs/api-spec.md (e.g. raw "61400" with price_precision=2 -> "614.00").
        let target: Option<(Option<String>, i32, i32, i32, i32)> = sqlx::query_as(
            r#"select m.orderbook_address,
                      mo.outcome_id,
                      mo.price_precision,
                      mo.quantity_precision,
                      rt.decimals
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
                 join ref_tokens rt on rt.token_type = m.token_type
                where m.pmp_address = $1
                  and mo.symbol = $2
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("resolve orderbook_address from (marketAddress, symbol)")?;

        let Some((orderbook_address, outcome_id, price_precision, quantity_precision, decimals)) =
            target
        else {
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };
        let Some(orderbook_address) = orderbook_address.and_then(filter_orderbook) else {
            // Reconciled market without a usable orderbook address: violates
            // the migration-0014 invariant. Fail closed (HTTP 503) instead of
            // serving an empty book that would silently hide the corruption.
            return Err(anyhow!(DomainError::MarketInconsistent));
        };

        let limit = limit.max(1) as i64;

        // Push the top-N per side into Postgres. Two UNION ALL'd subqueries
        // each apply ORDER BY price + LIMIT inside the database, so the API
        // never materialises the full open book in memory. Sorting on the
        // numeric `price` column is exact (no string compare) and the
        // `live_orders_open_book_idx` partial index covers it.
        let rows: Vec<DepthLevelRow> = sqlx::query_as(
            r#"(select true  as is_buy, price::text as price,
                       sum(amount_remaining)::text as quantity
                  from live_orders
                 where orderbook_address = $1
                   and outcome_id = $2
                   and status = 'OPEN'
                   and amount_remaining > 0
                   and is_buy
                 group by price
                 order by price desc
                 limit $3)
               union all
               (select false as is_buy, price::text as price,
                       sum(amount_remaining)::text as quantity
                  from live_orders
                 where orderbook_address = $1
                   and outcome_id = $2
                   and status = 'OPEN'
                   and amount_remaining > 0
                   and not is_buy
                 group by price
                 order by price asc
                 limit $3)"#,
        )
        .bind(&orderbook_address)
        .bind(outcome_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("aggregate live_orders for depth")?;

        // Each inner subquery applied ORDER BY + LIMIT, but UNION ALL does not
        // guarantee that ordering is preserved in the outer result. Re-sort
        // each side explicitly: bids descending by price, asks ascending. The
        // raw `price` column is a non-negative integer string, so BigUint
        // gives an exact numeric compare without depending on string length.
        // A non-numeric price is read-model corruption — fail closed rather
        // than collapse silently to 0 and reorder the book.
        let mut bids: Vec<(BigUint, PriceLevel)> = Vec::new();
        let mut asks: Vec<(BigUint, PriceLevel)> = Vec::new();
        for row in rows {
            let key = BigUint::parse_bytes(row.price.as_bytes(), 10).ok_or_else(|| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    raw = %row.price,
                    "live_orders.price is not a non-negative integer",
                );
                anyhow!(DomainError::MarketInconsistent)
            })?;
            let level = (key, PriceLevel { price: row.price, quantity: row.quantity });
            if row.is_buy {
                bids.push(level);
            } else {
                asks.push(level);
            }
        }
        bids.sort_by(|a, b| b.0.cmp(&a.0));
        asks.sort_by(|a, b| a.0.cmp(&b.0));
        let mut bids: Vec<PriceLevel> = bids.into_iter().map(|(_, l)| l).collect();
        let mut asks: Vec<PriceLevel> = asks.into_iter().map(|(_, l)| l).collect();

        // `market_outcomes.(price|quantity)_precision` is `integer` (signed)
        // but bounded by domain contract: non-negative AND <=
        // MAX_DECIMAL_PRECISION (= NUMERIC(38, …)). A negative value is
        // structural corruption; a value above the cap would let
        // scale_uint_to_decimal's "0".repeat(scale) detonate the allocator
        // on the first scaled level. Both modes lift to MarketInconsistent.
        let price_scale = validate_decimal_scale(price_precision).map_err(|reason| {
            tracing::warn!(
                orderbook = %orderbook_address,
                outcome_id,
                raw = price_precision,
                max = MAX_DECIMAL_PRECISION,
                reason = match reason {
                    InvalidScale::Negative => "negative",
                    InvalidScale::AboveMax => "above MAX_DECIMAL_PRECISION",
                },
                "market_outcomes.price_precision out of range",
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;
        let quantity_scale = validate_decimal_scale(quantity_precision).map_err(|reason| {
            tracing::warn!(
                orderbook = %orderbook_address,
                outcome_id,
                raw = quantity_precision,
                max = MAX_DECIMAL_PRECISION,
                reason = match reason {
                    InvalidScale::Negative => "negative",
                    InvalidScale::AboveMax => "above MAX_DECIMAL_PRECISION",
                },
                "market_outcomes.quantity_precision out of range",
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;
        // live_orders mirrors the chain: price is in basis points, amount in
        // token atoms. Step each down to its display grid before formatting —
        // price bps → probability (price_precision), amount atoms → tokens
        // (quantity_precision). descale_pow10 returns the exact quotient when
        // the dropped digits are all zero and fails closed otherwise, so an
        // on-grid value renders exactly and an off-grid one is rejected. A
        // display precision finer than the chain scale (price_precision > the
        // basis-point exponent, or quantity_precision > decimals) is read-model
        // misconfiguration, caught as a negative drop when computing
        // price_drop / amount_drop.
        let price_drop =
            usize::try_from(i32::from(PRICE_BPS_DECIMALS) - price_precision).map_err(|_| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    price_precision,
                    "price_precision exceeds the basis-point scale",
                );
                anyhow!(DomainError::MarketInconsistent)
            })?;
        let amount_drop = usize::try_from(decimals - quantity_precision).map_err(|_| {
            tracing::warn!(
                orderbook = %orderbook_address,
                outcome_id,
                quantity_precision,
                decimals,
                "quantity_precision exceeds the quote-asset decimals",
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;
        for level in bids.iter_mut().chain(asks.iter_mut()) {
            let price_grid = descale_pow10(&level.price, price_drop).map_err(|e| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    axis = "price",
                    raw = %level.price,
                    "depth level value cannot be descaled to the display grid",
                );
                anyhow!(e)
            })?;
            let qty_grid = descale_pow10(&level.quantity, amount_drop).map_err(|e| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    axis = "quantity",
                    raw = %level.quantity,
                    "depth level value cannot be descaled to the display grid",
                );
                anyhow!(e)
            })?;
            level.price = scale_uint_to_decimal(&price_grid, price_scale);
            level.quantity = scale_uint_to_decimal(&qty_grid, quantity_scale);
        }

        // Scope to (orderbook, outcome_id): the depth response is per-outcome,
        // so a quiet outcome must not surface a chain-order cursor from a
        // sibling outcome's activity on the same orderbook. `last_chain_order`
        // is a lex-comparable string (gateway `msg_chain_order`), so the SQL
        // `max(text)` returns the highest chain-order across all touches.
        let last_update_id: Option<String> = sqlx::query_scalar(
            "select max(last_chain_order) from live_orders \
             where orderbook_address = $1 and outcome_id = $2",
        )
        .bind(&orderbook_address)
        .bind(outcome_id)
        .fetch_one(&self.pool)
        .await
        .context("compute last_update_id")?;

        Ok(DepthSnapshot {
            market_address: market_address.clone(),
            symbol: symbol.clone(),
            // Empty string == "no order event has touched this pair yet";
            // documented in DepthSnapshot::last_update_id.
            last_update_id: last_update_id.unwrap_or_default(),
            bids,
            asks,
        })
    }

    async fn get_trades(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: TradesLimit,
    ) -> Result<Vec<Trade>, anyhow::Error> {
        // Resolution mirrors `get_depth`: the quote-asset `decimals` (joined
        // from ref_tokens) feeds `quoteQty` scaling, and a reconciled row is
        // gated on `last_reconciled_at IS NOT NULL` so an unknown pair and a
        // never-reconciled one collapse to the same `InvalidMarketOrSymbol`.
        let target: Option<(Option<String>, i32, i32, i32, i32)> = sqlx::query_as(
            r#"select m.orderbook_address,
                      mo.outcome_id,
                      mo.price_precision,
                      mo.quantity_precision,
                      rt.decimals
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
                 join ref_tokens rt on rt.token_type = m.token_type
                where m.pmp_address = $1
                  and mo.symbol = $2
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("resolve orderbook_address from (marketAddress, symbol)")?;

        let Some((orderbook_address, outcome_id, price_precision, quantity_precision, decimals)) =
            target
        else {
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };
        let Some(orderbook_address) = orderbook_address.and_then(filter_orderbook) else {
            // Reconciled market mid-replay (blank orderbook). Fail closed
            // (503) so the client retries, never an empty tape.
            tracing::warn!(
                market = %market_address.0,
                symbol = %symbol.0,
                "reconciled market has a NULL/blank orderbook_address; trades fails closed",
            );
            return Err(anyhow!(DomainError::MarketInconsistent));
        };

        // Scales bound by the NUMERIC(38, …) ceiling, exactly as in depth: a
        // value above the cap would let scale_uint_to_decimal's "0".repeat
        // detonate the allocator. price/quantity render at the outcome's
        // precision; quote_qty renders at the quote asset's decimals. Each
        // guard logs its own axis: a bare 503 with nothing greppable would
        // leave the poisoned outcome unattributable.
        let bad_scale = |axis: &'static str, raw: i32| {
            tracing::warn!(
                orderbook = %orderbook_address,
                outcome_id,
                axis,
                raw,
                max = MAX_DECIMAL_PRECISION,
                "trades decimal scale out of range",
            );
            anyhow!(DomainError::MarketInconsistent)
        };
        let price_scale = validate_decimal_scale(price_precision)
            .map_err(|_| bad_scale("price", price_precision))?;
        let quantity_scale = validate_decimal_scale(quantity_precision)
            .map_err(|_| bad_scale("quantity", quantity_precision))?;
        let quote_scale =
            validate_decimal_scale(decimals).map_err(|_| bad_scale("quote", decimals))?;

        // Step price (basis points) and qty (token atoms) down to their display
        // grids before formatting. A display precision finer than the chain
        // scale is read-model misconfiguration, caught here as a negative drop.
        let price_drop =
            usize::try_from(i32::from(PRICE_BPS_DECIMALS) - price_precision).map_err(|_| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    price_precision,
                    "trades price_precision exceeds the basis-point scale",
                );
                anyhow!(DomainError::MarketInconsistent)
            })?;
        let amount_drop = usize::try_from(decimals - quantity_precision).map_err(|_| {
            tracing::warn!(
                orderbook = %orderbook_address,
                outcome_id,
                quantity_precision,
                decimals,
                "trades quantity_precision exceeds the quote-asset decimals",
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;

        // TradesLimit's [1, TRADES_MAX_LIMIT] invariant arrives by type — no
        // clamp here, because the public trades contract rejects out-of-range
        // limits upstream instead of silently clamping like depth.
        let limit = i64::from(limit.get());

        // `trade_id` is the lex-monotonic taker chain-order, so `ORDER BY
        // trade_id DESC` already yields true newest-first chain order with no
        // Rust-side re-sort. The plain (non-partial) `trades_tape_idx` serves
        // it as a range scan. `chain_time IS NOT NULL` guards the rare row the
        // gateway delivered without a parseable time, matching /api/v1/prediction/orders.
        let rows: Vec<TradeRow> = sqlx::query_as(
            r#"select trade_id,
                      price::text as price,
                      qty::text as qty,
                      is_buyer_maker,
                      (extract(epoch from chain_time) * 1000000)::bigint as chain_time_us
                 from trades
                where orderbook_address = $1
                  and outcome_id = $2
                  and chain_time is not null
                order by trade_id desc
                limit $3"#,
        )
        .bind(&orderbook_address)
        .bind(outcome_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("read trades tape")?;

        // FULL_PERCENT = 10^PRICE_BPS_DECIMALS (= 10_000). quoteQty is derived
        // from the same two raw integers the contract used —
        // notional_atoms = price * qty / FULL_PERCENT, integer division — so it
        // never drifts from the on-chain notional by a rounding ulp.
        let full_percent = BigUint::from(10u32).pow(u32::from(PRICE_BPS_DECIMALS));

        let mut trades = Vec::with_capacity(rows.len());
        for row in rows {
            let parse_uint = |raw: &str, axis: &str| {
                BigUint::parse_bytes(raw.as_bytes(), 10).ok_or_else(|| {
                    tracing::warn!(
                        orderbook = %orderbook_address,
                        outcome_id,
                        trade_id = %row.trade_id,
                        axis,
                        raw,
                        "trades raw value is not a non-negative integer",
                    );
                    anyhow!(DomainError::MarketInconsistent)
                })
            };
            let price_uint = parse_uint(&row.price, "price")?;
            let qty_uint = parse_uint(&row.qty, "qty")?;

            let notional = (&price_uint * &qty_uint) / &full_percent;

            let log_off_grid = |axis: &'static str, raw: &str| {
                tracing::warn!(
                    orderbook = %orderbook_address,
                    outcome_id,
                    trade_id = %row.trade_id,
                    axis,
                    raw,
                    "trade value cannot be descaled to the display grid",
                );
            };
            let price_grid = descale_pow10(&row.price, price_drop).map_err(|e| {
                log_off_grid("price", &row.price);
                anyhow!(e)
            })?;
            let qty_grid = descale_pow10(&row.qty, amount_drop).map_err(|e| {
                log_off_grid("qty", &row.qty);
                anyhow!(e)
            })?;

            trades.push(Trade {
                trade_id: row.trade_id,
                price: scale_uint_to_decimal(&price_grid, price_scale),
                qty: scale_uint_to_decimal(&qty_grid, quantity_scale),
                quote_qty: scale_uint_to_decimal(&notional.to_str_radix(10), quote_scale),
                // Microseconds → Unix milliseconds, the same truncation as
                // `time` / `updateTime` on /api/v1/prediction/orders.
                time: row.chain_time_us / 1_000,
                is_buyer_maker: row.is_buyer_maker,
            });
        }

        Ok(trades)
    }

    async fn resolve_for_new_order(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        now: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
        // Single round-trip for the trading hot path: join `markets` and
        // `market_outcomes` on `(pmp_address, symbol)`. Oracle / event
        // aggregation (which `list_markets` performs) is irrelevant
        // here — the use case only consumes `event_id`,
        // `oracle_list_hash`, `token_type`, the outcome row, and a
        // status derived from the timing columns we pull below.
        let row: Option<PlacementRow> = sqlx::query_as(
            r#"select m.oracle_list_hash::text as oracle_list_hash,
                      m.token_type             as token_type,
                      m.event_id::text         as event_id,
                      m.stake_start            as stake_start,
                      m.stake_end              as stake_end,
                      m.result_start           as result_start,
                      m.result_end             as result_end,
                      m.frozen_at              as frozen_at,
                      m.resolved_at            as resolved_at,
                      m.cancelled_at           as cancelled_at,
                      m.is_cancelled           as is_cancelled,
                      mo.outcome_id            as outcome_id,
                      mo.outcome_name          as outcome_name,
                      mo.price_precision       as price_precision,
                      mo.quantity_precision    as quantity_precision,
                      mo.tick_size             as tick_size,
                      mo.step_size             as step_size,
                      mo.min_notional          as min_notional,
                      rt.decimals              as decimals
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
                 join ref_tokens rt on rt.token_type = m.token_type
                where m.pmp_address = $1
                  and mo.symbol = $2
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("resolve_for_new_order: select market+outcome")?;

        let Some(row) = row else {
            // Same collapse the depth handler uses: missing market vs
            // missing symbol-within-market are indistinguishable to the
            // client and the spec does not require we tell them apart.
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };

        let status = compute_status(
            row.cancelled_at,
            row.is_cancelled,
            row.resolved_at,
            row.stake_start,
            row.stake_end,
            row.result_start,
            row.result_end,
            row.frozen_at,
            now,
        );

        // A reconciled row is supposed to carry a non-blank oracle_list_hash —
        // the chain ABI requires it for placeOrder, and the use case would
        // immediately reject a blank value. Lift to MarketInconsistent at the
        // boundary so the caller sees a typed error instead of a sentinel
        // empty string.
        let oracle_list_hash = match row.oracle_list_hash {
            Some(raw) if !raw.trim().is_empty() => raw,
            other => {
                warn!(
                    pmp_address = %market_address.0,
                    null = other.is_none(),
                    "resolve_for_new_order: oracle_list_hash NULL/blank on reconciled row",
                );
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
        };

        let token_type: u32 = row.token_type.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.token_type,
                "placement_row token_type is negative",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let outcome_id: u32 = row.outcome_id.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.outcome_id,
                "placement_row outcome_id is negative",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let price_precision: u8 = row.price_precision.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.price_precision,
                "placement_row price_precision out of range",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let quantity_precision: u8 = row.quantity_precision.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.quantity_precision,
                "placement_row quantity_precision out of range",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let decimals: u8 = row.decimals.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.decimals,
                "placement_row decimals out of range",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        Ok(MarketForPlacement {
            event_id: row.event_id,
            oracle_list_hash,
            token_type,
            status,
            outcome: Outcome {
                outcome_id,
                outcome_name: row.outcome_name,
                symbol: symbol.clone(),
                price_precision,
                quantity_precision,
                tick_size: row.tick_size,
                step_size: row.step_size,
                min_notional: row.min_notional,
            },
            decimals,
        })
    }

    async fn resolve_for_cancel(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_id: u64,
        owner_pn_address: &str,
        now: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        // Single round-trip. Join `live_orders` to `markets` on
        // `orderbook_address` (CHECK-enforced on every reconciled
        // market, see migration 0014), then to `market_outcomes` on
        // `outcome_id` so the `(marketAddress, symbol)` filter from
        // the caller actually constrains the row. Every miss collapses
        // to `UnknownOrder` — the use case docs why.
        //
        // `lo.order_id` is `numeric(78,0)`; bind as decimal string and
        // cast (same pattern the indexer uses in `apply_order_*`).
        let order_id_decimal = order_id.to_string();
        let row: Option<CancelRow> = sqlx::query_as(
            r#"select m.event_id::text         as event_id,
                      m.oracle_list_hash::text as oracle_list_hash,
                      m.token_type             as token_type,
                      m.stake_start            as stake_start,
                      m.stake_end              as stake_end,
                      m.result_start           as result_start,
                      m.result_end             as result_end,
                      m.frozen_at              as frozen_at,
                      m.resolved_at            as resolved_at,
                      m.cancelled_at           as cancelled_at,
                      m.is_cancelled           as is_cancelled,
                      lo.client_order_id       as client_order_id
                 from live_orders lo
                 join markets m on m.orderbook_address = lo.orderbook_address
                 join market_outcomes mo
                   on mo.market_id_fk = m.id
                  and mo.outcome_id = lo.outcome_id
                where m.pmp_address       = $1
                  and mo.symbol           = $2
                  and lo.order_id         = $3::numeric
                  and lo.owner_pn_address = $4
                  and lo.status           = 'OPEN'
                  and lo.amount_remaining > 0
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .bind(order_id_decimal.as_str())
        .bind(owner_pn_address)
        .fetch_optional(&self.pool)
        .await
        .context("resolve_for_cancel: select live_orders + market")?;

        let Some(row) = row else {
            // Single ambiguous miss code: unknown id, wrong owner,
            // wrong market for this id, market hidden from API,
            // or order already closed.
            return Err(anyhow!(DomainError::UnknownOrder));
        };

        let status = compute_status(
            row.cancelled_at,
            row.is_cancelled,
            row.resolved_at,
            row.stake_start,
            row.stake_end,
            row.result_start,
            row.result_end,
            row.frozen_at,
            now,
        );

        // See resolve_for_new_order: a blank oracle_list_hash on a reconciled
        // row is read-model corruption. Fail closed at the repo boundary.
        let oracle_list_hash = match row.oracle_list_hash {
            Some(raw) if !raw.trim().is_empty() => raw,
            other => {
                warn!(
                    pmp_address = %market_address.0,
                    null = other.is_none(),
                    "resolve_for_cancel: oracle_list_hash NULL/blank on reconciled row",
                );
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
        };

        let token_type: u32 = row.token_type.try_into().map_err(|_| {
            tracing::warn!(
                market_address = %market_address.0,
                symbol = %symbol.0,
                raw = row.token_type,
                "cancel_row token_type is negative",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        let client_order_id = row.client_order_id.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        Ok(OrderForCancel {
            event_id: row.event_id,
            oracle_list_hash,
            token_type,
            market_status: status,
            client_order_id,
        })
    }

    async fn resolve_for_cancel_batch(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_ids: &[u64],
        owner_pn_address: &str,
        now: i64,
    ) -> Result<Option<CancelBatchResolution>, anyhow::Error> {
        // Mirror `resolve_for_cancel`'s join shape (live_orders ⨝
        // markets ⨝ market_outcomes), filtered by the bind array via
        // `lo.order_id = ANY($3::text[]::numeric[])`. The cast lives
        // on the bind side, not on the indexed column — that
        // preserves the planner's ability to use the
        // `(orderbook_address, order_id)` primary key for the
        // per-id lookup. Project `lo.order_id::text` so the
        // application layer can reassemble a
        // `HashMap<u64, OrderForCancelBatch>` keyed on the natural
        // chain identity — the SELECT predicate guarantees every
        // returned `order_id` is a member of the caller's slice
        // (trait contract). Market identity + timing columns project
        // in the same statement so chain payload and `compute_status`
        // both run against the snapshot that matched the orders.
        let order_ids_decimal: Vec<String> = order_ids.iter().map(|id| id.to_string()).collect();
        let rows: Vec<CancelBatchRow> = sqlx::query_as(
            r#"select lo.order_id::text            as order_id,
                      lo.client_order_id           as client_order_id,
                      m.event_id::text             as event_id,
                      m.oracle_list_hash::text     as oracle_list_hash,
                      m.token_type                 as token_type,
                      m.stake_start                as stake_start,
                      m.stake_end                  as stake_end,
                      m.result_start               as result_start,
                      m.result_end                 as result_end,
                      m.frozen_at                  as frozen_at,
                      m.resolved_at                as resolved_at,
                      m.cancelled_at               as cancelled_at,
                      m.is_cancelled               as is_cancelled
                 from markets m
                 join market_outcomes mo
                   on mo.market_id_fk = m.id
                  and mo.symbol = $2
                 join live_orders lo
                   on lo.orderbook_address = m.orderbook_address
                  and lo.outcome_id        = mo.outcome_id
                  and lo.owner_pn_address  = $4
                  and lo.status            = 'OPEN'
                  and lo.amount_remaining  > 0
                  and lo.order_id          = ANY($3::text[]::numeric[])
                where m.pmp_address = $1
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .bind(&order_ids_decimal)
        .bind(owner_pn_address)
        .fetch_all(&self.pool)
        .await
        .context("resolve_for_cancel_batch: select live_orders + market")?;

        // Zero matches: no `(pmp_address, symbol)` row joined any
        // input id, so there's no `markets` snapshot to anchor
        // identity to. The use case maps `None` to `UnknownOrder`.
        let Some(head) = rows.first() else {
            return Ok(None);
        };

        // Identity comes from the JOINed `markets` row; by the SELECT
        // filter (`m.pmp_address = $1 AND mo.symbol = $2`) every row
        // shares the same `(event_id, oracle_list_hash, token_type)`
        // and `market_status`. Project from `head` once.
        let event_id = head.event_id.clone();
        let token_type = head.token_type;
        let oracle_list_hash = match &head.oracle_list_hash {
            Some(raw) if !raw.trim().is_empty() => raw.clone(),
            other => {
                warn!(
                    pmp_address = %market_address.0,
                    null = other.is_none(),
                    "resolve_for_cancel_batch: oracle_list_hash NULL/blank on reconciled row",
                );
                String::new()
            }
        };
        let market_status = compute_status(
            head.cancelled_at,
            head.is_cancelled,
            head.resolved_at,
            head.stake_start,
            head.stake_end,
            head.result_start,
            head.result_end,
            head.frozen_at,
            now,
        );

        let mut orders: HashMap<u64, OrderForCancelBatch> = HashMap::with_capacity(rows.len());
        for row in rows {
            // `live_orders.order_id` is the chain-assigned uint128, but
            // the application boundary caps at u64 (SDK ceiling — see
            // `chain_sender.rs`). A row whose stored value exceeds
            // u64 means a producer wrote a value above that cap — same
            // class of read-model-vs-chain drift as the negative
            // token_type / NULL oracle_list_hash branches, so surface
            // as MarketInconsistent (503) rather than Unexpected (500).
            let order_id = row.order_id.parse::<u64>().map_err(|err| {
                anyhow::Error::from(DomainError::MarketInconsistent).context(format!(
                    "resolve_for_cancel_batch: order_id `{}` is not u64: {err}",
                    row.order_id
                ))
            })?;
            let client_order_id = row.client_order_id.and_then(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            });
            // Unreachable today: `markets.pmp_address` UNIQUE plus
            // the `(orderbook_address, order_id)` PK on `live_orders`
            // guarantee at most one row per chain `order_id` from
            // this SELECT — no schema state reachable from migration
            // 0001 can produce duplicates. Kept as defence-in-depth
            // against future schema relaxation (e.g. if a multi-
            // generation pmp_address or a UNION-ALL refactor ever
            // changed the JOIN cardinality) so a duplicate surfaces
            // as MarketInconsistent rather than silently overwriting
            // the prior `client_order_id`.
            if orders.insert(order_id, OrderForCancelBatch { client_order_id }).is_some() {
                return Err(anyhow::Error::from(DomainError::MarketInconsistent).context(format!(
                    "resolve_for_cancel_batch: duplicate order_id `{order_id}` returned (live_orders PK violated)",
                )));
            }
        }
        Ok(Some(CancelBatchResolution {
            event_id,
            oracle_list_hash,
            token_type,
            market_status,
            orders,
        }))
    }

    async fn list_orders(&self, query: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        let target = match &query.market {
            Some(filter) => {
                let target: Option<(Option<String>, i32)> = sqlx::query_as(
                    r#"select m.orderbook_address,
                              mo.outcome_id
                         from markets m
                         join market_outcomes mo on mo.market_id_fk = m.id
                        where m.pmp_address = $1
                          and mo.symbol = $2
                          and m.last_reconciled_at is not null"#,
                )
                .bind(filter.market_address().0.as_str())
                .bind(filter.symbol().0.as_str())
                .fetch_optional(&self.pool)
                .await
                .context("resolve orders market filter")?;

                let Some((orderbook_address, outcome_id)) = target else {
                    return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
                };
                let Some(orderbook_address) = orderbook_address.and_then(filter_orderbook) else {
                    return Err(anyhow!(DomainError::MarketInconsistent));
                };
                Some((orderbook_address, outcome_id))
            }
            None => None,
        };

        let limit_plus_one = i64::from(query.limit.get()) + 1;
        let status_sql = match build_status_predicate(&query.status) {
            Some(clause) => format!(" AND ({clause}) "),
            None => String::new(),
        };

        // The microsecond extraction `(extract(epoch from <timestamptz>) *
        // 1000000)::bigint` feeds response fields `time` / `updateTime`
        // only; the cursor is placed_chain_order (text). Deployment is
        // pinned to PG15+ (Supabase) and PG16 (docker-compose.test.yml);
        // both return numeric from extract(epoch ...), so the bigint cast
        // is exact.
        // The filtered/unfiltered SQL blocks are intentionally duplicated:
        // their bind arity differs, while the projection/predicate contract
        // must stay obvious because it backs the public /orders shape.
        let rows: Vec<OrderRow> = match target {
            Some((orderbook_address, outcome_id)) => {
                let sql = format!(
                    r#"select m.pmp_address as market_address,
                          mo.symbol as symbol,
                          lo.order_id::text as order_id,
                          coalesce(lo.client_order_id, '') as client_order_id,
                          lo.price::text as price,
                          lo.amount_initial::text as orig_qty,
                          greatest(lo.amount_initial - lo.amount_remaining, 0)::text as executed_qty,
                          (lo.amount_remaining = 0) as fully_filled,
                          (lo.amount_remaining > lo.amount_initial) as corrupt_remainder,
                          lo.is_buy as is_buy,
                          (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
                          (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
                          lo.placed_chain_order as placed_chain_order,
                          mo.price_precision as price_precision,
                          mo.quantity_precision as quantity_precision,
                          rt.decimals as decimals,
                          lo.status as raw_status
                     from live_orders lo
                     join markets m on m.orderbook_address = lo.orderbook_address
                     join market_outcomes mo
                       on mo.market_id_fk = m.id
                      and mo.outcome_id = lo.outcome_id
                     join ref_tokens rt on rt.token_type = m.token_type
                    where lo.owner_pn_address = $1
                      and lo.chain_created_at is not null
                      and lo.chain_updated_at is not null
                      and m.last_reconciled_at is not null
                      and lo.orderbook_address = $2
                      and lo.outcome_id = $3
                      and ($4::text is null or lo.placed_chain_order < $4::text)
                      {status_sql}
                    order by lo.placed_chain_order desc
                    limit $5"#
                );
                sqlx::query_as::<_, OrderRow>(&sql)
                    .bind(query.owner_pn_address.as_str())
                    .bind(orderbook_address)
                    .bind(outcome_id)
                    .bind(query.cursor.as_ref().map(OrdersCursor::as_str))
                    .bind(limit_plus_one)
                    .fetch_all(&self.pool)
                    .await
                    .context("select filtered orders")?
            }
            None => {
                let sql = format!(
                    r#"select m.pmp_address as market_address,
                          mo.symbol as symbol,
                          lo.order_id::text as order_id,
                          coalesce(lo.client_order_id, '') as client_order_id,
                          lo.price::text as price,
                          lo.amount_initial::text as orig_qty,
                          greatest(lo.amount_initial - lo.amount_remaining, 0)::text as executed_qty,
                          (lo.amount_remaining = 0) as fully_filled,
                          (lo.amount_remaining > lo.amount_initial) as corrupt_remainder,
                          lo.is_buy as is_buy,
                          (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
                          (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
                          lo.placed_chain_order as placed_chain_order,
                          mo.price_precision as price_precision,
                          mo.quantity_precision as quantity_precision,
                          rt.decimals as decimals,
                          lo.status as raw_status
                     from live_orders lo
                     join markets m on m.orderbook_address = lo.orderbook_address
                     join market_outcomes mo
                       on mo.market_id_fk = m.id
                      and mo.outcome_id = lo.outcome_id
                     join ref_tokens rt on rt.token_type = m.token_type
                    where lo.owner_pn_address = $1
                      and lo.chain_created_at is not null
                      and lo.chain_updated_at is not null
                      and m.last_reconciled_at is not null
                      and ($2::text is null or lo.placed_chain_order < $2::text)
                      {status_sql}
                    order by lo.placed_chain_order desc
                    limit $3"#
                );
                sqlx::query_as::<_, OrderRow>(&sql)
                    .bind(query.owner_pn_address.as_str())
                    .bind(query.cursor.as_ref().map(OrdersCursor::as_str))
                    .bind(limit_plus_one)
                    .fetch_all(&self.pool)
                    .await
                    .context("select all orders")?
            }
        };

        let limit = usize::from(query.limit.get());
        let has_more = rows.len() > limit;
        let mut orders_raw = rows;
        if has_more {
            orders_raw.truncate(limit);
        }

        let next_cursor = if has_more {
            // `GetOrdersUseCase::execute` clamps `limit` to
            // `[1, ORDERS_MAX_LIMIT]` before constructing the query;
            // combined with `has_more = rows.len() > limit`, the
            // post-truncate page is non-empty. A non-HTTP caller
            // bypassing the use case would turn this into a panic —
            // the application-side tests
            // `get_orders_defaults_limit_when_absent` and
            // `get_orders_rejects_limit_out_of_range` pin that invariant.
            let row = orders_raw.last().expect("has_more implies non-empty page");
            Some(OrdersCursor::from_db_token(row.placed_chain_order.clone()).map_err(|err| {
                error!(
                    market = %row.market_address,
                    order_id = %row.order_id,
                    placed_chain_order = %row.placed_chain_order,
                    "live_orders row has invalid placed_chain_order for nextCursor"
                );
                anyhow!(err)
            })?)
        } else {
            None
        };

        // `order_from_row` returns `None` for projector-bug rows (logs an
        // error! inside). `next_cursor` was captured above from the
        // pre-filter tail, so a corrupt boundary row advances the cursor
        // past itself instead of freezing pagination — pinned by
        // `cursor_advances_past_corrupt_row_at_page_tail`.
        let raw_len = orders_raw.len();
        let orders = orders_raw.into_iter().filter_map(order_from_row).collect::<Vec<_>>();
        let skipped = raw_len.saturating_sub(orders.len());
        if skipped > 0 {
            error!(
                skipped_rows = skipped,
                returned_rows = orders.len(),
                "list_orders skipped corrupt live_orders rows while rendering page"
            );
        }

        // (empty orders, Some(cursor)) is the corrupt-window page:
        // every row in this `has_more=true` page was filtered by
        // `order_from_row` (per-row error! already logged inside the
        // mapper). Surface the page anyway so the client can paginate
        // past the corrupt window via the cursor — `next_cursor` was
        // captured from the last retained row *before* the filter
        // pass for exactly this case (read-api.md §SQL).
        Ok(OrdersPage { orders, next_cursor })
    }

    async fn resolve_market_for_balances(
        &self,
        market_address: &dodex_domain::MarketAddress,
    ) -> Result<dodex_application::MarketBalancesResolution, anyhow::Error> {
        // Read markets + market_outcomes inside a REPEATABLE READ transaction
        // so both queries see the same snapshot. Without it a reconciler
        // reseed (UPDATE markets.num_outcomes paired with DELETE/INSERT in
        // market_outcomes) interleaved between the two SELECTs can produce a
        // num_outcomes/raw_outcomes.len() mismatch and trip
        // `MarketInconsistent` (503) on an otherwise valid market. Plain
        // `pool.begin()` opens READ COMMITTED in Postgres; bump the isolation
        // immediately after BEGIN, before the first read.
        let mut tx = self.pool.begin().await.context("resolve_market_for_balances: tx begin")?;
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await
            .context("resolve_market_for_balances: set repeatable read")?;

        // Resolve the market row + outcomes in two SELECTs to keep types
        // simple. The visibility gate `last_reconciled_at IS NOT NULL`
        // matches /api/v1/prediction/markets — pre-reconcile markets are invisible.
        let market: Option<(
            String,         // event_id (numeric → text via ::text)
            Option<String>, // oracle_list_hash
            i32,            // token_type
            Option<String>, // orderbook_address
            i32,            // num_outcomes
            i64,            // markets.id
            i32,            // ref_tokens.decimals
        )> = sqlx::query_as(
            // INNER join ref_tokens for the quote-asset `decimals`. `_stakes`
            // amounts are atoms at this scale; the balances use case scales by
            // the full `decimals`. `markets.token_type` has an FK to
            // ref_tokens, so the join never narrows a visible market.
            r#"select m.event_id::text,
                      m.oracle_list_hash::text,
                      m.token_type,
                      m.orderbook_address,
                      m.num_outcomes,
                      m.id,
                      rt.decimals
                 from markets m
                 join ref_tokens rt on rt.token_type = m.token_type
                where m.pmp_address = $1
                  and m.last_reconciled_at is not null"#,
        )
        .bind(&market_address.0)
        .fetch_optional(&mut *tx)
        .await
        .context("resolve_market_for_balances: select markets")?;

        let (
            event_id,
            oracle_list_hash,
            token_type_raw,
            orderbook_address,
            num_outcomes,
            market_id,
            decimals_raw,
        ) = match market {
            Some(m) => m,
            None => return Err(anyhow::anyhow!(dodex_domain::DomainError::InvalidMarketOrSymbol)),
        };

        // `ref_tokens.decimals` is `integer` (signed) but non-negative and
        // bounded by the same domain cap as price/quantity precision:
        // <= MAX_DECIMAL_PRECISION (NUMERIC(38,…)). Validate here rather than a
        // bare `u8::try_from`, which would admit 39..=255 to be caught only
        // later in scale_decimal. Negative or above-cap is read-model
        // corruption → MarketInconsistent (api-spec "decimals out of range → 503").
        let decimals_scale = validate_decimal_scale(decimals_raw).map_err(|reason| {
            tracing::warn!(
                pmp = %market_address.0,
                raw = decimals_raw,
                max = MAX_DECIMAL_PRECISION,
                reason = match reason {
                    InvalidScale::Negative => "negative",
                    InvalidScale::AboveMax => "above MAX_DECIMAL_PRECISION",
                },
                "ref_tokens.decimals out of range — read-model corruption",
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;
        // Total conversion: validate_decimal_scale already capped this at
        // MAX_DECIMAL_PRECISION (38). Checked rather than `as u8` so a future
        // cap raised above 255 can't silently truncate.
        let decimals = u8::try_from(decimals_scale).map_err(|_| {
            tracing::warn!(
                pmp = %market_address.0,
                raw = decimals_scale,
                "decimals exceeds u8 after scale validation — read-model corruption"
            );
            anyhow!(DomainError::MarketInconsistent)
        })?;

        // `oracle_list_hash` is nullable at the schema level (pre-reconcile),
        // but we already gated on last_reconciled_at IS NOT NULL — a NULL here
        // means data corruption.
        let oracle_list_hash = oracle_list_hash.ok_or_else(|| {
            tracing::warn!(pmp = %market_address.0, "reconciled market has NULL oracle_list_hash");
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let orderbook_address = orderbook_address
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                tracing::warn!(pmp = %market_address.0, "reconciled market has NULL/blank orderbook_address");
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;

        // `token_type` is `integer` (signed) in Postgres but non-negative by
        // contract (written by the reconciler from `PMP.getDetails()`).
        // Treat a negative value as read-model corruption, mirroring the
        // `num_outcomes` check below.
        let token_type: u32 = token_type_raw.try_into().map_err(|_| {
            tracing::warn!(
                pmp = %market_address.0,
                raw = token_type_raw,
                "token_type is negative — read-model corruption"
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        let raw_outcomes: Vec<(i32, String, i32)> = sqlx::query_as(
            r#"select outcome_id, symbol, quantity_precision
                 from market_outcomes
                where market_id_fk = $1
                order by outcome_id asc"#,
        )
        .bind(market_id)
        .fetch_all(&mut *tx)
        .await
        .context("resolve_market_for_balances: select market_outcomes")?;

        tx.commit().await.context("resolve_market_for_balances: tx commit")?;

        // `num_outcomes` is `integer` (signed) in Postgres but non-negative
        // by contract. Treat a negative value as read-model corruption.
        let num_outcomes: u32 = num_outcomes.try_into().map_err(|_| {
            tracing::warn!(
                pmp = %market_address.0,
                raw = num_outcomes,
                "num_outcomes is negative — read-model corruption"
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        // Check count before per-row conversions so a count mismatch (the more
        // actionable signal) is not masked by a bad-value error on the first row.
        if raw_outcomes.len() != num_outcomes as usize {
            tracing::warn!(
                pmp = %market_address.0,
                outcomes_len = raw_outcomes.len(),
                num_outcomes,
                "market_outcomes row count does not match markets.num_outcomes"
            );
            return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
        }

        let outcomes: Vec<dodex_application::BalanceOutcome> = raw_outcomes
            .into_iter()
            .map(|(outcome_id, symbol, qp)| -> Result<_, anyhow::Error> {
                let outcome_id = u32::try_from(outcome_id).map_err(|_| {
                    tracing::warn!(pmp = %market_address.0, raw = outcome_id, "outcome_id is negative");
                    anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
                })?;
                let quantity_precision = u8::try_from(qp).map_err(|_| {
                    tracing::warn!(pmp = %market_address.0, outcome_id, raw = qp, "quantity_precision out of range");
                    anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
                })?;
                Ok(dodex_application::BalanceOutcome {
                    outcome_id,
                    symbol: dodex_domain::Symbol(symbol),
                    quantity_precision,
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(dodex_application::MarketBalancesResolution {
            event_id,
            oracle_list_hash,
            token_type,
            orderbook_address,
            decimals,
            num_outcomes,
            outcomes,
        })
    }

    async fn resolve_for_buy_full_set(
        &self,
        market_address: &MarketAddress,
        now: i64,
    ) -> Result<dodex_application::MarketForBuyFullSet, anyhow::Error> {
        // splitFullSet is a market-level chain op (collateral → one
        // outcome token of every outcome), so no `market_outcomes` join
        // is needed. Single SELECT, same visibility gate as the other
        // trading-path resolvers (`last_reconciled_at IS NOT NULL`),
        // same timing columns feeding `compute_status` so the use case
        // can gate `AWAITING_FREEZE | TRADING` from one round-trip.
        let row: Option<BuyFullSetRow> = sqlx::query_as(
            r#"select event_id::text         as event_id,
                      oracle_list_hash::text as oracle_list_hash,
                      token_type             as token_type,
                      stake_start            as stake_start,
                      stake_end              as stake_end,
                      result_start           as result_start,
                      result_end             as result_end,
                      frozen_at              as frozen_at,
                      resolved_at            as resolved_at,
                      cancelled_at           as cancelled_at,
                      is_cancelled           as is_cancelled
                 from markets
                where pmp_address = $1
                  and last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("resolve_for_buy_full_set: select markets")?;

        let Some(row) = row else {
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };
        project_buy_full_set_row(row, market_address, now)
    }

    async fn sum_open_sell_remaining(
        &self,
        orderbook_address: &str,
        owner_pn_address: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        let rows: Vec<(i32, String)> = sqlx::query_as(
            r#"select outcome_id, sum(amount_remaining)::text
                 from live_orders
                where orderbook_address = $1
                  and owner_pn_address  = $2
                  and status = 'OPEN'
                  and is_buy = false
                group by outcome_id"#,
        )
        .bind(orderbook_address)
        .bind(owner_pn_address)
        .fetch_all(&self.pool)
        .await
        .context("sum_open_sell_remaining: aggregate live_orders")?;
        rows.into_iter()
            .map(|(oid, sum)| {
                let oid = u32::try_from(oid).map_err(|_| {
                    tracing::warn!(raw = oid, "outcome_id in live_orders is negative");
                    anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
                })?;
                Ok((oid, sum))
            })
            .collect()
    }

    async fn list_oracles(&self, request: &OraclesRequest) -> Result<OraclesPage, anyhow::Error> {
        let limit = request.limit.clamp(1, 200) as i64;

        // eventId (hex) → decimal, fail closed (400) on bad hex.
        let event_id_decimal = match &request.filter.event_id {
            Some(hex) => Some(oracle_event_id_to_decimal(hex)?),
            None => None,
        };

        // Decode the cursor (400 on garbage); split into name/id binds.
        let (cursor_id, cursor_name) = match &request.cursor {
            Some(raw) => {
                let (id, name) = decode_oracles_cursor(raw)?;
                (Some(id), Some(name))
            }
            None => (None, None),
        };

        // Phase 1: oracle page. Placeholders: $1 now, $2 oracle_address,
        // $3 cursor_name, $4 cursor_id, $5 event_id_decimal, $6 deadline_before,
        // $7 limit+1. $1/$5/$6 are referenced inside the EXISTS availability.
        let phase1 = format!(
            r#"select o.id, o.name, o.address
                 from oracles o
                where ($2::text is null or o.address = $2)
                  and ($3::text is null or o.name > $3 or (o.name = $3 and o.id > $4))
                  and exists (
                      select 1
                        from oracle_event_lists oel
                        join oracle_events oe on oe.eventlist_id = oel.id
                       where oel.oracle_id = o.id
                         and {availability}
                  )
                order by o.name asc, o.id asc
                limit $7"#,
            availability = oracle_event_availability(1, 5, 6),
        );

        let heads: Vec<OracleHeadRow> = sqlx::query_as(&phase1)
            .bind(request.now)
            .bind(request.filter.oracle_address.as_deref())
            .bind(cursor_name.as_deref())
            .bind(cursor_id)
            .bind(event_id_decimal.as_deref())
            .bind(request.filter.deadline_before)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await
            .context("select oracles page")?;

        let mut heads = heads;
        let has_more = heads.len() as i64 > limit;
        if has_more {
            heads.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            heads.last().map(|h| encode_oracles_cursor(h.id, &h.name))
        } else {
            None
        };

        if heads.is_empty() {
            return Ok(OraclesPage { oracles: Vec::new(), next_cursor: None, has_more: false });
        }

        // Phase 2: list+event rows for the retained oracle ids. Placeholders:
        // $1 oracle ids, $2 now, $3 event_id_decimal, $4 deadline_before.
        let ids: Vec<i64> = heads.iter().map(|h| h.id).collect();
        let phase2 = format!(
            r#"select oel.oracle_id,
                      oel.list_index,
                      oel.address                       as eventlist_address,
                      oel.description                   as eventlist_description,
                      oe.internal_id_in_eventlist::text as event_id,
                      oe.event_name,
                      oe.describe                       as event_description,
                      oe.oracle_fee::text               as oracle_fee,
                      oe.deadline,
                      oe.trust_addr,
                      oe.outcome_names_jsonb
                 from oracle_event_lists oel
                 join oracle_events oe on oe.eventlist_id = oel.id
                where oel.oracle_id = any($1)
                  and {availability}
                order by oel.oracle_id, oel.list_index asc, oe.deadline asc, oe.internal_id_in_eventlist asc"#,
            availability = oracle_event_availability(2, 3, 4),
        );

        let rows: Vec<OracleListEventRow> = sqlx::query_as(&phase2)
            .bind(ids.as_slice())
            .bind(request.now)
            .bind(event_id_decimal.as_deref())
            .bind(request.filter.deadline_before)
            .fetch_all(&self.pool)
            .await
            .context("select oracle list+event rows")?;

        Ok(assemble_oracles_page(heads, rows, next_cursor, has_more)?)
    }
}

#[derive(Debug, sqlx::FromRow)]
struct PlacementRow {
    decimals: i32,
    oracle_list_hash: Option<String>,
    token_type: i32,
    event_id: String,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    resolved_at: Option<i64>,
    cancelled_at: Option<i64>,
    is_cancelled: bool,
    outcome_id: i32,
    outcome_name: String,
    price_precision: i32,
    quantity_precision: i32,
    tick_size: String,
    step_size: String,
    min_notional: String,
}

#[derive(Debug, sqlx::FromRow)]
struct BuyFullSetRow {
    event_id: String,
    oracle_list_hash: Option<String>,
    token_type: i32,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    resolved_at: Option<i64>,
    cancelled_at: Option<i64>,
    is_cancelled: bool,
}

fn project_buy_full_set_row(
    row: BuyFullSetRow,
    market_address: &MarketAddress,
    now: i64,
) -> Result<dodex_application::MarketForBuyFullSet, anyhow::Error> {
    let status = compute_status(
        row.cancelled_at,
        row.is_cancelled,
        row.resolved_at,
        row.stake_start,
        row.stake_end,
        row.result_start,
        row.result_end,
        row.frozen_at,
        now,
    );

    let oracle_list_hash = match row.oracle_list_hash {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                warn!(
                    pmp_address = %market_address.0,
                    "resolve_for_buy_full_set: oracle_list_hash blank on reconciled row",
                );
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
            trimmed.to_string()
        }
        None => {
            warn!(
                pmp_address = %market_address.0,
                "resolve_for_buy_full_set: oracle_list_hash NULL on reconciled row",
            );
            return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
        }
    };

    let event_id = {
        let trimmed = row.event_id.trim();
        if trimmed.is_empty() {
            warn!(
                pmp_address = %market_address.0,
                "resolve_for_buy_full_set: event_id blank on reconciled row",
            );
            return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
        }
        trimmed.to_string()
    };

    let token_type: u32 = row.token_type.try_into().map_err(|_| {
        tracing::warn!(
            pmp = %market_address.0,
            raw = row.token_type,
            "resolve_for_buy_full_set: token_type is negative",
        );
        anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
    })?;

    Ok(dodex_application::MarketForBuyFullSet { event_id, oracle_list_hash, token_type, status })
}

#[derive(Debug, sqlx::FromRow)]
struct CancelRow {
    event_id: String,
    oracle_list_hash: Option<String>,
    token_type: i32,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    resolved_at: Option<i64>,
    cancelled_at: Option<i64>,
    is_cancelled: bool,
    client_order_id: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct CancelBatchRow {
    // Chain `order_id` projected as text (numeric → text cast in the
    // SELECT) so the application boundary doesn't round-trip a
    // `numeric(78,0)` directly through `i64`. Parsed to u64 at
    // assembly time — values above u64 surface as anyhow rather than
    // truncation. The chain ABI is uint128 but the SDK ceiling caps
    // us at u64 (see `chain_sender.rs`).
    order_id: String,
    client_order_id: Option<String>,
    // Market identity + timing columns join in the same statement as
    // the order row, so chain identity (`event_id`, `oracle_list_hash`,
    // `token_type`) and `compute_status` both run against the snapshot
    // that produced this `live_orders` row — closing the race against
    // `resolve_for_new_order`'s separate MVCC view.
    event_id: String,
    oracle_list_hash: Option<String>,
    token_type: i32,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    resolved_at: Option<i64>,
    cancelled_at: Option<i64>,
    is_cancelled: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct DepthLevelRow {
    is_buy: bool,
    price: String,
    quantity: String,
}

#[derive(Debug, sqlx::FromRow)]
struct TradeRow {
    trade_id: String,
    // Raw contract integers as text: price in basis points, qty in token
    // atoms. Decoded to the display grid at render (see get_trades).
    price: String,
    qty: String,
    is_buyer_maker: bool,
    // Microseconds since the epoch, from chain_time. This is intentionally
    // non-Option: the read query's `chain_time IS NOT NULL` predicate is the
    // safety guard that keeps sqlx from decoding NULL into an i64.
    chain_time_us: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct OrderRow {
    decimals: i32,
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    /// Primary discriminator: `true` either drops a corrupt OPEN row
    /// (amount_remaining = 0 with status still OPEN) or, in the
    /// amount_initial = amount_remaining = 0 sentinel case, prevents
    /// the row from being mis-bucketed as NEW. NEW vs PARTIALLY_FILLED
    /// for surviving OPEN rows is then derived from `executed_qty`.
    fully_filled: bool,
    /// `amount_remaining > amount_initial` evaluated in SQL. A `true` here
    /// is a storage-invariant violation: more units claim to remain than
    /// were ever placed, so `executed_qty` (clamped to 0 in SQL) would
    /// otherwise render a corrupt row as a clean `NEW`. Fail closed.
    corrupt_remainder: bool,
    is_buy: bool,
    // Microseconds since the epoch — sourced from chain_created_at /
    // chain_updated_at (timestamptz). The API renders `time` / `updateTime`
    // in milliseconds by dividing by 1_000 at the boundary. These columns
    // are display-only; the cursor is built from placed_chain_order.
    chain_created_at_us: i64,
    chain_updated_at_us: i64,
    placed_chain_order: String,
    price_precision: i32,
    quantity_precision: i32,
    /// Raw status string from the live_orders table (e.g. "OPEN", "FILLED",
    /// "CANCELLED", "REJECTED"). The public `OrderStatus` is derived from
    /// this in combination with `fully_filled` (for OPEN rows).
    raw_status: String,
}

/// Build the SQL fragment that filters `live_orders` rows by the requested
/// status set. Returns `None` when every status is allowed — the caller
/// then emits no status predicate at all.
///
/// The fragment is composed from compile-time `const &str` literals chosen
/// by an exhaustive `match`; no user-supplied bytes ever reach the SQL.
fn build_status_predicate(filter: &OrderStatusFilter) -> Option<String> {
    let statuses = match filter {
        OrderStatusFilter::All => return None,
        OrderStatusFilter::Only(statuses) => statuses,
    };
    // Mirrors docs/tech-specs/read-api.md §Status mapping. BTreeSet
    // iteration is QueryableOrderStatus's Ord-derived enum-declaration
    // order, which is load-bearing for deterministic SQL composition.
    const NEW: &str = "(lo.status = 'OPEN' AND lo.amount_remaining = lo.amount_initial)";
    const PARTIALLY_FILLED: &str = "(lo.status = 'OPEN' AND lo.amount_remaining < lo.amount_initial AND lo.amount_remaining > 0)";
    const FILLED: &str = "lo.status = 'FILLED'";
    const CANCELED: &str = "lo.status = 'CANCELLED'";
    // Public status token. The disjunct selects no rows while the
    // `live_orders.status` CHECK constraint forbids `'REJECTED'`; the
    // query plan stays valid either way.
    const REJECTED: &str = "lo.status = 'REJECTED'";

    let disjuncts: Vec<&'static str> = statuses
        .iter()
        .copied()
        .map(|status| match status {
            QueryableOrderStatus::New => NEW,
            QueryableOrderStatus::PartiallyFilled => PARTIALLY_FILLED,
            QueryableOrderStatus::Filled => FILLED,
            QueryableOrderStatus::Canceled => CANCELED,
            QueryableOrderStatus::Rejected => REJECTED,
        })
        .collect();

    Some(disjuncts.join(" OR "))
}

fn filter_orderbook(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Scales a non-negative integer represented as a decimal string by 10^-scale,
/// returning a fixed-point DECIMAL string with `scale` digits after the point.
/// Used to render live_orders.price / amount_remaining (stored as raw contract
/// uint256/uint128 integers) as the human DECIMAL the API spec mandates.
pub(crate) fn scale_uint_to_decimal(raw: &str, scale: u32) -> String {
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

/// Project a `live_orders` row into the public [`Order`] DTO.
///
/// Returns `None` for rows that fail invariants the projector should
/// guarantee (OPEN with `amount_remaining = 0`, or an unrecognised
/// `raw_status`). The skip is logged inside this function; callers
/// just `filter_map(order_from_row)` to drop bad rows from the page.
/// "Skip" is encoded as `None` rather than a synthetic
/// `Err(Unexpected)` so the semantics live in the type and the caller
/// composes with `filter_map` directly instead of erasing a synthetic
/// error.
fn order_from_row(row: OrderRow) -> Option<Order> {
    if row.corrupt_remainder {
        error!(
            order_id = %row.order_id,
            market = %row.market_address,
            raw_status = %row.raw_status,
            "live_orders row has amount_remaining > amount_initial (storage invariant violated); skipping"
        );
        return None;
    }
    let price_scale = precision_to_scale(row.price_precision, "price", &row)?;
    let quantity_scale = precision_to_scale(row.quantity_precision, "quantity", &row)?;
    // live_orders mirrors chain units (price in basis points, amount in token
    // atoms); step each down to its display grid before formatting.
    // descale_pow10 renders an on-grid value exactly and rejects an off-grid
    // one. A negative drop (display precision finer than the chain scale) is
    // read-model misconfiguration. Either failure drops the row from the page
    // (None) with a logged reason, so a vanished order is never silent.
    let price_drop = match usize::try_from(i32::from(PRICE_BPS_DECIMALS) - row.price_precision) {
        Ok(d) => d,
        Err(_) => {
            error!(
                order_id = %row.order_id,
                market = %row.market_address,
                price_precision = row.price_precision,
                "price_precision exceeds the basis-point scale; skipping order row"
            );
            return None;
        }
    };
    let amount_drop = match usize::try_from(row.decimals - row.quantity_precision) {
        Ok(d) => d,
        Err(_) => {
            error!(
                order_id = %row.order_id,
                market = %row.market_address,
                quantity_precision = row.quantity_precision,
                decimals = row.decimals,
                "quantity_precision exceeds the quote-asset decimals; skipping order row"
            );
            return None;
        }
    };
    let descale = |raw: &str, k: usize, field: &str| -> Option<String> {
        match descale_pow10(raw, k) {
            Ok(grid) => Some(grid),
            Err(e) => {
                error!(
                    order_id = %row.order_id,
                    market = %row.market_address,
                    field,
                    reason = ?e,
                    "live_orders value cannot be descaled to the display grid; skipping order row"
                );
                None
            }
        }
    };
    let price_grid = descale(&row.price, price_drop, "price")?;
    let orig_qty_grid = descale(&row.orig_qty, amount_drop, "orig_qty")?;
    let executed_qty_grid = descale(&row.executed_qty, amount_drop, "executed_qty")?;

    // Derive the public OrderStatus from the stored raw_status and
    // (for OPEN rows) the SQL-side `fully_filled` boolean.
    let status = match row.raw_status.as_str() {
        "OPEN" => {
            // `fully_filled` (amount_remaining == 0) is checked before
            // `executed_is_zero` because amount_initial = amount_remaining = 0
            // satisfies both predicates: ordering the executed-zero branch
            // first would mis-bucket the row as NEW with origQty=0 and
            // never reach this guard. Per read-api.md §Field projection,
            // any OPEN row with amount_remaining == 0 is a projector bug
            // — fail closed at error! (storage-invariant violation,
            // per-occurrence is the right granularity for operator triage).
            if row.fully_filled {
                error!(
                    order_id = %row.order_id,
                    market = %row.market_address,
                    "live_orders row has status=OPEN with amount_remaining=0 (projector bug); skipping"
                );
                return None;
            }
            let executed_is_zero = match decimal_string_is_zero(&row.executed_qty) {
                Ok(value) => value,
                Err(err) => {
                    error!(
                        order_id = %row.order_id,
                        market = %row.market_address,
                        executed_qty = %row.executed_qty,
                        error = ?err,
                        "live_orders row has malformed executed quantity; skipping"
                    );
                    return None;
                }
            };
            if executed_is_zero {
                OrderStatus::New
            } else {
                OrderStatus::PartiallyFilled
            }
        }
        "FILLED" => OrderStatus::Filled,
        "CANCELLED" => OrderStatus::Canceled,
        // Reachable only when the `live_orders.status` CHECK constraint
        // admits `'REJECTED'`; the current schema forbids it, so this arm
        // is structurally unreachable until that migration ships.
        "REJECTED" => {
            if row.order_id != "0" {
                error!(
                    order_id = %row.order_id,
                    market = %row.market_address,
                    "REJECTED live_orders row has unexpected chain order_id; skipping"
                );
                return None;
            }
            OrderStatus::Rejected
        }
        other => {
            // Unknown raw_status: either schema drift the read path
            // hasn't caught up with, or read-model corruption. Either
            // way it's an invariant violation — error!, not warn!.
            error!(
                order_id = %row.order_id,
                market = %row.market_address,
                raw_status = %other,
                "live_orders row has unrecognised status; skipping"
            );
            return None;
        }
    };

    let identity = if status == OrderStatus::Rejected {
        OrderIdentity::Rejected
    } else {
        OrderIdentity::Chain(row.order_id)
    };

    let order = Order::new(
        MarketAddress(row.market_address.clone()),
        Symbol(row.symbol),
        identity,
        row.client_order_id,
        scale_uint_to_decimal(&price_grid, price_scale),
        scale_uint_to_decimal(&orig_qty_grid, quantity_scale),
        scale_uint_to_decimal(&executed_qty_grid, quantity_scale),
        status,
        TimeInForce::Gtc,
        OrderType::Limit,
        if row.is_buy { OrderSide::Buy } else { OrderSide::Sell },
        // The API contract is unix milliseconds; storage and cursor are at
        // microsecond precision. Truncating div is fine — sub-ms detail is
        // not exposed externally.
        row.chain_created_at_us / 1_000,
        row.chain_updated_at_us / 1_000,
    );

    match order {
        Ok(order) => Some(order),
        Err(err) => {
            error!(
                market = %row.market_address,
                error = ?err,
                "live_orders row violates Order DTO invariants; skipping"
            );
            None
        }
    }
}

/// Upper bound on the decimal-scale columns read off the model:
/// `market_outcomes.price_precision` / `quantity_precision` and
/// `ref_tokens.decimals`. Matches the SQL NUMERIC(38, …) cap — financial
/// decimal precision never reaches this in practice, but
/// `scale_uint_to_decimal` allocates `O(scale)` bytes per row via
/// `"0".repeat(...)`, so an unbounded value on a corrupt row would OOM the
/// API process on the first page that touches it. Neither column carries a
/// `CHECK` in 0001_initial.sql; this is the read-side defence.
const MAX_DECIMAL_PRECISION: u32 = 38;

/// Reason a decimal-scale value (`(price|quantity)_precision` or
/// `ref_tokens.decimals`) is unusable. Distinguishes the two corruption
/// modes so callers can log them with the same field names while keeping
/// their own surrounding context (skip-row vs MarketInconsistent).
pub(crate) enum InvalidScale {
    Negative,
    AboveMax,
}

pub(crate) fn validate_decimal_scale(raw: i32) -> Result<u32, InvalidScale> {
    let scale = u32::try_from(raw).map_err(|_| InvalidScale::Negative)?;
    if scale > MAX_DECIMAL_PRECISION {
        return Err(InvalidScale::AboveMax);
    }
    Ok(scale)
}

fn precision_to_scale(raw: i32, field: &str, row: &OrderRow) -> Option<u32> {
    match validate_decimal_scale(raw) {
        Ok(scale) => Some(scale),
        Err(InvalidScale::Negative) => {
            error!(
                order_id = %row.order_id,
                market = %row.market_address,
                precision_field = field,
                precision = raw,
                "live_orders row has negative decimal precision; skipping"
            );
            None
        }
        Err(InvalidScale::AboveMax) => {
            error!(
                order_id = %row.order_id,
                market = %row.market_address,
                precision_field = field,
                precision = raw,
                max = MAX_DECIMAL_PRECISION,
                "live_orders row has decimal precision above MAX_DECIMAL_PRECISION; skipping to prevent unbounded allocation in scale_uint_to_decimal"
            );
            None
        }
    }
}

impl PostgresReadModelRepository {
    async fn fetch_one(
        &self,
        market_address: &MarketAddress,
        now: i64,
    ) -> Result<MarketsPage, anyhow::Error> {
        let row: Option<MarketRow> = sqlx::query_as(&market_select_sql(
            "where m.last_reconciled_at is not null and m.pmp_address = $1",
            "limit 1",
        ))
        .bind(market_address.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("select single market")?;

        let Some(row) = row else {
            // Single-market lookup: an unknown / not-yet-reconciled address is
            // a client-side miss, not an empty listing. Surface it as a typed
            // domain error so the API handler maps it to 404, mirroring the
            // /api/v1/prediction/depth contract above.
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };

        let mut outcomes = self.fetch_outcomes(&[row.id]).await?;
        let market_outcomes = outcomes.remove(&row.id).unwrap_or_default();
        let mut oracle_blocks =
            self.fetch_oracle_events(std::slice::from_ref(&row.pmp_address)).await?;
        let oracle_block = oracle_blocks.remove(&row.pmp_address).unwrap_or_default();
        let market = assemble_market(row, market_outcomes, oracle_block, now)?;
        Ok(MarketsPage { markets: vec![market], next_cursor: None, has_more: false })
    }

    async fn fetch_listing(&self, listing: &MarketsListing) -> Result<MarketsPage, anyhow::Error> {
        let limit = listing.limit.max(1) as i64;
        let (sql, params) = build_listing_query(listing)?;
        let mut query = sqlx::query_as::<_, MarketRow>(&sql);
        for p in &params {
            query = match p {
                Param::BigInt(v) => query.bind(*v),
                Param::Text(v) => query.bind(v.clone()),
                Param::TextArray(v) => query.bind(v.clone()),
            };
        }
        let mut rows: Vec<MarketRow> =
            query.fetch_all(&self.pool).await.context("select markets listing")?;

        let has_more = rows.len() as i64 > limit;
        if has_more {
            rows.truncate(limit as usize);
        }

        let next_cursor = if has_more {
            rows.last().map(|row| {
                let sort_key = match listing.sort {
                    MarketsSort::ResultStartAsc => row.result_start.unwrap_or(i64::MAX),
                    MarketsSort::CreatedAtDesc => row.created_at_micros,
                };
                encode_cursor(sort_key, row.id)
            })
        } else {
            None
        };

        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let mut outcomes_by_market = self.fetch_outcomes(&ids).await?;
        let pmp_addresses: Vec<String> = rows.iter().map(|r| r.pmp_address.clone()).collect();
        let mut oracle_blocks = self.fetch_oracle_events(&pmp_addresses).await?;

        let mut markets = Vec::with_capacity(rows.len());
        for row in rows {
            let outcomes = outcomes_by_market.remove(&row.id).unwrap_or_default();
            let oracle_block = oracle_blocks.remove(&row.pmp_address).unwrap_or_default();
            markets.push(assemble_market(row, outcomes, oracle_block, listing.now)?);
        }

        Ok(MarketsPage { markets, next_cursor, has_more })
    }

    async fn fetch_outcomes(
        &self,
        market_ids: &[i64],
    ) -> Result<HashMap<i64, Vec<Outcome>>, anyhow::Error> {
        if market_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<OutcomeRow> = sqlx::query_as(
            r#"select market_id_fk,
                      outcome_id,
                      outcome_name,
                      symbol,
                      price_precision,
                      quantity_precision,
                      tick_size,
                      step_size,
                      min_notional
                 from market_outcomes
                where market_id_fk = any($1)
                order by market_id_fk, outcome_id"#,
        )
        .bind(market_ids)
        .fetch_all(&self.pool)
        .await
        .context("select market_outcomes for ids")?;

        let mut by_market: HashMap<i64, Vec<Outcome>> = HashMap::new();
        for r in rows {
            let outcome_id: u32 = r.outcome_id.try_into().map_err(|_| {
                tracing::warn!(
                    market_id_fk = r.market_id_fk,
                    raw = r.outcome_id,
                    "fetch_outcomes outcome_id is negative",
                );
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;
            let price_precision: u8 = r.price_precision.try_into().map_err(|_| {
                tracing::warn!(
                    market_id_fk = r.market_id_fk,
                    raw = r.price_precision,
                    "fetch_outcomes price_precision out of range",
                );
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;
            let quantity_precision: u8 = r.quantity_precision.try_into().map_err(|_| {
                tracing::warn!(
                    market_id_fk = r.market_id_fk,
                    raw = r.quantity_precision,
                    "fetch_outcomes quantity_precision out of range",
                );
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;
            by_market.entry(r.market_id_fk).or_default().push(Outcome {
                outcome_id,
                outcome_name: r.outcome_name,
                symbol: Symbol(r.symbol),
                price_precision,
                quantity_precision,
                tick_size: r.tick_size,
                step_size: r.step_size,
                min_notional: r.min_notional,
            });
        }
        Ok(by_market)
    }

    /// One round-trip per page that returns every confirmed `oracle_events`
    /// row for the listed markets, joined with `oracle_event_lists` and
    /// `oracles` for naming. Groups the rows in Rust and validates the
    /// hash-derived invariant `event_name == … and describe == …` across
    /// every row sharing the same `pmp_address` (a PMP can confirm against
    /// multiple `OracleEventList` contracts per
    /// `PMPDeployed.oracleEventLists: address[]`; `event_id =
    /// hash(eventName, description, deadline, outcomeNames)`, so the
    /// per-list metadata must match by construction). Returns
    /// `DomainError::MarketInconsistent` on mismatch so the API fails
    /// closed (HTTP 503) rather than picking an arbitrary row.
    async fn fetch_oracle_events(
        &self,
        pmp_addresses: &[String],
    ) -> Result<HashMap<String, OracleEventBlock>, anyhow::Error> {
        if pmp_addresses.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<OracleEventJoinRow> = sqlx::query_as(
            r#"select oe.confirmed_pmp_address              as pmp_address,
                      oe.event_name                         as event_name,
                      oe.describe                           as event_description,
                      o.name                                as oracle_name,
                      o.address                             as oracle_address,
                      oe.oracle_fee::text                   as oracle_fee
                 from oracle_events oe
                 left join oracle_event_lists oel on oel.id = oe.eventlist_id
                 left join oracles o on o.id = oel.oracle_id
                where oe.confirmed_pmp_address = any($1)
                order by oe.confirmed_pmp_address, oe.confirmed_at nulls last, oe.id"#,
        )
        .bind(pmp_addresses)
        .fetch_all(&self.pool)
        .await
        .context("select oracle_events for pmp addresses")?;

        aggregate_oracle_events(rows)
    }
}

/// Group `OracleEventJoinRow` rows by `pmp_address`, validating the
/// hash-derived invariant that `event_name`/`description` are equal across
/// every row sharing the same `pmp_address` (see `fetch_oracle_events`).
/// NULL values are tolerated as "not yet observed" (EventAdded lag against
/// EventConfirmed for one of the lists); only conflicting non-NULL values
/// trigger `MarketInconsistent`.
fn aggregate_oracle_events(
    rows: Vec<OracleEventJoinRow>,
) -> Result<HashMap<String, OracleEventBlock>, anyhow::Error> {
    let mut by_pmp: HashMap<String, OracleEventBlock> = HashMap::new();
    for row in rows {
        let block = by_pmp.entry(row.pmp_address.clone()).or_default();
        unify_optional(&mut block.event_name, row.event_name, "event_name", &row.pmp_address)?;
        unify_optional(
            &mut block.event_description,
            row.event_description,
            "description",
            &row.pmp_address,
        )?;
        block.oracles.push(OracleEntry {
            name: row.oracle_name,
            address: row.oracle_address,
            fee: row.oracle_fee,
        });
    }
    Ok(by_pmp)
}

/// Group Phase-2 rows under their Phase-1 oracle heads, preserving SQL order
/// (oracles by Phase-1 order; lists by `list_index`; events by `deadline`,
/// then `internal_id`). Fails closed (`MarketInconsistent`) on a non-renderable
/// `eventId` or malformed `outcome_names_jsonb`. The two-step
/// contains-key/insert/get_mut avoids holding a `&mut acc.lists` borrow across
/// the `acc.order.push`, which the borrow checker rejects.
fn assemble_oracles_page(
    heads: Vec<OracleHeadRow>,
    rows: Vec<OracleListEventRow>,
    next_cursor: Option<String>,
    has_more: bool,
) -> Result<OraclesPage, anyhow::Error> {
    // Per-oracle accumulator: `order` preserves first-seen list_index order,
    // `lists` holds the entries keyed by list_index.
    struct ListAcc {
        order: Vec<i64>,
        lists: HashMap<i64, OracleEventListEntry>,
    }
    let mut by_oracle: HashMap<i64, ListAcc> = HashMap::new();

    for row in rows {
        let event_id = numeric_to_hex(&row.event_id).map_err(|cause| {
            tracing::warn!(event_id = %row.event_id, %cause, "oracle event_id not renderable");
            anyhow!(DomainError::MarketInconsistent)
        })?;
        // Borrow eventlist_address for the ctx label before it is moved below.
        let outcomes = parse_oracle_outcomes(&row.outcome_names_jsonb, &row.eventlist_address)?;
        let event = OracleEventEntry {
            event_id,
            event_name: row.event_name,
            description: row.event_description,
            oracle_fee: OracleFee {
                asset: "SHELL".to_string(),
                amount: row.oracle_fee.unwrap_or_else(|| "0".to_string()),
            },
            deadline: row.deadline,
            trust_address: row.trust_addr,
            outcomes,
        };

        let list_index = row.list_index.unwrap_or(0);
        let acc = by_oracle
            .entry(row.oracle_id)
            .or_insert_with(|| ListAcc { order: Vec::new(), lists: HashMap::new() });
        if !acc.lists.contains_key(&list_index) {
            acc.order.push(list_index);
            acc.lists.insert(
                list_index,
                OracleEventListEntry {
                    index: list_index,
                    address: row.eventlist_address,
                    description: row.eventlist_description,
                    events: Vec::new(),
                },
            );
        }
        acc.lists.get_mut(&list_index).expect("list entry inserted above").events.push(event);
    }

    let oracles = heads
        .into_iter()
        .map(|h| {
            let event_lists = match by_oracle.remove(&h.id) {
                Some(mut acc) => acc.order.iter().filter_map(|idx| acc.lists.remove(idx)).collect(),
                None => Vec::new(),
            };
            OracleListing { name: h.name, address: h.address, event_lists }
        })
        .collect();

    Ok(OraclesPage { oracles, next_cursor, has_more })
}

fn unify_optional(
    slot: &mut Option<String>,
    incoming: Option<String>,
    field: &str,
    pmp_address: &str,
) -> Result<(), anyhow::Error> {
    let Some(value) = incoming else { return Ok(()) };
    match slot {
        Some(existing) if *existing != value => Err(anyhow!(DomainError::MarketInconsistent))
            .with_context(|| {
                format!(
                    "oracle_events.{field} disagrees across rows for pmp_address={pmp_address}: \
                     {existing:?} vs {value:?}"
                )
            }),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

// `m.is_cancelled` is the on-chain flag pulled by the reconciler via
// `PMP.getDetails().isCancelled`. The cancellation event projector stamps both
// `cancelled_at` and `is_cancelled`; the reconciler stamps `is_cancelled`
// (plus a discovery timestamp into `cancelled_at` when null) even if the
// cancellation event was missed or has not been replayed yet — surfacing
// either signal keeps the API consistent with the on-chain terminal state
// the spec requires for CANCELLED markets.
// PoolsFrozen gates the post-freeze branch. Per docs/tech-specs/read-api.md
// §Status derivation, RESOLVING and EXPIRED must not be reachable while
// frozen_at is null — that scenario is AWAITING_FREEZE indefinitely. Without
// this gate the SQL `?status=RESOLVING` filter would match rows whose
// Rust-derived `frozenAt` is still null, exposing a state the spec forbids.
const STATUS_CASE: &str = r#"case
        when m.cancelled_at is not null or m.is_cancelled then 'CANCELLED'
        when m.resolved_at is not null then 'RESOLVED'
        when m.stake_start is null then 'PENDING'
        when m.frozen_at is null and $1 >= m.stake_end then 'AWAITING_FREEZE'
        when m.frozen_at is null and $1 >= m.stake_start then 'STAKING'
        when m.frozen_at is null then 'UPCOMING'
        when $1 >= m.result_end then 'EXPIRED'
        when $1 >= m.result_start then 'RESOLVING'
        else 'TRADING'
      end"#;

fn market_select_sql(where_clause: &str, tail: &str) -> String {
    // Oracle/event fields are fetched in a separate batch (see
    // `fetch_oracle_events`) so a multi-oracle PMP (`PMPDeployed.oracleEventLists:
    // address[]`) does not duplicate the market row. Joining `oracle_events`
    // here would multiply each market by N (one per confirmed list), which
    // would inflate `has_more`/cursor and surface duplicates in the listing.
    format!(
        r#"select
               m.id                                          as id,
               m.pmp_address                                 as pmp_address,
               m.orderbook_address                           as orderbook_address,
               m.oracle_list_hash::text                      as oracle_list_hash,
               m.market_id                                   as market_name,
               m.token_type                                  as token_type,
               m.token_code                                  as token_code,
               m.event_id::text                              as event_id,
               m.stake_start                                 as stake_start,
               m.stake_end                                   as stake_end,
               m.result_start                                as result_start,
               m.result_end                                  as result_end,
               m.frozen_at                                   as frozen_at,
               m.resolved_at                                 as resolved_at,
               m.resolved_outcome_id                         as resolved_outcome_id,
               m.cancelled_at                                as cancelled_at,
               m.cancel_reason                               as cancel_reason,
               m.is_cancelled                                as is_cancelled,
               extract(epoch from m.created_at)::bigint                  as created_at_unix,
               (extract(epoch from m.created_at) * 1000000)::bigint      as created_at_micros
             from markets m
             {where_clause}
             {tail}"#
    )
}

#[derive(Debug)]
enum Param {
    BigInt(i64),
    Text(String),
    TextArray(Vec<String>),
}

fn build_listing_query(listing: &MarketsListing) -> Result<(String, Vec<Param>), anyhow::Error> {
    let limit = listing.limit.max(1) as i64;

    let mut where_parts: Vec<String> = vec!["m.last_reconciled_at is not null".to_string()];
    let mut params: Vec<Param> = Vec::new();

    if !listing.filter.statuses.is_empty() {
        // STATUS_CASE references $1 directly, so `now` must be the first bind here.
        params.push(Param::BigInt(listing.now));
        let status_strs: Vec<String> =
            listing.filter.statuses.iter().map(|s| s.as_str().to_string()).collect();
        params.push(Param::TextArray(status_strs));
        where_parts.push(format!("({STATUS_CASE}) = any(${})", params.len()));
    }
    if let Some(qa) = &listing.filter.quote_asset {
        params.push(Param::Text(qa.clone()));
        where_parts.push(format!("m.token_code = ${}", params.len()));
    }
    if let Some(name) = &listing.filter.oracle_name {
        // Multi-oracle markets carry N rows in `oracle_events`. Matching with
        // EXISTS keeps the listing one-row-per-market while still surfacing
        // the market if any of its oracles matches the filter.
        params.push(Param::Text(name.clone()));
        where_parts.push(format!(
            "exists (select 1 from oracle_events oe \
                       join oracle_event_lists oel on oel.id = oe.eventlist_id \
                       join oracles o on o.id = oel.oracle_id \
                      where oe.confirmed_pmp_address = m.pmp_address \
                        and o.name = ${})",
            params.len()
        ));
    }
    if let Some(closing_before) = listing.filter.closing_before {
        params.push(Param::BigInt(closing_before));
        where_parts.push(format!("m.result_end < ${}", params.len()));
    }
    if let Some(cursor) = &listing.cursor {
        let decoded = decode_cursor(cursor)?;
        params.push(Param::BigInt(decoded.sort_key_i64));
        let key_idx = params.len();
        params.push(Param::BigInt(decoded.id));
        let id_idx = params.len();
        match listing.sort {
            MarketsSort::ResultStartAsc => {
                where_parts.push(format!(
                    "(coalesce(m.result_start, 9223372036854775807), m.id) > (${key_idx}, ${id_idx})"
                ));
            }
            MarketsSort::CreatedAtDesc => {
                where_parts.push(format!(
                    "((extract(epoch from m.created_at) * 1000000)::bigint, m.id) < (${key_idx}, ${id_idx})"
                ));
            }
        }
    }

    let where_clause = format!("where {}", where_parts.join(" and "));
    let order_clause = match listing.sort {
        MarketsSort::ResultStartAsc => {
            "order by coalesce(m.result_start, 9223372036854775807) asc, m.id asc"
        }
        // Sort and cursor share the same microsecond bigint expression
        // (see `MarketRow::created_at_micros` and the keyset predicate
        // above). Earlier the cursor encoded whole seconds while ORDER BY
        // ran on raw `timestamptz`, so two markets created in the same
        // second could be skipped or duplicated across pages. `m.id desc`
        // is the tiebreaker for the (rare) collisions that survive
        // microsecond resolution.
        MarketsSort::CreatedAtDesc => {
            "order by (extract(epoch from m.created_at) * 1000000)::bigint desc, m.id desc"
        }
    };
    let limit_clause = format!("limit {}", limit + 1);

    let sql = market_select_sql(&where_clause, &format!("{order_clause} {limit_clause}"));
    Ok((sql, params))
}

fn assemble_market(
    row: MarketRow,
    outcomes: Vec<Outcome>,
    oracle_block: OracleEventBlock,
    now: i64,
) -> Result<Market, anyhow::Error> {
    let market_name = row.market_name.clone().ok_or_else(|| {
        anyhow!(
            "market {} has last_reconciled_at set but market_id (marketName) is NULL",
            row.pmp_address
        )
    })?;
    // The listing/single-market queries already filter
    // `m.last_reconciled_at IS NOT NULL`, and the migration-0014 CHECK
    // pins `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL`.
    // A NULL therefore cannot reach this point. A blank/whitespace-only
    // string slips past the CHECK but breaks the public contract that
    // visible markets always carry an order-book address — surface it as
    // `MarketInconsistent` (503), mirroring the depth handler's treatment
    // of the same corruption.
    let order_book_address = match row.orderbook_address.clone().and_then(filter_orderbook) {
        Some(addr) => addr,
        None => {
            return Err(anyhow!(DomainError::MarketInconsistent)).with_context(|| {
                format!("market {} has blank orderbook_address", row.pmp_address)
            });
        }
    };

    // `oracle_list_hash` is reconciler-only and has no CHECK constraint
    // pinning it post-reconcile (unlike `orderbook_address`). A
    // NULL/blank value here must NOT fail-close the read endpoints —
    // `/api/v1/prediction/markets` and `/api/v1/prediction/depth` do not surface this field
    // and would silently hide an otherwise-valid market. Paths that
    // DO depend on the value reject NULL/blank at their own repo
    // boundary, emitting `MarketInconsistent` only when the field is
    // actually needed:
    //   - trading: `resolve_for_new_order` and `resolve_for_cancel`
    //     lift the blank to `MarketInconsistent` before the use case
    //     ever sees the projection;
    //   - balances: `resolve_market_for_balances` rejects NULL/blank
    //     outright (the value feeds the off-chain `stake_hash`, so a
    //     missing one cannot be papered over).
    // So we render NULL as the empty string here and let those
    // strict consumers fail closed on demand instead of pre-emptively
    // hiding the listing.
    let oracle_list_hash = row
        .oracle_list_hash
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(numeric_to_hex)
        .transpose()?
        .unwrap_or_default();

    let status = derive_status(&row, now);
    let timings = build_timings(&row, status);
    let terminal = build_terminal(&row, status)?;
    // docs/tech-specs/read-api.md §Fail-closed validation — inconsistent rows
    // MUST fail the request closed. Validate the *built* DTO rather than the
    // raw row: a bad `cancel_reason` string would silently become
    // `cancelReason: null` via `CancelReason::parse`, and a non-PENDING status
    // with a NULL timing column becomes `timings: null` after `build_timings`
    // returns `None`. Both shapes violate the API contract.
    validate_invariants(status, &timings, &terminal).map_err(|err| anyhow!(err))?;
    let event = MarketEvent {
        event_id: numeric_to_hex(&row.event_id)?,
        event_name: oracle_block.event_name,
        description: oracle_block.event_description,
        oracles: oracle_block.oracles,
    };

    Ok(Market {
        market_address: MarketAddress(row.pmp_address),
        order_book_address,
        oracle_list_hash,
        market_name: MarketName(market_name),
        status,
        quote_asset: row.token_code,
        token_type: row.token_type,
        maker_commission: dodex_domain::MAKER_COMMISSION.to_string(),
        taker_commission: dodex_domain::TAKER_COMMISSION.to_string(),
        created_at: row.created_at_unix,
        timings,
        event,
        terminal,
        outcomes,
    })
}

fn derive_status(row: &MarketRow, now: i64) -> MarketStatus {
    compute_status(
        row.cancelled_at,
        row.is_cancelled,
        row.resolved_at,
        row.stake_start,
        row.stake_end,
        row.result_start,
        row.result_end,
        row.frozen_at,
        now,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_status(
    cancelled_at: Option<i64>,
    is_cancelled: bool,
    resolved_at: Option<i64>,
    stake_start: Option<i64>,
    stake_end: Option<i64>,
    result_start: Option<i64>,
    result_end: Option<i64>,
    frozen_at: Option<i64>,
    now: i64,
) -> MarketStatus {
    // Either signal is enough to flip the market terminal: `cancelled_at` is
    // set by the cancellation-event projector, `is_cancelled` is set by the
    // reconciler from `PMP.getDetails().isCancelled`. If the event was never
    // observed (or has not been replayed yet) the on-chain flag is still
    // authoritative, and the API spec requires the CANCELLED + terminal
    // response for cancelled markets.
    if cancelled_at.is_some() || is_cancelled {
        return MarketStatus::Cancelled;
    }
    if resolved_at.is_some() {
        return MarketStatus::Resolved;
    }
    let Some(stake_start) = stake_start else {
        return MarketStatus::Pending;
    };
    let stake_end = stake_end.unwrap_or(stake_start);
    let result_start = result_start.unwrap_or(stake_end);
    let result_end = result_end.unwrap_or(result_start);

    // PoolsFrozen gate: docs/tech-specs/read-api.md §Status derivation ties
    // RESOLVING (and by extension the post-result_end EXPIRED) to
    // `frozenAt != null`; unfrozen markets stay in AWAITING_FREEZE regardless
    // of how far past `stakeEnd` server time is. If freeze was never observed
    // we stay in the pre-freeze branch indefinitely; otherwise the listing
    // endpoint would return a market whose Rust-derived status disagrees with
    // its `frozenAt = null` timings and trips the API spec's status⇄timings
    // consistency contract.
    if frozen_at.is_none() {
        if now >= stake_end {
            return MarketStatus::AwaitingFreeze;
        } else if now >= stake_start {
            return MarketStatus::Staking;
        } else {
            return MarketStatus::Upcoming;
        }
    }

    // Spec (docs/api-spec.md §Market Status): EXPIRED applies once the market
    // has *reached* `resultEnd`, not strictly past it. Both this branch and
    // the SQL `STATUS_CASE` use `>=` so `?status=EXPIRED` filtering and
    // Rust-derived status agree on the boundary.
    if now >= result_end {
        MarketStatus::Expired
    } else if now >= result_start {
        MarketStatus::Resolving
    } else {
        MarketStatus::Trading
    }
}

/// Cross-checks the assembled DTO against the API/tech-specs invariants. Per
/// `docs/tech-specs/read-api.md §Fail-closed validation`, an inconsistent row
/// MUST be rejected rather than serialized. Called from `assemble_market`
/// after `derive_status`, `build_timings`, and `build_terminal` have run;
/// it validates the shapes they produce, not the raw `MarketRow`, because the
/// build helpers can silently elide invalid fields (e.g. `CancelReason::parse`
/// collapses an unknown string into `None`, and `build_timings` returns `None`
/// if any of the four timing columns is NULL).
fn validate_invariants(
    status: MarketStatus,
    timings: &Option<Timings>,
    terminal: &Option<Terminal>,
) -> Result<(), DomainError> {
    // api-spec.md §Market Status: "timings itself is null only for PENDING."
    // docs/tech-specs/read-api.md §Status derivation invariant: PENDING ⇒ timings == null.
    if matches!(status, MarketStatus::Pending) != timings.is_none() {
        return Err(DomainError::MarketInconsistent);
    }
    // api-spec.md:349: terminal is non-null iff status ∈ {RESOLVED, CANCELLED,
    // EXPIRED}. Anything else is a shape we promised never to serialize.
    let is_terminal_status =
        matches!(status, MarketStatus::Resolved | MarketStatus::Cancelled | MarketStatus::Expired);
    if is_terminal_status != terminal.is_some() {
        return Err(DomainError::MarketInconsistent);
    }

    match status {
        MarketStatus::Resolved => {
            // docs/tech-specs/read-api.md §Status derivation: RESOLVED ⇒ frozenAt != null.
            // api-spec.md §Terminal: `resolvedOutcomeId` is the whole
            // point of the terminal block — without it the client cannot know which side won.
            let t = timings.as_ref().expect("timings checked non-null above");
            let term = terminal.as_ref().expect("terminal checked non-null above");
            if t.frozen_at.is_none()
                || !matches!(term.kind, TerminalKind::Resolved)
                || term.resolved_outcome_id.is_none()
            {
                return Err(DomainError::MarketInconsistent);
            }
        }
        MarketStatus::Cancelled => {
            // docs/tech-specs/read-api.md §Fail-closed validation: cancelReason
            // MUST distinguish PMP_REJECTED_BY_ORACLE vs EVENT_CANCELLED. A NULL
            // on the row OR an unknown string both manifest here as
            // `cancel_reason.is_none()` after `build_terminal`'s `CancelReason::parse`.
            let term = terminal.as_ref().expect("terminal checked non-null above");
            if !matches!(term.kind, TerminalKind::Cancelled) || term.cancel_reason.is_none() {
                return Err(DomainError::MarketInconsistent);
            }
        }
        MarketStatus::Expired => {
            // EXPIRED is a time-driven terminal; the rest of the invariant
            // (timings present + correct kind) is covered by the cross-checks
            // above plus this `kind` match.
            let term = terminal.as_ref().expect("terminal checked non-null above");
            if !matches!(term.kind, TerminalKind::Expired) {
                return Err(DomainError::MarketInconsistent);
            }
        }
        MarketStatus::Trading | MarketStatus::Resolving => {
            // docs/tech-specs/read-api.md §Status derivation: TRADING and RESOLVING
            // imply frozenAt != null. `derive_status` already gates on this (see
            // the `row.frozen_at.is_none()` branch), but assert here so the
            // contract holds even if a future refactor of derive_status forgets the gate.
            let t = timings.as_ref().expect("timings checked non-null above");
            if t.frozen_at.is_none() {
                return Err(DomainError::MarketInconsistent);
            }
        }
        MarketStatus::Pending
        | MarketStatus::Upcoming
        | MarketStatus::Staking
        | MarketStatus::AwaitingFreeze => {}
    }

    Ok(())
}

fn build_timings(row: &MarketRow, status: MarketStatus) -> Option<Timings> {
    if matches!(status, MarketStatus::Pending) {
        return None;
    }
    let stake_start = row.stake_start?;
    let stake_end = row.stake_end?;
    let result_start = row.result_start?;
    let result_end = row.result_end?;
    Some(Timings { stake_start, stake_end, result_start, result_end, frozen_at: row.frozen_at })
}

fn build_terminal(row: &MarketRow, status: MarketStatus) -> Result<Option<Terminal>, DomainError> {
    match status {
        MarketStatus::Resolved => {
            let resolved_outcome_id =
                row.resolved_outcome_id.map(u32::try_from).transpose().map_err(|_| {
                    tracing::warn!(
                        raw = ?row.resolved_outcome_id,
                        "resolved_outcome_id is negative"
                    );
                    DomainError::MarketInconsistent
                })?;
            let at = match row.resolved_at {
                Some(at) => at,
                None => {
                    tracing::warn!(
                        pmp = %row.pmp_address,
                        kind = "RESOLVED",
                        "resolved_at is NULL on a RESOLVED row — read-model corruption",
                    );
                    return Err(DomainError::MarketInconsistent);
                }
            };
            Ok(Some(Terminal {
                kind: TerminalKind::Resolved,
                at,
                resolved_outcome_id,
                cancel_reason: None,
            }))
        }
        MarketStatus::Cancelled => {
            let cancel_reason = row.cancel_reason.as_deref().and_then(CancelReason::parse);
            let at = match row.cancelled_at {
                Some(at) => at,
                None => {
                    tracing::warn!(
                        pmp = %row.pmp_address,
                        kind = "CANCELLED",
                        "cancelled_at is NULL on a CANCELLED row — read-model corruption",
                    );
                    return Err(DomainError::MarketInconsistent);
                }
            };
            Ok(Some(Terminal {
                kind: TerminalKind::Cancelled,
                at,
                resolved_outcome_id: None,
                cancel_reason,
            }))
        }
        MarketStatus::Expired => {
            let at = match row.result_end {
                Some(at) => at,
                None => {
                    tracing::warn!(
                        pmp = %row.pmp_address,
                        kind = "EXPIRED",
                        "result_end is NULL on an EXPIRED row — read-model corruption",
                    );
                    return Err(DomainError::MarketInconsistent);
                }
            };
            Ok(Some(Terminal {
                kind: TerminalKind::Expired,
                at,
                resolved_outcome_id: None,
                cancel_reason: None,
            }))
        }
        _ => Ok(None),
    }
}

fn numeric_to_hex(decimal: &str) -> Result<String, anyhow::Error> {
    let big = BigUint::parse_bytes(decimal.as_bytes(), 10)
        .ok_or_else(|| anyhow!("invalid numeric: {decimal}"))?;
    Ok(format!("0x{:0>64}", big.to_str_radix(16)))
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedCursor {
    pub(crate) sort_key_i64: i64,
    pub(crate) id: i64,
}

pub(crate) fn encode_cursor(sort_key: i64, id: i64) -> String {
    let payload = format!("{sort_key}:{id}");
    URL_SAFE_NO_PAD.encode(payload)
}

pub(crate) fn decode_cursor(raw: &str) -> Result<DecodedCursor, anyhow::Error> {
    // Any failure here is the client's fault (the cursor came from a previous
    // response and they shouldn't be hand-crafting it). Wrap the typed
    // `DomainError::InvalidParameter` as the chain root so the API handler's
    // `downcast_ref::<DomainError>()` produces a 400; the human-readable
    // cause is preserved as a context layer for logs.
    decode_cursor_inner(raw).map_err(|cause| {
        anyhow::Error::from(DomainError::InvalidParameter).context(format!("cursor: {cause}"))
    })
}

fn decode_cursor_inner(raw: &str) -> Result<DecodedCursor, anyhow::Error> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).context("not valid base64")?;
    let s = std::str::from_utf8(&bytes).context("not utf-8")?;
    let (key, id) = s.split_once(':').context("missing separator")?;
    Ok(DecodedCursor {
        sort_key_i64: key.parse().context("sort_key not i64")?,
        id: id.parse().context("id not i64")?,
    })
}

/// Shared availability predicate for `/api/v1/oracles`, parameterised by the
/// 1-based placeholder positions each query uses for `now`, the optional
/// eventId (decimal), and the optional `deadlineBefore`. Both Phase-1 EXISTS
/// and Phase-2 fetch format this with their own positions so the rule has a
/// single source of truth.
fn oracle_event_availability(now: usize, event_id: usize, deadline_before: usize) -> String {
    format!(
        "oe.is_deleted = false \
         and oe.deadline > ${now} \
         and oe.meta_reconciled_at is not null \
         and (${event_id}::numeric is null or oe.internal_id_in_eventlist = ${event_id}::numeric) \
         and (${deadline_before}::bigint is null or oe.deadline < ${deadline_before})"
    )
}

/// Oracle pagination cursor: base64url of `"<id>:<name>"`. The id is written
/// first so the split stays unambiguous when an oracle name contains `:`.
fn encode_oracles_cursor(id: i64, name: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{id}:{name}"))
}

fn decode_oracles_cursor(raw: &str) -> Result<(i64, String), anyhow::Error> {
    decode_oracles_cursor_inner(raw).map_err(|cause| {
        anyhow::Error::from(DomainError::InvalidParameter).context(format!("cursor: {cause}"))
    })
}

fn decode_oracles_cursor_inner(raw: &str) -> Result<(i64, String), anyhow::Error> {
    let bytes = URL_SAFE_NO_PAD.decode(raw).context("not valid base64")?;
    let s = std::str::from_utf8(&bytes).context("not utf-8")?;
    let (id, name) = s.split_once(':').context("missing separator")?;
    Ok((id.parse().context("id not i64")?, name.to_string()))
}

/// Convert a client-supplied hex eventId to the decimal form stored in
/// `internal_id_in_eventlist`. Un-decodable hex is the client's fault → 400.
fn oracle_event_id_to_decimal(hex: &str) -> Result<String, anyhow::Error> {
    uint256_hex_to_decimal(hex).map_err(|cause| {
        anyhow::Error::from(DomainError::InvalidParameter).context(format!("eventId: {cause}"))
    })
}

/// Decode `outcome_names_jsonb` (`{"<outcomeId>": "<name>"}`) into a sorted
/// `Vec<OracleOutcome>`. A malformed blob fails closed (`MarketInconsistent`).
fn parse_oracle_outcomes(
    raw: &serde_json::Value,
    ctx: &str,
) -> Result<Vec<OracleOutcome>, anyhow::Error> {
    let obj = raw.as_object().ok_or_else(|| {
        tracing::warn!(ctx, "outcome_names_jsonb is not a JSON object");
        anyhow!(DomainError::MarketInconsistent)
    })?;
    let mut out = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let outcome_id: u32 = k.parse().map_err(|_| {
            tracing::warn!(ctx, key = %k, "outcome_names_jsonb key is not a u32");
            anyhow!(DomainError::MarketInconsistent)
        })?;
        let name = v.as_str().ok_or_else(|| {
            tracing::warn!(ctx, key = %k, "outcome_names_jsonb value is not a string");
            anyhow!(DomainError::MarketInconsistent)
        })?;
        out.push(OracleOutcome { outcome_id, outcome_name: name.to_string() });
    }
    out.sort_by_key(|o| o.outcome_id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use dodex_application::MarketsFilter;

    use super::*;

    fn row(
        stake_start: Option<i64>,
        stake_end: Option<i64>,
        result_start: Option<i64>,
        result_end: Option<i64>,
    ) -> MarketRow {
        MarketRow {
            id: 1,
            pmp_address: "0:pmp".into(),
            orderbook_address: None,
            oracle_list_hash: None,
            market_name: Some("PM".into()),
            token_type: 3,
            token_code: "USDC".into(),
            event_id: "1".into(),
            stake_start,
            stake_end,
            result_start,
            result_end,
            frozen_at: None,
            resolved_at: None,
            resolved_outcome_id: None,
            cancelled_at: None,
            cancel_reason: None,
            is_cancelled: false,
            created_at_unix: 0,
            created_at_micros: 0,
        }
    }

    #[test]
    fn pending_when_no_stake_start() {
        let r = row(None, None, None, None);
        assert_eq!(derive_status(&r, 100), MarketStatus::Pending);
    }

    #[test]
    fn upcoming_before_stake_start() {
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 100), MarketStatus::Upcoming);
    }

    #[test]
    fn staking_after_stake_start() {
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 250), MarketStatus::Staking);
    }

    #[test]
    fn awaiting_freeze_after_stake_end_no_frozen() {
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 350), MarketStatus::AwaitingFreeze);
    }

    #[test]
    fn trading_after_frozen() {
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.frozen_at = Some(310);
        assert_eq!(derive_status(&r, 350), MarketStatus::Trading);
    }

    #[test]
    fn resolving_after_result_start() {
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.frozen_at = Some(310);
        assert_eq!(derive_status(&r, 450), MarketStatus::Resolving);
    }

    #[test]
    fn expired_after_result_end() {
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.frozen_at = Some(310);
        assert_eq!(derive_status(&r, 600), MarketStatus::Expired);
    }

    #[test]
    fn expired_at_result_end_boundary() {
        // Spec: EXPIRED applies once the market reaches `resultEnd`.
        // `now == result_end` must flip to EXPIRED. This pins the
        // inclusive boundary against future regressions.
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.frozen_at = Some(310);
        assert_eq!(derive_status(&r, 500), MarketStatus::Expired);
    }

    #[test]
    fn resolving_just_before_result_end() {
        // Sanity check: one second before `resultEnd` is still RESOLVING.
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.frozen_at = Some(310);
        assert_eq!(derive_status(&r, 499), MarketStatus::Resolving);
    }

    #[test]
    fn awaiting_freeze_holds_past_result_start_without_freeze() {
        // docs/tech-specs/read-api.md §Status derivation: RESOLVING implies
        // frozenAt != null. With PoolsFrozen still unobserved the market must
        // stay AWAITING_FREEZE regardless of how far past result_start/result_end we are.
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 450), MarketStatus::AwaitingFreeze);
        assert_eq!(derive_status(&r, 600), MarketStatus::AwaitingFreeze);
    }

    #[test]
    fn resolved_overrides_timing() {
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.resolved_at = Some(450);
        assert_eq!(derive_status(&r, 250), MarketStatus::Resolved);
    }

    #[test]
    fn cancelled_overrides_resolved() {
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.resolved_at = Some(450);
        r.cancelled_at = Some(220);
        assert_eq!(derive_status(&r, 250), MarketStatus::Cancelled);
    }

    #[test]
    fn cancelled_status_from_reconciler_flag_only() {
        // Reconciler observes `isCancelled = true` from PMP.getDetails() but
        // the cancellation event hasn't materialised (or never will): the
        // API must still return CANCELLED so the response carries the
        // terminal state mandated by docs/api-spec.md §Terminal.
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.is_cancelled = true;
        assert_eq!(derive_status(&r, 250), MarketStatus::Cancelled);
    }

    #[test]
    fn cancelled_flag_outranks_resolved() {
        // Symmetric to `cancelled_overrides_resolved` but when the only
        // cancellation signal is the reconciler-set flag.
        let mut r = row(Some(200), Some(300), Some(400), Some(500));
        r.resolved_at = Some(450);
        r.is_cancelled = true;
        assert_eq!(derive_status(&r, 250), MarketStatus::Cancelled);
    }

    // ------------------------------------------------------------------
    // validate_invariants — pins the fail-closed contract from
    // docs/tech-specs/read-api.md §Fail-closed validation against the built DTO shape.
    // ------------------------------------------------------------------

    fn timings_full(frozen: Option<i64>) -> Timings {
        Timings {
            stake_start: 100,
            stake_end: 200,
            result_start: 300,
            result_end: 400,
            frozen_at: frozen,
        }
    }

    fn terminal_resolved(outcome: Option<u32>) -> Terminal {
        Terminal {
            kind: TerminalKind::Resolved,
            at: 350,
            resolved_outcome_id: outcome,
            cancel_reason: None,
        }
    }

    fn terminal_cancelled(reason: Option<CancelReason>) -> Terminal {
        Terminal {
            kind: TerminalKind::Cancelled,
            at: 150,
            resolved_outcome_id: None,
            cancel_reason: reason,
        }
    }

    #[test]
    fn validate_pending_with_timings_fails() {
        // api-spec.md:328 / invariant #3: timings must be null IFF PENDING.
        let err = validate_invariants(MarketStatus::Pending, &Some(timings_full(None)), &None)
            .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_non_pending_with_null_timings_fails() {
        // Catches the `build_timings -> None` path when a non-PENDING status
        // is paired with one NULL timing column, which would otherwise
        // surface a terminal/non-pending status with `timings: null`.
        let err = validate_invariants(MarketStatus::AwaitingFreeze, &None, &None).unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_resolved_without_outcome_id_fails() {
        // api-spec.md:391: `resolvedOutcomeId` MUST be set when kind=RESOLVED.
        // `build_terminal` just maps `Option<i32> -> Option<u32>`, so this
        // validator must fail closed before a Resolved terminal can carry
        // `resolvedOutcomeId: null`.
        let err = validate_invariants(
            MarketStatus::Resolved,
            &Some(timings_full(Some(250))),
            &Some(terminal_resolved(None)),
        )
        .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_resolved_without_freeze_fails() {
        // Mirrors the integration test `resolved_without_freeze_fails_closed`
        // — invariant #4. Pinned at unit level too because the validation now
        // lives here, not inline in `assemble_market`.
        let err = validate_invariants(
            MarketStatus::Resolved,
            &Some(timings_full(None)),
            &Some(terminal_resolved(Some(1))),
        )
        .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_cancelled_without_reason_fails() {
        // A CANCELLED row whose `cancel_reason` column is a string outside
        // `{PMP_REJECTED_BY_ORACLE, EVENT_CANCELLED}` is parsed to `None` by
        // `build_terminal::CancelReason::parse`. Validating the built DTO
        // catches the invalid terminal shape.
        let err = validate_invariants(
            MarketStatus::Cancelled,
            &Some(timings_full(Some(250))),
            &Some(terminal_cancelled(None)),
        )
        .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_terminal_status_without_terminal_block_fails() {
        let err = validate_invariants(MarketStatus::Expired, &Some(timings_full(Some(250))), &None)
            .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_live_status_with_terminal_block_fails() {
        // api-spec.md:349: terminal is null while market is alive.
        let err = validate_invariants(
            MarketStatus::Trading,
            &Some(timings_full(Some(250))),
            &Some(terminal_resolved(Some(1))),
        )
        .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_trading_without_freeze_fails() {
        // Defense in depth: derive_status already gates TRADING/RESOLVING on
        // frozen_at != null, but the validator asserts it independently so a
        // future change to derive_status cannot silently break the contract.
        let err = validate_invariants(MarketStatus::Trading, &Some(timings_full(None)), &None)
            .unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_consistent_shapes_pass() {
        // Sanity checks that the validator does not over-fire.
        validate_invariants(MarketStatus::Pending, &None, &None).unwrap();
        validate_invariants(MarketStatus::Staking, &Some(timings_full(None)), &None).unwrap();
        validate_invariants(MarketStatus::Trading, &Some(timings_full(Some(250))), &None).unwrap();
        validate_invariants(
            MarketStatus::Resolved,
            &Some(timings_full(Some(250))),
            &Some(terminal_resolved(Some(1))),
        )
        .unwrap();
        validate_invariants(
            MarketStatus::Cancelled,
            &Some(timings_full(Some(250))),
            &Some(terminal_cancelled(Some(CancelReason::PmpRejectedByOracle))),
        )
        .unwrap();
        validate_invariants(
            MarketStatus::Expired,
            &Some(timings_full(Some(250))),
            &Some(Terminal {
                kind: TerminalKind::Expired,
                at: 400,
                resolved_outcome_id: None,
                cancel_reason: None,
            }),
        )
        .unwrap();
    }

    #[test]
    fn status_case_sql_checks_is_cancelled() {
        // Regression: the SQL CASE that drives `status=…` filter pushdown
        // must mirror Rust's derive_status. If this drifts, listing endpoints
        // can return rows whose Rust-derived status fails the SQL filter.
        assert!(
            STATUS_CASE.contains("m.is_cancelled"),
            "STATUS_CASE must surface m.is_cancelled, got:\n{STATUS_CASE}"
        );
    }

    #[test]
    fn cursor_roundtrip() {
        let encoded = encode_cursor(1_710_000_000, 42);
        let decoded = decode_cursor(&encoded).unwrap();
        assert_eq!(decoded.sort_key_i64, 1_710_000_000);
        assert_eq!(decoded.id, 42);
    }

    #[test]
    fn numeric_to_hex_works() {
        assert_eq!(
            numeric_to_hex("0").unwrap(),
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            numeric_to_hex("255").unwrap(),
            "0x00000000000000000000000000000000000000000000000000000000000000ff"
        );
    }

    #[test]
    fn filter_orderbook_drops_blank() {
        assert!(filter_orderbook("".into()).is_none());
        assert!(filter_orderbook("   ".into()).is_none());
        assert_eq!(filter_orderbook("0:abc".into()).as_deref(), Some("0:abc"));
    }

    fn order_row(raw_status: &str, executed_qty: &str) -> OrderRow {
        OrderRow {
            decimals: 2,
            market_address: "0:market".into(),
            symbol: "YES".into(),
            order_id: "123".into(),
            client_order_id: "456".into(),
            price: "100".into(),
            orig_qty: "1000".into(),
            executed_qty: executed_qty.into(),
            fully_filled: false,
            corrupt_remainder: false,
            is_buy: true,
            chain_created_at_us: 1_700_000_000_000_000,
            chain_updated_at_us: 1_700_000_001_000_000,
            placed_chain_order: "5f80000000000000000000000000000123".into(),
            price_precision: 2,
            quantity_precision: 2,
            raw_status: raw_status.into(),
        }
    }

    #[test]
    fn order_from_row_renders_open_zero_executed_as_new() {
        // live_orders stores integer raw (atoms); executed_qty "0" → NEW,
        // rendered at quantity_precision. (decimals == quantity_precision in
        // the fixture, so the atom→token descale is a no-op here.)
        let row = order_row("OPEN", "0");
        let order = order_from_row(row).expect("zero executed must not be treated as malformed");
        assert_eq!(order.status().as_str(), "NEW");
        assert_eq!(order.executed_qty(), "0.00");
    }

    #[test]
    fn order_from_row_skips_rejected_rows_with_chain_order_id() {
        let row = order_row("REJECTED", "0");
        assert!(order_from_row(row).is_none(), "corrupt REJECTED chain id must be skipped");
    }

    #[test]
    fn order_from_row_skips_negative_precision() {
        let mut row = order_row("OPEN", "0");
        row.quantity_precision = -2;
        assert!(order_from_row(row).is_none(), "negative quantity precision must be corruption");
    }

    #[test]
    fn order_from_row_skips_negative_price_precision() {
        let mut row = order_row("OPEN", "0");
        row.price_precision = -1;
        row.quantity_precision = 2;
        assert!(order_from_row(row).is_none(), "negative price precision must be corruption");
    }

    /// `market_outcomes.*_precision` has no SQL `CHECK`, so an absurd
    /// positive value would slip past the DB into `scale_uint_to_decimal`,
    /// where `"0".repeat(scale)` allocates `O(scale)` bytes per row and
    /// can OOM the API process. The read-path guard rejects values above
    /// `MAX_DECIMAL_PRECISION` before the scaler is called.
    #[test]
    fn order_from_row_skips_precision_above_max() {
        let mut row = order_row("OPEN", "0");
        row.price_precision = i32::try_from(MAX_DECIMAL_PRECISION).unwrap() + 1;
        row.quantity_precision = 2;
        assert!(
            order_from_row(row).is_none(),
            "price_precision above MAX_DECIMAL_PRECISION must skip the row",
        );

        let mut row = order_row("OPEN", "0");
        row.price_precision = 3;
        row.quantity_precision = i32::MAX;
        assert!(order_from_row(row).is_none(), "quantity_precision at i32::MAX must skip the row",);
    }

    #[test]
    fn order_from_row_skips_corrupt_remainder_rows() {
        let mut row = order_row("OPEN", "0");
        row.corrupt_remainder = true;
        assert!(
            order_from_row(row).is_none(),
            "amount_remaining > amount_initial must not render as NEW"
        );
    }

    /// `amount_initial = amount_remaining = 0` satisfies both
    /// `executed_qty == 0` and `fully_filled == true`. The projector-bug
    /// guard must take precedence; otherwise the row surfaces as a
    /// valid NEW order with origQty=0 instead of being log-and-skipped
    /// per read-api.md §Field projection.
    #[test]
    fn order_from_row_skips_open_with_zero_initial_and_zero_remainder() {
        let mut row = order_row("OPEN", "0");
        row.orig_qty = "0".into();
        row.fully_filled = true;
        assert!(
            order_from_row(row).is_none(),
            "OPEN with amount_initial = amount_remaining = 0 must be projector-bug-skipped"
        );
    }

    #[test]
    fn order_from_row_accepts_rejected_sentinel_identity() {
        let mut row = order_row("REJECTED", "0");
        row.order_id = "0".into();
        let order = order_from_row(row).expect("REJECTED sentinel row should render");
        assert_eq!(order.status().as_str(), "REJECTED");
        assert_eq!(order.order_id(), "");
    }

    #[test]
    fn order_from_row_descales_production_precision() {
        // Production USDC config: decimals=6, quantity_precision=2 (amount drop
        // 4), price_precision=3 (price drop 1). Fixtures where
        // decimals == quantity_precision make the amount descale a no-op; this
        // one pins the real chain-units → display descale on both axes.
        let mut row = order_row("OPEN", "0");
        row.decimals = 6;
        row.price_precision = 3;
        row.quantity_precision = 2;
        row.price = "4880".into(); // 0.488 on the 10-bps tick grid
        row.orig_qty = "10000000".into(); // 10 USDC on the 10^4-atom lot grid
        let order = order_from_row(row).expect("on-grid production row must render");
        assert_eq!(order.price(), "0.488");
        assert_eq!(order.orig_qty(), "10.00");
        assert_eq!(order.status().as_str(), "NEW");
    }

    #[test]
    fn order_from_row_skips_price_precision_above_bps_scale() {
        // price_precision in (PRICE_BPS_DECIMALS, MAX] clears the earlier scale
        // guard but asks for finer price detail than the chain carries — a
        // negative drop, so the row is skipped.
        let mut row = order_row("OPEN", "0");
        row.price_precision = i32::from(PRICE_BPS_DECIMALS) + 1;
        row.quantity_precision = 2;
        assert!(
            order_from_row(row).is_none(),
            "price_precision finer than the basis-point scale must skip the row"
        );
    }

    #[test]
    fn order_from_row_skips_quantity_precision_above_decimals() {
        // quantity_precision finer than the quote asset's decimals asks for more
        // amount detail than the chain carries — a negative amount drop, the
        // amount-axis counterpart of an over-fine price_precision. Either is
        // read-model misconfiguration and skips the row.
        let mut row = order_row("OPEN", "0");
        row.decimals = 6;
        row.price_precision = 3;
        row.quantity_precision = 7; // > decimals → negative amount drop
        assert!(
            order_from_row(row).is_none(),
            "quantity_precision finer than the quote decimals must skip the row"
        );
    }

    #[test]
    fn order_from_row_skips_off_grid_chain_price() {
        // A raw price not on the tick grid (last bps digit nonzero) cannot be
        // rendered at price_precision=3 without rounding — fail closed.
        let mut row = order_row("OPEN", "0");
        row.decimals = 6;
        row.price_precision = 3;
        row.quantity_precision = 2;
        row.price = "4885".into(); // 4885 % TICK_SIZE(10) != 0
        row.orig_qty = "10000000".into();
        assert!(
            order_from_row(row).is_none(),
            "off-tick chain price must skip the row rather than round"
        );
    }

    #[test]
    fn order_from_row_skips_off_grid_executed_qty() {
        // A partially-filled OPEN row with on-grid price and orig_qty but an
        // off-lot executed_qty (last atom digit nonzero) must skip rather than
        // render a rounded fill — the fill amount is held to the same lot grid
        // as the placement amounts.
        let mut row = order_row("OPEN", "10000001"); // off lot: amount drop 4 strips a nonzero "1"
        row.decimals = 6;
        row.price_precision = 3;
        row.quantity_precision = 2;
        row.price = "4880".into(); // on-grid (drops one zero)
        row.orig_qty = "20000000".into(); // on-grid; > executed, so amount_remaining stays non-negative
        assert!(
            order_from_row(row).is_none(),
            "off-grid executed_qty on a partial fill must skip the row rather than round"
        );
    }

    #[test]
    fn order_from_row_renders_partial_fill_at_production_precision() {
        // Positive partial fill with a real amount drop (decimals=6,
        // quantity_precision=2 → drop 4): executed_qty must descale to its
        // display grid and surface as PARTIALLY_FILLED, not merely be checked
        // for off-grid rejection.
        let mut row = order_row("OPEN", "5000000"); // 5.00 filled
        row.decimals = 6;
        row.price_precision = 3;
        row.quantity_precision = 2;
        row.price = "4880".into(); // 0.488
        row.orig_qty = "20000000".into(); // 20.00 placed
        let order = order_from_row(row).expect("on-grid partial fill must render");
        assert_eq!(order.status().as_str(), "PARTIALLY_FILLED");
        assert_eq!(order.orig_qty(), "20.00");
        assert_eq!(order.executed_qty(), "5.00");
    }

    #[test]
    fn scale_uint_to_decimal_handles_all_magnitudes() {
        // scale = 0 is a passthrough.
        assert_eq!(scale_uint_to_decimal("100", 0), "100");
        assert_eq!(scale_uint_to_decimal("0", 0), "0");

        // raw shorter than the scale → zero-padded fractional, integer part
        // becomes "0".
        assert_eq!(scale_uint_to_decimal("0", 2), "0.00");
        assert_eq!(scale_uint_to_decimal("5", 2), "0.05");
        assert_eq!(scale_uint_to_decimal("50", 2), "0.50");

        // raw longer than the scale → split at len - scale.
        assert_eq!(scale_uint_to_decimal("614", 3), "0.614");
        assert_eq!(scale_uint_to_decimal("61400", 2), "614.00");
        assert_eq!(scale_uint_to_decimal("12345", 2), "123.45");
        assert_eq!(scale_uint_to_decimal("10000", 2), "100.00");

        // uint256-sized input keeps every digit, just inserts a decimal
        // point at len - scale.
        let big = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        let scaled = scale_uint_to_decimal(big, 18);
        assert_eq!(
            scaled,
            "115792089237316195423570985008687907853269984665640564039457.584007913129639935"
        );
    }

    fn placeholder_indices(sql: &str) -> std::collections::BTreeSet<usize> {
        let mut indices = std::collections::BTreeSet::new();
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let num: usize = std::str::from_utf8(&bytes[start..end]).unwrap().parse().unwrap();
                indices.insert(num);
                i = end;
            } else {
                i += 1;
            }
        }
        indices
    }

    fn listing(filter: MarketsFilter, cursor: Option<String>) -> MarketsListing {
        MarketsListing {
            filter,
            sort: MarketsSort::ResultStartAsc,
            cursor,
            limit: 50,
            now: 1_700_000_000,
        }
    }

    fn assert_placeholders_match_params(label: &str, listing: &MarketsListing) {
        let (sql, params) = build_listing_query(listing).expect(label);
        let used = placeholder_indices(&sql);
        let expected: std::collections::BTreeSet<usize> = (1..=params.len()).collect();
        assert_eq!(
            used, expected,
            "{label}: placeholders in SQL must equal {{1..=params.len()}}; sql=\n{sql}\nparams={params:?}"
        );
    }

    #[test]
    fn listing_query_default_has_no_params() {
        // The default listing query has no dynamic filters, so it must not
        // carry any bind parameters.
        let l = listing(MarketsFilter::default(), None);
        let (sql, params) = build_listing_query(&l).unwrap();
        assert!(params.is_empty(), "no filters → no binds; got {params:?}");
        assert!(placeholder_indices(&sql).is_empty(), "no filters → no $N in SQL");
    }

    #[test]
    fn listing_query_quote_asset_only() {
        let l = listing(
            MarketsFilter { quote_asset: Some("USDC".into()), ..MarketsFilter::default() },
            None,
        );
        assert_placeholders_match_params("quoteAsset only", &l);
    }

    #[test]
    fn listing_query_oracle_name_only() {
        let l = listing(
            MarketsFilter { oracle_name: Some("Oracle".into()), ..MarketsFilter::default() },
            None,
        );
        assert_placeholders_match_params("oracleName only", &l);
    }

    #[test]
    fn listing_query_closing_before_only() {
        let l = listing(
            MarketsFilter { closing_before: Some(1_700_000_000), ..MarketsFilter::default() },
            None,
        );
        assert_placeholders_match_params("closingBefore only", &l);
    }

    #[test]
    fn listing_query_status_only_uses_dollar_one_for_now() {
        let l = listing(
            MarketsFilter { statuses: vec![MarketStatus::Trading], ..MarketsFilter::default() },
            None,
        );
        let (sql, params) = build_listing_query(&l).unwrap();
        // STATUS_CASE hardcodes $1 for `now`; with status filter, $1 must be referenced.
        assert!(placeholder_indices(&sql).contains(&1), "STATUS_CASE must reference $1");
        assert_eq!(params.len(), 2, "expected [now, statuses]");
        assert_placeholders_match_params("status only", &l);
    }

    #[test]
    fn listing_query_cursor_only() {
        let l = listing(MarketsFilter::default(), Some(encode_cursor(1_700_000_000, 7)));
        assert_placeholders_match_params("cursor only", &l);
    }

    #[test]
    fn listing_query_created_at_desc_order_matches_cursor_key() {
        // Regression: ordering by `m.created_at desc` while comparing the
        // cursor against `extract(epoch ...)::bigint` (whole seconds) made
        // keyset pagination skip/duplicate rows that shared an epoch second.
        // Sort key and cursor key must be the same microsecond expression.
        let mut l = listing(MarketsFilter::default(), Some(encode_cursor(1_700_000_000, 7)));
        l.sort = MarketsSort::CreatedAtDesc;
        let (sql, _) = build_listing_query(&l).unwrap();
        assert!(
            sql.contains(
                "order by (extract(epoch from m.created_at) * 1000000)::bigint desc, m.id desc"
            ),
            "ORDER BY must use the microsecond bigint expression; sql=\n{sql}"
        );
        assert!(
            sql.contains("((extract(epoch from m.created_at) * 1000000)::bigint, m.id) <"),
            "cursor predicate must compare on the same microsecond expression; sql=\n{sql}"
        );
        assert!(
            !sql.contains("order by m.created_at"),
            "raw timestamptz ordering would re-introduce the keyset bug; sql=\n{sql}"
        );
        assert!(
            !sql.contains("order by extract(epoch from m.created_at)::bigint"),
            "seconds-only ordering would re-introduce the keyset bug; sql=\n{sql}"
        );
    }

    fn buy_full_set_row(event_id: &str, oracle_list_hash: Option<&str>) -> BuyFullSetRow {
        BuyFullSetRow {
            event_id: event_id.into(),
            oracle_list_hash: oracle_list_hash.map(str::to_string),
            token_type: 3,
            stake_start: None,
            stake_end: None,
            result_start: None,
            result_end: None,
            frozen_at: None,
            resolved_at: None,
            cancelled_at: None,
            is_cancelled: false,
        }
    }

    #[test]
    fn project_buy_full_set_row_rejects_blank_oracle_list_hash() {
        let row = buy_full_set_row("42", Some("   "));
        let err = project_buy_full_set_row(row, &MarketAddress("0:pmp".into()), 0).unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[test]
    fn project_buy_full_set_row_rejects_blank_event_id() {
        let row = buy_full_set_row("   ", Some("0xfeedface"));
        let err = project_buy_full_set_row(row, &MarketAddress("0:pmp".into()), 0).unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[test]
    fn listing_query_full_combo() {
        let mut l = listing(
            MarketsFilter {
                statuses: vec![MarketStatus::Trading, MarketStatus::Resolving],
                quote_asset: Some("USDC".into()),
                oracle_name: Some("Oracle".into()),
                closing_before: Some(1_700_000_000),
            },
            Some(encode_cursor(1_700_000_000, 7)),
        );
        l.sort = MarketsSort::CreatedAtDesc;
        assert_placeholders_match_params("full combo", &l);
    }

    #[test]
    fn oracles_cursor_roundtrip() {
        let c = encode_oracles_cursor(42, "Election:Oracle"); // name with a colon
        let (id, name) = decode_oracles_cursor(&c).expect("decode");
        assert_eq!(id, 42);
        assert_eq!(name, "Election:Oracle");
    }

    #[test]
    fn oracles_cursor_rejects_garbage() {
        let err = decode_oracles_cursor("!!!not-base64!!!").unwrap_err();
        assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidParameter)));
    }

    #[test]
    fn parse_oracle_outcomes_sorts_by_id() {
        let v = serde_json::json!({ "1": "YES", "0": "NO" });
        let out = parse_oracle_outcomes(&v, "test").expect("parse");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].outcome_id, 0);
        assert_eq!(out[0].outcome_name, "NO");
        assert_eq!(out[1].outcome_id, 1);
        assert_eq!(out[1].outcome_name, "YES");
    }

    #[test]
    fn parse_oracle_outcomes_rejects_non_object() {
        let v = serde_json::json!(["NO", "YES"]);
        let err = parse_oracle_outcomes(&v, "test").unwrap_err();
        assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));
    }

    #[test]
    fn parse_oracle_outcomes_rejects_non_u32_key() {
        let v = serde_json::json!({ "-1": "NO" });
        let err = parse_oracle_outcomes(&v, "test").unwrap_err();
        assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));
    }

    #[test]
    fn oracle_event_availability_uses_given_placeholders() {
        let frag = oracle_event_availability(1, 5, 6);
        assert!(frag.contains("oe.deadline > $1"));
        assert!(frag.contains("$5::numeric"));
        assert!(frag.contains("$6::bigint"));
        assert!(frag.contains("meta_reconciled_at is not null"));
        assert!(frag.contains("oe.is_deleted = false"));
    }
}

/// Postgres-backed repository for `ref_tokens` lookups. Kept as a
/// separate type from `PostgresReadModelRepository` because callers
/// on the balance path need only `lookup_ref_token` and pulling in
/// the full market-read surface would widen coupling unnecessarily.
#[derive(Clone)]
pub struct PostgresReferenceRepository {
    pool: PgPool,
}

impl PostgresReferenceRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl dodex_application::ReferenceRepository for PostgresReferenceRepository {
    async fn lookup_ref_token(
        &self,
        token_type: u32,
    ) -> Result<Option<dodex_application::RefToken>, anyhow::Error> {
        // The DB column is `integer` (i32 range). A u32 above i32::MAX
        // cannot exist in the table — chain ABI is uint32 but the DB column
        // is signed, so such a value is structurally impossible read-model
        // corruption. Lift to MarketInconsistent instead of collapsing to
        // `None` (which would be indistinguishable from an unknown row).
        let bind = i32::try_from(token_type).map_err(|_| {
            tracing::warn!(
                token_type,
                "ref_tokens lookup: token_type exceeds i32::MAX; structurally impossible value",
            );
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;
        let row: Option<(String, i32)> = sqlx::query_as(
            r#"select token_code, decimals
                 from ref_tokens
                where token_type = $1"#,
        )
        .bind(bind)
        .fetch_optional(&self.pool)
        .await
        .context("lookup_ref_token: select ref_tokens")?;
        row.map(|(token_code, decimals)| -> Result<_, anyhow::Error> {
            let decimals = u8::try_from(decimals).map_err(|_| {
                tracing::warn!(token_type, raw = decimals, "decimals is out of range for u8");
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;
            Ok(dodex_application::RefToken { token_type, token_code, decimals })
        })
        .transpose()
    }
}
