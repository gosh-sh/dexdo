use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u32;
use ackinacki_kit::contracts::deserialize::deserialize_u64;
use ackinacki_kit::contracts::deserialize::deserialize_u8;
use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::traits::DecodeMessage;
use ackinacki_kit::contracts::traits::FromEvent;
use ackinacki_kit::contracts::KitResult;
use serde::Deserialize;

/// External event IDs are defined in `dex/modifiers/modifiers.sol`. Each event
/// is emitted to its own `address.makeAddrExtern(<id>, 256)`, so the
/// destination id alone identifies the event.
/// `OB_EPOCH_SETTLED = 145` exists as a constant but is no longer emitted
/// by the contract; intentionally omitted here.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `OrderBook`.
pub enum OrderBookEvent {
    OrderPlaced = 143,
    OrderCancelled = 144,
    OrderFilled = 146,
    PartialFill = 157,
    FullyFilled = 158,
    Queued = 159,
    Rejected = 160,
    CallbackBounced = 161,
}

impl TryFrom<String> for OrderBookEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse order book event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            143 => Ok(OrderBookEvent::OrderPlaced),
            144 => Ok(OrderBookEvent::OrderCancelled),
            146 => Ok(OrderBookEvent::OrderFilled),
            157 => Ok(OrderBookEvent::PartialFill),
            158 => Ok(OrderBookEvent::FullyFilled),
            159 => Ok(OrderBookEvent::Queued),
            160 => Ok(OrderBookEvent::Rejected),
            161 => Ok(OrderBookEvent::CallbackBounced),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown order book event `{cleaned}`"),
            )),
        }
    }
}

impl Display for OrderBookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl OrderBookEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `OrderBook` external event.
pub enum DecodedOrderBookEvent {
    OrderPlaced { event: Event, kind: OrderBookEvent, data: OrderPlacedData },
    OrderCancelled { event: Event, kind: OrderBookEvent, data: OrderCancelledData },
    OrderFilled { event: Event, kind: OrderBookEvent, data: OrderFilledData },
    PartialFill { event: Event, kind: OrderBookEvent, data: PartialFillData },
    FullyFilled { event: Event, kind: OrderBookEvent, data: FullyFilledData },
    Queued { event: Event, kind: OrderBookEvent, data: QueuedData },
    Rejected { event: Event, kind: OrderBookEvent, data: RejectedData },
    CallbackBounced { event: Event, kind: OrderBookEvent, data: CallbackBouncedData },
}

impl FromEvent for DecodedOrderBookEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = OrderBookEvent::try_from(event.dst.clone())?;
        match kind {
            OrderBookEvent::OrderPlaced => {
                let data = decode_or_err::<OrderPlacedData>(event, contract)?;
                Ok(DecodedOrderBookEvent::OrderPlaced { event: event.clone(), kind, data })
            }
            OrderBookEvent::OrderCancelled => {
                let data = decode_or_err::<OrderCancelledData>(event, contract)?;
                Ok(DecodedOrderBookEvent::OrderCancelled { event: event.clone(), kind, data })
            }
            OrderBookEvent::OrderFilled => {
                let data = decode_or_err::<OrderFilledData>(event, contract)?;
                Ok(DecodedOrderBookEvent::OrderFilled { event: event.clone(), kind, data })
            }
            OrderBookEvent::PartialFill => {
                let data = decode_or_err::<PartialFillData>(event, contract)?;
                Ok(DecodedOrderBookEvent::PartialFill { event: event.clone(), kind, data })
            }
            OrderBookEvent::FullyFilled => {
                let data = decode_or_err::<FullyFilledData>(event, contract)?;
                Ok(DecodedOrderBookEvent::FullyFilled { event: event.clone(), kind, data })
            }
            OrderBookEvent::Queued => {
                let data = decode_or_err::<QueuedData>(event, contract)?;
                Ok(DecodedOrderBookEvent::Queued { event: event.clone(), kind, data })
            }
            OrderBookEvent::Rejected => {
                let data = decode_or_err::<RejectedData>(event, contract)?;
                Ok(DecodedOrderBookEvent::Rejected { event: event.clone(), kind, data })
            }
            OrderBookEvent::CallbackBounced => {
                let data = decode_or_err::<CallbackBouncedData>(event, contract)?;
                Ok(DecodedOrderBookEvent::CallbackBounced { event: event.clone(), kind, data })
            }
        }
    }
}

