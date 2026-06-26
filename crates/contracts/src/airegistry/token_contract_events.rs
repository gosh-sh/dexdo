use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u64;
use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::traits::DecodeMessage;
use ackinacki_kit::contracts::traits::FromEvent;
use ackinacki_kit::contracts::KitResult;
use serde::Deserialize;

/// External event ids are defined in `airegistry/modifiers/modifiers.sol`.
/// Each event is emitted to its own `address.makeAddrExtern(<id>, 256)`, so
/// the destination id alone identifies the event. The ABI event names differ
/// from the `*Emit` constant names — these ids are taken from the actual
/// `emit ... makeAddrExtern(<const>)` sites in `TokenContract.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `TokenContract`.
pub enum TokenContractEvent {
    ContractDestroyed = 709,
    ShellWithdrawn = 710,
    StreamFunded = 720,
    StreamOpened = 721,
    TickFinalized = 722,
    StreamStopped = 723,
    StreamDisputed = 724,
    DisputeResolved = 725,
    StreamReclaimed = 726,
    ProbeCommissionFunded = 727,
    ProbeAccepted = 728,
    ProbeBurned = 729,
    ContractDeployed = 703,
}

impl TryFrom<String> for TokenContractEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse token contract event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            703 => Ok(TokenContractEvent::ContractDeployed),
            709 => Ok(TokenContractEvent::ContractDestroyed),
            710 => Ok(TokenContractEvent::ShellWithdrawn),
            720 => Ok(TokenContractEvent::StreamFunded),
            721 => Ok(TokenContractEvent::StreamOpened),
            722 => Ok(TokenContractEvent::TickFinalized),
            723 => Ok(TokenContractEvent::StreamStopped),
            724 => Ok(TokenContractEvent::StreamDisputed),
            725 => Ok(TokenContractEvent::DisputeResolved),
            726 => Ok(TokenContractEvent::StreamReclaimed),
            727 => Ok(TokenContractEvent::ProbeCommissionFunded),
            728 => Ok(TokenContractEvent::ProbeAccepted),
            729 => Ok(TokenContractEvent::ProbeBurned),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown token contract event `{cleaned}`"),
            )),
        }
    }
}

impl Display for TokenContractEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl TokenContractEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `TokenContract` external event.
pub enum DecodedTokenContractEvent {
    ContractDeployed {
        event: Event,
        kind: TokenContractEvent,
        data: ContractDeployedData,
    },
    StreamFunded {
        event: Event,
        kind: TokenContractEvent,
        data: StreamFundedData,
    },
    ProbeCommissionFunded {
        event: Event,
        kind: TokenContractEvent,
        data: ProbeCommissionFundedData,
    },
    StreamOpened {
        event: Event,
        kind: TokenContractEvent,
        data: StreamOpenedData,
    },
    ProbeAccepted {
        event: Event,
        kind: TokenContractEvent,
        data: ProbeAcceptedData,
    },
    ProbeBurned {
        event: Event,
        kind: TokenContractEvent,
        data: ProbeBurnedData,
    },
    TickFinalized {
        event: Event,
        kind: TokenContractEvent,
        data: TickFinalizedData,
    },
    StreamStopped {
        event: Event,
        kind: TokenContractEvent,
        data: StreamStoppedData,
    },
    StreamDisputed {
        event: Event,
        kind: TokenContractEvent,
        data: StreamDisputedData,
    },
    DisputeResolved {
        event: Event,
        kind: TokenContractEvent,
        data: DisputeResolvedData,
    },
    StreamReclaimed {
        event: Event,
        kind: TokenContractEvent,
        data: StreamReclaimedData,
    },
    ShellWithdrawn {
        event: Event,
        kind: TokenContractEvent,
        data: ShellWithdrawnData,
    },
    ContractDestroyed {
        event: Event,
        kind: TokenContractEvent,
        data: ContractDestroyedData,
    },
}

impl FromEvent for DecodedTokenContractEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = TokenContractEvent::try_from(event.dst.clone())?;
        match kind {
            TokenContractEvent::ContractDeployed => {
                let data = decode_or_err::<ContractDeployedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ContractDeployed { event: event.clone(), kind, data })
            }
            TokenContractEvent::StreamFunded => {
                let data = decode_or_err::<StreamFundedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamFunded { event: event.clone(), kind, data })
            }
            TokenContractEvent::ProbeCommissionFunded => {
                let data = decode_or_err::<ProbeCommissionFundedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ProbeCommissionFunded {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            TokenContractEvent::StreamOpened => {
                let data = decode_or_err::<StreamOpenedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamOpened { event: event.clone(), kind, data })
            }
            TokenContractEvent::ProbeAccepted => {
                let data = decode_or_err::<ProbeAcceptedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ProbeAccepted { event: event.clone(), kind, data })
            }
            TokenContractEvent::ProbeBurned => {
                let data = decode_or_err::<ProbeBurnedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ProbeBurned { event: event.clone(), kind, data })
            }
            TokenContractEvent::TickFinalized => {
                let data = decode_or_err::<TickFinalizedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::TickFinalized { event: event.clone(), kind, data })
            }
            TokenContractEvent::StreamStopped => {
                let data = decode_or_err::<StreamStoppedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamStopped { event: event.clone(), kind, data })
            }
            TokenContractEvent::StreamDisputed => {
                let data = decode_or_err::<StreamDisputedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamDisputed { event: event.clone(), kind, data })
            }
            TokenContractEvent::DisputeResolved => {
                let data = decode_or_err::<DisputeResolvedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::DisputeResolved { event: event.clone(), kind, data })
            }
            TokenContractEvent::StreamReclaimed => {
                let data = decode_or_err::<StreamReclaimedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamReclaimed { event: event.clone(), kind, data })
            }
            TokenContractEvent::ShellWithdrawn => {
                let data = decode_or_err::<ShellWithdrawnData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ShellWithdrawn { event: event.clone(), kind, data })
            }
            TokenContractEvent::ContractDestroyed => {
                let data = decode_or_err::<ContractDestroyedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ContractDestroyed {
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
            format!("Unexpected empty data for token contract event `{}`", event.dst),
        )
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ContractDeployed`.
pub struct ContractDeployedData {
    #[serde(rename = "self")]
    pub self_address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::StreamFunded`.
pub struct StreamFundedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub deposit: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ProbeCommissionFunded`.
pub struct ProbeCommissionFundedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::StreamOpened`.
pub struct StreamOpenedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub price_per_tick: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ProbeAccepted`.
pub struct ProbeAcceptedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub to_seller: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub commission_returned: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ProbeBurned`.
pub struct ProbeBurnedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub burned_probe: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub burned_commission: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund_to_buyer: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::TickFinalized`.
pub struct TickFinalizedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub finalized_owed: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub deposit: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::StreamStopped`.
pub struct StreamStoppedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub to_seller: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund_to_buyer: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::StreamDisputed`.
pub struct StreamDisputedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub at: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::DisputeResolved`.
pub struct DisputeResolvedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub to_seller: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund_to_buyer: u128,
    pub released: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::StreamReclaimed`.
pub struct StreamReclaimedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund_to_buyer: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ShellWithdrawn`.
pub struct ShellWithdrawnData {
    pub recipient: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `TokenContractEvent::ContractDestroyed`.
pub struct ContractDestroyedData {
    #[serde(rename = "self")]
    pub self_address: String,
}
