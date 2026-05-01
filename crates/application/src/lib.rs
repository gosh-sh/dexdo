// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;

use async_trait::async_trait;
use dodex_domain::DepthSnapshot;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;

#[async_trait]
pub trait MarketReadRepository: Send + Sync {
    async fn list_markets(
        &self,
        market_address: Option<&MarketAddress>,
    ) -> Result<Vec<Market>, anyhow::Error>;

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + MarketReadRepository> MarketReadRepository for Arc<T> {
    async fn list_markets(
        &self,
        market_address: Option<&MarketAddress>,
    ) -> Result<Vec<Market>, anyhow::Error> {
        (**self).list_markets(market_address).await
    }

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        (**self).get_depth(market_address, symbol, limit).await
    }
}

#[derive(Debug, Clone)]
pub struct GetMarketsQuery {
    pub market_address: Option<MarketAddress>,
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub market_address: MarketAddress,
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
        self.repo.list_markets(query.market_address.as_ref()).await
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
        self.repo.get_depth(&query.market_address, &query.symbol, query.limit).await
    }
}
