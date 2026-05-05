// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;

use async_trait::async_trait;
use dodex_domain::DepthSnapshot;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::Symbol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarketsSort {
    #[default]
    ResultStartAsc,
    CreatedAtDesc,
}

#[derive(Debug, Clone, Default)]
pub struct MarketsFilter {
    pub statuses: Vec<MarketStatus>,
    pub quote_asset: Option<String>,
    pub oracle_name: Option<String>,
    pub closing_before: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MarketsListing {
    pub filter: MarketsFilter,
    pub sort: MarketsSort,
    pub cursor: Option<String>,
    pub limit: u16,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub enum MarketsRequest {
    One { market_address: MarketAddress, now: i64 },
    Listing(MarketsListing),
}

#[async_trait]
pub trait MarketReadRepository: Send + Sync {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error>;

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + MarketReadRepository> MarketReadRepository for Arc<T> {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        (**self).list_markets(request).await
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
    pub async fn execute(&self, request: MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        self.repo.list_markets(&request).await
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
