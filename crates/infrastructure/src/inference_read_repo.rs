// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Postgres read-model impl of `InferenceReadRepository` over
// `inference_markets` / `inference_orders`. Mirrors the prediction read path
// but with no `symbol` dimension and one book per market.

use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use dodex_application::InferenceMarketsListing;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceOrderRow;
use dodex_application::InferenceOrderStatus;
use dodex_application::InferenceOrdersPage;
use dodex_application::InferenceOrdersQuery;
use dodex_application::InferenceReadRepository;
use dodex_application::InferenceSide;
use dodex_domain::bps_to_decimal_string;
use dodex_domain::DomainError;
use dodex_domain::InferenceDepthSnapshot;
use dodex_domain::InferenceMarket;
use dodex_domain::InferenceMarketStatus;
use dodex_domain::InferenceMarketsPage;
use dodex_domain::InferenceModel;
use dodex_domain::PriceLevel;
use dodex_domain::INFERENCE_MAKER_REBATE_CAP_BPS;
use num_bigint::BigUint;
use tracing::debug;
use tracing::error;
use tracing::warn;

use crate::config::CAPTURE_FRESHNESS_SECS;
use crate::indexer_repo::CAPTURE_STREAM;
use crate::postgres_repo::decode_cursor;
use crate::postgres_repo::encode_cursor;
use crate::postgres_repo::scale_uint_to_decimal;
use crate::postgres_repo::validate_decimal_scale;
use crate::postgres_repo::InvalidScale;
use crate::postgres_repo::PostgresReadModelRepository;

#[derive(Debug, sqlx::FromRow)]
struct InferenceMarketRow {
    id: i64,
    orderbook_address: String,
    model_hash: Option<String>,
    model_ref: Option<String>,
    producer: Option<String>,
    model_name: Option<String>,
    version: Option<String>,
    platform_fee_bps: Option<i32>,
    price_precision: Option<i32>,
    quantity_precision: Option<i32>,
    tick_size: Option<String>,
    step_size: Option<String>,
    min_notional: Option<String>,
    reference_price: Option<String>,
    created_at: i64,
    created_at_micros: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct InferenceDepthLevelRow {
    is_buy: bool,
    price: String,
    quantity: String,
}

// Column list shared by the listing and single-market queries. `created_at`
// and `created_at_micros` are coalesced so a NULL `created_at_chain` decodes as
// non-null `i64` (0) and the SELECT is the single source of the coalesce used
// by ORDER BY / keyset / cursor — see the design doc §6.1 NULL handling.
const INFERENCE_MARKET_COLUMNS: &str = r#"
    id, orderbook_address, model_hash::text as model_hash, model_ref, producer,
    model_name, model_version as version, platform_fee_bps, price_precision, quantity_precision,
    tick_size, step_size, min_notional, reference_price::text as reference_price,
    coalesce(extract(epoch from created_at_chain)::bigint, 0) as created_at,
    coalesce((least(greatest(extract(epoch from created_at_chain), 0), 4102444800) * 1000000)::bigint, 0) as created_at_micros
"#;

impl PostgresReadModelRepository {
    async fn fetch_one_inference(
        &self,
        orderbook_address: &str,
    ) -> Result<InferenceMarketsPage, anyhow::Error> {
        let sql = format!(
            "select {INFERENCE_MARKET_COLUMNS} from inference_markets \
             where orderbook_address = $1 and last_reconciled_at is not null limit 1"
        );
        let row: Option<InferenceMarketRow> = sqlx::query_as(&sql)
            .bind(orderbook_address)
            .fetch_optional(self.pool())
            .await
            .context("select single inference market")?;
        let Some(row) = row else {
            return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
        };
        let market = assemble_inference_market(row)?;
        Ok(InferenceMarketsPage { markets: vec![market], next_cursor: None, has_more: false })
    }

