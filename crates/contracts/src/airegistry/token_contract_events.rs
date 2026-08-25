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
    SellerBondFunded = 727,
    ProbeAccepted = 728,
    TicksClaimed = 730,
    EndpointSet = 731,
    BuyerBondFunded = 733,
    ProbeBurned = 729,
    /// The DEAL deploy, `DealDeployedEmit` (732). Not `ContractDeployedEmit` (703):
    /// that constant belongs to `RootModel`, and both sides declare a byte-for-byte
    /// identical `ContractDeployed(address self)`, i.e. they share a body id. Only the
    /// external `dst` can tell them apart — the route table in
    /// `crates/infrastructure/src/decoder.rs`.
    ContractDeployed = 732,
}

impl TokenContractEvent {
    /// See `InferenceOrderBookEvent::ALL`: a test checks the list against the ABI, so
    /// a forgotten variant turns red rather than drifting silently.
    pub const ALL: &'static [Self] = &[
        Self::ContractDestroyed,
        Self::ShellWithdrawn,
        Self::StreamFunded,
        Self::StreamOpened,
        Self::TickFinalized,
        Self::StreamStopped,
        Self::StreamDisputed,
        Self::DisputeResolved,
        Self::SellerBondFunded,
        Self::ProbeAccepted,
        Self::TicksClaimed,
        Self::EndpointSet,
        Self::BuyerBondFunded,
        Self::ProbeBurned,
        Self::ContractDeployed,
    ];
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
            732 => Ok(TokenContractEvent::ContractDeployed),
            709 => Ok(TokenContractEvent::ContractDestroyed),
            710 => Ok(TokenContractEvent::ShellWithdrawn),
            720 => Ok(TokenContractEvent::StreamFunded),
            721 => Ok(TokenContractEvent::StreamOpened),
            722 => Ok(TokenContractEvent::TickFinalized),
            723 => Ok(TokenContractEvent::StreamStopped),
            724 => Ok(TokenContractEvent::StreamDisputed),
            725 => Ok(TokenContractEvent::DisputeResolved),
            727 => Ok(TokenContractEvent::SellerBondFunded),
            728 => Ok(TokenContractEvent::ProbeAccepted),
            730 => Ok(TokenContractEvent::TicksClaimed),
            731 => Ok(TokenContractEvent::EndpointSet),
            733 => Ok(TokenContractEvent::BuyerBondFunded),
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
    ContractDeployed { event: Event, kind: TokenContractEvent, data: ContractDeployedData },
    StreamFunded { event: Event, kind: TokenContractEvent, data: StreamFundedData },
    SellerBondFunded { event: Event, kind: TokenContractEvent, data: SellerBondFundedData },
    StreamOpened { event: Event, kind: TokenContractEvent, data: StreamOpenedData },
    ProbeAccepted { event: Event, kind: TokenContractEvent, data: ProbeAcceptedData },
    TicksClaimed { event: Event, kind: TokenContractEvent, data: TicksClaimedData },
    EndpointSet { event: Event, kind: TokenContractEvent, data: EndpointSetData },
    BuyerBondFunded { event: Event, kind: TokenContractEvent, data: BuyerBondFundedData },
    ProbeBurned { event: Event, kind: TokenContractEvent, data: ProbeBurnedData },
    TickFinalized { event: Event, kind: TokenContractEvent, data: TickFinalizedData },
    StreamStopped { event: Event, kind: TokenContractEvent, data: StreamStoppedData },
    StreamDisputed { event: Event, kind: TokenContractEvent, data: StreamDisputedData },
    DisputeResolved { event: Event, kind: TokenContractEvent, data: DisputeResolvedData },
    ShellWithdrawn { event: Event, kind: TokenContractEvent, data: ShellWithdrawnData },
    ContractDestroyed { event: Event, kind: TokenContractEvent, data: ContractDestroyedData },
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
            TokenContractEvent::SellerBondFunded => {
                let data = decode_or_err::<SellerBondFundedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::SellerBondFunded { event: event.clone(), kind, data })
            }
            TokenContractEvent::StreamOpened => {
                let data = decode_or_err::<StreamOpenedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::StreamOpened { event: event.clone(), kind, data })
            }
            TokenContractEvent::ProbeAccepted => {
                let data = decode_or_err::<ProbeAcceptedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::ProbeAccepted { event: event.clone(), kind, data })
            }
            TokenContractEvent::TicksClaimed => {
                let data = decode_or_err::<TicksClaimedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::TicksClaimed { event: event.clone(), kind, data })
            }
            TokenContractEvent::EndpointSet => {
                let data = decode_or_err::<EndpointSetData>(event, contract)?;
                Ok(DecodedTokenContractEvent::EndpointSet { event: event.clone(), kind, data })
            }
            TokenContractEvent::BuyerBondFunded => {
                let data = decode_or_err::<BuyerBondFundedData>(event, contract)?;
                Ok(DecodedTokenContractEvent::BuyerBondFunded { event: event.clone(), kind, data })
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
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::ContractDeployed`.
pub struct ContractDeployedData {
    #[serde(rename = "self")]
    pub self_address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::StreamFunded`.
pub struct StreamFundedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub deposit: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::SellerBondFunded`.
pub struct SellerBondFundedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::StreamOpened`.
pub struct StreamOpenedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub price_per_tick: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::ProbeAccepted`.
pub struct ProbeAcceptedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub to_seller: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub bond_returned: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::ProbeBurned`.
pub struct ProbeBurnedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub burned_probe: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub burned_bond: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub refund_to_buyer: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::TickFinalized`.
pub struct TickFinalizedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub finalized_owed: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub deposit: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
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
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::StreamDisputed`.
pub struct StreamDisputedData {
    pub buyer: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub at: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
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
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::ShellWithdrawn`.
pub struct ShellWithdrawnData {
    pub recipient: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::ContractDestroyed`.
pub struct ContractDestroyedData {
    #[serde(rename = "self")]
    pub self_address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::TicksClaimed`.
pub struct TicksClaimedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub trusted: u128,
    #[serde(deserialize_with = "deserialize_u128")]
    pub claimed: u128,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::EndpointSet`.
pub struct EndpointSetData {
    /// The ABI type is `bytes` — it arrives as a hex string, nothing to convert.
    pub endpoint_cipher: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, serde(deny_unknown_fields))]
/// Payload of `TokenContractEvent::BuyerBondFunded`.
pub struct BuyerBondFundedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}
