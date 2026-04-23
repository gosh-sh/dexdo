use std::env;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsQuery;
use dodex_application::GetMarketsUseCase;
use dodex_domain::DomainError;
use dodex_domain::MarketId;
use dodex_domain::Symbol;
use dodex_infrastructure::config::AppConfig;
use dodex_infrastructure::signal::run_config_reload_loop;
use dodex_infrastructure::stub::StubReadModelRepository;
use salvo::prelude::*;
use salvo::writing::Json;
use salvo_extra::affix_state::inject;
use serde::Serialize;
use tokio::sync::RwLock;
use tracing::info;

#[derive(Clone)]
struct AppState {
    repo: StubReadModelRepository,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketsResponse {
    server_time: u64,
    markets: Vec<MarketDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketDto {
    market_id: String,
    name: String,
    status: String,
    quote_asset: String,
    market_address: String,
    outcomes: Vec<OutcomeDto>,
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
    symbol: String,
    last_update_id: u64,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[handler]
async fn healthz() -> &'static str {
    "ok"
}

#[handler]
async fn get_markets(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<MarketsResponse>, StatusError> {
    let state = depot.obtain::<AppState>().map_err(|_| internal_error())?.clone();
    let market_id = req.query::<String>("marketId").map(MarketId);

    let use_case = GetMarketsUseCase::new(state.repo);
    let market_items =
        use_case.execute(GetMarketsQuery { market_id }).await.map_err(|_| internal_error())?;

    let payload = MarketsResponse {
        server_time: now_millis(),
        markets: market_items
            .into_iter()
            .map(|market| MarketDto {
                market_id: market.market_id.0,
                name: market.name,
                status: market.status,
                quote_asset: market.quote_asset,
                market_address: market.market_address,
                outcomes: market
                    .outcomes
                    .into_iter()
                    .map(|outcome| OutcomeDto {
                        outcome_id: outcome.outcome_id,
                        outcome_name: outcome.outcome_name,
                        symbol: outcome.symbol.0,
                        price_precision: outcome.price_precision,
                        quantity_precision: outcome.quantity_precision,
                        tick_size: outcome.tick_size,
                        step_size: outcome.step_size,
                        min_notional: outcome.min_notional,
                        max_batch_size: outcome.max_batch_size,
                    })
                    .collect(),
            })
            .collect(),
    };

    Ok(Json(payload))
}

#[handler]
async fn get_depth(
    req: &mut Request,
    depot: &mut Depot,
) -> Result<Json<DepthResponse>, StatusError> {
    let state = depot.obtain::<AppState>().map_err(|_| internal_error())?.clone();
    let symbol =
        req.query::<String>("symbol").ok_or_else(|| api_error(DomainError::MissingParameter))?;

    let limit = req.query::<u16>("limit").unwrap_or(100).min(1000);
    if symbol.trim().is_empty() {
        return Err(api_error(DomainError::InvalidSymbol));
    }

    let use_case = GetDepthUseCase::new(state.repo);
    let snapshot = use_case
        .execute(GetDepthQuery { symbol: Symbol(symbol), limit })
        .await
        .map_err(|_| internal_error())?;

    Ok(Json(DepthResponse {
        symbol: snapshot.symbol.0,
        last_update_id: snapshot.last_update_id,
        bids: snapshot.bids.into_iter().map(|level| [level.price, level.quantity]).collect(),
        asks: snapshot.asks.into_iter().map(|level| [level.price, level.quantity]).collect(),
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = env::var("APP_CONFIG").unwrap_or_else(|_| "config/local.yaml".to_string());
    let config = AppConfig::load_from_path(&config_path)?;
    let config_state = Arc::new(RwLock::new(config.clone()));
    let state = AppState { repo: StubReadModelRepository };

    tokio::spawn(run_config_reload_loop(config_path.clone(), Arc::clone(&config_state), "api"));

    let router = Router::new()
        .hoop(inject(state))
        .push(Router::with_path("healthz").get(healthz))
        .push(Router::with_path("api/v1/markets").get(get_markets))
        .push(Router::with_path("api/v1/depth").get(get_depth));

    let acceptor = TcpListener::new((config.server.host.clone(), config.server.port)).bind().await;
    info!(host = %config.server.host, port = config.server.port, "api server starting");
    Server::new(acceptor).serve(router).await;
    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}

fn internal_error() -> StatusError {
    StatusError::internal_server_error()
}

fn api_error(err: DomainError) -> StatusError {
    let msg = match err {
        DomainError::MissingParameter => (-1102, "Mandatory parameter was not sent."),
        DomainError::InvalidSymbol => (-1121, "Invalid symbol."),
        DomainError::Unexpected => (-1000, "Unknown error."),
    }
    .1;

    StatusError::bad_request().brief(msg)
}
