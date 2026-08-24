use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_option_u32;
use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u128_vec;
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

/// External event IDs are defined in `dex/modifiers/modifiers.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `PrivateNote`.
pub enum PrivateNoteEvent {
    PmpDeployed = 111,
    OwnerChanged = 112,
    StakeConfirmed = 113,
    ClaimAccepted = 114,
    StakeCancelled = 115,
    // Emitted to the SPLIT/MERGE address slots (138/139), not the like-named
    // FULLSET_STAKE_* constants 116/117 — see PrivateNote.sol:946,1059.
    FullSetStakeConfirmed = 138,
    FullSetStakeCancelled = 139,
    OrderPlacedConfirmed = 147,
    OrderFilledConfirmed = 148,
    TransferInitiated = 149,
    TransferReceived = 150,
    OrderSubmitted = 151,
    OrderCancelledConfirmed = 152,
    OrderPlaceRejected = 153,
}

impl TryFrom<String> for PrivateNoteEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse private note event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            111 => Ok(PrivateNoteEvent::PmpDeployed),
            112 => Ok(PrivateNoteEvent::OwnerChanged),
            113 => Ok(PrivateNoteEvent::StakeConfirmed),
            114 => Ok(PrivateNoteEvent::ClaimAccepted),
            115 => Ok(PrivateNoteEvent::StakeCancelled),
            138 => Ok(PrivateNoteEvent::FullSetStakeConfirmed),
            139 => Ok(PrivateNoteEvent::FullSetStakeCancelled),
            147 => Ok(PrivateNoteEvent::OrderPlacedConfirmed),
            148 => Ok(PrivateNoteEvent::OrderFilledConfirmed),
            149 => Ok(PrivateNoteEvent::TransferInitiated),
            150 => Ok(PrivateNoteEvent::TransferReceived),
            151 => Ok(PrivateNoteEvent::OrderSubmitted),
            152 => Ok(PrivateNoteEvent::OrderCancelledConfirmed),
            153 => Ok(PrivateNoteEvent::OrderPlaceRejected),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown private note event `{cleaned}`"),
            )),
        }
    }
}

impl Display for PrivateNoteEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl PrivateNoteEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `PrivateNote` external event.
pub enum DecodedPrivateNoteEvent {
    PmpDeployed {
        event: Event,
        kind: PrivateNoteEvent,
        data: PmpDeployedData,
    },
    OwnerChanged {
        event: Event,
        kind: PrivateNoteEvent,
        data: OwnerChangedData,
    },
    StakeConfirmed {
        event: Event,
        kind: PrivateNoteEvent,
        data: StakeConfirmedData,
    },
    ClaimAccepted {
        event: Event,
        kind: PrivateNoteEvent,
        data: ClaimAcceptedData,
    },
    StakeCancelled {
        event: Event,
        kind: PrivateNoteEvent,
        data: StakeCancelledData,
    },
    FullSetStakeConfirmed {
        event: Event,
        kind: PrivateNoteEvent,
        data: FullSetStakeConfirmedData,
    },
    FullSetStakeCancelled {
        event: Event,
        kind: PrivateNoteEvent,
        data: FullSetStakeCancelledData,
    },
    TransferInitiated {
        event: Event,
        kind: PrivateNoteEvent,
        data: TransferInitiatedData,
    },
    TransferReceived {
        event: Event,
        kind: PrivateNoteEvent,
        data: TransferReceivedData,
    },
    OrderSubmitted {
        event: Event,
        kind: PrivateNoteEvent,
        data: OrderSubmittedData,
    },
    OrderPlacedConfirmed {
        event: Event,
        kind: PrivateNoteEvent,
        data: OrderPlacedConfirmedData,
    },
    OrderFilledConfirmed {
        event: Event,
        kind: PrivateNoteEvent,
        data: OrderFilledConfirmedData,
    },
    OrderCancelledConfirmed {
        event: Event,
        kind: PrivateNoteEvent,
        data: OrderCancelledConfirmedData,
    },
    OrderPlaceRejected {
        event: Event,
        kind: PrivateNoteEvent,
        data: OrderPlaceRejectedData,
    },
}