    async fn fetch_listing_inference(
        &self,
        listing: &InferenceMarketsListing,
    ) -> Result<InferenceMarketsPage, anyhow::Error> {
        let limit = listing.limit.max(1) as i64;
        let (cursor_key, cursor_id) = match &listing.cursor {
            Some(c) => {
                let d = decode_cursor(c)?;
                (Some(d.sort_key_i64), Some(d.id))
            }
            None => (None, None),
        };

        // Static SQL with optional binds. `status` is TRADING-only today, so it
        // is not a SQL predicate (every visible row matches); the handler still
        // validates the query value. The keyset and ORDER BY share the coalesced
        // microsecond expression so a NULL `created_at_chain` row sorts last and
        // is never skipped or stranded across pages.
        let sql = format!(
            "select {INFERENCE_MARKET_COLUMNS} from inference_markets \
             where last_reconciled_at is not null \
               and ($1::text is null or producer = $1) \
               and ($2::bigint is null \
                    or (coalesce((least(greatest(extract(epoch from created_at_chain), 0), 4102444800) * 1000000)::bigint, 0), id) \
                       < ($2, $3)) \
             order by coalesce((least(greatest(extract(epoch from created_at_chain), 0), 4102444800) * 1000000)::bigint, 0) desc, \
                      id desc \
             limit $4"
        );
        let mut rows: Vec<InferenceMarketRow> = sqlx::query_as(&sql)
            .bind(listing.filter.producer.clone())
            .bind(cursor_key)
            .bind(cursor_id)
            .bind(limit + 1)
            .fetch_all(self.pool())
            .await
            .context("select inference markets listing")?;

        let has_more = rows.len() as i64 > limit;
        if has_more {
            rows.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            rows.last().map(|r| encode_cursor(r.created_at_micros, r.id))
        } else {
            None
        };

        let markets =
            rows.into_iter().map(assemble_inference_market).collect::<Result<Vec<_>, _>>()?;
        Ok(InferenceMarketsPage { markets, next_cursor, has_more })
    }
}

#[async_trait]
impl InferenceReadRepository for PostgresReadModelRepository {
    async fn list_inference_markets(
        &self,
        request: &InferenceMarketsRequest,
    ) -> Result<InferenceMarketsPage, anyhow::Error> {
        match request {
            InferenceMarketsRequest::One { orderbook_address } => {
                self.fetch_one_inference(orderbook_address).await
            }
            InferenceMarketsRequest::Listing(listing) => {
                self.fetch_listing_inference(listing).await
            }
        }
    }

    async fn get_inference_depth(
        &self,
        orderbook_address: &str,
        limit: u16,
    ) -> Result<InferenceDepthSnapshot, anyhow::Error> {
        get_inference_depth_impl(self, orderbook_address, limit).await
    }

