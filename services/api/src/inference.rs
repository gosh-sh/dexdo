// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Handlers + DTOs for the public inference market-data endpoints.

use dodex_application::GetInferenceMarketsUseCase;
use dodex_application::InferenceMarketsFilter;
use dodex_application::InferenceMarketsListing;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceMarketsSort;
use dodex_domain::DomainError;
use dodex_domain::InferenceMarket;
use dodex_domain::InferenceMarketStatus;
use salvo::prelude::*;
use salvo::writing::Json;
use salvo_oapi::endpoint;
use salvo_oapi::ToSchema;
use serde::Serialize;
use tracing::error;

use crate::map_domain_or_unexpected;
use crate::non_empty_query;
use crate::now_seconds;
use crate::optional_typed_query;
use crate::ApiError;
use crate::AppState;

const INFERENCE_DEFAULT_LIMIT: u16 = 50;
const INFERENCE_MAX_LIMIT: u16 = 200;

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceMarketsResponse {
    server_time: i64,
    next_cursor: Option<String>,
    has_more: bool,
    markets: Vec<InferenceMarketDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceMarketDto {
    #[serde(rename = "inferenceOrderBookAddress")]
    orderbook_address: String,
    model: InferenceModelDto,
    status: InferenceMarketStatusDto,
    quote_asset: String,
    /// Signed decimal string; the seller rebate cap (negative).
    maker_commission: String,
    /// Decimal string; the buyer fee `platform_fee_bps / 10000`.
    taker_commission: String,
    price_precision: u8,
    quantity_precision: u8,
    tick_size: String,
    step_size: String,
    min_notional: String,
    /// Weekly-median price per tick; `null` when the book has no recent liquidity.
    reference_price: Option<String>,
    created_at: i64,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceModelDto {
    producer: Option<String>,
    name: Option<String>,
    version: Option<String>,
    /// Canonical `producer--name--version`, or the model hash when unknown.
    #[serde(rename = "ref")]
    model_ref: String,
}

/// Reserved for future inactive states; clients treat it as opaque.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum InferenceMarketStatusDto {
    Trading,
}

impl From<InferenceMarketStatus> for InferenceMarketStatusDto {
    fn from(value: InferenceMarketStatus) -> Self {
        match value {
            InferenceMarketStatus::Trading => Self::Trading,
        }
    }
}

/// List inference markets, or look up one by `inferenceOrderBookAddress`.
#[endpoint(
    tags("inference-market-data"),
    summary = "List inference markets",
    parameters(
        ("inferenceOrderBookAddress" = Option<String>, Query, description = "Single-market lookup. Mutually exclusive with filters and pagination."),
        ("producer" = Option<String>, Query, description = "Filter by model producer."),
        ("status" = Option<String>, Query, description = "Comma-separated statuses to include. Currently only TRADING."),
        ("sort" = Option<String>, Query, description = "Sort field. createdAt (default, DESC)."),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor from a previous call."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 200, description = "Page size. Default 50, max 200; out-of-range values clamp."),
    ),
    security(()),
)]
pub(crate) async fn get_inference_markets(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<InferenceMarketsResponse>, ApiError> {
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();
    let inference_repo = state.inference_repo.clone().ok_or_else(|| {
        error!("inference_repo not wired in AppState");
        ApiError::from(DomainError::Unexpected)
    })?;

    let now = now_seconds();
    let request = build_inference_markets_request(req)?;

    let use_case = GetInferenceMarketsUseCase::new(inference_repo);
    let page = use_case
        .execute(request)
        .await
        .map_err(|err| map_domain_or_unexpected(err, "list_inference_markets"))?;

    Ok(Json(InferenceMarketsResponse {
        server_time: now,
        next_cursor: page.next_cursor,
        has_more: page.has_more,
        markets: page.markets.into_iter().map(inference_market_to_dto).collect(),
    }))
}

fn build_inference_markets_request(req: &mut Request) -> Result<InferenceMarketsRequest, ApiError> {
    let address = non_empty_query(req, "inferenceOrderBookAddress");

    if let Some(addr) = address {
        // Single-market lookup is mutually exclusive with ANY filter/pagination
        // parameter — detected by raw PRESENCE (a present-but-blank `&limit=` or
        // `&producer=` counts), per api-spec §6.1. Using raw `req.query` (not
        // `non_empty_query`) means presence beats both the blank-collapse and
        // the typed parse, so the conflict is always -1102, never -1130 or a
        // silent single-market success. (Intentionally stricter than
        // prediction's `build_markets_request`.)
        let conflicting = ["producer", "status", "sort", "cursor", "limit"]
            .iter()
            .any(|&k| req.query::<String>(k).is_some());
        if conflicting {
            return Err(ApiError::from(DomainError::MissingParameter));
        }
        return Ok(InferenceMarketsRequest::One { orderbook_address: addr });
    }

    // Listing path. `status` is validated (TRADING-only) but not stored: every
    // visible row is TRADING, so the filter is a no-op on results. An unknown
    // token is -1130.
    let producer = non_empty_query(req, "producer");
    let status = non_empty_query(req, "status");
    let sort_param = non_empty_query(req, "sort");
    let cursor = non_empty_query(req, "cursor");
    if let Some(s) = status {
        for token in s.split(',').map(str::trim).filter(|v| !v.is_empty()) {
            InferenceMarketStatus::parse(token)
                .ok_or(ApiError::from(DomainError::InvalidParameter))?;
        }
    }

    let sort = match sort_param.as_deref() {
        None | Some("createdAt") => InferenceMarketsSort::CreatedAtDesc,
        Some(_) => return Err(ApiError::from(DomainError::InvalidParameter)),
    };
    let limit = optional_typed_query::<i64>(req, "limit")?
        .map(|v| v.clamp(1, INFERENCE_MAX_LIMIT as i64) as u16)
        .unwrap_or(INFERENCE_DEFAULT_LIMIT);

    Ok(InferenceMarketsRequest::Listing(InferenceMarketsListing {
        filter: InferenceMarketsFilter { producer },
        sort,
        cursor,
        limit,
    }))
}

fn inference_market_to_dto(m: InferenceMarket) -> InferenceMarketDto {
    InferenceMarketDto {
        orderbook_address: m.orderbook_address,
        model: InferenceModelDto {
            producer: m.model.producer,
            name: m.model.name,
            version: m.model.version,
            model_ref: m.model.model_ref,
        },
        status: m.status.into(),
        quote_asset: m.quote_asset,
        maker_commission: m.maker_commission,
        taker_commission: m.taker_commission,
        price_precision: m.price_precision,
        quantity_precision: m.quantity_precision,
        tick_size: m.tick_size,
        step_size: m.step_size,
        min_notional: m.min_notional,
        reference_price: m.reference_price,
        created_at: m.created_at,
    }
}
