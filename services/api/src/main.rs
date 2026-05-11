// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::env;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsUseCase;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketStatus;
use dodex_domain::Symbol;
use dodex_domain::Terminal;
use dodex_domain::TerminalKind;
use dodex_domain::Timings;
use dodex_infrastructure::config::ApiConfig;
use dodex_infrastructure::database::build_pool;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::writing::Json;
use salvo_extra::affix_state::inject;
use serde::Serialize;
use tracing::error;
use tracing::info;

type SharedRepo = Arc<dyn MarketReadRepository + Send + Sync>;

#[derive(Clone)]
struct AppState {
    repo: SharedRepo,
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
    // `None` only while the backend has not resolved the deterministic
    // OrderBook address yet. Clients gate trading on `status`, not on the
    // presence of this field.
    order_book_address: Option<String>,
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
    oracle_name: Option<String>,
    oracle_address: Option<String>,
    oracle_fee: Option<String>,
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
    last_update_id: u64,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Serialize)]
struct ErrorBody {
    code: i32,
    msg: &'static str,
}

#[derive(Debug)]
struct ApiError(DomainError);

impl ApiError {
    fn status(&self) -> StatusCode {
        match self.0 {
            DomainError::AuthRequired
            | DomainError::TimestampOutsideRecvWindow
            | DomainError::InvalidSignature => StatusCode::UNAUTHORIZED,
            DomainError::UnknownOrder | DomainError::InvalidMarketOrSymbol => StatusCode::NOT_FOUND,
            // Transient indexer state (e.g. RESOLVED row whose PoolsFrozen
            // has not been replayed yet). 503 tells the client to retry —
            // the underlying inconsistency clears as the indexer catches up.
            // See docs/tech-spec.md:113 ("fail the request closed").
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
    let limit = limit_param
        .map(|v| v.clamp(1, MAX_LIMIT as i64) as u16)
        .unwrap_or(DEFAULT_LIMIT);

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
        oracle_name: e.oracle_name,
        oracle_address: e.oracle_address,
        oracle_fee: e.oracle_fee,
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
    let limit = optional_typed_query::<i64>(req, "limit")?
        .map(|v| v.clamp(1, 1000) as u16)
        .unwrap_or(100);

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path =
        env::var("APP_CONFIG").unwrap_or_else(|_| "config/api.local.yaml".to_string());
    let config = ApiConfig::load_from_path(&config_path)?;

    let pool = build_pool(&config.common.database).await?;
    info!("api running with postgres read-model repository");
    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool));
    let state = AppState { repo };

    // The API is intentionally restart-to-reconfigure. None of the live
    // request paths read runtime config — pool, server bind, request_timeout
    // are all baked at startup — so a SIGUSR1 reload-loop would be cargo-cult.
    // The indexer keeps its loop because its background tasks do consume new
    // config (graphql endpoint/timeouts, ignored_addresses, intervals).

    let router = Router::new()
        .hoop(inject(state))
        .push(Router::with_path("readiness").get(readiness))
        .push(Router::with_path("api/v1/markets").get(get_markets))
        .push(Router::with_path("api/v1/depth").get(get_depth));

    let acceptor = TcpListener::new((config.server.host.clone(), config.server.port)).bind().await;
    info!(host = %config.server.host, port = config.server.port, "api server starting");
    Server::new(acceptor).serve(router).await;
    Ok(())
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or_default()
}
