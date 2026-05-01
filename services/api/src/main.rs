// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::env;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_application::GetDepthQuery;
use dodex_application::GetDepthUseCase;
use dodex_application::GetMarketsQuery;
use dodex_application::GetMarketsUseCase;
use dodex_application::MarketReadRepository;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;
use dodex_infrastructure::config::ApiConfig;
use dodex_infrastructure::database::build_pool;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::signal::run_config_reload_loop;
use dodex_infrastructure::stub::StubReadModelRepository;
use salvo::http::StatusCode;
use salvo::prelude::*;
use salvo::writing::Json;
use salvo_extra::affix_state::inject;
use serde::Serialize;
use tokio::sync::RwLock;
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
    server_time: u64,
    markets: Vec<MarketDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketDto {
    quote_asset: String,
    market_address: String,
    market_name: String,
    status: String,
    outcomes: Vec<OutcomeDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutcomeDto {
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
            DomainError::UnknownOrder => StatusCode::NOT_FOUND,
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

    let market_address = req.query::<String>("marketAddress").map(MarketAddress);

    let use_case = GetMarketsUseCase::new(state.repo);
    let market_items =
        use_case.execute(GetMarketsQuery { market_address }).await.map_err(|err| {
            error!(?err, "list_markets failed");
            ApiError::from(DomainError::Unexpected)
        })?;

    let payload = MarketsResponse {
        server_time: now_millis(),
        markets: market_items
            .into_iter()
            .map(|market| MarketDto {
                quote_asset: market.quote_asset,
                market_address: market.market_address.0,
                market_name: market.market_name.0,
                status: market.status,
                outcomes: market
                    .outcomes
                    .into_iter()
                    .map(|outcome| OutcomeDto {
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

    let limit = req.query::<u16>("limit").unwrap_or(100).min(1000);

    let use_case = GetDepthUseCase::new(state.repo);
    let snapshot = use_case
        .execute(GetDepthQuery {
            market_address: MarketAddress(market_address),
            symbol: Symbol(symbol),
            limit,
        })
        .await
        .map_err(|err| {
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
    let config_state = Arc::new(RwLock::new(config.clone()));

    let repo: SharedRepo = if config.common.features.stub_mode {
        info!("api running with stub read-model repository");
        Arc::new(StubReadModelRepository)
    } else {
        let pool = build_pool(&config.common.database).await?;
        info!("api running with postgres read-model repository");
        Arc::new(PostgresReadModelRepository::new(pool))
    };
    let state = AppState { repo };

    tokio::spawn(run_config_reload_loop(config_path.clone(), Arc::clone(&config_state), "api"));

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

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default()
}