    async fn list_inference_orders(
        &self,
        query: &InferenceOrdersQuery,
    ) -> Result<InferenceOrdersPage, anyhow::Error> {
        list_inference_orders_impl(self, query).await
    }
}

/// Validate a nullable precision column on a reconciled row. NULL or
/// out-of-range → `MarketInconsistent` (fail closed).
fn inference_scale(raw: Option<i32>, ob: &str, field: &str) -> Result<u32, anyhow::Error> {
    let raw = raw.ok_or_else(|| {
        warn!(orderbook = %ob, field, "inference precision column is null on a reconciled row");
        anyhow!(DomainError::MarketInconsistent)
    })?;
    validate_decimal_scale(raw).map_err(|reason| {
        warn!(
            orderbook = %ob,
            field,
            raw,
            reason = match reason {
                InvalidScale::Negative => "negative",
                InvalidScale::AboveMax => "above MAX_DECIMAL_PRECISION",
            },
            "inference precision out of range",
        );
        anyhow!(DomainError::MarketInconsistent)
    })
}

fn assemble_inference_market(row: InferenceMarketRow) -> Result<InferenceMarket, anyhow::Error> {
    let ob = row.orderbook_address;

    // Precision validated unconditionally — even on a dry book — because the
    // values are public DTO fields and are consumed by depth scaling.
    let price_scale = inference_scale(row.price_precision, &ob, "price_precision")?;
    let quantity_scale = inference_scale(row.quantity_precision, &ob, "quantity_precision")?;

    let fee_bps = row.platform_fee_bps.ok_or_else(|| {
        warn!(orderbook = %ob, "platform_fee_bps null on a reconciled inference market");
        anyhow!(DomainError::MarketInconsistent)
    })?;
    // `platform_fee_bps` is an unchecked `integer`; the buyer fee is always
    // non-negative per api-spec, so a negative raw value is corruption — fail
    // closed rather than serve a negative `takerCommission`. (The maker side is
    // intentionally negative: it is the rebate cap, not this column.)
    if fee_bps < 0 {
        warn!(orderbook = %ob, fee_bps, "platform_fee_bps is negative");
        return Err(anyhow!(DomainError::MarketInconsistent));
    }
    let taker_commission = bps_to_decimal_string(fee_bps);
    let maker_commission = bps_to_decimal_string(-(INFERENCE_MAKER_REBATE_CAP_BPS as i32));

    let model_ref = row
        .model_ref
        .filter(|s| !s.is_empty())
        .or_else(|| row.model_hash.filter(|s| !s.is_empty()))
        .ok_or_else(|| {
            warn!(orderbook = %ob, "inference market has neither model_ref nor model_hash");
            anyhow!(DomainError::MarketInconsistent)
        })?;

    let reference_price = match row.reference_price {
        None => None,
        Some(raw) => {
            if BigUint::parse_bytes(raw.as_bytes(), 10).is_none() {
                warn!(orderbook = %ob, raw = %raw, "inference reference_price is not a non-negative integer");
                return Err(anyhow!(DomainError::MarketInconsistent));
            }
            Some(scale_uint_to_decimal(&raw, price_scale))
        }
    };

    let inconsistent = |field: &str| {
        warn!(orderbook = %ob, field, "inference trading-rule column null on a reconciled row");
        anyhow!(DomainError::MarketInconsistent)
    };
    let tick_size = row.tick_size.ok_or_else(|| inconsistent("tick_size"))?;
    let step_size = row.step_size.ok_or_else(|| inconsistent("step_size"))?;
    let min_notional = row.min_notional.ok_or_else(|| inconsistent("min_notional"))?;

    Ok(InferenceMarket {
        orderbook_address: ob,
        model: InferenceModel {
            producer: row.producer,
            name: row.model_name,
            version: row.version,
            model_ref,
        },
        status: InferenceMarketStatus::Trading,
        quote_asset: "SHELL".to_string(),
        maker_commission,
        taker_commission,
        price_precision: price_scale as u8,
        quantity_precision: quantity_scale as u8,
        tick_size,
        step_size,
        min_notional,
        reference_price,
        created_at: row.created_at,
    })
}

async fn get_inference_depth_impl(
    repo: &PostgresReadModelRepository,
    orderbook_address: &str,
    limit: u16,
) -> Result<InferenceDepthSnapshot, anyhow::Error> {
    // Resolve + visibility gate. A missing row is a client miss (-1121); a
    // reconciled row with NULL precision is corruption (-1500 via inference_scale).
    let precisions: Option<(Option<i32>, Option<i32>)> = sqlx::query_as(
        "select price_precision, quantity_precision from inference_markets \
         where orderbook_address = $1 and last_reconciled_at is not null",
    )
    .bind(orderbook_address)
    .fetch_optional(repo.pool())
    .await
    .context("resolve inference market for depth")?;
    let Some((price_precision, quantity_precision)) = precisions else {
        return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
    };
    let price_scale = inference_scale(price_precision, orderbook_address, "price_precision")?;
    let quantity_scale =
        inference_scale(quantity_precision, orderbook_address, "quantity_precision")?;

    let limit = limit.max(1) as i64;
    let rows: Vec<InferenceDepthLevelRow> = sqlx::query_as(
        r#"(select true  as is_buy, price::text as price,
                   sum(amount_remaining)::text as quantity
              from inference_orders
             where orderbook_address = $1 and status = 'OPEN' and amount_remaining > 0 and is_buy
             group by price order by price desc limit $2)
           union all
           (select false as is_buy, price::text as price,
                   sum(amount_remaining)::text as quantity
              from inference_orders
             where orderbook_address = $1 and status = 'OPEN' and amount_remaining > 0 and not is_buy
             group by price order by price asc limit $2)"#,
    )
    .bind(orderbook_address)
    .bind(limit)
    .fetch_all(repo.pool())
    .await
    .context("aggregate inference_orders for depth")?;

