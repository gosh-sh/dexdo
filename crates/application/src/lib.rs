use async_trait::async_trait;
use dodex_domain::DepthSnapshot;
use dodex_domain::Market;
use dodex_domain::MarketId;
use dodex_domain::Symbol;

#[async_trait]
pub trait MarketReadRepository: Send + Sync {
    async fn list_markets(
        &self,
        market_id: Option<&MarketId>,
    ) -> Result<Vec<Market>, anyhow::Error>;
    async fn get_depth(&self, symbol: &Symbol, limit: u16) -> Result<DepthSnapshot, anyhow::Error>;
}

#[derive(Debug, Clone)]
pub struct GetMarketsQuery {
    pub market_id: Option<MarketId>,
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub symbol: Symbol,
    pub limit: u16,
}

pub struct GetMarketsUseCase<R> {
    repo: R,
}

impl<R> GetMarketsUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetMarketsUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(&self, query: GetMarketsQuery) -> Result<Vec<Market>, anyhow::Error> {
        self.repo.list_markets(query.market_id.as_ref()).await
    }
}

pub struct GetDepthUseCase<R> {
    repo: R,
}

impl<R> GetDepthUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetDepthUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(&self, query: GetDepthQuery) -> Result<DepthSnapshot, anyhow::Error> {
        self.repo.get_depth(&query.symbol, query.limit).await
    }
}