fn decode_or_err<T>(event: &Event, contract: &impl DecodeMessage) -> KitResult<T>
where
    T: serde::de::DeserializeOwned,
{
    let decoded = event.decode::<T>(contract)?;
    decoded.ok_or_else(|| {
        KitError::new(
            KitModule::Event,
            KitErrorCode::EmptyData,
            format!("Unexpected empty data for order book event `{}`", event.dst),
        )
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::OrderPlaced`.
pub struct OrderPlacedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    /// `uint256` represented as returned by ABI.
    pub deposit_hash: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::OrderCancelled`.
pub struct OrderCancelledData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::OrderFilled`.
pub struct OrderFilledData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub filled_amount: u128,
    /// `uint256` represented as returned by ABI.
    pub clearing_price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub fee_amount: u128,
    pub is_taker: bool,
    #[serde(deserialize_with = "deserialize_u64")]
    pub match_id: u64,
    /// `uint256` represented as returned by ABI.
    pub deposit_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::PartialFill`, emitted whenever a resting maker
/// order absorbs a partial fill.
pub struct PartialFillData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub filled_amount: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub remaining_amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::FullyFilled`, emitted when an order's remaining
/// amount reaches zero.
pub struct FullyFilledData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub filled_amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::Queued`. Emitted when an op is admitted into
/// the matching queue.
pub struct QueuedData {
    #[serde(deserialize_with = "deserialize_u8")]
    pub slot: u8,
    #[serde(deserialize_with = "deserialize_u32")]
    pub queue_id: u32,
    #[serde(deserialize_with = "deserialize_u8")]
    pub entry_type: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::Rejected`. Emitted when an op cannot be queued
/// (queue full / shutdown / invalid).
pub struct RejectedData {
    #[serde(deserialize_with = "deserialize_u8")]
    pub entry_type: u8,
    /// `uint256` represented as returned by ABI.
    pub deposit_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OrderBookEvent::CallbackBounced`. Emitted when a callback to
/// a `PrivateNote` bounces back to the OrderBook.
pub struct CallbackBouncedData {
    pub dest: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub lt: u64,
}

#[cfg(test)]
mod multicell_orderplaced_tests {
    //! Regression for the `tvm_abi` off-by-32 multi-cell decode bug.
    //!
    //! `OrderPlaced` grew to 9 fields; its body is now 1033 (id+data) bits, so the
    //! last field (`opNonce`) correctly spills into a continuation cell
    //! (`cell0 = 969 bits + ref`, `ref = 64 bits`). The contract encodes this
    //! correctly at ABI 2.4 — verified: the SDK encoder produces a byte-identical
    //! 969/64 split.
    //!
    //! The bug is in `tvm_abi`: the *event* decode path (`Event::decode_input` →
    //! `decode_params` → `From<SliceData> for Cursor`) seeds `Cursor.used_bits = 0`,
    //! dropping the 32 id bits already read. `check_layout` then computes
    //! `937 + 64 = 1001 <= 1023` and wrongly rejects the legit ref-spill with
    //! `WrongDataLayout` (ton-client error 304). Real cell0 holds `32 + 937 = 969`,
    //! and `969 + 64 = 1033 > 1023`, so the spill is correct. The *function* decode
    //! path is unaffected (it seeds the cursor via `decode_header`), which is the
    //! ONLY reason a function/internal round-trip masks the bug.
    //!
    //! This test decodes the REAL shellnet event body through the REAL event path
    //! (bundled ABI, `is_internal = false`) and asserts all 9 fields. It passes only
    //! with the fixed `tvm_abi` (the kit's `tvm_client` is currently pinned to the
    //! fix branch in `Cargo.toml`); on the unfixed `v3.0.0.an` it fails with 304.

    use ackinacki_kit::tvm_client::abi::decode_message_body;
    use ackinacki_kit::tvm_client::abi::Abi;
    use ackinacki_kit::tvm_client::abi::ParamsOfDecodeMessageBody;
    use num_bigint::BigUint;

    use super::OrderPlacedData;
    use crate::tests::create_context;

    const BUNDLED_ABI: &str = include_str!("../../../../contracts/dex/OrderBook.abi.json");

    // Real ext-out OrderPlaced body captured from a live shellnet OrderBook
    // (event-id 0x1b9c6957; cell0 = 969 bits + ref, ref = 64 bits = opNonce).
    const SHELLNET_BODY: &str = "te6ccgEBAgEAhwAB8xucaVcAAAAAAAAAAAAAAAAAAAABAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAJxAAAAAAAAAAAAAAAA34R1gAAAAAAAAAAADUWAPOAAAfQ5a/DxBF+SmrdO5oXyLM+jKFg9ytsUGhHlk2qac6wCgvAAQAQAAAAAAAAAAE=";

    const DEPOSIT_HASH: &str = "0xcb5f878822fc94d5ba77342f91667d1942c1ee56d8a0d08f2c9b54d39d601417";

    fn to_uint(s: &str) -> BigUint {
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => BigUint::parse_bytes(hex.as_bytes(), 16).unwrap(),
            None => BigUint::parse_bytes(s.as_bytes(), 10).unwrap(),
        }
    }

    #[test]
    fn shellnet_orderplaced_event_decodes_all_9_fields() {
        let decoded = decode_message_body(
            create_context(),
            ParamsOfDecodeMessageBody {
                abi: Abi::Json(BUNDLED_ABI.to_string()),
                body: SHELLNET_BODY.to_string(),
                is_internal: false,
                allow_partial: true,
                function_name: None,
                data_layout: None,
            },
        )
        .expect("event body decodes (needs the tvm_abi off-by-32 fix)");

        assert_eq!(decoded.name, "OrderPlaced");
        let d: OrderPlacedData =
            serde_json::from_value(decoded.value.expect("value")).expect("OrderPlacedData");

        assert_eq!(d.order_id, 1);
        assert_eq!(d.outcome_id, 1);
        assert!(!d.is_buy);
        assert_eq!(d.flags, 0);
        assert_eq!(to_uint(&d.price), BigUint::from(5000u32)); // 0x1388
        assert_eq!(d.amount, 30_000_000_000);
        assert_eq!(d.client_order_id, 7_650_491_958_644_707_233);
        assert_eq!(to_uint(&d.deposit_hash), to_uint(DEPOSIT_HASH));
        assert_eq!(d.op_nonce, 1); // the field that spilled into the continuation cell
    }
}