    // Re-sort each side by exact numeric value (UNION ALL drops inner ordering).
    // A non-numeric raw price is read-model corruption — fail closed.
    let mut bids: Vec<(BigUint, PriceLevel)> = Vec::new();
    let mut asks: Vec<(BigUint, PriceLevel)> = Vec::new();
    for row in rows {
        let key = BigUint::parse_bytes(row.price.as_bytes(), 10).ok_or_else(|| {
            warn!(orderbook = %orderbook_address, raw = %row.price, "inference_orders.price is not a non-negative integer");
            anyhow!(DomainError::MarketInconsistent)
        })?;
        let level = PriceLevel {
            price: scale_uint_to_decimal(&row.price, price_scale),
            quantity: scale_uint_to_decimal(&row.quantity, quantity_scale),
        };
        if row.is_buy {
            bids.push((key, level));
        } else {
            asks.push((key, level));
        }
    }
    bids.sort_by(|a, b| b.0.cmp(&a.0));
    asks.sort_by(|a, b| a.0.cmp(&b.0));
    let bids = bids.into_iter().map(|(_, l)| l).collect();
    let asks = asks.into_iter().map(|(_, l)| l).collect();

    let last_update_id: Option<String> = sqlx::query_scalar(
        "select max(last_chain_order) from inference_orders where orderbook_address = $1",
    )
    .bind(orderbook_address)
    .fetch_one(repo.pool())
    .await
    .context("compute inference last_update_id")?;

    Ok(InferenceDepthSnapshot {
        orderbook_address: orderbook_address.to_string(),
        last_update_id: last_update_id.unwrap_or_default(),
        bids,
        asks,
    })
}

/// One row of the snapshot statement.
///
/// EVERY `page.*` column is `Option`: a valid query that matches no order still returns
/// exactly one row — the `mkt × gate × wm` constants left-joined against an empty page —
/// and Postgres fills the page columns with NULL. sqlx decodes the row before any
/// application-level filter runs, so a non-`Option` field here would turn the endpoint's
/// central acceptance case ("this TokenContract has no live SELL") into an internal error.
#[derive(sqlx::FromRow)]
struct InferenceOrderQueryRow {
    order_id: Option<String>,
    token_contract: Option<String>,
    note_address: Option<String>,
    is_buy: Option<bool>,
    price: Option<String>,
    amount_remaining: Option<String>,
    amount_initial: Option<String>,
    deadline: Option<String>,
    status: Option<String>,
    /// Unix seconds, extracted in SQL: the application layer has no chrono dependency and
    /// this API returns unix seconds throughout.
    created_at_unix: Option<i64>,
    updated_at_unix: Option<i64>,
    // Repeated on every row: the statement is one snapshot, so these are constant.
    last_update_id: Option<String>,
    /// True when this book's state cannot support a claim of absence: a resting SELL with
    /// an unknown TokenContract, pending unprojected events for the book, or capture
    /// behind the chain head.
    // The gate's three arms are kept apart all the way out of SQL. They collapse to one
    // `-1500` on the wire, but they are three different events for an operator: a violated
    // invariant, a routine projection backlog, and a wedged capture loop.
    tc_unknown: bool,
    unprojected: bool,
    capture_stale: bool,
    price_precision: Option<i32>,
    quantity_precision: Option<i32>,
}

/// Which sides the page may contain.
///
/// A `tokenContract` filter admits SELL branches only: `token_contract` is non-null
/// exclusively on SELL rows, so a BUY branch is provably empty — but not *cheap*.
/// `inference_orders_book_tc_idx` is keyed `(orderbook_address, token_contract, status,
/// order_id)` with no `is_buy`, so Postgres would walk that TC's entire status group
/// before the residual discarded every row and the `LIMIT` never filled. On a public
/// endpoint that is an unbounded scan for an empty answer.
fn resolve_sides(query: &InferenceOrdersQuery) -> Vec<bool> {
    match (query.token_contract.is_some(), query.side) {
        (true, Some(InferenceSide::Buy)) => Vec::new(),
        (true, _) => vec![false],
        (false, Some(side)) => vec![side.is_buy()],
        (false, None) => vec![true, false],
    }
}

/// Columns every page branch projects, in the order `InferenceOrderQueryRow` reads.
const PAGE_COLUMNS: &str = "order_id, token_contract, note_address, is_buy, price, \
                            amount_remaining, amount_initial, deadline, status, \
                            chain_created_at, chain_updated_at";

