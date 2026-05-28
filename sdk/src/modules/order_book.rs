use ackinacki_kit::contracts::dex::order_book::OrderBook;
use ackinacki_kit::contracts::dex::order_book::ParamsOfGetOrder;
use ackinacki_kit::contracts::dex::order_book::ParamsOfGetOrdersByOwner;
use ackinacki_kit::contracts::dex::order_book::ResultOfGetDetails;
use ackinacki_kit::contracts::dex::order_book::ResultOfGetOrder;
use ackinacki_kit::contracts::dex::order_book::ResultOfGetOrdersByOwner;
use ackinacki_kit::contracts::dex::order_book::ResultOfGetQueueSize;
use ackinacki_kit::contracts::dex::order_book::ResultOfGetShutdownState;

use crate::client::DexContext;
use crate::errors::AppResult;
use crate::services;

pub(crate) struct OrderBookModule<'a> {
    ctx: &'a DexContext,
}

impl<'a> OrderBookModule<'a> {
    pub fn new(ctx: &'a DexContext) -> Self {
        Self { ctx }
    }

    fn ob(&self, address: &str) -> OrderBook {
        OrderBook::new(self.ctx.tvm_client.clone(), address)
    }

    pub async fn get_details(&self, ob_address: &str) -> AppResult<ResultOfGetDetails> {
        self.ctx.acquire().await;
        services::order_book::get_details(&self.ob(ob_address)).await
    }

    pub async fn get_queue_size(&self, ob_address: &str) -> AppResult<ResultOfGetQueueSize> {
        self.ctx.acquire().await;
        services::order_book::get_queue_size(&self.ob(ob_address)).await
    }

    pub async fn get_order(
        &self,
        ob_address: &str,
        params: ParamsOfGetOrder,
    ) -> AppResult<ResultOfGetOrder> {
        self.ctx.acquire().await;
        services::order_book::get_order(&self.ob(ob_address), params).await
    }

    pub async fn get_orders_by_owner(
        &self,
        ob_address: &str,
        params: ParamsOfGetOrdersByOwner,
    ) -> AppResult<ResultOfGetOrdersByOwner> {
        self.ctx.acquire().await;
        services::order_book::get_orders_by_owner(&self.ob(ob_address), params).await
    }

    pub async fn get_shutdown_state(
        &self,
        ob_address: &str,
    ) -> AppResult<ResultOfGetShutdownState> {
        self.ctx.acquire().await;
        services::order_book::get_shutdown_state(&self.ob(ob_address)).await
    }
}
