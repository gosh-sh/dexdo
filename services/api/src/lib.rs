// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

mod auth_hoop;
mod dto;
#[doc(hidden)]
pub mod testkit;
mod timeout_hoop;

use std::env;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use dodex_application::AuthContext;
use dodex_application::Authenticator;
use dodex_application::BatchOrderInputItem;
use dodex_application::BuyFullSetInput;
use dodex_application::BuyFullSetUseCase;
use dodex_application::CancelBatchOrdersInput;
use dodex_application::CancelBatchOrdersUseCase;
use dodex_application::CancelOrderInput;
use dodex_application::CancelOrderUseCase;
use dodex_application::ChainOrderSender;
use dodex_application::CreateBatchOrdersInput;
use dodex_application::CreateBatchOrdersUseCase;
use dodex_application::CreateOrderUseCase;
use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsUseCase;
use dodex_application::GetOraclesUseCase;
use dodex_application::GetOrdersInput;
use dodex_application::GetOrdersUseCase;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_application::NewOrderInput;
use dodex_application::OraclesFilter;
use dodex_application::OraclesRequest;
use dodex_application::OrdersCursor;
use dodex_application::OrdersMarketFilter;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketStatus;
use dodex_domain::Order;
use dodex_domain::OrderParts;
use dodex_domain::OrderSide;
use dodex_domain::OrderType;
use dodex_domain::Permission;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TimeInForce;
use dodex_domain::Timings;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::chain_sender::DexChainSender;
use dodex_infrastructure::config::ApiConfig;
use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::database;
use dodex_infrastructure::database::build_pool;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::seed;
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::writing::Json;
use salvo_extra::affix_state::inject;
use salvo_oapi::endpoint;
use salvo_oapi::security::ApiKey;
use salvo_oapi::security::ApiKeyValue;
use salvo_oapi::security::SecurityScheme;
use salvo_oapi::Components;
use salvo_oapi::EndpointOutRegister;
use salvo_oapi::Info;
use salvo_oapi::OpenApi;
use salvo_oapi::Operation;
use salvo_oapi::Response as OapiResponse;
use salvo_oapi::Server as OapiServer;
use salvo_oapi::ToSchema;
use serde::Deserialize;
use serde::Serialize;
use tracing::error;
use tracing::info;
use tracing::warn;

#[doc(hidden)]
pub type SharedRepo = Arc<dyn MarketReadRepository>;
#[doc(hidden)]
pub type SharedAuth = Arc<dyn Authenticator>;
#[doc(hidden)]
pub type SharedChainSender = Arc<dyn ChainOrderSender>;
#[doc(hidden)]
pub type SharedPnReader = Arc<dyn dodex_application::PnStateReader>;
#[doc(hidden)]
pub type SharedRefRepo = Arc<dyn dodex_application::ReferenceRepository>;

#[doc(hidden)]
#[derive(Clone)]
pub struct AppState {
    pub(crate) repo: SharedRepo,
    pub(crate) authenticator: SharedAuth,
    pub(crate) chain_sender: SharedChainSender,
    pub(crate) pn_reader: SharedPnReader,
    pub(crate) ref_repo: SharedRefRepo,
    /// Per-request wall-clock budget enforced by the `request_timeout`
    /// hoop on every route. `Duration::ZERO` disables the hoop, which
    /// is the implicit default `AppState::new` chooses so tests that
    /// don't care about timeouts can ignore it.
    pub(crate) request_timeout: Duration,
    /// `chain.max_batch_size` from api config: the batch-length cap the
    /// batch use cases enforce and `/api/v1/markets` advertises as
    /// `maxBatchSize`. `AppState::new` defaults it to the config
    /// default; tests pin a different cap via `with_max_batch_size`.
    pub(crate) max_batch_size: u16,
}

impl AppState {
    /// Wire-up constructor. Re-exported through the `testkit` module
    /// for integration tests; production code reaches it through `run`.
    /// The request timeout defaults to `Duration::ZERO`, which keeps
    /// the timeout hoop a no-op — tests that don't exercise it stay
    /// terse. Production wires the configured value via
    /// `with_request_timeout`.
    #[doc(hidden)]
    pub fn new(
        repo: SharedRepo,
        authenticator: SharedAuth,
        chain_sender: SharedChainSender,
        pn_reader: SharedPnReader,
        ref_repo: SharedRefRepo,
    ) -> Self {
        Self {
            repo,
            authenticator,
            chain_sender,
            pn_reader,
            ref_repo,
            request_timeout: Duration::ZERO,
            max_batch_size: 10,
        }
    }