/// One statement: visibility gate, fail-closed gate, watermark and page share an MVCC
/// snapshot. The page is a UNION ALL of branches that each pin `is_buy` and `status`, so
/// every branch rides an index in `order_id` order and the outer merge sorts at most
/// `branches × fetch` rows. Built with `QueryBuilder` because the branch count varies with
/// the filters, and hand-numbered `$n` placeholders are how such a query acquires an
/// off-by-one.
fn build_snapshot_query<'a>(
    q: &'a InferenceOrdersQuery,
    fetch: i64,
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let ob = &q.orderbook_address;
    let mut b = sqlx::QueryBuilder::new(
        "with mkt as (select price_precision, quantity_precision from inference_markets \
         where orderbook_address = ",
    );
    b.push_bind(ob);
    // The fail-closed gate. Three ways this book's state cannot vouch for an absence:
    //
    //  1. a resting SELL whose TokenContract the indexer does not know;
    //  2. captured events for this book that are decoded but not yet projected — the very
    //     placement being asked about may sit there, so no row exists to be found;
    //  3. capture is behind the chain head, so the placement may not even be captured.
    //
    // (2) and (3) echo the reconciler's own gates: if it will not trust this view enough to
    // sweep, a reader must not assert absence from it. But (2) is deliberately WIDER than
    // the reconciler's `pending_events_exist`, which adds `event_type is not null and
    // decoded is not null` because an undecodable row would wedge its sweep forever. A
    // reader asking "is my view complete?" must count exactly those rows. That is why this
    // arm rides `raw_events_unprocessed_src_idx` and not `raw_events_pending_src_idx`,
    // whose narrower predicate the bare `processed_at is null` clause does not imply, so
    // Postgres could not use it. (3) is a primary-key lookup on `indexer_cursors`.
    b.push(
        " and last_reconciled_at is not null), gate as (select exists(\
             select 1 from inference_orders where orderbook_address = ",
    );
    b.push_bind(ob);
    b.push(
        " and is_buy = false and status = 'OPEN' and token_contract is null) as tc_unknown, \
             exists(select 1 from raw_events where src_address = ",
    );
    b.push_bind(ob);
    // Any unprojected message under this book's address. Every event an InferenceOrderBook
    // emits is in the ABI this indexer loads, so such a row — pending, undecodable,
    // bodyless, or an id we do not know — can only mean our view of this book is
    // incomplete. Scoping by src is what makes the predicate safe: a benign unknown id
    // from another contract cannot reach it.
    //
    // The third arm needs `updated_at`, not `at_head` alone. `at_head` records that the
    // LAST poll saw no next page; it says nothing about the chain since. A placement that
    // landed after that poll leaves the flag true while neither raw_events nor
    // inference_orders knows about it. Requiring a recent poll bounds the blind spot to one
    // capture interval, which is the best a read model can promise without reading the
    // chain itself — and the caller can still compare `lastUpdateId` across calls.
    b.push(
        " and processed_at is null) as unprojected, \
          not coalesce((select at_head and updated_at > now() - make_interval(secs => ",
    );
    b.push_bind(CAPTURE_FRESHNESS_SECS);
    b.push(") from indexer_cursors where stream_name = ");
    // `crate::indexer_repo::CAPTURE_STREAM` — the one cursor row the capture loop maintains.
    b.push_bind(CAPTURE_STREAM);
    b.push(
        "), false) as capture_stale), \
         wm as (select max(last_chain_order) as last_update_id from inference_orders \
         where orderbook_address = ",
    );
    b.push_bind(ob);
    b.push("), page as (");

    let sides = resolve_sides(q);
    if sides.is_empty() {
        // Typed, never-scanned: the outer SELECT still needs the column types, and the
        // book still resolves through `mkt`.
        b.push("select ").push(PAGE_COLUMNS).push(" from inference_orders where false");
    } else {
        let mut first = true;
        for &is_buy in &sides {
            for status in q.statuses.iter() {
                if !first {
                    b.push(" union all ");
                }
                first = false;
                b.push("(select ").push(PAGE_COLUMNS);
                b.push(" from inference_orders where orderbook_address = ");
                b.push_bind(ob);
                b.push(" and is_buy = ");
                b.push_bind(is_buy);
                b.push(" and status = ");
                b.push_bind(status.db_status());
                // No residual on any branch: `db_status()` pins one stored value, and LIVE
                // is exactly OPEN. An index scan alone decides membership.
                if let Some(tc) = &q.token_contract {
                    b.push(" and token_contract = ");
                    b.push_bind(tc);
                }
                if let Some(note) = &q.note {
                    b.push(" and note_address = ");
                    b.push_bind(note);
                }
                if let Some(cursor) = &q.cursor {
                    // `order_id` is numeric(78,0); u128 has no Postgres encoding, so pass
                    // the digits and cast.
                    b.push(" and order_id < ");
                    b.push_bind(cursor.as_u128().to_string());
                    b.push("::numeric");
                }
                b.push(" order by order_id desc limit ");
                b.push_bind(fetch);
                b.push(")");
            }
        }
    }

    b.push(
        ") select p.order_id::text as order_id, p.token_contract, p.note_address, p.is_buy, \
                  p.price::text as price, p.amount_remaining::text as amount_remaining, \
                  p.amount_initial::text as amount_initial, p.deadline::text as deadline, \
                  p.status, \
                  extract(epoch from p.chain_created_at)::bigint as created_at_unix, \
                  extract(epoch from p.chain_updated_at)::bigint as updated_at_unix, \
                  wm.last_update_id, gate.tc_unknown, gate.unprojected, gate.capture_stale, \
                  mkt.price_precision, mkt.quantity_precision \
             from mkt cross join gate cross join wm \
             left join page p on true \
            order by p.order_id desc nulls last limit ",
    );
    b.push_bind(fetch);
    b
}

