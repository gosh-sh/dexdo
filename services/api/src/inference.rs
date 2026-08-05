// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Handlers + DTOs for the public inference market-data endpoints.

use dodex_application::GetInferenceDepthQuery;
use dodex_application::GetInferenceDepthUseCase;
use dodex_application::GetInferenceMarketsUseCase;
use dodex_application::GetInferenceOrdersInput;
use dodex_application::GetInferenceOrdersUseCase;
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

use crate::dto;
use crate::map_domain_or_unexpected;
use crate::non_blank_query;
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
    /// Version of the deployed order-book contract (e.g. `"4.0.30"`). Distinct
    /// from `model.version` (the AI model's own version). `null` when not yet
    /// known on chain.
    contract_version: Option<String>,
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
        contract_version: m.contract_version,
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

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceDepthResponse {
    #[serde(rename = "inferenceOrderBookAddress")]
    orderbook_address: String,
    /// Version of the deployed order-book contract for this book; `null` when
    /// not yet known on chain.
    contract_version: Option<String>,
    /// Opaque lex-comparable chain-order cursor; empty when no order has landed.
    last_update_id: String,
    #[salvo(schema(schema_with = inference_depth_bids_schema))]
    bids: Vec<[String; 2]>,
    #[salvo(schema(schema_with = inference_depth_asks_schema))]
    asks: Vec<[String; 2]>,
}

// `[String; 2]` derives as an unbounded array; pin the [price, quantity] shape.
fn inference_depth_levels_schema(description: &str) -> salvo_oapi::schema::Array {
    use salvo_oapi::Array;
    use salvo_oapi::BasicType;
    use salvo_oapi::Object;
    Array::new()
        .items(
            Array::new()
                .items(Object::new().schema_type(BasicType::String))
                .min_items(2)
                .max_items(2),
        )
        .description(description)
}

fn inference_depth_bids_schema() -> salvo_oapi::schema::Array {
    inference_depth_levels_schema(
        "Price levels as [pricePerTick, ticks] decimal-string pairs, best bid first.",
    )
}

fn inference_depth_asks_schema() -> salvo_oapi::schema::Array {
    inference_depth_levels_schema(
        "Price levels as [pricePerTick, ticks] decimal-string pairs, best ask first.",
    )
}