    #[doc(hidden)]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[doc(hidden)]
    pub fn with_max_batch_size(mut self, max_batch_size: u16) -> Self {
        self.max_batch_size = max_batch_size;
        self
    }
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct MarketsResponse {
    /// Unix seconds. All timestamps in this response are unix seconds
    /// unless stated otherwise.
    server_time: i64,
    /// Pagination cursor for the next page. `null` when `hasMore` is
    /// `false`.
    next_cursor: Option<String>,
    /// Whether more pages follow.
    has_more: bool,
    /// Markets matching the request.
    markets: Vec<MarketDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct MarketDto {
    /// Stable market identifier.
    market_address: String,
    /// Deterministic order-book address. Always present; trading
    /// availability depends on `status`.
    order_book_address: String,
    /// Technical market name. Not the user-facing title; see
    /// `event.eventName`.
    market_name: String,
    status: dto::MarketStatus,
    /// Quote-asset symbol for display.
    quote_asset: String,
    /// Numeric quote-asset token type.
    token_type: i32,
    /// Maker fee rate as a signed decimal string. A negative value is
    /// a maker rebate credited to the maker.
    maker_commission: String,
    /// Taker fee rate as a decimal string, charged to the taker.
    /// Always non-negative.
    taker_commission: String,
    /// Unix seconds. Market creation timestamp.
    created_at: i64,
    // Nullability notes for `timings` / `terminal` live in the
    // referenced schemas' doc comments: the ToSchema derive drops
    // field-level doc comments next to `$ref`s.
    timings: Option<TimingsDto>,
    event: EventDto,
    terminal: Option<TerminalDto>,
    /// Outcome-token descriptors.
    outcomes: Vec<OutcomeDto>,
}

/// Market lifecycle timestamps, all unix seconds. The market's
/// `timings` is `null` while `status` is `PENDING`.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct TimingsDto {
    stake_start: i64,
    stake_end: i64,
    result_start: i64,
    result_end: i64,
    /// `null` before the order book is active.
    frozen_at: Option<i64>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct EventDto {
    /// `0x`-prefixed uint256 hex digest, computed on-chain from the
    /// event metadata; identical across every oracle confirming the
    /// same event.
    event_id: String,
    /// User-facing event title. `null` until at least one oracle
    /// confirmation has landed.
    event_name: Option<String>,
    /// User-facing description. Same confirmation caveat as
    /// `eventName`.
    description: Option<String>,
    /// One entry per oracle that confirmed this market. Empty when no
    /// confirmation has landed yet.
    oracles: Vec<OracleDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleDto {
    /// Oracle name. `null` if the oracle row has not been reconciled
    /// yet.
    name: Option<String>,
    /// Oracle contract address.
    address: Option<String>,
    /// Oracle fee for this confirmation, as a uint128 decimal string.
    fee: Option<String>,
}

/// How the market ended. The market's `terminal` is `null` while the
/// market is alive and populated for the terminal statuses `RESOLVED`,
/// `CANCELLED`, `EXPIRED`.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct TerminalDto {
    kind: dto::TerminalKind,
    /// Unix seconds. When the market entered the terminal state.
    at: i64,
    /// The winning outcome's `outcomeId`. Present only when `kind` is
    /// `RESOLVED`; `null` otherwise.
    resolved_outcome_id: Option<u32>,
    cancel_reason: Option<dto::CancelReason>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OutcomeDto {
    /// Stable outcome ID. Clients MUST use this field, not the array
    /// index.
    outcome_id: u32,
    /// Human-readable outcome name.
    outcome_name: String,
    /// Outcome-token symbol used in trading and order-book requests.
    symbol: String,
    /// Maximum number of decimal places accepted for order prices.
    price_precision: u8,
    /// Maximum number of decimal places accepted for order quantities.
    quantity_precision: u8,
    /// Minimum price increment.
    tick_size: String,
    /// Minimum quantity increment.
    step_size: String,
    /// Minimum accepted notional value for an order.
    min_notional: String,
    /// Maximum number of orders accepted in one batch request for this
    /// outcome.
    max_batch_size: u16,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OraclesResponse {
    server_time: i64,
    next_cursor: Option<String>,
    has_more: bool,
    oracles: Vec<OracleEntryDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleEntryDto {
    name: String,
    address: String,
    event_lists: Vec<OracleEventListDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleEventListDto {
    index: i64,
    address: String,
    description: String,
    events: Vec<OracleEventDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleEventDto {
    event_id: String,
    event_name: String,
    description: Option<String>,
    oracle_fee: OracleFeeDto,
    deadline: i64,
    trust_address: Option<String>,
    outcomes: Vec<OracleOutcomeDto>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleFeeDto {
    asset: String,
    amount: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OracleOutcomeDto {
    outcome_id: u32,
    outcome_name: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct DepthResponse {
    /// Market address.
    market_address: String,
    /// Outcome-token symbol.
    symbol: String,
    /// Opaque lex-comparable chain-order cursor; a larger string means
    /// a newer event has touched this `(marketAddress, symbol)`. Empty
    /// string when no order event has landed yet. Do not parse as an
    /// integer.
    last_update_id: String,
    #[salvo(schema(schema_with = depth_bids_schema))]
    bids: Vec<[String; 2]>,
    #[salvo(schema(schema_with = depth_asks_schema))]
    asks: Vec<[String; 2]>,
}

/// `[String; 2]` derives as an unbounded `array of string`; pin the
/// exact `[price, quantity]` pair shape for codegen clients. The
/// description lives here too — the derive drops doc comments on
/// `schema_with` fields.
fn depth_levels_schema(description: &str) -> salvo_oapi::schema::Array {
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

fn depth_bids_schema() -> salvo_oapi::schema::Array {
    depth_levels_schema("Price levels as [price, quantity] decimal-string pairs, best bid first.")
}

fn depth_asks_schema() -> salvo_oapi::schema::Array {
    depth_levels_schema("Price levels as [price, quantity] decimal-string pairs, best ask first.")
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OrderResponse {
    /// Market address.
    market_address: String,
    /// Outcome-token symbol.
    symbol: String,
    /// Chain-side order id, u64 as a decimal string. Empty string for
    /// `REJECTED` orders.
    order_id: String,
    /// Client-supplied id, or an empty string if absent.
    client_order_id: String,
    /// Limit price, scaled by the outcome price precision.
    price: String,
    /// Original order quantity, scaled by the outcome quantity
    /// precision.
    orig_qty: String,
    /// Filled quantity. Can be `> 0` for `CANCELED` orders that filled
    /// partially before cancellation.
    executed_qty: String,
    status: dto::OrderStatus,
    time_in_force: dto::TimeInForce,
    #[serde(rename = "type")]
    order_type: dto::OrderType,
    side: dto::OrderSide,
    /// Unix milliseconds. On-chain order creation time.
    time: i64,
    /// Unix milliseconds. On-chain time of the most recent book event
    /// that touched the order.
    update_time: i64,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OrdersPageResponse {
    /// Orders matching the filter, most recently placed first.
    orders: Vec<OrderResponse>,
    /// Opaque pagination cursor. Pass back verbatim to fetch the next
    /// page; `null` when the last page has been returned.
    next_cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AccountResponse {
    /// Account UUID.
    account_id: String,
    /// Unix milliseconds. When the balances snapshot was assembled.
    update_time: i64,
    /// Per-asset balances aggregated across all markets.
    balances: Vec<AccountBalanceItem>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AccountBalanceItem {
    /// Asset symbol.
    asset: String,
    /// Spendable amount, as a decimal string.
    free: String,
    /// Amount locked in open orders, as a decimal string.
    locked: String,
}

impl AccountResponse {
    fn from_domain(d: dodex_domain::AccountBalances) -> Self {
        Self {
            account_id: d.account_id.to_string(),
            update_time: d.update_time_ms,
            balances: d
                .balances
                .into_iter()
                .map(|b| AccountBalanceItem { asset: b.asset, free: b.free, locked: b.locked })
                .collect(),
        }
    }
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct MarketBalancesResponse {
    /// Market address.
    market_address: String,
    /// Unix milliseconds. When the balances snapshot was assembled.
    update_time: i64,
    /// Per-outcome balances on this market.
    balances: Vec<OutcomeBalanceItem>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct OutcomeBalanceItem {
    /// Stable outcome ID.
    outcome_id: u32,
    /// Outcome-token symbol.
    symbol: String,
    /// Spendable outcome-token amount, as a decimal string.
    free: String,
    /// Amount locked in open orders, as a decimal string.
    locked_in_orders: String,
}

impl MarketBalancesResponse {
    fn from_domain(d: dodex_domain::MarketBalances) -> Self {
        Self {
            market_address: d.market_address.0,
            update_time: d.update_time_ms,
            balances: d
                .balances
                .into_iter()
                .map(|b| OutcomeBalanceItem {
                    outcome_id: b.outcome_id,
                    symbol: b.symbol.0,
                    free: b.free,
                    locked_in_orders: b.locked_in_orders,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ErrorBody {
    /// Negative machine-readable error code. See docs/api-spec.md
    /// §Error Codes for the full table.
    code: i32,
    /// Human-readable error message.
    msg: &'static str,
}

#[derive(Debug)]
pub(crate) struct ApiError(DomainError);

impl ApiError {
    pub(crate) fn status(&self) -> StatusCode {
        // Matches are intentionally exhaustive (no `_`): when a new
        // `DomainError` variant lands, the compiler forces an update
        // here AND in `map_domain_or_unexpected` below — so the two
        // sites cannot disagree about whether a new variant is 4xx
        // or 5xx.
        match self.0 {
            DomainError::AuthRequired
            | DomainError::AuthEnvelopeIncomplete
            | DomainError::TimestampOutsideRecvWindow
            | DomainError::InvalidSignature => StatusCode::UNAUTHORIZED,
            DomainError::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            DomainError::UnknownOrder
            | DomainError::InvalidMarketOrSymbol
            | DomainError::AccountNotDeployed => StatusCode::NOT_FOUND,
            // Transient indexer state — fail closed, client retries when
            // the indexer catches up.
            DomainError::MarketInconsistent => StatusCode::SERVICE_UNAVAILABLE,
            // The request_timeout hoop tripped — emit 504 so clients can
            // distinguish "our budget elapsed" from "upstream gateway
            // failed" (502).
            DomainError::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
            // Per-PN serialisation is a chain invariant: only one
            // chain operation per trading PN can be in flight at a
            // time. 429 is the canonical "you sent too many to this
            // PN; back off and retry" — distinct from a 401 (auth)
            // or 400 (bad order).
            DomainError::OrderPnBusy => StatusCode::TOO_MANY_REQUESTS,
            DomainError::Unexpected => StatusCode::INTERNAL_SERVER_ERROR,
            DomainError::MissingParameter
            | DomainError::InvalidParameter
            | DomainError::PrecisionExceeded
            | DomainError::OrderValidationFailed => StatusCode::BAD_REQUEST,
        }
    }
}

impl From<DomainError> for ApiError {
    fn from(value: DomainError) -> Self {
        Self(value)
    }
}

impl Scribe for ApiError {
    fn render(self, res: &mut Response) {
        res.status_code(self.status());
        res.render(Json(ErrorBody { code: self.0.code(), msg: self.0.msg() }));
    }
}

/// Map an `anyhow::Error` to `ApiError`: if the error is a typed `DomainError`,
/// return the matching `ApiError` and emit a `warn!` for non-client variants so
/// 5xx responses surface in ops dashboards. Unknown errors fall through to
/// `DomainError::Unexpected` with an `error!` log.
fn map_domain_or_unexpected(err: anyhow::Error, context: &str) -> ApiError {
    if let Some(domain) = err.downcast_ref::<DomainError>() {
        // Tap: log non-client domain errors at warn level. Match is
        // exhaustive (no `_`) so a new variant lands in the classifier
        // alongside the status-code site above.
        match domain {
            DomainError::MissingParameter
            | DomainError::InvalidParameter
            | DomainError::InvalidMarketOrSymbol
            | DomainError::UnknownOrder
            | DomainError::AccountNotDeployed
            | DomainError::AuthRequired
            | DomainError::AuthEnvelopeIncomplete
            | DomainError::TimestampOutsideRecvWindow
            | DomainError::InvalidSignature
            | DomainError::RequestTooLarge
            | DomainError::OrderValidationFailed
            | DomainError::PrecisionExceeded
            | DomainError::OrderPnBusy => {} // client error, no log
            DomainError::MarketInconsistent
            | DomainError::RequestTimeout
            | DomainError::Unexpected => {
                // Log the full anyhow chain (including any `.context()`
                // breadcrumbs from the use case / repo) — `?domain` alone
                // collapses to the variant name and drops upstream
                // diagnostics that ops need to triage 5xx.
                tracing::warn!(?err, ?domain, context, "handler surfacing 5xx domain error")
            }
        }
        return ApiError::from(*domain);
    }
    error!(?err, context, "handler failed with non-domain error");
    ApiError::from(DomainError::Unexpected)
}

// All error paths render an `ErrorBody` JSON. Status codes vary by `DomainError`
// variant — see `ApiError::status`. Spec-wise we collapse the matrix into a
// single `default` response so the OpenAPI consumer reads one error schema
// rather than 7 nearly-identical entries.
impl EndpointOutRegister for ApiError {
    fn register(components: &mut Components, operation: &mut Operation) {
        operation.responses.insert(
            "default",
            OapiResponse::new("Error response")
                .add_content("application/json", <ErrorBody as ToSchema>::to_schema(components)),
        );
    }
}

/// Service readiness probe. Returns `ok` once the process is accepting traffic.
#[endpoint(tags("system"), summary = "Readiness probe", security(()))]
async fn readiness() -> &'static str {
    "ok"
}

const DEFAULT_LIMIT: u16 = 50;
const MAX_LIMIT: u16 = 200;

/// List markets or look up a single market by `marketAddress`.
#[endpoint(
    tags("market-data"),
    summary = "List markets",
    parameters(
        ("marketAddress" = Option<String>, Query, description = "Single-market lookup. Mutually exclusive with listing filters and pagination."),
        ("status" = Option<Vec<dto::MarketStatus>>, Query, style = Form, explode = false, description = "Comma-separated MarketStatus filter."),
        ("quoteAsset" = Option<String>, Query, description = "Filter by quote asset symbol."),
        ("oracleName" = Option<String>, Query, description = "Filter by oracle name."),
        ("closingBefore" = Option<i64>, Query, description = "Return only markets with timings.resultEnd before this unix-seconds bound."),
        ("sort" = Option<dto::MarketsSort>, Query, description = "Sort order: resultStart (default, ascending) or createdAt (descending)."),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor returned from a previous page."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 200, description = "Page size. Default 50, max 200; out-of-range values clamp."),
    ),
    security(()),
)]
async fn get_markets(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<MarketsResponse>, ApiError> {
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let now = now_seconds();
    let request = build_markets_request(req, now)?;

    let use_case = GetMarketsUseCase::new(state.repo);
    let page = use_case
        .execute(request)
        .await
        .map_err(|err| map_domain_or_unexpected(err, "list_markets"))?;

    let payload = MarketsResponse {
        server_time: now,
        next_cursor: page.next_cursor,
        has_more: page.has_more,
        markets: page.markets.into_iter().map(|m| market_to_dto(m, state.max_batch_size)).collect(),
    };
    Ok(Json(payload))
}

fn build_markets_request(req: &mut Request, now: i64) -> Result<MarketsRequest, ApiError> {
    let market_address = non_empty_query(req, "marketAddress");
    let status = non_empty_query(req, "status");
    let quote_asset = non_empty_query(req, "quoteAsset");
    let oracle_name = non_empty_query(req, "oracleName");
    let closing_before = optional_typed_query::<i64>(req, "closingBefore")?;
    let sort_param = non_empty_query(req, "sort");
    let cursor = non_empty_query(req, "cursor");
    // Parse `limit` permissively as i64 so out-of-u16-range values (e.g.
    // `limit=99999`) clamp to MAX_LIMIT instead of failing with 400. Only
    // non-numeric input still returns InvalidParameter.
    let limit_param = optional_typed_query::<i64>(req, "limit")?;

    if let Some(addr) = market_address {
        if status.is_some()
            || quote_asset.is_some()
            || oracle_name.is_some()
            || closing_before.is_some()
            || sort_param.is_some()
            || cursor.is_some()
            || limit_param.is_some()
        {
            return Err(ApiError::from(DomainError::MissingParameter));
        }
        return Ok(MarketsRequest::One { market_address: MarketAddress(addr), now });
    }

    let statuses = match status {
        Some(s) => s
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| MarketStatus::parse(v).ok_or(ApiError::from(DomainError::InvalidParameter)))
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
    };
    let sort = match sort_param.as_deref() {
        None | Some("resultStart") => MarketsSort::ResultStartAsc,
        Some("createdAt") => MarketsSort::CreatedAtDesc,
        Some(_) => return Err(ApiError::from(DomainError::InvalidParameter)),
    };
    let limit = limit_param.map(|v| v.clamp(1, MAX_LIMIT as i64) as u16).unwrap_or(DEFAULT_LIMIT);

    Ok(MarketsRequest::Listing(MarketsListing {
        filter: MarketsFilter { statuses, quote_asset, oracle_name, closing_before },
        sort,
        cursor,
        limit,
        now,
    }))
}

fn market_to_dto(market: Market, max_batch_size: u16) -> MarketDto {
    MarketDto {
        market_address: market.market_address.0,
        order_book_address: market.order_book_address,
        market_name: market.market_name.0,
        status: market.status.into(),
        quote_asset: market.quote_asset,
        token_type: market.token_type,
        maker_commission: market.maker_commission,
        taker_commission: market.taker_commission,
        created_at: market.created_at,
        timings: market.timings.map(timings_to_dto),
        event: event_to_dto(market.event),
        terminal: market.terminal.map(terminal_to_dto),
        outcomes: market.outcomes.into_iter().map(|o| outcome_to_dto(o, max_batch_size)).collect(),
    }
}

fn timings_to_dto(t: Timings) -> TimingsDto {
    TimingsDto {
        stake_start: t.stake_start,
        stake_end: t.stake_end,
        result_start: t.result_start,
        result_end: t.result_end,
        frozen_at: t.frozen_at,
    }
}

fn event_to_dto(e: MarketEvent) -> EventDto {
    EventDto {
        event_id: e.event_id,
        event_name: e.event_name,
        description: e.description,
        oracles: e
            .oracles
            .into_iter()
            .map(|o| OracleDto { name: o.name, address: o.address, fee: o.fee })
            .collect(),
    }
}

fn terminal_to_dto(t: Terminal) -> TerminalDto {
    TerminalDto {
        kind: t.kind.into(),
        at: t.at,
        resolved_outcome_id: t.resolved_outcome_id,
        cancel_reason: t.cancel_reason.map(Into::into),
    }
}

/// `max_batch_size` comes from api config (`chain.max_batch_size`), not
/// the read model — it is backend policy mirroring the chain's
/// compiled-in cap, advertised here and enforced by the batch use cases
/// from the same source.
fn outcome_to_dto(o: dodex_domain::Outcome, max_batch_size: u16) -> OutcomeDto {
    OutcomeDto {
        outcome_id: o.outcome_id,
        outcome_name: o.outcome_name,
        symbol: o.symbol.0,
        price_precision: o.price_precision,
        quantity_precision: o.quantity_precision,
        tick_size: o.tick_size,
        step_size: o.step_size,
        min_notional: o.min_notional,
        max_batch_size,
    }
}

/// List oracles, their event lists, and the events available for market creation.
#[endpoint(
    tags("market-data"),
    summary = "List oracles",
    parameters(
        ("oracleAddress" = Option<String>, Query, description = "Filter by oracle address."),
        ("eventId" = Option<String>, Query, description = "Return only the event list containing this event id; events[] is narrowed to it."),
        ("deadlineBefore" = Option<i64>, Query, description = "Include only events with deadline < deadlineBefore (unix seconds)."),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor returned from a previous page."),
        ("limit" = Option<i64>, Query, description = "Number of oracles. Default 50, max 200."),
    ),
    security(()),
)]
async fn get_oracles(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<OraclesResponse>, ApiError> {
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let now = now_seconds();
    let request = build_oracles_request(req, now)?;

    let use_case = GetOraclesUseCase::new(state.repo);
    let page = use_case
        .execute(request)
        .await
        .map_err(|err| map_domain_or_unexpected(err, "list_oracles"))?;

    let payload = OraclesResponse {
        server_time: now,
        next_cursor: page.next_cursor,
        has_more: page.has_more,
        oracles: page.oracles.into_iter().map(oracle_to_dto).collect(),
    };
    Ok(Json(payload))
}

fn build_oracles_request(req: &mut Request, now: i64) -> Result<OraclesRequest, ApiError> {
    let oracle_address = non_empty_query(req, "oracleAddress");
    let event_id = non_empty_query(req, "eventId");
    let deadline_before = optional_typed_query::<i64>(req, "deadlineBefore")?;
    let cursor = non_empty_query(req, "cursor");
    // Permissive i64 parse so out-of-range limits clamp instead of 400ing;
    // only non-numeric input is InvalidParameter (matches get_markets).
    let limit_param = optional_typed_query::<i64>(req, "limit")?;
    let limit = limit_param.map(|v| v.clamp(1, MAX_LIMIT as i64) as u16).unwrap_or(DEFAULT_LIMIT);

    Ok(OraclesRequest {
        filter: OraclesFilter { oracle_address, event_id, deadline_before },
        cursor,
        limit,
        now,
    })
}

fn oracle_to_dto(o: dodex_domain::OracleListing) -> OracleEntryDto {
    OracleEntryDto {
        name: o.name,
        address: o.address,
        event_lists: o.event_lists.into_iter().map(oracle_event_list_to_dto).collect(),
    }
}

fn oracle_event_list_to_dto(l: dodex_domain::OracleEventListEntry) -> OracleEventListDto {
    OracleEventListDto {
        index: l.index,
        address: l.address,
        description: l.description,
        events: l.events.into_iter().map(oracle_event_to_dto).collect(),
    }
}

fn oracle_event_to_dto(e: dodex_domain::OracleEventEntry) -> OracleEventDto {
    OracleEventDto {
        event_id: e.event_id,
        event_name: e.event_name,
        description: e.description,
        oracle_fee: OracleFeeDto { asset: e.oracle_fee.asset, amount: e.oracle_fee.amount },
        deadline: e.deadline,
        trust_address: e.trust_address,
        outcomes: e
            .outcomes
            .into_iter()
            .map(|o| OracleOutcomeDto { outcome_id: o.outcome_id, outcome_name: o.outcome_name })
            .collect(),
    }
}

/// Order book depth snapshot for a (marketAddress, symbol).
#[endpoint(
    tags("market-data"),
    summary = "Order book depth",
    parameters(
        ("marketAddress" = String, Query, description = "Market address."),
        ("symbol" = String, Query, description = "Outcome-token symbol."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 1000, description = "Levels per side. Default 100, max 1000; out-of-range values clamp."),
    ),
    security(()),
)]
async fn get_depth(req: &mut Request, depot: &mut Depot) -> Result<Json<DepthResponse>, ApiError> {
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_empty_query(req, "marketAddress")
        .ok_or(ApiError::from(DomainError::MissingParameter))?;
    let symbol =
        non_empty_query(req, "symbol").ok_or(ApiError::from(DomainError::MissingParameter))?;

    // Parse as i64 so values >u16::MAX clamp to 1000 rather than 400ing.
    let limit =
        optional_typed_query::<i64>(req, "limit")?.map(|v| v.clamp(1, 1000) as u16).unwrap_or(100);

    let use_case = GetDepthUseCase::new(state.repo);
    let snapshot = use_case
        .execute(GetDepthQuery {
            market_address: MarketAddress(market_address),
            symbol: Symbol(symbol),
            limit,
        })
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_depth"))?;

    Ok(Json(DepthResponse {
        market_address: snapshot.market_address.0,
        symbol: snapshot.symbol.0,
        last_update_id: snapshot.last_update_id,
        bids: snapshot.bids.into_iter().map(|level| [level.price, level.quantity]).collect(),
        asks: snapshot.asks.into_iter().map(|level| [level.price, level.quantity]).collect(),
    }))
}

/// List orders for the authenticated trading PN, with optional filters.
#[endpoint(
    tags("trading"),
    summary = "List orders",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
        ("marketAddress" = Option<String>, Query, description = "Market filter. Must pair with symbol when set."),
        ("symbol" = Option<String>, Query, description = "Symbol filter. Must pair with marketAddress."),
        ("status" = Option<Vec<dto::QueryableOrderStatus>>, Query, style = Form, explode = false, description = "Comma-separated OrderStatus filter. PENDING_NEW and PENDING_CANCEL are not queryable. Default: all statuses."),
        ("limit" = Option<i64>, Query, minimum = 1, maximum = 500, description = "Page size, 1..=500. Default 100."),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor."),
    ),
    security(("apiKey" = [])),
)]
async fn get_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<OrdersPageResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::UserData)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_blank_query(req, "marketAddress")?.map(MarketAddress);
    let symbol = non_blank_query(req, "symbol")?.map(Symbol);
    let market_filter = OrdersMarketFilter::pair(market_address, symbol).map_err(ApiError::from)?;
    // status: raw CSV, validated by OrderStatusFilter::from_csv inside the
    // use case. Absent / blank → "all statuses".
    let status = req.query::<String>("status");
    // `optional_typed_query` returns `Err(InvalidParameter)` when the
    // raw value is present but unparseable (e.g. `limit=abc`). That maps
    // to -1130 ("Invalid value for a query or body parameter") per the
    // api-spec.md error table, which is the precise diagnosis for a
    // non-numeric `limit`. Out-of-range numeric inputs (e.g. `limit=501`
    // or `limit=0`) still come back as -1102 because the use case applies
    // the `[1, 500]` bound check after parsing succeeds; see
    // `DomainError::MissingParameter` for the Binance-shaped wire message.
    let limit = optional_typed_query::<i64>(req, "limit")?;
    // cursor: raw string forwarded to `OrdersCursor::new` inside the
    // use case, which trims and rejects blank as `MissingParameter`.
    // The blank-rejects-loudly contract lives in the cursor type, not
    // at this call site; `marketAddress` / `symbol` enforce the same
    // contract one layer up via `non_blank_query` because the use
    // case never sees their raw strings.
    let cursor = req.query::<String>("cursor");

    let use_case = GetOrdersUseCase::new(state.repo);
    let page = use_case
        .execute(GetOrdersInput {
            owner_pn_address: ctx.trading_pn.pn_address.clone(),
            market_filter,
            status,
            limit,
            cursor,
        })
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_orders"))?;

    Ok(Json(OrdersPageResponse {
        orders: page.orders.into_iter().map(order_to_dto).collect(),
        next_cursor: page.next_cursor.map(OrdersCursor::into_string),
    }))
}

fn order_to_dto(order: Order) -> OrderResponse {
    let OrderParts {
        market_address,
        symbol,
        order_id,
        client_order_id,
        price,
        orig_qty,
        executed_qty,
        status,
        time_in_force,
        order_type,
        side,
        time,
        update_time,
        ..
    } = order.into_parts();

    OrderResponse {
        market_address: market_address.0,
        symbol: symbol.0,
        order_id,
        client_order_id,
        price,
        orig_qty,
        executed_qty,
        status: status.into(),
        time_in_force: time_in_force.into(),
        order_type: order_type.into(),
        side: side.into(),
        time,
        update_time,
    }
}

fn non_empty_query(req: &mut Request, key: &str) -> Option<String> {
    req.query::<String>(key).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Strict variant of [`non_empty_query`]: a present-but-blank value is
/// rejected as `MissingParameter` instead of being silently collapsed
/// to "absent". Mirrors `OrdersCursor::new`'s contract — a client that
/// sends `?marketAddress=&symbol=` is signalling a bug (an unbound
/// template variable), not "no filter". See read-api.md §error table.
fn non_blank_query(req: &mut Request, key: &str) -> Result<Option<String>, ApiError> {
    let Some(raw) = req.query::<String>(key) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::from(DomainError::MissingParameter));
    }
    Ok(Some(trimmed.to_string()))
}

/// Parse an optional typed query parameter the strict way:
/// absent → `Ok(None)`, present-but-blank → `Ok(None)`, present-but-unparseable
/// → `Err(InvalidParameter)`. The default Salvo `req.query::<T>` swallows parse
/// failures and returns `None`, which is a footgun for a public API: callers
/// silently get the default value back instead of `400`.
fn optional_typed_query<T: std::str::FromStr>(
    req: &mut Request,
    key: &str,
) -> Result<Option<T>, ApiError> {
    let Some(raw) = req.query::<String>(key) else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<T>().map(Some).map_err(|_| ApiError::from(DomainError::InvalidParameter))
}

// Request body for `POST /api/v1/order`. Field names match
// docs/api-spec.md §New Order verbatim; `type` is the reserved keyword
// we rename for serde and rebind to `order_type` internally.
// Every field is `Option` at runtime so the handler can distinguish
// missing (-1102) from unknown value (-1130). `value_type` overrides
// restore the contract in the spec: mandatory fields override to a
// non-Option type (lands in `required`, drops the spurious `null`),
// enum-shaped fields override to the typed enum reference the runtime
// parse enforces.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateOrderRequest {
    /// Market address.
    #[salvo(schema(value_type = String))]
    market_address: Option<String>,
    /// Outcome-token symbol.
    #[salvo(schema(value_type = String))]
    symbol: Option<String>,
    /// Client-supplied order id. Auto-generated by the backend when
    /// omitted.
    new_order_client_id: Option<String>,
    #[salvo(schema(value_type = dto::OrderSide))]
    side: Option<String>,
    /// Order quantity as a decimal string.
    #[salvo(schema(value_type = String))]
    quantity: Option<String>,
    /// Limit price as a decimal string. Required for `LIMIT` orders.
    price: Option<String>,
    #[serde(rename = "type")]
    #[salvo(schema(value_type = Option<dto::OrderType>))]
    order_type: Option<String>,
    #[salvo(schema(value_type = Option<dto::TimeInForce>))]
    time_in_force: Option<String>,
}

// Minimal by design — only facts the caller does not already have.
// `clientOrderId` may have been generated by the backend, `transactTime`
// is the moment we accepted, `status` is always `PENDING_NEW` because the
// order has only entered the chain queue at this point. The full order
// shape with chain-assigned `orderId` arrives later via `GET /api/v1/orders`
// once `OrderBook.OrderPlaced` projects.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateOrderResponse {
    /// Echo of `newOrderClientId`, or the backend-generated id when
    /// none was supplied.
    client_order_id: String,
    /// Unix milliseconds. The moment the order was accepted.
    transact_time: i64,
    /// Always `PENDING_NEW` on success.
    status: dto::OrderStatus,
}

// Minimal by design, parallel to CreateOrderResponse. `clientOrderId` is
// the value recorded on placement, useful for correlating with the prior
// POST. Final state — CANCELED, or FILLED if matching raced the cancel —
// becomes visible later via `GET /api/v1/orders`.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CancelOrderResponse {
    /// Echo of the cancelled order's id.
    order_id: String,
    /// Id recorded at placement; empty string when the order was
    /// placed without a `newOrderClientId`.
    client_order_id: String,
    /// Unix milliseconds. The moment the cancel was accepted.
    transact_time: i64,
    /// Always `PENDING_CANCEL` on success.
    status: dto::OrderStatus,
}

// One market+symbol per request; every item is placed on that single
// book — matches the chain ABI's `PrivateNote.placeBatch(eventId,
// oracleListHash, tokenType, OrderBookOrder[])`. Per-item field names
// mirror `POST /api/v1/order` so a client can reuse the same type for
// both endpoints.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct BatchOrdersRequest {
    /// Market address.
    #[salvo(schema(value_type = String))]
    market_address: Option<String>,
    /// Outcome-token symbol shared by every item.
    #[salvo(schema(value_type = String))]
    symbol: Option<String>,
    /// Orders to place atomically. At most `outcome.maxBatchSize`
    /// items.
    #[salvo(schema(value_type = Vec<BatchOrdersRequestItem>))]
    orders: Option<Vec<BatchOrdersRequestItem>>,
}

// Same `Option`-plus-`value_type` treatment as `CreateOrderRequest`,
// for the same missing-vs-invalid reason.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct BatchOrdersRequestItem {
    /// Client-supplied order id. Auto-generated by the backend when
    /// omitted.
    new_order_client_id: Option<String>,
    #[salvo(schema(value_type = dto::OrderSide))]
    side: Option<String>,
    /// Order quantity as a decimal string.
    #[salvo(schema(value_type = String))]
    quantity: Option<String>,
    /// Limit price as a decimal string. Required for `LIMIT` orders.
    price: Option<String>,
    #[serde(rename = "type")]
    #[salvo(schema(value_type = Option<dto::OrderType>))]
    order_type: Option<String>,
    #[salvo(schema(value_type = Option<dto::TimeInForce>))]
    time_in_force: Option<String>,
}

// Same `PENDING_NEW` envelope as the single-order endpoint — see
// CreateOrderResponse for the rationale. Returned in request order;
// one element per accepted item.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct BatchOrderResponseItem {
    /// Echo of the item's `newOrderClientId`, or the backend-generated
    /// id when none was supplied.
    client_order_id: String,
    /// Unix milliseconds. The moment the batch was accepted.
    transact_time: i64,
    /// Always `PENDING_NEW` on success.
    status: dto::OrderStatus,
}

/// Request body for `DELETE /api/v1/batchOrders`. One market+symbol per
/// request, every id is cancelled on that single book — matches the
/// chain ABI's `PrivateNote.placeBatch(eventId, oracleListHash,
/// tokenType, orders = [], cancelIds: uint128[])`.
/// `deny_unknown_fields` is strict on this
/// destructive write surface: a typo like `orderIDs` would otherwise
/// silently deserialise as `order_ids = None` and surface as
/// MissingParameter, masking the real bug — better to 400 with
/// `unknown field` and let the caller fix the key. `CreateOrderRequest`
/// and `BatchOrdersRequest` ship lenient by historical default;
/// flipping them strict is a repo-wide DTO policy change and is
/// tracked separately, not here.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelBatchOrdersRequest {
    market_address: Option<String>,
    symbol: Option<String>,
    order_ids: Option<Vec<String>>,
}

// Manual `ToSchema` impl: `#[derive(ToSchema)]` is incompatible with
// `#[serde(deny_unknown_fields)]` in salvo-oapi-macros 0.74.3 — the
// macro emits `additional_properties(Some(...))` where the builder
// expects `Into<AdditionalProperties<Schema>>`, so the derive fails
// to compile. We keep `deny_unknown_fields` (a strict-input contract
// pinned by `unknown_field_in_body_returns_400_minus_1130`) and
// reproduce here what the derive would have generated: camelCase
// property names and an explicit `additionalProperties: false` so the
// OpenAPI consumer sees the same strict signal as the runtime. All
// three fields are marked required: the `Option<_>` only exists so a
// missing field surfaces as a typed `MissingParameter` via the
// `non_empty(...).ok_or(...)` chain in
// `build_cancel_batch_orders_input`, not because the contract treats
// any field as optional.
impl ToSchema for CancelBatchOrdersRequest {
    fn to_schema(_components: &mut Components) -> salvo_oapi::RefOr<salvo_oapi::schema::Schema> {
        use salvo_oapi::schema::AdditionalProperties;
        use salvo_oapi::Array;
        use salvo_oapi::BasicType;
        use salvo_oapi::Object;
        Object::new()
            .property(
                "marketAddress",
                Object::new().schema_type(BasicType::String).description("Market address."),
            )
            .property(
                "symbol",
                Object::new()
                    .schema_type(BasicType::String)
                    .description("Outcome-token symbol shared by every id."),
            )
            .property(
                "orderIds",
                Array::new()
                    .items(Object::new().schema_type(BasicType::String))
                    .description("Chain-assigned order ids, u64 decimal strings. Cancelled atomically; at most `outcome.maxBatchSize` ids."),
            )
            .required("marketAddress")
            .required("symbol")
            .required("orderIds")
            .additional_properties(AdditionalProperties::FreeForm(false))
            .into()
    }
}

// Response item for `DELETE /api/v1/batchOrders`. Same `PENDING_CANCEL`
// envelope as the single-order DELETE — see `CancelOrderResponse`
// for the rationale. Returned in request order; the array has one
// element per accepted id.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct CancelBatchOrderResponseItem {
    /// Echo of the cancelled order's id.
    order_id: String,
    /// Id recorded at placement; empty string when the order was
    /// placed without a `newOrderClientId`.
    client_order_id: String,
    /// Unix milliseconds. The moment the batch cancel was accepted.
    transact_time: i64,
    /// Always `PENDING_CANCEL` on success.
    status: dto::OrderStatus,
}

/// Request body for `POST /api/v1/buyFullSet`. Field names match
/// docs/api-spec.md §Buy Full Set verbatim. `deny_unknown_fields` is
/// strict on this destructive write surface — same rationale as
/// `CancelBatchOrdersRequest`: a typo like `marketAddres` would
/// otherwise silently deserialise as `market_address = None` and
/// surface as MissingParameter, masking the real bug; -1130 with
/// `unknown field` is the actionable signal.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BuyFullSetRequest {
    market_address: Option<String>,
    collateral: Option<String>,
}

// Manual `ToSchema` impl: `#[derive(ToSchema)]` is incompatible with
// `#[serde(deny_unknown_fields)]` in salvo-oapi-macros 0.74.3 — same
// reason as `CancelBatchOrdersRequest` above.
//
// Both fields are marked required even though the Rust struct uses
// `Option<String>`: the `Option` only exists so a missing field
// surfaces as a typed `MissingParameter` (-1102) via the
// `non_empty(...).ok_or(...)` chain in the handler, not because the
// API contract treats either field as optional. The schema reflects
// the runtime contract so codegen'd clients get an actionable
// signal.
impl ToSchema for BuyFullSetRequest {
    fn to_schema(_components: &mut Components) -> salvo_oapi::RefOr<salvo_oapi::schema::Schema> {
        use salvo_oapi::schema::AdditionalProperties;
        use salvo_oapi::BasicType;
        use salvo_oapi::Object;
        Object::new()
            .property(
                "marketAddress",
                Object::new().schema_type(BasicType::String).description("Market address."),
            )
            .property(
                "collateral",
                Object::new()
                    .schema_type(BasicType::String)
                    .description("Quote-asset amount to spend, as a decimal string. Spent from the caller's free balance; any remainder that does not divide evenly is refunded."),
            )
            .required("marketAddress")
            .required("collateral")
            .additional_properties(AdditionalProperties::FreeForm(false))
            .into()
    }
}

// Minimal acceptance envelope per docs/api-spec.md §Buy Full Set: the
// resulting collateral debit and outcome-token credits become visible
// through `GET /api/v1/account` and `GET /api/v1/account/balances`
// once the chain confirms, so the synchronous response carries only
// the echoed identifier plus the moment we accepted.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct BuyFullSetResponse {
    /// Echo of the request's `marketAddress`.
    market_address: String,
    /// Unix milliseconds. The moment the request was accepted.
    transact_time: i64,
}