impl FromEvent for DecodedPrivateNoteEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = PrivateNoteEvent::try_from(event.dst.clone())?;
        match kind {
            PrivateNoteEvent::PmpDeployed => {
                let decoded = event.decode::<PmpDeployedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::PmpDeployed { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::OwnerChanged => {
                let decoded = event.decode::<OwnerChangedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OwnerChanged { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::StakeConfirmed => {
                let decoded = event.decode::<StakeConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::StakeConfirmed { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::ClaimAccepted => {
                let decoded = event.decode::<ClaimAcceptedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::ClaimAccepted { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::StakeCancelled => {
                let decoded = event.decode::<StakeCancelledData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::StakeCancelled { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::FullSetStakeConfirmed => {
                let decoded = event.decode::<FullSetStakeConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::FullSetStakeConfirmed {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            PrivateNoteEvent::FullSetStakeCancelled => {
                let decoded = event.decode::<FullSetStakeCancelledData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::FullSetStakeCancelled {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            PrivateNoteEvent::TransferInitiated => {
                let decoded = event.decode::<TransferInitiatedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::TransferInitiated { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::TransferReceived => {
                let decoded = event.decode::<TransferReceivedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::TransferReceived { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::OrderSubmitted => {
                let decoded = event.decode::<OrderSubmittedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OrderSubmitted { event: event.clone(), kind, data })
            }
            PrivateNoteEvent::OrderPlacedConfirmed => {
                let decoded = event.decode::<OrderPlacedConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OrderPlacedConfirmed {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            PrivateNoteEvent::OrderFilledConfirmed => {
                let decoded = event.decode::<OrderFilledConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OrderFilledConfirmed {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            PrivateNoteEvent::OrderCancelledConfirmed => {
                let decoded = event.decode::<OrderCancelledConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OrderCancelledConfirmed {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            PrivateNoteEvent::OrderPlaceRejected => {
                let decoded = event.decode::<OrderPlaceRejectedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for private note event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPrivateNoteEvent::OrderPlaceRejected { event: event.clone(), kind, data })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::OwnerChanged`.
pub struct OwnerChangedData {
    #[serde(rename = "oldPubkey")]
    pub old_pubkey: String,
    #[serde(rename = "newPubkey")]
    pub new_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::StakeConfirmed`.
pub struct StakeConfirmedData {
    #[serde(rename = "stakeController")]
    pub stake_controller: String,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(rename = "betType", deserialize_with = "deserialize_u8")]
    pub bet_type: u8,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::StakeCancelled`.
pub struct StakeCancelledData {
    #[serde(rename = "stakeController")]
    pub stake_controller: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub value: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::FullSetStakeConfirmed`.
pub struct FullSetStakeConfirmedData {
    #[serde(rename = "stakeController")]
    pub stake_controller: String,
    #[serde(deserialize_with = "deserialize_u128_vec")]
    pub amount: Vec<u128>,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::FullSetStakeCancelled`.
pub struct FullSetStakeCancelledData {
    #[serde(rename = "stakeController")]
    pub stake_controller: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub value: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::ClaimAccepted`.
pub struct ClaimAcceptedData {
    #[serde(rename = "stakeController")]
    pub stake_controller: String,
    #[serde(deserialize_with = "deserialize_option_u32")]
    pub outcome: Option<u32>,
    #[serde(deserialize_with = "deserialize_u128")]
    pub payout: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::PmpDeployed`.
pub struct PmpDeployedData {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(rename = "pmpAddress")]
    pub pmp_address: String,
    #[serde(rename = "oracleEventLists")]
    pub oracle_event_lists: Vec<String>,
    #[serde(rename = "oracleFee", deserialize_with = "deserialize_u128_vec")]
    pub oracle_fee: Vec<u128>,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::TransferInitiated`.
pub struct TransferInitiatedData {
    pub dest: String,
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PrivateNoteEvent::TransferReceived`.
pub struct TransferReceivedData {
    pub from: String,
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `PrivateNoteEvent::OrderSubmitted`. Emitted by the owner-facing
/// `placeOrder` / `placeBatch` call before the OrderBook callback round-trip.
pub struct OrderSubmittedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    pub event_id: String,
    #[serde(deserialize_with = "deserialize_u32")]
    pub token_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `PrivateNoteEvent::OrderPlacedConfirmed`. Emitted after the
/// `OrderBook.onOrderPlaced` callback has been processed.
pub struct OrderPlacedConfirmedData {
    pub order_book: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `PrivateNoteEvent::OrderFilledConfirmed`. Emitted after each
/// `OrderBook.onOrderFilled` callback has settled.
pub struct OrderFilledConfirmedData {
    pub order_book: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub filled_amount: u128,
    /// `uint256` represented as returned by ABI.
    pub clearing_price: String,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u128")]
    pub fee_amount: u128,
    pub is_rebate: bool,
    pub is_final: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `PrivateNoteEvent::OrderCancelledConfirmed`. Emitted after each
/// `OrderBook.onOrderCancelled` callback has settled.
pub struct OrderCancelledConfirmedData {
    pub order_book: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u128")]
    pub return_amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `PrivateNoteEvent::OrderPlaceRejected`. Emitted when the
/// OrderBook bounces back a `placeOrder` request before it could be accepted.
pub struct OrderPlaceRejectedData {
    pub order_book: String,
    /// `uint256` represented as returned by ABI.
    pub event_id: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub client_order_id: u128,
    #[serde(deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    pub is_buy: bool,
    #[serde(deserialize_with = "deserialize_u8")]
    pub flags: u8,
    /// `uint256` represented as returned by ABI.
    pub price: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(deserialize_with = "deserialize_u64")]
    pub op_nonce: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `PrivateNote.InferenceDealClosed` (external id 166).
///
/// ONE FIELD, AND THAT IS THE WHOLE EVENT. The note is told by a deal in the act
/// of self-destructing (`TokenContract._die`), so the deal both authenticates and
/// identifies itself as `msg.sender`, and there is nothing else to pass. What it
/// does NOT carry is the reason: `close_kind` and `clean_settlement` are not
/// derivable from this event, and the reader must leave them alone rather than
/// infer them from surrounding payments.
pub struct InferenceDealClosedData {
    /// Address of the `TokenContract` that closed — the deal, not the note. The
    /// note is the event's `src`; confusing the two writes the settlement onto a
    /// row keyed by the wrong party.
    pub deal: String,
}
