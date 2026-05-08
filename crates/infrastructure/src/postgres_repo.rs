// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_domain::CancelReason;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketName;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::Outcome;
use dodex_domain::PriceLevel;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TerminalKind;
use dodex_domain::Timings;
use num_bigint::BigUint;
use sqlx::PgPool;

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
    created_at_unix: i64,
    event_name: Option<String>,
    event_description: Option<String>,
    oracle_name: Option<String>,
    oracle_address: Option<String>,
    oracle_fee: Option<String>,
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
        // markets.orderbook_address is nullable (migration 0001) — decode it
        // as Option<String> so a NULL row does not surface as a sqlx decode
        // error. NULL and blank strings collapse into the same empty-book
        // path documented in services/api/README.md.
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
            // Market exists but its OrderBook has not been deployed yet
            // (reconciler ran before PoolsFrozen). No live orders possible.
            return Ok(DepthSnapshot {
                market_address: market_address.clone(),
                symbol: symbol.clone(),
                last_update_id: 0,
                bids: vec![],
                asks: vec![],
            });
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

        // Postgres returned each side already ordered (bids desc, asks asc)
        // and capped at `limit`. We just split into two vectors preserving
        // arrival order and scale below.
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

        let price_scale = u32::try_from(price_precision.max(0)).unwrap_or(0);
        let quantity_scale = u32::try_from(quantity_precision.max(0)).unwrap_or(0);
        for level in bids.iter_mut().chain(asks.iter_mut()) {
            level.price = scale_uint_to_decimal(&level.price, price_scale);
            level.quantity = scale_uint_to_decimal(&level.quantity, quantity_scale);
        }

        let last_update_id: Option<i64> = sqlx::query_scalar(
            "select coalesce(max(last_event_lt), 0) from live_orders where orderbook_address = $1",
        )
        .bind(&orderbook_address)
        .fetch_one(&self.pool)
        .await
        .context("compute last_update_id")?;

        Ok(DepthSnapshot {
            market_address: market_address.clone(),
            symbol: symbol.clone(),
            last_update_id: last_update_id.unwrap_or(0).max(0) as u64,
            bids,
            asks,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct DepthLevelRow {
    is_buy: bool,
    price: String,
    quantity: String,
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
            return Ok(MarketsPage { markets: vec![], next_cursor: None, has_more: false });
        };

        let mut outcomes = self.fetch_outcomes(&[row.id]).await?;
        let market_outcomes = outcomes.remove(&row.id).unwrap_or_default();
        let market = assemble_market(row, market_outcomes, now)?;
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
                    MarketsSort::CreatedAtDesc => row.created_at_unix,
                };
                encode_cursor(sort_key, row.id)
            })
        } else {
            None
        };

        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let mut outcomes_by_market = self.fetch_outcomes(&ids).await?;

        let mut markets = Vec::with_capacity(rows.len());
        for row in rows {
            let outcomes = outcomes_by_market.remove(&row.id).unwrap_or_default();
            markets.push(assemble_market(row, outcomes, listing.now)?);
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
}

// `m.is_cancelled` is the on-chain flag pulled by the reconciler via
// `PMP.getDetails().isCancelled`. The cancellation event projector stamps both
// `cancelled_at` and `is_cancelled`; the reconciler stamps `is_cancelled`
// (plus a discovery timestamp into `cancelled_at` when null) even if the
// cancellation event was missed or has not been replayed yet — surfacing
// either signal keeps the API consistent with the on-chain terminal state
// the spec requires for CANCELLED markets.
const STATUS_CASE: &str = r#"case
        when m.cancelled_at is not null or m.is_cancelled then 'CANCELLED'
        when m.resolved_at is not null then 'RESOLVED'
        when m.stake_start is null then 'PENDING'
        when $1 > m.result_end then 'EXPIRED'
        when $1 >= m.result_start then 'RESOLVING'
        when m.frozen_at is not null then 'TRADING'
        when $1 >= m.stake_end then 'AWAITING_FREEZE'
        when $1 >= m.stake_start then 'STAKING'
        else 'UPCOMING'
      end"#;