/// Read the authenticated identity from the depot and enforce the
/// endpoint's required permission in one call. Protected handlers
/// must call this rather than `depot.obtain::<AuthContext>()`
/// directly: the function signature makes the permission requirement
/// non-optional, so a new private endpoint cannot read its caller's
/// identity without naming the authorization it requires.
fn require_auth(depot: &Depot, permission: Permission) -> Result<&AuthContext, ApiError> {
    let ctx = depot.obtain::<AuthContext>().map_err(|err| {
        error!(?err, "AuthContext missing in protected handler");
        ApiError::from(DomainError::Unexpected)
    })?;
    ctx.require(permission)?;
    Ok(ctx)
}

/// Read a strict-shape JSON body from `req`. Distinguishes (in the
/// emitted `warn!`) between transport-level read failure, empty body,
/// malformed JSON, truncated JSON, and serde shape mismatch — so ops
/// can grep one reason tag per failure mode instead of inspecting
/// debug-repr substrings. Body-level failures (empty / malformed /
/// truncated / shape mismatch) collapse to `InvalidParameter` (-1130);
/// the request reached us intact and is shape-wrong, which is a 400.
/// Transport-level read failure (dropped TCP, truncated upload) maps
/// to `Unexpected` (-1000 / 500): we never got a request to classify,
/// matching the principle for chain-gateway transport failures in
/// `docs/tech-specs/write-api.md`.
///
/// `route` flows into the log line so a multi-handler regression
/// (e.g. an HMAC hoop that started double-consuming the body) shows
/// up under one queryable tag per endpoint.
async fn parse_strict_body<T: serde::de::DeserializeOwned>(
    req: &mut Request,
    route: &'static str,
) -> Result<T, ApiError> {
    let body_bytes = req.payload().await.map_err(|err| {
        warn!(route, reason = "transport", ?err, "body read failed");
        ApiError::from(DomainError::Unexpected)
    })?;
    if body_bytes.is_empty() {
        warn!(route, reason = "empty", "body did not parse");
        return Err(ApiError::from(DomainError::InvalidParameter));
    }
    serde_json::from_slice(body_bytes).map_err(|err| {
        // `serde_json::Category` separates structural failures
        // (`Syntax`) from prematurely truncated payloads (`Eof`) and
        // unknown / wrong-typed fields (`Data`); `Io` is structurally
        // unreachable for `from_slice` but enumerated for exhaustiveness.
        let reason = match err.classify() {
            serde_json::error::Category::Syntax => "malformed",
            serde_json::error::Category::Eof => "truncated",
            serde_json::error::Category::Data => "shape_mismatch",
            serde_json::error::Category::Io => "serde_io",
        };
        warn!(route, reason, ?err, "body did not parse");
        ApiError::from(DomainError::InvalidParameter)
    })
}

