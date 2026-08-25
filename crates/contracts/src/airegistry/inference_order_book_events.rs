use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_u128;
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

/// External event ids are defined in `airegistry/modifiers/modifiers.sol`
/// (dedicated 1000+ range). Each event is emitted to its own
/// `address.makeAddrExtern(<id>, 256)`, so the destination id alone
/// identifies the event. The ABI event names differ from the `*Emit` constant
/// names — these ids are taken from the actual `emit ... makeAddrExtern(<const>)`
/// sites in `InferenceOrderBook.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `InferenceOrderBook`.
pub enum InferenceOrderBookEvent {
    OrderPlaced = 1000,
    OrderCancelled = 1001,
    Refunded = 1002,
    Filled = 1003,
    Executed = 1004,
    // 1006 (CycleForfeitedEmit) and 1007 (ForfeitClaimedEmit) remain reserved in
    // `modifiers.sol`, but the contract no longer declares or emits the matching
    // events, so there is nothing to decode into.
    InferenceOrderBookDeployed = 1008,
    OrderExpired = 1010,
    OrderRejected = 1011,
    OrderCancelRejected = 1009,
}

impl InferenceOrderBookEvent {
    /// Every variant, exactly once. Rust cannot enumerate variants on its own, and a
    /// hand-written list HERE is safe precisely because a test checks it against the
    /// ABI: a forgotten variant is caught rather than carried forward.
    pub const ALL: &'static [Self] = &[
        Self::OrderPlaced,
        Self::OrderCancelled,
        Self::Refunded,
        Self::Filled,
        Self::Executed,
        Self::InferenceOrderBookDeployed,
        Self::OrderExpired,
        Self::OrderRejected,
        Self::OrderCancelRejected,
    ];
}

impl TryFrom<String> for InferenceOrderBookEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse inference order book event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            1000 => Ok(InferenceOrderBookEvent::OrderPlaced),
            1001 => Ok(InferenceOrderBookEvent::OrderCancelled),
            1002 => Ok(InferenceOrderBookEvent::Refunded),
            1003 => Ok(InferenceOrderBookEvent::Filled),
            1004 => Ok(InferenceOrderBookEvent::Executed),
            1008 => Ok(InferenceOrderBookEvent::InferenceOrderBookDeployed),
            1010 => Ok(InferenceOrderBookEvent::OrderExpired),
            1011 => Ok(InferenceOrderBookEvent::OrderRejected),
            1009 => Ok(InferenceOrderBookEvent::OrderCancelRejected),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown inference order book event `{cleaned}`"),
            )),
        }
    }
}

impl Display for InferenceOrderBookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl InferenceOrderBookEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `InferenceOrderBook` external event.
pub enum DecodedInferenceOrderBookEvent {
    OrderPlaced {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderPlacedData,
    },
    OrderCancelled {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderCancelledData,
    },
    Refunded {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: RefundedData,
    },
    Filled {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: FilledData,
    },
    Executed {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: ExecutedData,
    },
    InferenceOrderBookDeployed {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderBookDeployedData,
    },
    OrderExpired {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderExpiredData,
    },
    OrderRejected {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderRejectedData,
    },
    OrderCancelRejected {
        event: Event,
        kind: InferenceOrderBookEvent,
        data: OrderCancelRejectedData,
    },
}

impl FromEvent for DecodedInferenceOrderBookEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = InferenceOrderBookEvent::try_from(event.dst.clone())?;
        match kind {
            InferenceOrderBookEvent::OrderPlaced => {
                let data = decode_or_err::<OrderPlacedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::OrderPlaced { event: event.clone(), kind, data })
            }
            InferenceOrderBookEvent::OrderCancelled => {
                let data = decode_or_err::<OrderCancelledData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::OrderCancelled {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            InferenceOrderBookEvent::Refunded => {
                let data = decode_or_err::<RefundedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::Refunded { event: event.clone(), kind, data })
            }
            InferenceOrderBookEvent::Filled => {
                let data = decode_or_err::<FilledData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::Filled { event: event.clone(), kind, data })
            }
            InferenceOrderBookEvent::Executed => {
                let data = decode_or_err::<ExecutedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::Executed { event: event.clone(), kind, data })
            }
            InferenceOrderBookEvent::InferenceOrderBookDeployed => {
                let data = decode_or_err::<OrderBookDeployedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::InferenceOrderBookDeployed {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            InferenceOrderBookEvent::OrderExpired => {
                let data = decode_or_err::<OrderExpiredData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::OrderExpired {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            InferenceOrderBookEvent::OrderRejected => {
                let data = decode_or_err::<OrderRejectedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::OrderRejected {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            InferenceOrderBookEvent::OrderCancelRejected => {
                let data = decode_or_err::<OrderCancelRejectedData>(event, contract)?;
                Ok(DecodedInferenceOrderBookEvent::OrderCancelRejected {
                    event: event.clone(),
                    kind,
                    data,
                })
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
            format!("Unexpected empty data for inference order book event `{}`", event.dst),
        )
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::InferenceOrderBookDeployed`.
pub struct OrderBookDeployedData {
    /// Deployer note address; not stored as model identity.
    pub note: String,
    /// `uint256` model hash as returned by the ABI.
    pub model_hash: String,
    /// Canonical-ish model name (may be empty).
    pub model_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::OrderPlaced`.
pub struct OrderPlacedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    pub is_buy: bool,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub ticks: u128,
    pub note: String,
    pub token_contract: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub deadline: u64,
    /// Flag mask the placement carried (`IOC`/`FOK`/`MARKET`/`POST_ONLY`). The
    /// book emits this for EVERY placement, including pure-taker and rejected
    /// POST_ONLY orders that never rest, so it is what tells a resting order
    /// from one that was never going to rest.
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::OrderCancelled`.
pub struct OrderCancelledData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refunded: u128,
    pub note: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::Refunded`.
pub struct RefundedData {
    /// The order the refund concerns. It arrived with v4.0.33 and until wave 0 was
    /// read by nothing: without it a refund cannot be tied to a row, and a
    /// continuation expiry — the only path with no other events — did not close the
    /// order at all.
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    pub note: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::Filled`.
pub struct FilledData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub maker_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub taker_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub ticks: u128,
    /// `uint256` represented as returned by ABI.
    pub clearing_price: String,
    /// ABI field is `sellerTC` (both caps); `rename_all = camelCase` would emit
    /// `sellerTc`, so the name is pinned explicitly.
    /// `Option` rather than `String`, and that is a decision, not an oversight: the
    /// deal link is not worth failing an event forever over. ABI drift here is logged
    /// and skipped (`link_deal_from_filled`), and what catches it is the shape guard,
    /// not the runtime.
    #[serde(default, rename = "sellerTC")]
    pub seller_tc: Option<String>,
    pub buyer_note: String,
    #[serde(default)]
    pub seller_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::Executed`.
pub struct ExecutedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub ticks: u128,
    /// `uint256` represented as returned by ABI.
    pub clearing_price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub cost: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::OrderCancelRejected`. Emitted by
/// `_doCancel` so a cancel that changes nothing still has an owner-observable
/// outcome: `reason` is 0 when no such order rests on the book (already filled
/// or cancelled) and 1 when `note` is not the order's owner.
pub struct OrderCancelRejectedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u8")]
    pub reason: u8,
    pub note: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::OrderExpired`.
pub struct OrderExpiredData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    pub is_buy: bool,
    pub note: String,
    pub token_contract: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `InferenceOrderBookEvent::OrderRejected`.
pub struct OrderRejectedData {
    #[serde(deserialize_with = "deserialize_u8")]
    pub reason: u8,
    pub note: String,
    pub token_contract: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund: u128,
}

/// Platform fee in basis points, charged on top of every tick's price
/// (`InferenceOrderBook._tickFee`). Mirrors `PLATFORM_FEE_BPS` in
/// `contracts/airegistry/modifiers/modifiers.sol`.
///
/// A COPY OF A CONTRACT CONSTANT, AND THAT IS A LIABILITY, NOT A CONVENIENCE.
/// The indexer needs it because the escrow figure the book sends a deal —
/// `debit`, which the deal records as its `deposit` — is emitted by no event: it
/// is computed inside `_match` from values that ARE emitted. Reproducing that
/// arithmetic is the only way to recover the number, and a silent drift in either
/// constant would make every recovered deposit quietly wrong. `tests/order_book_fee_constants.rs`
/// re-derives both from the Solidity source on every run so the drift is loud.
pub const PLATFORM_FEE_BPS: u32 = 250;

/// Denominator for [`PLATFORM_FEE_BPS`] — `BPS_DENOMINATOR` in
/// `contracts/airegistry/modifiers/modifiers.sol`.
pub const BPS_DENOMINATOR: u32 = 10_000;

/// Flag bit marking a placement as a subscription (`FLAG_SUBSCRIPTION`). The
/// buyer of a subscription posts a `2 × clearingPrice` bond that rides along with
/// the escrow, so the deal's deposit is larger than the tick cost by exactly that
/// much; every other deal type carries no bond on this path.
pub const FLAG_SUBSCRIPTION: u8 = 0x40;
