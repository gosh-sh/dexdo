use crate::client::DexContext;
use crate::errors::AppResult;
use crate::services;

pub(crate) struct MarketModule<'a> {
    ctx: &'a DexContext,
}

impl<'a> MarketModule<'a> {
    pub fn new(ctx: &'a DexContext) -> Self {
        Self { ctx }
    }

    pub async fn discover_oracles(&self) -> AppResult<Vec<services::market::OracleInfo>> {
        self.ctx.acquire().await;
        services::market::discover_oracles(self.ctx.tvm_client.clone()).await
    }

    pub async fn discover_markets(
        &self,
        token_type: u32,
    ) -> AppResult<Vec<services::market::MarketInfo>> {
        self.ctx.acquire().await;
        services::market::discover_markets(self.ctx.tvm_client.clone(), token_type).await
    }

    pub async fn discover_active_markets(
        &self,
        token_type: u32,
    ) -> AppResult<Vec<services::market::MarketInfo>> {
        self.ctx.acquire().await;
        services::market::discover_active_markets(self.ctx.tvm_client.clone(), token_type).await
    }
}