// Auth hoop has already verified the request; `require_auth(Trade)`
// enforces the spec permission. The handler translates the parsed
// request + `AuthContext` into a `NewOrderInput`, hands the use case
// off, and shapes the three-field response. The chain-assigned
// `orderId` is not in this response by design — it arrives later via
// `GET /api/v1/orders` once the indexer projects `OrderBook.OrderPlaced`.
#[endpoint(
    tags("trading"),
    summary = "Submit a new order",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
    ),
    request_body = CreateOrderRequest,
    security(("apiKey" = [])),
)]
async fn create_order(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<CreateOrderResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    // Body has been HMAC-verified upstream; `parse_strict_body` tags the
    // failure mode in the warn so ops can grep `reason=malformed` vs
    // `reason=transport` etc.
    let body: CreateOrderRequest = parse_strict_body(req, "POST /api/v1/order").await?;

    let (now_seconds, now_ms) = now_pair();
    let input = build_new_order_input(body, ctx, now_seconds, now_ms)?;

    let use_case = CreateOrderUseCase::new(state.repo, state.chain_sender);
    let submitted = use_case.execute(input).await.map_err(ApiError::from)?;

    Ok(Json(CreateOrderResponse {
        client_order_id: submitted.client_order_id,
        transact_time: now_ms,
        status: dto::OrderStatus::PendingNew,
    }))
}