/// Order-book depth for one model's book.
#[endpoint(
    tags("inference-market-data"),
    summary = "Inference order book depth",
    parameters(
        ("inferenceOrderBookAddress" = String, Query, description = "The model's order-book address."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 1000, description = "Levels per side. Default 100, max 1000; out-of-range values clamp."),
    ),
    security(()),
)]
pub(crate) async fn get_inference_depth(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<InferenceDepthResponse>, ApiError> {
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

    let address = non_empty_query(req, "inferenceOrderBookAddress")
        .ok_or(ApiError::from(DomainError::MissingParameter))?;
    let limit =
        optional_typed_query::<i64>(req, "limit")?.map(|v| v.clamp(1, 1000) as u16).unwrap_or(100);

    let use_case = GetInferenceDepthUseCase::new(inference_repo);
    let snapshot = use_case
        .execute(GetInferenceDepthQuery { orderbook_address: address, limit })
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_inference_depth"))?;

    Ok(Json(InferenceDepthResponse {
        orderbook_address: snapshot.orderbook_address,
        contract_version: snapshot.contract_version,
        last_update_id: snapshot.last_update_id,
        bids: snapshot.bids.into_iter().map(|l| [l.price, l.quantity]).collect(),
        asks: snapshot.asks.into_iter().map(|l| [l.price, l.quantity]).collect(),
    }))
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceOrdersResponse {
    server_time: i64,
    /// `max(last_chain_order)` over the book. Covers chain-projected events only: the
    /// reconciler mutates rows without advancing it, so two snapshots sharing a
    /// `lastUpdateId` are not guaranteed identical.
    last_update_id: String,
    next_cursor: Option<String>,
    has_more: bool,
    orders: Vec<InferenceOrderDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InferenceOrderDto {
    order_id: String,
    inference_order_book_address: String,
    /// `null` on BUY orders and on rows whose TokenContract is not known to the indexer.
    /// A query filtering on `tokenContract` is refused while any live SELL of the book is
    /// in that state, so `null` here never hides a match.
    token_contract: Option<String>,
    note: Option<String>,
    side: &'static str,
    price: String,
    ticks: String,
    ticks_initial: String,
    /// `null` means no deadline is known to the indexer — either the chain has none, or it
    /// has one the read model has not recovered yet (subscriptions never publish theirs).
    deadline: Option<String>,
    status: &'static str,
    /// Unix seconds, like every timestamp this API returns. `null` when the chain
    /// timestamp was not recovered — the row is served regardless.
    created_at: Option<i64>,
    updated_at: Option<i64>,
}

/// Orders resting in, or historic to, one model's book.
///
/// Unlike this file's other handlers, which use `non_empty_query` and treat a
/// present-but-blank value as absent, this endpoint uses `non_blank_query` and rejects one
/// with `-1102`. Deliberate: `tokenContract=` silently becoming "no TokenContract filter"
/// would return the whole book and read as a confident answer to a question nobody asked.
#[endpoint(
    tags("inference-market-data"),
    summary = "List inference orders",
    parameters(
        ("inferenceOrderBookAddress" = String, Query, description = "The model's order-book address."),
        ("tokenContract" = Option<String>, Query, description = "Exact deal TokenContract. Mutually exclusive with `note`. Refused with -1500 (HTTP 503, retry) while the book has a live SELL whose TokenContract the indexer does not know."),
        ("note" = Option<String>, Query, description = "Exact owning PrivateNote address. Mutually exclusive with `tokenContract`."),
        ("side" = Option<String>, Query, description = "BUY or SELL."),
        ("status" = Option<Vec<dto::InferenceOrderStatus>>, Query, style = Form, explode = false, description = "Comma-separated: LIVE, FILLED, CANCELLED, EXPIRED. Default: all. LIVE means currently resting; EXPIRED means the book dropped it once its deadline passed."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 500, description = "Page size. Default 100, max 500; out-of-range values are rejected."),
        ("cursor" = Option<String>, Query, description = "Keyset cursor from a previous call's nextCursor."),
    ),
    security(()),
)]
pub(crate) async fn get_inference_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<InferenceOrdersResponse>, ApiError> {
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

    let address = non_blank_query(req, "inferenceOrderBookAddress")?
        .ok_or(ApiError::from(DomainError::MissingParameter))?;
    let input = GetInferenceOrdersInput {
        orderbook_address: address.clone(),
        token_contract: non_blank_query(req, "tokenContract")?,
        note: non_blank_query(req, "note")?,
        side: non_blank_query(req, "side")?,
        status_csv: non_blank_query(req, "status")?,
        // A non-numeric limit is InvalidParameter here; an out-of-range numeric one is
        // MissingParameter inside the use case, matching prediction/orders.
        limit: optional_typed_query::<i64>(req, "limit")?,
        cursor: req.query::<String>("cursor"),
    };

    let use_case = GetInferenceOrdersUseCase::new(inference_repo);
    let page = use_case
        .execute(input)
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_inference_orders"))?;

    // The cursor is the single source of "there is a next page": present exactly when the
    // scan was truncated. Derive the wire `hasMore` from it before the field is moved.
    let has_more = page.next_cursor.is_some();
    Ok(Json(InferenceOrdersResponse {
        server_time: now_seconds(),
        last_update_id: page.last_update_id,
        next_cursor: page.next_cursor,
        has_more,
        orders: page
            .orders
            .into_iter()
            .map(|o| InferenceOrderDto {
                order_id: o.order_id,
                inference_order_book_address: address.clone(),
                token_contract: o.token_contract,
                note: o.note,
                side: if o.is_buy { "BUY" } else { "SELL" },
                price: o.price,
                ticks: o.ticks,
                ticks_initial: o.ticks_initial,
                deadline: o.deadline,
                status: o.status.as_public(),
                created_at: o.created_at,
                updated_at: o.updated_at,
            })
            .collect(),
    }))
}
