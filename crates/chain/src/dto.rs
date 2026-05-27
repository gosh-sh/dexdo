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

#[cfg(test)]
mod tests {
    use super::*;

    fn kit_two_orders() -> KitOrdersByOwner {
        KitOrdersByOwner {
            order_ids: vec!["1".into(), "2".into()],
            outcome_ids: vec!["10".into(), "11".into()],
            is_buys: vec![true, false],
            prices: vec!["1000".into(), "2000".into()],
            amounts: vec!["500".into(), "600".into()],
            epoch_ids: vec!["7".into(), "8".into()],
            client_order_ids: vec!["1001".into(), "1002".into()],
        }
    }

    #[test]
    fn try_from_happy_path_round_trip() {
        let owned: OwnedOrders = kit_two_orders().try_into().expect("decode ok");
        assert_eq!(owned.orders.len(), 2);

        let o0 = &owned.orders[0];
        assert_eq!(o0.order_id, 1);
        assert_eq!(o0.outcome_id, 10);
        assert!(o0.is_buy);
        assert_eq!(o0.price, "1000");
        assert_eq!(o0.amount, 500);
        assert_eq!(o0.epoch_id, 7);
        assert_eq!(o0.client_order_id, 1001);

        let o1 = &owned.orders[1];
        assert_eq!(o1.order_id, 2);
        assert_eq!(o1.outcome_id, 11);
        assert!(!o1.is_buy);
        assert_eq!(o1.price, "2000");
        assert_eq!(o1.amount, 600);
        assert_eq!(o1.epoch_id, 8);
        assert_eq!(o1.client_order_id, 1002);
    }

    #[test]
    fn try_from_empty_input_yields_empty_orders() {
        let kit = KitOrdersByOwner {
            order_ids: vec![],
            outcome_ids: vec![],
            is_buys: vec![],
            prices: vec![],
            amounts: vec![],
            epoch_ids: vec![],
            client_order_ids: vec![],
        };
        let owned: OwnedOrders = kit.try_into().expect("decode ok");
        assert!(owned.orders.is_empty());
    }

    #[test]
    fn try_from_length_mismatch_returns_decode_error() {
        let mut kit = kit_two_orders();
        kit.outcome_ids.pop();
        let err = OwnedOrders::try_from(kit).expect_err("should reject mismatched arrays");
        match err {
            ChainError::Decode(msg) => assert!(
                msg.contains("mismatched parallel arrays"),
                "unexpected decode message: {msg}",
            ),
            other => panic!("expected ChainError::Decode, got {other:?}"),
        }
    }

    #[test]
    fn try_from_invalid_u128_field_returns_decode_error() {
        let mut kit = kit_two_orders();
        kit.order_ids[1] = "not-a-number".into();
        let err = OwnedOrders::try_from(kit).expect_err("should reject bad u128");
        match err {
            ChainError::Decode(msg) => assert!(
                msg.contains("order_id") && msg.contains("u128"),
                "unexpected decode message: {msg}",
            ),
            other => panic!("expected ChainError::Decode, got {other:?}"),
        }
    }

    #[test]
    fn try_from_invalid_u32_field_returns_decode_error() {
        let mut kit = kit_two_orders();
        kit.outcome_ids[0] = "99999999999".into();
        let err = OwnedOrders::try_from(kit).expect_err("should reject bad u32");
        match err {
            ChainError::Decode(msg) => assert!(
                msg.contains("outcome_id") && msg.contains("u32"),
                "unexpected decode message: {msg}",
            ),
            other => panic!("expected ChainError::Decode, got {other:?}"),
        }
    }

    #[test]
    fn try_from_invalid_u64_field_returns_decode_error() {
        let mut kit = kit_two_orders();
        kit.epoch_ids[0] = "abc".into();
        let err = OwnedOrders::try_from(kit).expect_err("should reject bad u64");
        match err {
            ChainError::Decode(msg) => assert!(
                msg.contains("epoch_id") && msg.contains("u64"),
                "unexpected decode message: {msg}",
            ),
            other => panic!("expected ChainError::Decode, got {other:?}"),
        }
    }
}