/// Translate the raw body + auth context into a `NewOrderInput`.
/// Returns typed `DomainError`s for missing or unknown enum values;
/// the use case takes over for resolution and validation.
fn build_new_order_input(
    body: CreateOrderRequest,
    ctx: AuthContext,
    now_seconds: i64,
    now_ms: i64,
) -> Result<NewOrderInput, ApiError> {
    let market_address =
        non_empty(body.market_address).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let symbol = non_empty(body.symbol).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let side_str = non_empty(body.side).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let side = OrderSide::parse(&side_str).ok_or(ApiError::from(DomainError::InvalidParameter))?;
    let quantity = non_empty(body.quantity).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let order_type = match body.order_type.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => OrderType::parse(s).ok_or(ApiError::from(DomainError::InvalidParameter))?,
        None => OrderType::Limit,
    };
    let time_in_force = match body.time_in_force.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        Some(s) => {
            Some(TimeInForce::parse(s).ok_or(ApiError::from(DomainError::InvalidParameter))?)
        }
        None => None,
    };

    Ok(NewOrderInput {
        trading_pn: ctx.trading_pn,
        market_address: MarketAddress(market_address),
        symbol: Symbol(symbol),
        side,
        quantity,
        price: non_empty(body.price),
        order_type,
        time_in_force,
        client_order_id: non_empty(body.new_order_client_id),
        now_seconds,
        now_ms,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

// Auth hoop verified the request; this handler enforces `TRADE`,
// parses query params, hands off to the use case, and shapes the
// four-field `PENDING_CANCEL` response.
#[endpoint(
    tags("trading"),
    summary = "Cancel an order by orderId",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
        ("marketAddress" = String, Query, description = "Market address."),
        ("symbol" = String, Query, description = "Outcome-token symbol."),
        ("orderId" = String, Query, description = "Chain-assigned order id, u64 decimal string."),
    ),
    security(("apiKey" = [])),
)]
async fn delete_order(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<CancelOrderResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_empty_query(req, "marketAddress")
        .ok_or(ApiError::from(DomainError::MissingParameter))?;
    let symbol =
        non_empty_query(req, "symbol").ok_or(ApiError::from(DomainError::MissingParameter))?;
    let order_id_raw =
        non_empty_query(req, "orderId").ok_or(ApiError::from(DomainError::MissingParameter))?;
    // u64 ceiling at the public boundary — same `serde_json` /
    // `arbitrary_precision` constraint as `clientOrderId` for POST.
    // Overflow or non-numeric → 400 / -1130.
    let order_id =
        order_id_raw.parse::<u64>().map_err(|_| ApiError::from(DomainError::InvalidParameter))?;

    let (now_seconds, now_ms) = now_pair();
    let input = CancelOrderInput {
        trading_pn: ctx.trading_pn,
        market_address: MarketAddress(market_address),
        symbol: Symbol(symbol),
        order_id,
        now_seconds,
        now_ms,
    };

    let use_case = CancelOrderUseCase::new(state.repo, state.chain_sender);
    let cancelled = use_case.execute(input).await.map_err(ApiError::from)?;

    Ok(Json(CancelOrderResponse {
        order_id: order_id.to_string(),
        // api-spec §Cancel Order: empty string when the order was
        // placed without a `newOrderClientId`.
        client_order_id: cancelled.client_order_id.unwrap_or_default(),
        transact_time: now_ms,
        status: dto::OrderStatus::PendingCancel,
    }))
}

