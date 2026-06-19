use dodex_contracts::dex::order_book::ResultOfGetDetails as KitOrderBookDetails;
use dodex_contracts::dex::order_book::ResultOfGetOrder as KitOrderInfo;
use dodex_contracts::dex::order_book::ResultOfGetOrdersByOwner as KitOrdersByOwner;
use dodex_contracts::dex::order_book::ResultOfGetShutdownState as KitOrderBookShutdownState;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::AppError;
use crate::errors::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookDetails {
    /// Bound event id (`uint256` decimal).
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub next_order_id: u128,
    pub order_count: u128,
    /// Lifetime maker rebates paid out by this OrderBook.
    pub total_maker_rebates_paid: u128,
    /// Lifetime protocol fees collected by this OrderBook.
    pub total_protocol_fees: u128,
}

impl From<KitOrderBookDetails> for OrderBookDetails {
    fn from(d: KitOrderBookDetails) -> Self {
        Self {
            event_id: d.event_id,
            oracle_list_hash: d.oracle_list_hash,
            token_type: d.token_type,
            next_order_id: d.next_order_id,
            order_count: d.order_count,
            total_maker_rebates_paid: d.total_maker_rebates_paid,
            total_protocol_fees: d.total_protocol_fees,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookShutdownState {
    pub shutting_down: bool,
    pub shutdown_pending: bool,
}

impl From<KitOrderBookShutdownState> for OrderBookShutdownState {
    fn from(s: KitOrderBookShutdownState) -> Self {
        Self { shutting_down: s.shutting_down, shutdown_pending: s.shutdown_pending }
    }
}

/// Resolved view of a single order on the OrderBook (`OrderBook.getOrder`).
///
/// `price` is preserved as a `uint256` decimal/hex string to match the
/// contract representation; amounts/ids are parsed into native ints since
/// they fit u128/u64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderInfo {
    pub deposit_identifier_hash: String,
    pub outcome_id: u32,
    pub is_buy: bool,
    pub flags: u8,
    pub price: String,
    pub amount: u128,
    pub min_amount: u128,
    pub epoch_id: u64,
}

impl From<KitOrderInfo> for OrderInfo {
    fn from(o: KitOrderInfo) -> Self {
        Self {
            deposit_identifier_hash: o.deposit_identifier_hash,
            outcome_id: o.outcome_id,
            is_buy: o.is_buy,
            flags: o.flags,
            price: o.price,
            amount: o.amount,
            min_amount: o.min_amount,
            epoch_id: o.epoch_id,
        }
    }
}

/// One entry from `OrderBook.getOrdersByOwner`. The on-chain getter returns
/// parallel `Vec<String>` arrays; this struct collapses them into typed
/// per-order records (parsed lazily in `From<KitOrdersByOwner>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedOrder {
    pub order_id: u128,
    pub outcome_id: u32,
    pub is_buy: bool,
    /// `uint256` price, kept as the string returned by ABI.
    pub price: String,
    pub amount: u128,
    pub epoch_id: u64,
    pub client_order_id: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedOrders {
    pub orders: Vec<OwnedOrder>,
}

impl TryFrom<KitOrdersByOwner> for OwnedOrders {
    type Error = AppError;

    fn try_from(r: KitOrdersByOwner) -> AppResult<Self> {
        let n = r.order_ids.len();
        if r.outcome_ids.len() != n
            || r.is_buys.len() != n
            || r.prices.len() != n
            || r.amounts.len() != n
            || r.epoch_ids.len() != n
            || r.client_order_ids.len() != n
        {
            return Err(AppError::new(format!(
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

fn parse_u128(s: &str, field: &str) -> AppResult<u128> {
    s.parse::<u128>().map_err(|e| {
        AppError::new(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u128: {e}"))
    })
}

fn parse_u64(s: &str, field: &str) -> AppResult<u64> {
    s.parse::<u64>().map_err(|e| {
        AppError::new(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u64: {e}"))
    })
}

fn parse_u32(s: &str, field: &str) -> AppResult<u32> {
    s.parse::<u32>().map_err(|e| {
        AppError::new(format!("OrderBook.getOrdersByOwner: parse {field}=`{s}` as u32: {e}"))
    })
}
