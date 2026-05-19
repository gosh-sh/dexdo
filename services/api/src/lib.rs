// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

mod auth_hoop;
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
use dodex_application::CancelOrderInput;
use dodex_application::CancelOrderUseCase;
use dodex_application::ChainOrderSender;
use dodex_application::CreateOrderUseCase;
use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsUseCase;
use dodex_application::GetOpenOrdersUseCase;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_application::NewOrderInput;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketStatus;
use dodex_domain::OpenOrder;
use dodex_domain::OrderSide;
use dodex_domain::OrderStatus;
use dodex_domain::OrderType;
use dodex_domain::Permission;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TerminalKind;
use dodex_domain::TimeInForce;
use dodex_domain::Timings;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::chain_sender::BeeDexChainSender;
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
#[derive(Clone)]
pub struct AppState {
    pub(crate) repo: SharedRepo,
    pub(crate) authenticator: SharedAuth,
    pub(crate) chain_sender: SharedChainSender,
    /// Per-request wall-clock budget enforced by the `request_timeout`
    /// hoop on every route. `Duration::ZERO` disables the hoop, which
    /// is the implicit default `AppState::new` chooses so tests that
    /// don't care about timeouts can ignore it.
    pub(crate) request_timeout: Duration,
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
    ) -> Self {
        Self { repo, authenticator, chain_sender, request_timeout: Duration::ZERO }
    }

    #[doc(hidden)]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketsResponse {
    server_time: i64,
    next_cursor: Option<String>,
    has_more: bool,
    markets: Vec<MarketDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketDto {
    market_address: String,
    order_book_address: String,
    market_name: String,
    status: &'static str,
    quote_asset: String,
    token_type: i32,
    created_at: i64,
    timings: Option<TimingsDto>,
    event: EventDto,
    terminal: Option<TerminalDto>,
    outcomes: Vec<OutcomeDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingsDto {
    stake_start: i64,
    stake_end: i64,
    result_start: i64,
    result_end: i64,
    frozen_at: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventDto {
    event_id: String,
    event_name: Option<String>,
    description: Option<String>,
    oracles: Vec<OracleDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OracleDto {
    name: Option<String>,
    address: Option<String>,
    fee: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalDto {
    kind: &'static str,
    at: i64,
    resolved_outcome_id: Option<u32>,
    cancel_reason: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeDto {
    outcome_id: u32,
    outcome_name: String,
    symbol: String,
    price_precision: u8,
    quantity_precision: u8,
    tick_size: String,
    step_size: String,
    min_notional: String,
    max_batch_size: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DepthResponse {
    market_address: String,
    symbol: String,
    last_update_id: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenOrderResponse {
    market_address: String,
    symbol: String,
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    status: &'static str,
    time_in_force: &'static str,
    #[serde(rename = "type")]
    order_type: &'static str,
    side: &'static str,
    time: i64,
    update_time: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenOrdersPageResponse {
    orders: Vec<OpenOrderResponse>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct ErrorBody {
    code: i32,
    msg: &'static str,
}

#[derive(Debug)]
pub(crate) struct ApiError(DomainError);

impl ApiError {
    pub(crate) fn status(&self) -> StatusCode {
        match self.0 {
            DomainError::AuthRequired
            | DomainError::AuthEnvelopeIncomplete
            | DomainError::TimestampOutsideRecvWindow
            | DomainError::InvalidSignature => StatusCode::UNAUTHORIZED,
            DomainError::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            DomainError::UnknownOrder | DomainError::InvalidMarketOrSymbol => StatusCode::NOT_FOUND,
            // Transient indexer state — fail closed, client retries when
            // the indexer catches up.
            DomainError::MarketInconsistent => StatusCode::SERVICE_UNAVAILABLE,
            // The request_timeout hoop tripped — emit 504 so clients can
            // distinguish "our budget elapsed" from "upstream gateway
            // failed" (502).
            DomainError::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
            // Per-PN serialisation is a chain invariant: only one
            // `placeOrder` per trading PN can be in flight. 429 is the
            // canonical "you sent too many to this PN; back off and
            // retry" — distinct from a 401 (auth) or 400 (bad order).
            DomainError::OrderPnBusy => StatusCode::TOO_MANY_REQUESTS,
            DomainError::Unexpected => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
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

#[handler]
async fn readiness() -> &'static str {
    "ok"
}

const DEFAULT_LIMIT: u16 = 50;
const MAX_LIMIT: u16 = 200;

#[handler]
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
    let page = use_case.execute(request).await.map_err(|err| {
        // Repo emits typed DomainError variants for client-input failures
        // (e.g. cursor decode failure → InvalidParameter). Surface those as
        // their proper HTTP status; everything else is a real 500.
        if let Some(domain) = err.downcast_ref::<DomainError>() {
            return ApiError::from(*domain);
        }
        error!(?err, "list_markets failed");
        ApiError::from(DomainError::Unexpected)
    })?;

    let payload = MarketsResponse {
        server_time: now,
        next_cursor: page.next_cursor,
        has_more: page.has_more,
        markets: page.markets.into_iter().map(market_to_dto).collect(),
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

fn market_to_dto(market: Market) -> MarketDto {
    MarketDto {
        market_address: market.market_address.0,
        order_book_address: market.order_book_address,
        market_name: market.market_name.0,
        status: market.status.as_str(),
        quote_asset: market.quote_asset,
        token_type: market.token_type,
        created_at: market.created_at,
        timings: market.timings.map(timings_to_dto),
        event: event_to_dto(market.event),
        terminal: market.terminal.map(terminal_to_dto),
        outcomes: market.outcomes.into_iter().map(outcome_to_dto).collect(),
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
        kind: match t.kind {
            TerminalKind::Resolved => "RESOLVED",
            TerminalKind::Cancelled => "CANCELLED",
            TerminalKind::Expired => "EXPIRED",
        },
        at: t.at,
        resolved_outcome_id: t.resolved_outcome_id,
        cancel_reason: t.cancel_reason.map(|r| r.as_str()),
    }
}

fn outcome_to_dto(o: dodex_domain::Outcome) -> OutcomeDto {
    OutcomeDto {
        outcome_id: o.outcome_id,
        outcome_name: o.outcome_name,
        symbol: o.symbol.0,
        price_precision: o.price_precision,
        quantity_precision: o.quantity_precision,
        tick_size: o.tick_size,
        step_size: o.step_size,
        min_notional: o.min_notional,
        max_batch_size: o.max_batch_size,
    }
}

#[handler]
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
        .map_err(|err| {
            if let Some(domain) = err.downcast_ref::<DomainError>() {
                return ApiError::from(*domain);
            }
            error!(?err, "get_depth failed");
            ApiError::from(DomainError::Unexpected)
        })?;

    Ok(Json(DepthResponse {
        market_address: snapshot.market_address.0,
        symbol: snapshot.symbol.0,
        last_update_id: snapshot.last_update_id,
        bids: snapshot.bids.into_iter().map(|level| [level.price, level.quantity]).collect(),
        asks: snapshot.asks.into_iter().map(|level| [level.price, level.quantity]).collect(),
    }))
}

#[handler]
async fn get_open_orders(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<OpenOrdersPageResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::UserData)?.clone();
    let state = depot
        .obtain::<AppState>()
        .map_err(|err| {
            error!(?err, "missing AppState in depot");
            ApiError::from(DomainError::Unexpected)
        })?
        .clone();

    let market_address = non_empty_query(req, "marketAddress").map(MarketAddress);
    let symbol = non_empty_query(req, "symbol").map(Symbol);
    // Map any limit-parse failure to MissingParameter so the documented
    // -1102 fires for both out-of-range (e.g., 501) and unparseable
    // (e.g., "abc") inputs. `optional_typed_query` distinguishes them
    // structurally as InvalidParameter (-1130), which conflicts with the
    // openOrders error contract.
    let limit = optional_typed_query::<i64>(req, "limit")
        .map_err(|_| ApiError::from(DomainError::MissingParameter))?;
    // Cursor is the lex-comparable placed_chain_order value from a prior
    // page response. An empty / whitespace-only `?cursor=` is treated as
    // malformed (-1102 / 400) rather than "no cursor". The use case does
    // the trim + non-empty check.
    let cursor = req.query::<String>("cursor");

    let use_case = GetOpenOrdersUseCase::new(state.repo);
    let page = use_case
        .execute(&ctx, market_address, symbol, limit, cursor.as_deref())
        .await
        .map_err(|err| {
            if let Some(domain) = err.downcast_ref::<DomainError>() {
                return ApiError::from(*domain);
            }
            error!(?err, "get_open_orders failed");
            ApiError::from(DomainError::Unexpected)
        })?;

    Ok(Json(OpenOrdersPageResponse {
        orders: page.orders.into_iter().map(open_order_to_dto).collect(),
        next_cursor: page.next_cursor.map(|c| c.0),
    }))
}

fn open_order_to_dto(order: OpenOrder) -> OpenOrderResponse {
    OpenOrderResponse {
        market_address: order.market_address.0,
        symbol: order.symbol.0,
        order_id: order.order_id,
        client_order_id: order.client_order_id,
        price: order.price,
        orig_qty: order.orig_qty,
        executed_qty: order.executed_qty,
        status: order.status.as_str(),
        time_in_force: order.time_in_force.as_str(),
        order_type: order.order_type.as_str(),
        side: order.side.as_str(),
        time: order.time,
        update_time: order.update_time,
    }
}

fn non_empty_query(req: &mut Request, key: &str) -> Option<String> {
    req.query::<String>(key).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
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

/// Request body for `POST /api/v1/order`. Field names match
/// [api-spec §New Order](../../docs/api-spec.md#new-order) verbatim;
/// `type` is the reserved keyword we rename for serde and rebind to
/// `order_type` internally.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateOrderRequest {
    market_address: Option<String>,
    symbol: Option<String>,
    new_order_client_id: Option<String>,
    side: Option<String>,
    quantity: Option<String>,
    price: Option<String>,
    #[serde(rename = "type")]
    order_type: Option<String>,
    time_in_force: Option<String>,
}

/// Response shape for `POST /api/v1/order`. Minimal by design — we
/// only return facts the caller does not already have:
/// `clientOrderId` (which the backend may have generated),
/// `transactTime` (the moment we accepted), and `status` (always
/// `PENDING_NEW` for a successful submission — the order is in the
/// chain queue, not yet on the book). The full order shape with
/// chain-assigned `orderId` becomes available through
/// `GET /api/v1/openOrders` once `OrderBook.OrderPlaced` projects.
/// See `docs/tech-specs/write-api.md §Response` for the rationale.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateOrderResponse {
    client_order_id: String,
    transact_time: i64,
    status: &'static str,
}

/// Response shape for `DELETE /api/v1/order`. Minimal by design,
/// parallel to [`CreateOrderResponse`]: we only return facts the
/// caller does not already have. `clientOrderId` is the value
/// recorded on placement (`live_orders.client_order_id`) — useful
/// for correlation with the prior POST. The final state arrives
/// later through `/api/v1/openOrders` (the order disappears) and
/// `/api/v1/allOrders` (CANCELED, or FILLED if matching raced the
/// cancel). See `docs/tech-specs/write-api.md §Response` for
/// `DELETE /api/v1/order`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelOrderResponse {
    order_id: String,
    client_order_id: String,
    transact_time: i64,
    status: &'static str,
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

/// `POST /api/v1/order`. Auth hoop has already verified the request;
/// `require_auth(Trade)` enforces the spec permission. The handler
/// translates the parsed request + `AuthContext` into a
/// `NewOrderInput`, hands the use case off, and shapes the
/// three-field response (clientOrderId / transactTime / status) per
/// [write-api.md §Response]. The chain-assigned `orderId` is not in
/// this response by design — it arrives later through
/// `GET /api/v1/openOrders` once the indexer projects
/// `OrderBook.OrderPlaced`.
#[handler]
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

    let body: CreateOrderRequest = req.parse_json().await.map_err(|err| {
        // Body has been HMAC-verified upstream, so a parse failure here
        // is a client-shape bug (malformed JSON, wrong types) — surface
        // as -1130 InvalidParameter rather than a generic 500. `warn`
        // not `error`: a misbehaving caller is not an ops issue, just
        // a debugging breadcrumb (mirrors `chain_sender.rs`'s `warn`
        // for known-mapped chain rejects).
        warn!(?err, "POST /api/v1/order body did not parse");
        ApiError::from(DomainError::InvalidParameter)
    })?;

    let (now_seconds, now_ms) = now_pair();
    let input = build_new_order_input(body, ctx, now_seconds, now_ms)?;

    let use_case = CreateOrderUseCase::new(state.repo, state.chain_sender);
    let submitted = use_case.execute(input).await.map_err(ApiError::from)?;

    Ok(Json(CreateOrderResponse {
        client_order_id: submitted.client_order_id,
        transact_time: now_ms,
        status: OrderStatus::PendingNew.as_str(),
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

/// `DELETE /api/v1/order`. Auth hoop verified the request; this
/// handler enforces `TRADE`, parses query params, hands off to the use
/// case, and shapes the four-field `PENDING_CANCEL` response per
/// `docs/tech-specs/write-api.md §Response` (DELETE).
#[handler]
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
        status: OrderStatus::PendingCancel.as_str(),
    }))
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
                .push(Router::with_path("api/v1/openOrders").get(get_open_orders)),
        )
}

/// Production bootstrap. `main` defers to this so the executable shim
/// stays a single line and every meaningful step is testable in
/// isolation.
pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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
    let authenticator: SharedAuth = Arc::new(PostgresAuthenticator::new(pool, kek, &config.auth));
    let chain_sender: SharedChainSender = Arc::new(BeeDexChainSender::new(
        vec![config.chain.gateway_endpoint.clone()],
        Duration::from_millis(config.chain.place_order_timeout_ms),
        Duration::from_millis(config.chain.cancel_order_timeout_ms),
    )?);
    let state = AppState::new(repo, authenticator, chain_sender)
        .with_request_timeout(Duration::from_millis(config.server.request_timeout_ms));

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