// Parses one (marketAddress, symbol) plus `orders[]`, hands off to
// the batch-create use case, and shapes a flat array of `PENDING_NEW`
// envelopes. The use case enforces non-empty `orders[]` and the
// `outcome.max_batch_size` cap; the chain enforces atomic placement.
#[endpoint(
    tags("trading"),
    summary = "Submit a batch of orders atomically",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
    ),
    request_body = BatchOrdersRequest,
    security(("apiKey" = [])),
)]
async fn create_batch_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<Vec<BatchOrderResponseItem>>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let body: BatchOrdersRequest = parse_strict_body(req, "POST /api/v1/batchOrders").await?;

    let (now_seconds, now_ms) = now_pair();
    let input = build_batch_orders_input(body, ctx, now_seconds, now_ms)?;

    let use_case =
        CreateBatchOrdersUseCase::new(state.repo, state.chain_sender, state.max_batch_size);
    let submitted = use_case.execute(input).await.map_err(ApiError::from)?;

    let response = submitted
        .items
        .into_iter()
        .map(|item| BatchOrderResponseItem {
            client_order_id: item.client_order_id,
            transact_time: now_ms,
            status: dto::OrderStatus::PendingNew,
        })
        .collect();
    Ok(Json(response))
}

/// Translate the parsed body + auth context into a
/// `CreateBatchOrdersInput`. The top-level (`marketAddress`, `symbol`)
/// resolves once for the whole request; per-item fields go through
/// the same trim+enum-parse the single-order handler runs. Any
/// missing or unknown enum value on any item collapses the whole
/// request with the matching `DomainError` (`MissingParameter`
/// → -1102 or `InvalidParameter` → -1130). Empty `orders[]` parses
/// here without complaint; the use case enforces non-empty + the
/// per-outcome `max_batch_size` cap.
fn build_batch_orders_input(
    body: BatchOrdersRequest,
    ctx: AuthContext,
    now_seconds: i64,
    now_ms: i64,
) -> Result<CreateBatchOrdersInput, ApiError> {
    let market_address =
        non_empty(body.market_address).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let symbol = non_empty(body.symbol).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let raw_orders = body.orders.ok_or(ApiError::from(DomainError::MissingParameter))?;

    let mut orders = Vec::with_capacity(raw_orders.len());
    // Per-item enum/missing-field parse runs before the use case so a
    // batch that fails here never reaches `validate_and_encode_order_item`
    // (which logs `phase = "validate"`). The matching `warn!`s below
    // give ops the same `item_index` when a 60-item batch trips on
    // item 47. `phase = "parse"` lets a single substring query
    // (`batchOrders rejected`) span the parse, validate, and shape
    // gates without OR'ing three different messages.
    for (item_index, item) in raw_orders.into_iter().enumerate() {
        let side_str = non_empty(item.side).ok_or_else(|| {
            warn!(phase = "parse", item_index, field = "side", "batchOrders rejected");
            ApiError::from(DomainError::MissingParameter)
        })?;
        let side = OrderSide::parse(&side_str).ok_or_else(|| {
            warn!(
                phase = "parse",
                item_index,
                field = "side",
                value = %side_str,
                "batchOrders rejected",
            );
            ApiError::from(DomainError::InvalidParameter)
        })?;
        let quantity = non_empty(item.quantity).ok_or_else(|| {
            warn!(phase = "parse", item_index, field = "quantity", "batchOrders rejected");
            ApiError::from(DomainError::MissingParameter)
        })?;
        let order_type = match item.order_type.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => OrderType::parse(s).ok_or_else(|| {
                warn!(
                    phase = "parse",
                    item_index,
                    field = "type",
                    value = %s,
                    "batchOrders rejected",
                );
                ApiError::from(DomainError::InvalidParameter)
            })?,
            None => OrderType::Limit,
        };
        let time_in_force =
            match item.time_in_force.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(s) => Some(TimeInForce::parse(s).ok_or_else(|| {
                    warn!(
                        phase = "parse",
                        item_index,
                        field = "timeInForce",
                        value = %s,
                        "batchOrders rejected",
                    );
                    ApiError::from(DomainError::InvalidParameter)
                })?),
                None => None,
            };
        orders.push(BatchOrderInputItem {
            side,
            quantity,
            price: non_empty(item.price),
            order_type,
            time_in_force,
            client_order_id: non_empty(item.new_order_client_id),
        });
    }

    Ok(CreateBatchOrdersInput {
        trading_pn: ctx.trading_pn,
        market_address: MarketAddress(market_address),
        symbol: Symbol(symbol),
        orders,
        now_seconds,
        now_ms,
    })
}

// Parses one `(marketAddress, symbol)` plus `orderIds[]`, hands off
// to `CancelBatchOrdersUseCase`, and shapes a flat array of
// `PENDING_CANCEL` envelopes. The use case enforces non-empty
// `orderIds[]`, intra-batch dedup, the `outcome.max_batch_size` cap,
// and bulk order resolution. The chain (a cancel-only
// `PrivateNote.placeBatch`) accepts the list atomically under one
// `_busy` window.
#[endpoint(
    tags("trading"),
    summary = "Cancel a batch of orders atomically",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
    ),
    request_body = CancelBatchOrdersRequest,
    security(("apiKey" = [])),
)]
async fn delete_batch_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<Vec<CancelBatchOrderResponseItem>>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let body: CancelBatchOrdersRequest =
        parse_strict_body(req, "DELETE /api/v1/batchOrders").await?;

    let (now_seconds, now_ms) = now_pair();
    let input = build_cancel_batch_orders_input(body, ctx, now_seconds, now_ms)?;
    // Use the `now_ms` the input carries so per-item `transactTime`
    // matches the value the use case logged and dispatched against.
    let response_now_ms = input.now_ms;

    // Audit breadcrumb fields captured before move into `execute`.
    // Without `order_ids` in the reject log a 50-id failure leaves ops
    // with no handle to grep the user's claim against — the
    // chain_sender `info!` only fires after resolution succeeds, so
    // anything that aborts upstream (validation,
    // resolve_for_cancel_batch shortfall, PnBusy, MarketInconsistent)
    // is otherwise silent at the request level.
    let audit_pn = input.trading_pn.pn_address.clone();
    let audit_market = input.market_address.0.clone();
    let audit_symbol = input.symbol.0.clone();
    let audit_order_ids = input.order_ids.clone();

    let use_case =
        CancelBatchOrdersUseCase::new(state.repo, state.chain_sender, state.max_batch_size);
    let cancelled = use_case.execute(input).await.map_err(|err| {
        warn!(
            pn = %audit_pn,
            market_address = %audit_market,
            symbol = %audit_symbol,
            order_count = audit_order_ids.len(),
            order_ids = ?audit_order_ids,
            err = ?err,
            "cancel_batch_orders failed",
        );
        ApiError::from(err)
    })?;

    let response = cancelled
        .into_iter()
        .map(|item| CancelBatchOrderResponseItem {
            order_id: item.order_id.to_string(),
            client_order_id: item.client_order_id.unwrap_or_default(),
            transact_time: response_now_ms,
            status: dto::OrderStatus::PendingCancel,
        })
        .collect();
    Ok(Json(response))
}