fn market_select_sql(where_clause: &str, tail: &str) -> String {
    format!(
        r#"select
               m.id                                          as id,
               m.pmp_address                                 as pmp_address,
               m.orderbook_address                           as orderbook_address,
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
               extract(epoch from m.created_at)::bigint      as created_at_unix,
               oe.event_name                                 as event_name,
               oe.describe                                   as event_description,
               o.name                                        as oracle_name,
               o.address                                     as oracle_address,
               oe.oracle_fee::text                           as oracle_fee
             from markets m
             left join oracle_events oe on oe.confirmed_pmp_address = m.pmp_address
             left join oracle_event_lists oel on oel.id = oe.eventlist_id
             left join oracles o on o.id = oel.oracle_id
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
        params.push(Param::Text(name.clone()));
        where_parts.push(format!("o.name = ${}", params.len()));
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
                    "(extract(epoch from m.created_at)::bigint, m.id) < (${key_idx}, ${id_idx})"
                ));
            }
        }
    }

    let where_clause = format!("where {}", where_parts.join(" and "));
    let order_clause = match listing.sort {
        MarketsSort::ResultStartAsc => {
            "order by coalesce(m.result_start, 9223372036854775807) asc, m.id asc"
        }
        MarketsSort::CreatedAtDesc => "order by m.created_at desc, m.id desc",
    };
    let limit_clause = format!("limit {}", limit + 1);

    let sql = market_select_sql(&where_clause, &format!("{order_clause} {limit_clause}"));
    Ok((sql, params))
}

fn assemble_market(
    row: MarketRow,
    outcomes: Vec<Outcome>,
    now: i64,
) -> Result<Market, anyhow::Error> {
    let market_name = row.market_name.clone().ok_or_else(|| {
        anyhow!(
            "market {} has last_reconciled_at set but market_id (marketName) is NULL",
            row.pmp_address
        )
    })?;
    // `orderbook_address` is allowed to be NULL: the reconciler can run
    // before the on-chain OrderBook contract is deployed, and the API
    // surfaces the gap as `orderBookAddress: null` rather than hiding the
    // market. `filter_orderbook` collapses blank/whitespace-only strings
    // into the same nullable contract.
    let order_book_address = row.orderbook_address.clone().and_then(filter_orderbook);

    let status = derive_status(&row, now);
    let timings = build_timings(&row, status);
    let terminal = build_terminal(&row, status, now);
    let event = MarketEvent {
        event_id: numeric_to_hex(&row.event_id)?,
        event_name: row.event_name,
        description: row.event_description,
        oracle_name: row.oracle_name,
        oracle_address: row.oracle_address,
        oracle_fee: row.oracle_fee,
    };

    Ok(Market {
        market_address: MarketAddress(row.pmp_address),
        order_book_address,
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
    // Either signal is enough to flip the market terminal: `cancelled_at` is
    // set by the cancellation-event projector, `is_cancelled` is set by the
    // reconciler from `PMP.getDetails().isCancelled`. If the event was never
    // observed (or has not been replayed yet) the on-chain flag is still
    // authoritative, and the API spec requires the CANCELLED + terminal
    // response for cancelled markets.
    if row.cancelled_at.is_some() || row.is_cancelled {
        return MarketStatus::Cancelled;
    }
    if row.resolved_at.is_some() {
        return MarketStatus::Resolved;
    }
    let Some(stake_start) = row.stake_start else {
        return MarketStatus::Pending;
    };
    let stake_end = row.stake_end.unwrap_or(stake_start);
    let result_start = row.result_start.unwrap_or(stake_end);
    let result_end = row.result_end.unwrap_or(result_start);

    if now > result_end {
        MarketStatus::Expired
    } else if now >= result_start {
        MarketStatus::Resolving
    } else if row.frozen_at.is_some() {
        MarketStatus::Trading
    } else if now >= stake_end {
        MarketStatus::AwaitingFreeze
    } else if now >= stake_start {
        MarketStatus::Staking
    } else {
        MarketStatus::Upcoming
    }
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
            event_name: None,
            event_description: None,
            oracle_name: None,
            oracle_address: None,
            oracle_fee: None,
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
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 450), MarketStatus::Resolving);
    }

    #[test]
    fn expired_after_result_end() {
        let r = row(Some(200), Some(300), Some(400), Some(500));
        assert_eq!(derive_status(&r, 600), MarketStatus::Expired);
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
