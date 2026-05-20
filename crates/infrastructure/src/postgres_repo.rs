// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_application::OrderForCancel;
use dodex_application::OrderStatusSet;
use dodex_application::OrdersCursor;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
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
use dodex_domain::Order;
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
use num_bigint::BigUint;
use sqlx::PgPool;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct PostgresReadModelRepository {
    pool: PgPool,
}

impl PostgresReadModelRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
    max_batch_size: i32,
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
        let target: Option<(Option<String>, i32, i32, i32)> = sqlx::query_as(
            r#"select m.orderbook_address,
                      mo.outcome_id,
                      mo.price_precision,
                      mo.quantity_precision
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
                where m.pmp_address = $1
                  and mo.symbol = $2
                  and m.last_reconciled_at is not null"#,
        )
        .bind(market_address.0.as_str())
        .bind(symbol.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("resolve orderbook_address from (marketAddress, symbol)")?;

        let Some((orderbook_address, outcome_id, price_precision, quantity_precision)) = target
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
        let mut bids: Vec<PriceLevel> = Vec::new();
        let mut asks: Vec<PriceLevel> = Vec::new();
        for row in rows {
            let level = PriceLevel { price: row.price, quantity: row.quantity };
            if row.is_buy {
                bids.push(level);
            } else {
                asks.push(level);
            }
        }
        bids.sort_by_cached_key(|l| {
            std::cmp::Reverse(BigUint::parse_bytes(l.price.as_bytes(), 10).unwrap_or_default())
        });
        asks.sort_by_cached_key(|l| {
            BigUint::parse_bytes(l.price.as_bytes(), 10).unwrap_or_default()
        });

        let price_scale = u32::try_from(price_precision.max(0)).unwrap_or(0);
        let quantity_scale = u32::try_from(quantity_precision.max(0)).unwrap_or(0);
        for level in bids.iter_mut().chain(asks.iter_mut()) {
            level.price = scale_uint_to_decimal(&level.price, price_scale);
            level.quantity = scale_uint_to_decimal(&level.quantity, quantity_scale);
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
                      mo.max_batch_size        as max_batch_size
                 from markets m
                 join market_outcomes mo on mo.market_id_fk = m.id
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

        // Log so ops can triage reconciler partial-writes; the empty
        // string then feeds the use case's `is_empty` invariant check.
        let oracle_list_hash = match row.oracle_list_hash {
            Some(raw) if !raw.trim().is_empty() => raw,
            other => {
                warn!(
                    pmp_address = %market_address.0,
                    null = other.is_none(),
                    "resolve_for_new_order: oracle_list_hash NULL/blank on reconciled row",
                );
                String::new()
            }
        };

        Ok(MarketForPlacement {
            event_id: row.event_id,
            oracle_list_hash,
            token_type: row.token_type,
            status,
            outcome: Outcome {
                outcome_id: row.outcome_id as u32,
                outcome_name: row.outcome_name,
                symbol: symbol.clone(),
                price_precision: row.price_precision as u8,
                quantity_precision: row.quantity_precision as u8,
                tick_size: row.tick_size,
                step_size: row.step_size,
                min_notional: row.min_notional,
                max_batch_size: row.max_batch_size as u16,
            },
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

        let oracle_list_hash = match row.oracle_list_hash {
            Some(raw) if !raw.trim().is_empty() => raw,
            other => {
                warn!(
                    pmp_address = %market_address.0,
                    null = other.is_none(),
                    "resolve_for_cancel: oracle_list_hash NULL/blank on reconciled row",
                );
                String::new()
            }
        };

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
            token_type: row.token_type,
            status,
            client_order_id,
        })
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
                .bind(filter.market_address.0.as_str())
                .bind(filter.symbol.0.as_str())
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

        let limit_plus_one = i64::from(query.limit) + 1;
        let status_sql = match build_status_predicate(&query.status) {
            Some(clause) => format!(" AND ({clause}) "),
            None => String::new(),
        };

        // The microsecond extraction `(extract(epoch from <timestamptz>) *
        // 1000000)::bigint` below was once cursor-load-bearing. It now
        // feeds response fields `time` / `updateTime` only — the cursor
        // is placed_chain_order (text). Deployment is pinned to PG15+
        // (Supabase) and PG16 (docker-compose.test.yml); both return
        // numeric from extract(epoch ...), so the bigint cast is exact.
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
                          lo.is_buy as is_buy,
                          (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
                          (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
                          lo.placed_chain_order as placed_chain_order,
                          mo.price_precision as price_precision,
                          mo.quantity_precision as quantity_precision,
                          lo.status as raw_status
                     from live_orders lo
                     join markets m on m.orderbook_address = lo.orderbook_address
                     join market_outcomes mo
                       on mo.market_id_fk = m.id
                      and mo.outcome_id = lo.outcome_id
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
                    .bind(query.cursor.as_ref().map(|c| c.0.as_str()))
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
                          lo.is_buy as is_buy,
                          (extract(epoch from lo.chain_created_at) * 1000000)::bigint as chain_created_at_us,
                          (extract(epoch from lo.chain_updated_at) * 1000000)::bigint as chain_updated_at_us,
                          lo.placed_chain_order as placed_chain_order,
                          mo.price_precision as price_precision,
                          mo.quantity_precision as quantity_precision,
                          lo.status as raw_status
                     from live_orders lo
                     join markets m on m.orderbook_address = lo.orderbook_address
                     join market_outcomes mo
                       on mo.market_id_fk = m.id
                      and mo.outcome_id = lo.outcome_id
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
                    .bind(query.cursor.as_ref().map(|c| c.0.as_str()))
                    .bind(limit_plus_one)
                    .fetch_all(&self.pool)
                    .await
                    .context("select all orders")?
            }
        };

        let limit = usize::from(query.limit);
        let has_more = rows.len() > limit;
        let mut orders_raw = rows;
        if has_more {
            orders_raw.truncate(limit);
        }

        let next_cursor = if has_more {
            orders_raw.last().map(|row| OrdersCursor(row.placed_chain_order.clone()))
        } else {
            None
        };

        // `order_from_row` returns `None` for projector-bug rows (logs a
        // warn! inside). `next_cursor` was captured above from the
        // pre-filter tail, so a corrupt boundary row advances the cursor
        // past itself instead of freezing pagination — pinned by
        // `cursor_advances_past_corrupt_row_at_page_tail` in
        // crates/infrastructure/tests/orders.rs.
        let orders = orders_raw.into_iter().filter_map(order_from_row).collect::<Vec<_>>();

        Ok(OrdersPage { orders, next_cursor })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct PlacementRow {
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
    max_batch_size: i32,
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
struct DepthLevelRow {
    is_buy: bool,
    price: String,
    quantity: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OrderRow {
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    /// `amount_remaining = 0` evaluated in SQL. Drives the OPEN-row
    /// status derivation without relying on `numeric::text` canonical
    /// formatting matching across `orig_qty` and `executed_qty`.
    fully_filled: bool,
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
fn build_status_predicate(set: &OrderStatusSet) -> Option<String> {
    if set.is_all() {
        return None;
    }
    // Mirrors docs/tech-specs/read-api.md §Status mapping. Iteration order
    // is the BTreeSet's Ord-derived enum-declaration order, which is
    // load-bearing for deterministic SQL composition; see the docstring
    // on dodex_domain::OrderStatus.
    const NEW: &str = "(lo.status = 'OPEN' AND lo.amount_remaining = lo.amount_initial)";
    const PARTIALLY_FILLED: &str = "(lo.status = 'OPEN' AND lo.amount_remaining < lo.amount_initial AND lo.amount_remaining > 0)";
    const FILLED: &str = "lo.status = 'FILLED'";
    const CANCELED: &str = "lo.status = 'CANCELLED'";
    const REJECTED: &str = "lo.status = 'REJECTED'";

    let disjuncts: Vec<&'static str> = set
        .canonical_vec()
        .into_iter()
        .map(|status| match status {
            OrderStatus::New => NEW,
            OrderStatus::PartiallyFilled => PARTIALLY_FILLED,
            OrderStatus::Filled => FILLED,
            OrderStatus::Canceled => CANCELED,
            OrderStatus::Rejected => REJECTED,
            OrderStatus::PendingNew | OrderStatus::PendingCancel => unreachable!(
                "OrderStatusSet::from_csv rejects PENDING_NEW and PENDING_CANCEL; reaching this arm means the application layer was bypassed"
            ),
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

/// Project a `live_orders` row into the public [`Order`] DTO.
///
/// Returns `None` for rows that fail invariants the projector should
/// guarantee (OPEN with `amount_remaining = 0`, or an unrecognised
/// `raw_status`). The skip is logged inside this function; callers
/// just `filter_map(order_from_row)` to drop bad rows from the page.
/// Encoding "skip" as `None` rather than a synthetic `Err(Unexpected)`
/// keeps the semantics in the type — the caller no longer has to
/// `.ok()` away a fake error to express the same intent.
fn order_from_row(row: OrderRow) -> Option<Order> {
    let quantity_scale = u32::try_from(row.quantity_precision.max(0)).unwrap_or(0);

    // Derive the public OrderStatus from the stored raw_status and
    // (for OPEN rows) the SQL-side `fully_filled` boolean.
    let status = match row.raw_status.as_str() {
        "OPEN" => {
            let executed_is_zero = decimal_string_is_zero(&row.executed_qty);
            if executed_is_zero {
                OrderStatus::New
            } else if row.fully_filled {
                // amount_remaining == 0 but status is still OPEN: projector bug.
                // Fail closed — do NOT surface as New or silently mis-bucket.
                warn!(
                    order_id = %row.order_id,
                    market = %row.market_address,
                    "live_orders row has status=OPEN with amount_remaining=0 (projector bug); skipping"
                );
                return None;
            } else {
                OrderStatus::PartiallyFilled
            }
        }
        "FILLED" => OrderStatus::Filled,
        "CANCELLED" => OrderStatus::Canceled,
        "REJECTED" => OrderStatus::Rejected,
        other => {
            warn!(
                order_id = %row.order_id,
                raw_status = %other,
                "live_orders row has unrecognised status; skipping"
            );
            return None;
        }
    };

    // REJECTED orders never receive a chain-assigned order_id.
    let order_id = if status == OrderStatus::Rejected { String::new() } else { row.order_id };

    Some(Order {
        market_address: MarketAddress(row.market_address),
        symbol: Symbol(row.symbol),
        order_id,
        client_order_id: row.client_order_id,
        price: scale_uint_to_decimal(
            &row.price,
            u32::try_from(row.price_precision.max(0)).unwrap_or(0),
        ),
        orig_qty: scale_uint_to_decimal(&row.orig_qty, quantity_scale),
        executed_qty: scale_uint_to_decimal(&row.executed_qty, quantity_scale),
        status,
        time_in_force: TimeInForce::Gtc,
        order_type: OrderType::Limit,
        side: if row.is_buy { OrderSide::Buy } else { OrderSide::Sell },
        // The API contract is unix milliseconds; storage and cursor are at
        // microsecond precision. Truncating div is fine — sub-ms detail is
        // not exposed externally.
        time: row.chain_created_at_us / 1_000,
        update_time: row.chain_updated_at_us / 1_000,
    })
}

fn decimal_string_is_zero(s: &str) -> bool {
    match BigUint::parse_bytes(s.as_bytes(), 10) {
        Some(v) => v == BigUint::from(0_u8),
        None => true,
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
            // /api/v1/depth contract above.
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
                      min_notional,
                      max_batch_size
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
            by_market.entry(r.market_id_fk).or_default().push(Outcome {
                outcome_id: r.outcome_id as u32,
                outcome_name: r.outcome_name,
                symbol: Symbol(r.symbol),
                price_precision: r.price_precision as u8,
                quantity_precision: r.quantity_precision as u8,
                tick_size: r.tick_size,
                step_size: r.step_size,
                min_notional: r.min_notional,
                max_batch_size: r.max_batch_size as u16,
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
// PoolsFrozen gates the post-freeze branch. Per docs/tech-spec.md invariant
// #2 ("status == RESOLVING implies frozenAt != null"), RESOLVING and EXPIRED
// must not be reachable while frozen_at is null — that scenario is
// AWAITING_FREEZE indefinitely (tech-spec.md:76). Without this gate the SQL
// `?status=RESOLVING` filter would match rows whose Rust-derived `frozenAt`
// is still null, exposing a state the spec forbids.
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
    // `/api/v1/markets` and `/api/v1/depth` do not surface this field
    // and would silently hide an otherwise-valid market. The trading
    // path needs the value populated and enforces that itself in the
    // application layer (`CreateOrderUseCase::execute`'s `is_empty`
    // check on the `MarketForPlacement` projection produced by
    // `resolve_for_new_order`), so we render NULL as the empty string
    // here and let the trading-side check emit `MarketInconsistent`
    // only when an order is being placed.
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
    let terminal = build_terminal(&row, status, now);
    // tech-spec.md:113 — invariant violations MUST fail the request closed.
    // Validate the *built* DTO rather than the raw row: `build_terminal` can
    // silently swallow a bad `cancel_reason` string via
    // `and_then(CancelReason::parse)` (turning it into `cancelReason: null`),
    // and a non-PENDING status with one NULL timing column lands as
    // `timings: null` after `build_timings` returns `None`. Both shapes
    // violate the API contract.
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

    // PoolsFrozen gate: tech-spec.md invariant #2 ties RESOLVING (and by
    // extension the post-result_end EXPIRED) to `frozenAt != null`, and
    // tech-spec.md:76 keeps unfrozen markets in AWAITING_FREEZE regardless
    // of how far past `stakeEnd` server time is. If freeze was never
    // observed we stay in the pre-freeze branch indefinitely; otherwise the
    // listing endpoint would return a market whose Rust-derived status
    // disagrees with its `frozenAt = null` timings and trips the API spec's
    // status⇄timings consistency contract.
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

/// Cross-checks the assembled DTO against the API/tech-spec invariants. Per
/// `docs/tech-spec.md:113`, an inconsistent row MUST be rejected rather than
/// serialized. Called from `assemble_market` after `derive_status`,
/// `build_timings`, and `build_terminal` have run; it validates the shapes
/// they produce, not the raw `MarketRow`, because the build helpers can
/// silently elide invalid fields (e.g. `CancelReason::parse` collapses an
/// unknown string into `None`, and `build_timings` returns `None` if any of
/// the four timing columns is NULL).
fn validate_invariants(
    status: MarketStatus,
    timings: &Option<Timings>,
    terminal: &Option<Terminal>,
) -> Result<(), DomainError> {
    // api-spec.md:328: "timings itself is null only for PENDING."
    // tech-spec.md:109 invariant #3: PENDING ⇒ timings == null.
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
            // tech-spec.md:110 invariant #4: RESOLVED ⇒ frozenAt != null.
            // api-spec.md:391: `resolvedOutcomeId` is the whole point of the
            // terminal block — without it the client cannot know which side won.
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
            // tech-spec.md:103: cancelReason MUST distinguish PMP_CANCELLED vs
            // EVENT_CANCELLED. A NULL on the row OR an unknown string on the
            // row both manifest here as `cancel_reason.is_none()` after
            // `build_terminal`'s `CancelReason::parse` filter.
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
            // tech-spec.md invariants #1, #2: these statuses imply
            // frozenAt != null. `derive_status` already gates on this (see
            // the `row.frozen_at.is_none()` branch), but assert here so the
            // contract holds even if a future refactor of derive_status
            // forgets the gate.
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

fn build_terminal(row: &MarketRow, status: MarketStatus, now: i64) -> Option<Terminal> {
    match status {
        MarketStatus::Resolved => Some(Terminal {
            kind: TerminalKind::Resolved,
            at: row.resolved_at.unwrap_or(now),
            resolved_outcome_id: row.resolved_outcome_id.map(|v| v as u32),
            cancel_reason: None,
        }),
        MarketStatus::Cancelled => Some(Terminal {
            kind: TerminalKind::Cancelled,
            at: row.cancelled_at.unwrap_or(now),
            resolved_outcome_id: None,
            cancel_reason: row.cancel_reason.as_deref().and_then(CancelReason::parse),
        }),
        MarketStatus::Expired => Some(Terminal {
            kind: TerminalKind::Expired,
            at: row.result_end.unwrap_or(now),
            resolved_outcome_id: None,
            cancel_reason: None,
        }),
        _ => None,
    }
}

fn numeric_to_hex(decimal: &str) -> Result<String, anyhow::Error> {
    let big = BigUint::parse_bytes(decimal.as_bytes(), 10)
        .ok_or_else(|| anyhow!("invalid numeric: {decimal}"))?;
    Ok(format!("0x{:0>64}", big.to_str_radix(16)))
}

#[derive(Debug, Clone)]
struct DecodedCursor {
    sort_key_i64: i64,
    id: i64,
}

fn encode_cursor(sort_key: i64, id: i64) -> String {
    let payload = format!("{sort_key}:{id}");
    URL_SAFE_NO_PAD.encode(payload)
}

fn decode_cursor(raw: &str) -> Result<DecodedCursor, anyhow::Error> {
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
        // Spec: EXPIRED applies once the market reaches `resultEnd`. `now == result_end`
        // must flip to EXPIRED; the previous `>` boundary kept it RESOLVING by one
        // tick. This pins the inclusive boundary against future regressions.
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
        // tech-spec.md invariant #2: RESOLVING implies frozenAt != null. With
        // PoolsFrozen still unobserved the market must stay AWAITING_FREEZE
        // regardless of how far past result_start/result_end we are.
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
    // docs/tech-spec.md:113 against the built DTO shape.
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
        // is paired with one NULL timing column. The reviewer's reported
        // shape: terminal/non-pending status surfacing with `timings: null`.
        let err = validate_invariants(MarketStatus::AwaitingFreeze, &None, &None).unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn validate_resolved_without_outcome_id_fails() {
        // api-spec.md:391: `resolvedOutcomeId` MUST be set when kind=RESOLVED.
        // `build_terminal` just maps `Option<i32> -> Option<u32>`, so a NULL
        // `resolved_outcome_id` row used to surface as a Resolved terminal
        // with `resolvedOutcomeId: null` — the client cannot tell who won.
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
        // The reviewer's new case: a CANCELLED row whose `cancel_reason`
        // column is a string outside `{PMP_CANCELLED, EVENT_CANCELLED}` is
        // parsed to `None` by `build_terminal::CancelReason::parse`. Looking
        // at `row.cancel_reason.is_none()` alone (the previous check) would
        // miss this — the row has a value, just an invalid one. Validating
        // the *built* DTO catches it.
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
            &Some(terminal_cancelled(Some(CancelReason::PmpCancelled))),
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
        // Regression: previously bound `now` as $1 even when STATUS_CASE was absent,
        // producing 08P01 "bind message supplies 1 parameters, but prepared statement requires 0".
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
}