/// Translate the parsed body + auth context into a
/// `CancelBatchOrdersInput`. Mirrors `build_batch_orders_input` for the
/// cancel path: each `orderIds[]` element is parsed as `u64` at the
/// public boundary — same `serde_json` / `arbitrary_precision`
/// constraint as `clientOrderId` and single-order DELETE.
fn build_cancel_batch_orders_input(
    body: CancelBatchOrdersRequest,
    ctx: AuthContext,
    now_seconds: i64,
    now_ms: i64,
) -> Result<CancelBatchOrdersInput, ApiError> {
    let market_address =
        non_empty(body.market_address).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let symbol = non_empty(body.symbol).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let raw_ids = body.order_ids.ok_or(ApiError::from(DomainError::MissingParameter))?;

    let mut order_ids = Vec::with_capacity(raw_ids.len());
    // Symmetric with `build_batch_orders_input`: same `phase = "parse"`
    // tag and `item_index` so ops can pinpoint which element of a
    // long `orderIds[]` rejected, and the shared "cancelBatch rejected"
    // substring spans parse + later shape gates.
    for (item_index, raw) in raw_ids.into_iter().enumerate() {
        let trimmed = non_empty(Some(raw)).ok_or_else(|| {
            warn!(phase = "parse", item_index, field = "orderId", "cancelBatch rejected");
            ApiError::from(DomainError::MissingParameter)
        })?;
        let id = trimmed.parse::<u64>().map_err(|err| {
            warn!(
                phase = "parse",
                item_index,
                field = "orderId",
                value = %trimmed,
                ?err,
                "cancelBatch rejected",
            );
            ApiError::from(DomainError::InvalidParameter)
        })?;
        order_ids.push(id);
    }

    Ok(CancelBatchOrdersInput {
        trading_pn: ctx.trading_pn,
        market_address: MarketAddress(market_address),
        symbol: Symbol(symbol),
        order_ids,
        now_seconds,
        now_ms,
    })
}

// Auth hoop verified the request; this handler enforces `TRADE`,
// hands the parsed body off to the use case, and shapes the minimal
// two-field acceptance envelope. The chain-side outcome (collateral
// debit, outcome-token credits) surfaces later through
// `GET /api/v1/account` / `/api/v1/account/balances`.
#[endpoint(
    tags("positions"),
    summary = "Buy a full set of outcome tokens",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
    ),
    request_body = BuyFullSetRequest,
    security(("apiKey" = [])),
)]
async fn buy_full_set(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<BuyFullSetResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let body: BuyFullSetRequest = parse_strict_body(req, "POST /api/v1/buyFullSet").await?;

    let market_address =
        non_empty(body.market_address).ok_or(ApiError::from(DomainError::MissingParameter))?;
    let collateral =
        non_empty(body.collateral).ok_or(ApiError::from(DomainError::MissingParameter))?;

    let (now_seconds, now_ms) = now_pair();
    let input = BuyFullSetInput {
        trading_pn: ctx.trading_pn,
        market_address: MarketAddress(market_address.clone()),
        collateral,
        now_seconds,
    };

    let use_case = BuyFullSetUseCase::new(state.repo, state.ref_repo, state.chain_sender);
    use_case.execute(input).await.map_err(ApiError::from)?;

    Ok(Json(BuyFullSetResponse { market_address, transact_time: now_ms }))
}

/// Account balances aggregated across all markets the authenticated PN holds.
#[endpoint(
    tags("account"),
    summary = "Account balances",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
    ),
    security(("apiKey" = [])),
)]
async fn get_account(
    _req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<AccountResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::UserData)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let now_ms = now_pair().1;
    let use_case =
        dodex_application::GetAccountUseCase::new(state.pn_reader.clone(), state.ref_repo.clone());
    let out = use_case
        .execute(dodex_application::GetAccountInput {
            account_id: ctx.account_id,
            pn_address: ctx.trading_pn.pn_address.clone(),
            now_ms,
        })
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_account"))?;

    Ok(Json(AccountResponse::from_domain(out)))
}

/// Per-outcome balances on a single market for the authenticated PN.
#[endpoint(
    tags("account"),
    summary = "Market balances",
    parameters(
        ("X-DODEX-APIKEY" = String, Header, description = "API key issued by the Dodex backend."),
        ("timestamp" = i64, Query, description = "Unix milliseconds. Included in the signed payload."),
        ("recvWindow" = Option<i64>, Query, description = "Request validity window in milliseconds. Default 5000, max 60000."),
        ("signature" = String, Query, description = "Hex HMAC SHA-256 of canonicalQueryString + canonicalRequestBody."),
        ("marketAddress" = String, Query, description = "Market address."),
    ),
    security(("apiKey" = [])),
)]
async fn get_account_balances(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<MarketBalancesResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::UserData)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_blank_query(req, "marketAddress")?
        .ok_or(ApiError::from(DomainError::MissingParameter))?;

    let now_ms = now_pair().1;
    let use_case = dodex_application::GetMarketBalancesUseCase::new(
        state.pn_reader.clone(),
        state.repo.clone(),
        balances_stake_hash,
    );
    let out = use_case
        .execute(dodex_application::GetMarketBalancesInput {
            pn_address: ctx.trading_pn.pn_address.clone(),
            market_address: MarketAddress(market_address),
            now_ms,
        })
        .await
        .map_err(|err| map_domain_or_unexpected(err, "get_account_balances"))?;

    Ok(Json(MarketBalancesResponse::from_domain(out)))
}

/// Production adapter for `application::StakeHasher`. All failure modes
/// surface as `MarketInconsistent` (503):
///
/// - Non-numeric `event_id` / `oracle_list_hash`: read-model corruption
///   (the indexer writes only valid numerics).
/// - `tvm_hash::stake_hash` failure: most plausibly an oversized BigUint
///   that does not fit `uint256` — also read-model corruption, since the
///   chain enforces `uint256` and the indexer ingests directly from
///   chain events. A genuine `tvm_abi` packing bug would surface the
///   same way; both warrant a 503 + ops triage rather than 500.
///
/// `token_type` is `u32` because the repo boundary already validates
/// that the DB value is non-negative — no secondary cast is needed here.
fn balances_stake_hash(
    event_id: &str,
    oracle_list_hash: &str,
    token_type: u32,
) -> Result<String, dodex_domain::DomainError> {
    use std::str::FromStr;

    use num_bigint::BigUint;
    let event = BigUint::from_str(event_id).map_err(|err| {
        tracing::warn!(event_id, error = %err, "event_id is not a numeric string");
        dodex_domain::DomainError::MarketInconsistent
    })?;
    let oracle = BigUint::from_str(oracle_list_hash).map_err(|err| {
        tracing::warn!(oracle_list_hash, error = %err, "oracle_list_hash is not a numeric string");
        dodex_domain::DomainError::MarketInconsistent
    })?;
    dodex_infrastructure::tvm_hash::stake_hash(&event, &oracle, token_type).map_err(|err| {
        tracing::warn!(
            event_id, oracle_list_hash, token_type,
            error = ?err,
            "stake_hash computation failed — oversized uint256 (read-model corruption) or tvm_abi packing bug",
        );
        dodex_domain::DomainError::MarketInconsistent
    })
}

/// Assemble the production router around `state`. Kept as a separate
/// function so integration tests can drive the same router with a
/// test-DB pool through Salvo's in-process `TestClient`; production
/// callers reach it indirectly through `run`.
#[doc(hidden)]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .hoop(inject(state))
        // request_timeout runs *after* inject so the hoop can read
        // `AppState.request_timeout` from the depot. Everything
        // below — public routes, the auth subrouter, the handler
        // chain — runs inside its budget.
        .hoop(timeout_hoop::enforce_request_timeout)
        .push(Router::with_path("readiness").get(readiness))
        .push(Router::with_path("api/v1/markets").get(get_markets))
        .push(Router::with_path("api/v1/depth").get(get_depth))
        .push(Router::with_path("api/v1/oracles").get(get_oracles))
        .push(
            // Subrouter scoped to private endpoints. The auth hoop
            // runs only for routes pushed under this branch, so the
            // public `markets` / `depth` endpoints above remain
            // `NONE`-security per docs/api-spec.md §Endpoint Summary.
            Router::new()
                .hoop(auth_hoop::authenticate)
                .push(
                    Router::with_path("api/v1/order")
                        .post(create_order)
                        .delete(delete_order),
                )
                .push(Router::with_path("api/v1/orders").get(get_orders))
                .push(
                    Router::with_path("api/v1/batchOrders")
                        .post(create_batch_orders)
                        .delete(delete_batch_orders),
                )
                .push(Router::with_path("api/v1/account").get(get_account))
                .push(Router::with_path("api/v1/account/balances").get(get_account_balances))
                .push(Router::with_path("api/v1/buyFullSet").post(buy_full_set)),
        )
}

/// Build the OpenAPI document by walking the route tree and collecting
/// metadata registered by each `#[endpoint]` attribute. Stateless: no
/// repo, no chain sender, no authenticator. The `gen-openapi` binary is
/// the only intended caller; tests may also use it as a golden-file
/// snapshot source.
pub fn openapi_doc() -> OpenApi {
    // Strip the `dodex_api.` crate prefix from schema names. `set_namer` is
    // process-global; gen-openapi is the only caller and runs single-threaded,
    // so the race window doesn't exist in practice.
    salvo_oapi::naming::set_namer(salvo_oapi::naming::FlexNamer::new().short_mode(true));

    let router = Router::new()
        .push(Router::with_path("readiness").get(readiness))
        .push(Router::with_path("api/v1/markets").get(get_markets))
        .push(Router::with_path("api/v1/depth").get(get_depth))
        .push(Router::with_path("api/v1/oracles").get(get_oracles))
        .push(Router::with_path("api/v1/order").post(create_order).delete(delete_order))
        .push(Router::with_path("api/v1/orders").get(get_orders))
        .push(
            Router::with_path("api/v1/batchOrders")
                .post(create_batch_orders)
                .delete(delete_batch_orders),
        )
        .push(Router::with_path("api/v1/account").get(get_account))
        .push(Router::with_path("api/v1/account/balances").get(get_account_balances))
        .push(Router::with_path("api/v1/buyFullSet").post(buy_full_set));

    OpenApi::new("Dodex REST API", env!("CARGO_PKG_VERSION"))
        .info(
            Info::new("Dodex REST API", env!("CARGO_PKG_VERSION")).description(
                "Public REST API for the Dodex prediction-market exchange. See docs/api-spec.md for the long-form contract.",
            ),
        )
        .add_server(OapiServer::new("https://api.dodex.example.com"))
        .add_security_scheme(
            "apiKey",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-DODEX-APIKEY",
                "API key issued by the Dodex backend. Signed requests also carry `timestamp`, `signature`, and an optional `recvWindow` as query parameters.",
            ))),
        )
        .merge_router(&router)
}