async fn list_inference_orders_impl(
    repo: &PostgresReadModelRepository,
    query: &InferenceOrdersQuery,
) -> Result<InferenceOrdersPage, anyhow::Error> {
    let limit = query.limit.get() as i64;

    // ONE statement: the page, the freshness watermark, the precision and the fail-closed
    // gate share a single MVCC snapshot. Split statements would let an order commit between
    // them — absent from the page, yet already covered by the lastUpdateId the caller is
    // handed.
    let rows: Vec<InferenceOrderQueryRow> = build_snapshot_query(query, limit + 1)
        .build_query_as::<InferenceOrderQueryRow>()
        .fetch_all(repo.pool())
        .await
        .context("list inference orders")?;

    // `mkt` is the visibility gate: no row ⇒ unknown or not-yet-reconciled book.
    let Some(first) = rows.first() else {
        return Err(anyhow!(DomainError::InvalidMarketOrSymbol));
    };

    // Fail closed rather than answer from state we cannot vouch for. An empty page is a
    // claim of absence, and a claim of absence needs a complete view: no live SELL with an
    // unknown TokenContract, no captured-but-unprojected events for this book (the very
    // placement being asked about could be one), and capture at the chain head. Only
    // queries that could turn an incomplete view into a false "not in use" are refused;
    // everything else is served.
    // The first arm is an invariant the write path cannot violate (see the module docs): a
    // live SELL always carries a TokenContract. If it is true, something is inserting rows
    // that should not exist — a defect, not a delay — and that must be reported whether or
    // not THIS request happens to be one the gate refuses. Tying the alarm to
    // `tokenContract=…&status=LIVE` would let a broken book stay silent for as long as
    // nobody asks it that particular question.
    //
    // A broken book therefore logs once per request, from an unauthenticated endpoint. That
    // is accepted: the condition is unreachable by construction, so the volume is a symptom
    // of the defect rather than a cost of the check, and a rate-limited alarm for an
    // impossible event is an alarm nobody will see fire.
    if first.tc_unknown {
        error!(
            orderbook_address = %query.orderbook_address,
            "a live SELL has no TokenContract: the read gate's invariant is violated",
        );
    }

    let asks_about_tc = query.token_contract.is_some();
    let scopes_live_sells = query.side != Some(InferenceSide::Buy)
        && query.statuses.contains(&InferenceOrderStatus::Live);
    if (first.tc_unknown || first.unprojected || first.capture_stale)
        && asks_about_tc
        && scopes_live_sells
    {
        // One error on the wire, three different things to say in the log. `tc_unknown` has
        // already spoken above; the other two are the indexer catching up, and their volume
        // scales with request rate during a normal backlog, so they stay quiet.
        if first.capture_stale {
            warn!(
                orderbook_address = %query.orderbook_address,
                "capture cursor is stale or behind the chain head; refusing TokenContract lookups",
            );
        } else if first.unprojected {
            debug!(
                orderbook_address = %query.orderbook_address,
                "unprojected events for this book; refusing TokenContract lookups",
            );
        }
        // MarketInconsistent (-1500 / 503), not InvalidMarketOrSymbol (-1121 / 404): the
        // book exists and is reconciled, and every arm of this gate is transient indexer
        // state. 404 is cacheable and tells the client to stop asking; 503 tells it to
        // retry, which is the remedy for all three arms.
        return Err(anyhow!(DomainError::MarketInconsistent));
    }

    let price_scale =
        inference_scale(first.price_precision, &query.orderbook_address, "price_precision")?;
    let quantity_scale =
        inference_scale(first.quantity_precision, &query.orderbook_address, "quantity_precision")?;
    let last_update_id = first.last_update_id.clone().unwrap_or_default();

    // An empty page is one row whose page columns are all NULL; `order_id` is the
    // discriminator. Every other page column is non-null whenever it is, because the
    // underlying columns are `not null` in the schema — a NULL there is read-model
    // corruption.
    let mut orders: Vec<InferenceOrderRow> = Vec::new();
    for r in rows.iter().filter(|r| r.order_id.is_some()) {
        let corrupt = |field: &str| {
            warn!(orderbook = %query.orderbook_address, field, "inference_orders row has a NULL not-null column");
            anyhow!(DomainError::MarketInconsistent)
        };
        // Validate each raw magnitude as a non-negative integer before scaling, exactly as
        // the depth path does: `scale_uint_to_decimal` slices the digits by length and would
        // render a corrupt off-grid value into a malformed decimal. An off-grid value is
        // unreachable under the write path; fail closed rather than serve it.
        let scale_guarded = |raw: &str, scale: u32, field: &str| -> Result<String, anyhow::Error> {
            if BigUint::parse_bytes(raw.as_bytes(), 10).is_none() {
                warn!(orderbook = %query.orderbook_address, raw = %raw, field, "inference_orders value is not a non-negative integer");
                return Err(anyhow!(DomainError::MarketInconsistent));
            }
            Ok(scale_uint_to_decimal(raw, scale))
        };
        orders.push(InferenceOrderRow {
            order_id: r.order_id.clone().expect("filtered on is_some"),
            token_contract: r.token_contract.clone(),
            note: r.note_address.clone(),
            is_buy: r.is_buy.ok_or_else(|| corrupt("is_buy"))?,
            price: scale_guarded(
                r.price.as_deref().ok_or_else(|| corrupt("price"))?,
                price_scale,
                "price",
            )?,
            ticks: scale_guarded(
                r.amount_remaining.as_deref().ok_or_else(|| corrupt("amount_remaining"))?,
                quantity_scale,
                "amount_remaining",
            )?,
            ticks_initial: scale_guarded(
                r.amount_initial.as_deref().ok_or_else(|| corrupt("amount_initial"))?,
                quantity_scale,
                "amount_initial",
            )?,
            deadline: r.deadline.clone(),
            status: public_status(r.status.as_deref().ok_or_else(|| corrupt("status"))?)?,
            created_at: r.created_at_unix,
            updated_at: r.updated_at_unix,
        });
    }

    // `has_more` drives truncation and whether a resume cursor exists; the page stores only
    // the cursor, so `has_more ⟺ next_cursor.is_some()` cannot drift.
    let has_more = orders.len() as i64 > limit;
    orders.truncate(limit as usize);
    let next_cursor = has_more.then(|| orders.last().map(|o| o.order_id.clone())).flatten();

    Ok(InferenceOrdersPage { orders, next_cursor, last_update_id })
}

/// Map a stored status onto the public vocabulary. The string table lives once in
/// [`InferenceOrderStatus::from_db_status`]; this boundary keeps the fail-closed policy — an
/// unknown value is read-model corruption, logged and surfaced as `MarketInconsistent`, as
/// `get_inference_depth_impl` does for a non-numeric price.
fn public_status(raw: &str) -> Result<InferenceOrderStatus, anyhow::Error> {
    InferenceOrderStatus::from_db_status(raw).map_err(|_| {
        warn!(status = raw, "inference_orders.status is not a known value");
        anyhow!(DomainError::MarketInconsistent)
    })
}
