// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Typed views over the parallel-array shapes the on-chain `OrderBook`
// getters return. Only the slice used by e2e test cleanup is ported —
// `getOrdersByOwner` for absence-polling between a cancel call and the
// next placement.

use ackinacki_kit::contracts::dex::order_book::ResultOfGetOrdersByOwner as KitOrdersByOwner;

use super::error::ChainError;
use super::error::ChainResult;

#[derive(Debug, Clone)]
pub struct OwnedOrder {
    pub order_id: u128,
    pub outcome_id: u32,
    pub is_buy: bool,
    /// `uint256` price preserved as the string ABI returns; the
    /// contract uses 256-bit prices that do not fit u128.
    pub price: String,
    pub amount: u128,
    pub epoch_id: u64,
    pub client_order_id: u128,
}

#[derive(Debug, Clone)]
pub struct OwnedOrders {
    pub orders: Vec<OwnedOrder>,
}

impl TryFrom<KitOrdersByOwner> for OwnedOrders {
    type Error = ChainError;

    fn try_from(r: KitOrdersByOwner) -> ChainResult<Self> {
        let n = r.order_ids.len();
        if r.outcome_ids.len() != n
            || r.is_buys.len() != n
            || r.prices.len() != n
            || r.amounts.len() != n
            || r.epoch_ids.len() != n
            || r.client_order_ids.len() != n
        {
            return Err(ChainError::Decode(format!(
                "OrderBook.getOrdersByOwner returned mismatched parallel arrays: \
                 order_ids={}, outcome_ids={}, is_buys={}, prices={}, amounts={}, \
                 epoch_ids={}, client_order_ids={}",
                r.order_ids.len(),
                r.outcome_ids.len(),
                r.is_buys.len(),
                r.prices.len(),
                r.amounts.len(),
                r.epoch_ids.len(),
                r.client_order_ids.len(),
            )));
        }

        let mut orders = Vec::with_capacity(n);
        for i in 0..n {
            orders.push(OwnedOrder {
                order_id: parse_u128(&r.order_ids[i], "order_id")?,
                outcome_id: parse_u32(&r.outcome_ids[i], "outcome_id")?,
                is_buy: r.is_buys[i],
                price: r.prices[i].clone(),
                amount: parse_u128(&r.amounts[i], "amount")?,
                epoch_id: parse_u64(&r.epoch_ids[i], "epoch_id")?,
                client_order_id: parse_u128(&r.client_order_ids[i], "client_order_id")?,
            });
        }
        Ok(Self { orders })
    }
}

fn parse_u128(s: &str, field: &str) -> ChainResult<u128> {
    s.parse::<u128>().map_err(|e| {
        ChainError::Decode(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u128: {e}"))
    })
}

fn parse_u64(s: &str, field: &str) -> ChainResult<u64> {
    s.parse::<u64>().map_err(|e| {
        ChainError::Decode(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u64: {e}"))
    })
}

fn parse_u32(s: &str, field: &str) -> ChainResult<u32> {
    s.parse::<u32>().map_err(|e| {
        ChainError::Decode(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u32: {e}"))
    })
}