/// Production bootstrap. `main` defers to this so the executable shim
/// stays a single line and every meaningful step is testable in
/// isolation.
pub async fn run() -> anyhow::Result<()> {
    // When LOG_DIR is set, these guards keep the background file-log writer
    // alive for the lifetime of the process; `run()` serves until shutdown.
    let _guards = dodex_logging::init("api");

    let config_path =
        env::var("APP_CONFIG").unwrap_or_else(|_| "config/api.local.yaml".to_string());
    let config = ApiConfig::load_from_path(&config_path)?;

    let kek = Arc::new(Kek::from_hex(&config.auth.kek_hex).context("auth.kek_hex")?);

    let pool = build_pool(&config.common.database).await?;

    // Migrations + seeding are gated on `auth.seed_accounts`. With the
    // flag off the api stays read-only against the schema (the indexer
    // applies migrations as today). With the flag on the api applies
    // migrations itself before inserting the hard-coded credentials —
    // sqlx::migrate! uses an advisory lock so racing with the indexer
    // on a fresh DB is safe.
    if config.auth.seed_accounts {
        database::run_migrations(&pool).await?;
        seed::seed_accounts(&pool, &kek).await?;
    }

    info!("api running with postgres read-model repository");
    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek, &config.auth));
    let chain_sender: SharedChainSender = Arc::new(DexChainSender::new(
        vec![config.chain.gateway_endpoint.clone()],
        Duration::from_millis(config.chain.place_order_timeout_ms),
        Duration::from_millis(config.chain.cancel_order_timeout_ms),
        Duration::from_millis(config.chain.place_batch_timeout_ms),
        Duration::from_millis(config.chain.cancel_batch_timeout_ms),
        Duration::from_millis(config.chain.split_full_set_timeout_ms),
    )?);
    let graphql = Arc::new(dodex_infrastructure::graphql::GraphqlClient::new(
        config.graphql.endpoint.clone(),
        Duration::from_millis(config.graphql.request_timeout_ms),
    )?);
    let pn_reader: SharedPnReader =
        Arc::new(dodex_infrastructure::pn_state_reader::GraphqlPnStateReader::new(graphql)?);
    let ref_repo: SharedRefRepo = Arc::new(
        dodex_infrastructure::postgres_repo::PostgresReferenceRepository::new(pool.clone()),
    );
    let state = AppState::new(repo, authenticator, chain_sender, pn_reader, ref_repo)
        .with_request_timeout(Duration::from_millis(config.server.request_timeout_ms))
        .with_max_batch_size(config.chain.max_batch_size);

    // The API is intentionally restart-to-reconfigure. None of the live
    // request paths read runtime config — pool, server bind, request_timeout
    // are all baked at startup — so a SIGUSR1 reload-loop would be cargo-cult.
    // The indexer keeps its loop because its background tasks do consume new
    // config (graphql endpoint/timeouts, ignored_addresses, intervals).

    // TODO(auth-phase): CORS hoop. Browsers calling private endpoints
    // need preflight + Access-Control-Allow-* headers; the auth-error
    // path currently returns 401 without them, which a browser client
    // surfaces as an opaque network error rather than the spec body.

    let router = build_router(state);

    let acceptor = TcpListener::new((config.server.host.clone(), config.server.port)).bind().await;
    info!(host = %config.server.host, port = config.server.port, "api server starting");
    Server::new(acceptor).serve(router).await;
    Ok(())
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}

/// Capture wall-clock once and project it into both (seconds, ms) so a
/// single request can derive market status against the same moment it
/// reports as `transactTime`. Avoids the (rare) race where one clock
/// read crosses a second boundary mid-request.
fn now_pair() -> (i64, i64) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.as_millis() as i64)
}

#[cfg(test)]
mod error_status_tests {
    use dodex_domain::DomainError;
    use salvo::http::StatusCode;

    use super::ApiError;

    #[test]
    fn market_inconsistent_maps_to_503() {
        // The read paths (get_depth, list_orders) fail closed with
        // MarketInconsistent when the read-model is off the chain grid. The
        // client must see 503 (transient, retryable) rather than 500 — this
        // pins the status code the descale fail-closed behaviour relies on,
        // which is otherwise asserted only as a domain variant, never as HTTP.
        assert_eq!(
            ApiError::from(DomainError::MarketInconsistent).status(),
            StatusCode::SERVICE_UNAVAILABLE,
        );
    }
}

#[cfg(test)]
mod balances_hasher_tests {
    use dodex_domain::DomainError;

    use super::*;

    #[test]
    fn non_numeric_event_id_returns_market_inconsistent() {
        let err = balances_stake_hash("not-a-number", "42", 1).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
    }

    #[test]
    fn non_numeric_oracle_list_hash_returns_market_inconsistent() {
        let err = balances_stake_hash("42", "garbage", 1).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
    }

    #[test]
    fn happy_path_produces_64_hex_chars() {
        let h = balances_stake_hash("42", "24", 1).expect("ok");
        assert!(h.starts_with("0x"));
        assert_eq!(h.len(), 2 + 64);
    }

    #[test]
    fn oversized_event_id_returns_market_inconsistent() {
        use dodex_domain::DomainError;
        // An `event_id` that exceeds `uint256::MAX` cannot be packed into the
        // ABI tuple `tvm_hash::stake_hash` builds. Since the chain enforces
        // `uint256` at write time, an oversized BigUint reaching this code path
        // is read-model corruption — surface as MarketInconsistent (503) so
        // operators get the same triage signal as other DB-shape failures.
        // 2^256 ≈ 1.16 × 10^77; 10^78 is comfortably above that ceiling.
        let huge = "1".to_string() + &"0".repeat(78);
        let err = balances_stake_hash(&huge, "1", 1).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent), "got {err:?}");
    }
}

#[cfg(test)]
mod dto_tests {
    use dodex_domain::AssetBalance;
    use dodex_domain::OutcomeBalance;

    use super::*;

    #[test]
    fn account_response_uses_camel_case_and_string_amounts() {
        let resp = AccountResponse::from_domain(dodex_domain::AccountBalances {
            account_id: uuid::Uuid::nil(),
            update_time_ms: 1710000000000,
            balances: vec![AssetBalance {
                asset: "NACKL".into(),
                free: "10.000000000".into(),
                locked: "1.500000000".into(),
            }],
        });
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["accountId"], "00000000-0000-0000-0000-000000000000");
        assert_eq!(v["updateTime"], 1_710_000_000_000i64);
        assert_eq!(v["balances"][0]["asset"], "NACKL");
        assert_eq!(v["balances"][0]["free"], "10.000000000");
        assert_eq!(v["balances"][0]["locked"], "1.500000000");
    }

    #[test]
    fn market_balances_response_uses_camel_case() {
        let resp = MarketBalancesResponse::from_domain(dodex_domain::MarketBalances {
            market_address: dodex_domain::MarketAddress("0:m".into()),
            update_time_ms: 1710000000000,
            balances: vec![OutcomeBalance {
                outcome_id: 1,
                symbol: dodex_domain::Symbol("PM-X-YES".into()),
                free: "5.50".into(),
                locked_in_orders: "1000.00".into(),
            }],
        });
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["marketAddress"], "0:m");
        assert_eq!(v["updateTime"], 1_710_000_000_000i64);
        assert_eq!(v["balances"][0]["outcomeId"], 1);
        assert_eq!(v["balances"][0]["symbol"], "PM-X-YES");
        assert_eq!(v["balances"][0]["free"], "5.50");
        assert_eq!(v["balances"][0]["lockedInOrders"], "1000.00");
    }

    #[test]
    fn market_to_dto_includes_camel_case_commission_fields() {
        use dodex_domain::Market;
        use dodex_domain::MarketAddress;
        use dodex_domain::MarketEvent;
        use dodex_domain::MarketName;
        use dodex_domain::MarketStatus;
        use dodex_domain::MAKER_COMMISSION;
        use dodex_domain::TAKER_COMMISSION;
        let market = Market {
            market_address: MarketAddress("0:m".into()),
            order_book_address: "0:ob".into(),
            oracle_list_hash: "0xdead".into(),
            market_name: MarketName("PM".into()),
            status: MarketStatus::Trading,
            quote_asset: "USDC".into(),
            token_type: 1,
            maker_commission: MAKER_COMMISSION.to_string(),
            taker_commission: TAKER_COMMISSION.to_string(),
            created_at: 0,
            timings: None,
            event: MarketEvent {
                event_id: "0x0".into(),
                event_name: None,
                description: None,
                oracles: vec![],
            },
            terminal: None,
            outcomes: vec![],
        };
        let dto = market_to_dto(market, 10);
        let v = serde_json::to_value(&dto).unwrap();
        // Snapshot: literals catch silent drift in the domain constants.
        assert_eq!(v["makerCommission"], "-0.0003375");
        assert_eq!(v["takerCommission"], "0.0004500");
        // tokenType must still precede the new fields per spec order.
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let pos = |k: &str| keys.iter().position(|x| *x == k).expect(k);
        assert!(pos("tokenType") < pos("makerCommission"));
        assert!(pos("makerCommission") < pos("takerCommission"));
        assert!(pos("takerCommission") < pos("createdAt"));
    }
}
