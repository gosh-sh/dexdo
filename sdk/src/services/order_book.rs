use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::order_book::ParamsOfGetOrder;
use dodex_contracts::dex::order_book::ParamsOfGetOrdersByOwner;
use dodex_contracts::dex::order_book::ResultOfGetDetails;
use dodex_contracts::dex::order_book::ResultOfGetOrder;
use dodex_contracts::dex::order_book::ResultOfGetOrdersByOwner;
use dodex_contracts::dex::order_book::ResultOfGetQueueSize;
use dodex_contracts::dex::order_book::ResultOfGetShutdownState;

use crate::errors::AppResult;

pub(crate) async fn get_details(ob: &OrderBook) -> AppResult<ResultOfGetDetails> {
    ob.get_details().await.map_err(Into::into)
}

pub(crate) async fn get_queue_size(ob: &OrderBook) -> AppResult<ResultOfGetQueueSize> {
    ob.get_queue_size().await.map_err(Into::into)
}

pub(crate) async fn get_order(
    ob: &OrderBook,
    params: ParamsOfGetOrder,
) -> AppResult<ResultOfGetOrder> {
    ob.get_order(params).await.map_err(Into::into)
}

pub(crate) async fn get_orders_by_owner(
    ob: &OrderBook,
    params: ParamsOfGetOrdersByOwner,
) -> AppResult<ResultOfGetOrdersByOwner> {
    ob.get_orders_by_owner(params).await.map_err(Into::into)
}

pub(crate) async fn get_shutdown_state(ob: &OrderBook) -> AppResult<ResultOfGetShutdownState> {
    ob.get_shutdown_state().await.map_err(Into::into)
}
