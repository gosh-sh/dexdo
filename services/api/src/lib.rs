// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

mod auth_hoop;
#[doc(hidden)]
pub mod testkit;

use std::env;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use dodex_application::AuthContext;
use dodex_application::Authenticator;
use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsUseCase;
use dodex_application::GetOpenOrdersUseCase;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_application::OpenOrdersCursor;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketStatus;
use dodex_domain::OpenOrder;
use dodex_domain::Permission;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TerminalKind;
use dodex_domain::Timings;
use dodex_infrastructure::auth::PostgresAuthenticator;
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
use serde::Serialize;
use tracing::error;
use tracing::info;
use uuid::Uuid;

#[doc(hidden)]
pub type SharedRepo = Arc<dyn MarketReadRepository>;
#[doc(hidden)]
pub type SharedAuth = Arc<dyn Authenticator>;

#[doc(hidden)]
#[derive(Clone)]
pub struct AppState {
    pub(crate) repo: SharedRepo,
    pub(crate) authenticator: SharedAuth,
}

impl AppState {
    /// Wire-up constructor. Re-exported through the `testkit` module
    /// for integration tests; production code reaches it through `run`.
    #[doc(hidden)]
    pub fn new(repo: SharedRepo, authenticator: SharedAuth) -> Self {
        Self { repo, authenticator }
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
    let limit = req.query::<u16>("limit");
    let cursor = non_empty_query(req, "cursor");

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
        next_cursor: page.next_cursor.as_ref().map(OpenOrdersCursor::encode),
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

/// Placeholder response for `POST /api/v1/order` until the real
/// order-placement pipeline lands. The route is wired now to give
/// integrators a real authenticated endpoint to smoke-test their
/// HMAC signing against; the response shape will change to match
/// `docs/api-spec.md §New Order` when the real handler ships.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrderStubResponse {
    account_id: Uuid,
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

/// Stub for `POST /api/v1/order`. The auth hoop has already verified
/// the request and placed `AuthContext` in the depot; `require_auth`
/// then enforces the spec-required `TRADE` permission. When the real
/// implementation lands, only the body construction below changes —
/// the authorization gate stays as-is.
#[handler]
async fn create_order(depot: &mut Depot) -> Result<Json<OrderStubResponse>, ApiError> {
    let ctx = require_auth(depot, Permission::Trade)?;
    Ok(Json(OrderStubResponse { account_id: ctx.account_id, status: "STUB" }))
}

/// Assemble the production router around `state`. Kept as a separate
/// function so integration tests can drive the same router with a
/// test-DB pool through Salvo's in-process `TestClient`; production
/// callers reach it indirectly through `run`.
#[doc(hidden)]
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .hoop(inject(state))
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
                .push(Router::with_path("api/v1/order").post(create_order))
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
    let state = AppState::new(repo, authenticator);

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
