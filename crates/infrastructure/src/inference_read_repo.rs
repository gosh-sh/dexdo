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
use dodex_application::InferenceReadRepository;
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
use tracing::warn;

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
    model_name, version, platform_fee_bps, price_precision, quantity_precision,
    tick_size, step_size, min_notional, reference_price::text as reference_price,
    coalesce(extract(epoch from created_at_chain)::bigint, 0) as created_at,
    coalesce((extract(epoch from created_at_chain) * 1000000)::bigint, 0) as created_at_micros
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
                    or (coalesce((extract(epoch from created_at_chain) * 1000000)::bigint, 0), id) \
                       < ($2, $3)) \
             order by coalesce((extract(epoch from created_at_chain) * 1000000)::bigint, 0) desc, \
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
        let next_cursor =
            if has_more { rows.last().map(|r| encode_cursor(r.created_at_micros, r.id)) } else { None };

        let markets = rows
            .into_iter()
            .map(assemble_inference_market)
            .collect::<Result<Vec<_>, _>>()?;
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

// Implemented in Task 4.
async fn get_inference_depth_impl(
    _repo: &PostgresReadModelRepository,
    _orderbook_address: &str,
    _limit: u16,
) -> Result<InferenceDepthSnapshot, anyhow::Error> {
    Ok(InferenceDepthSnapshot {
        orderbook_address: _orderbook_address.to_string(),
        last_update_id: String::new(),
        bids: vec![],
        asks: vec![],
    })
}
