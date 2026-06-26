use std::fmt::Display;

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
/// the destination id alone identifies the event.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `RootModel`.
pub enum RootModelEvent {
    TokenContractRegistered = 702,
    ContractDeployed = 703,
}

impl TryFrom<String> for RootModelEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse root model event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            702 => Ok(RootModelEvent::TokenContractRegistered),
            703 => Ok(RootModelEvent::ContractDeployed),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown root model event `{cleaned}`"),
            )),
        }
    }
}

impl Display for RootModelEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl RootModelEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `RootModel` external event.
pub enum DecodedRootModelEvent {
    TokenContractRegistered {
        event: Event,
        kind: RootModelEvent,
        data: TokenContractRegisteredData,
    },
    ContractDeployed {
        event: Event,
        kind: RootModelEvent,
        data: ContractDeployedData,
    },
}

impl FromEvent for DecodedRootModelEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = RootModelEvent::try_from(event.dst.clone())?;
        match kind {
            RootModelEvent::TokenContractRegistered => {
                let data = decode_or_err::<TokenContractRegisteredData>(event, contract)?;
                Ok(DecodedRootModelEvent::TokenContractRegistered {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
            RootModelEvent::ContractDeployed => {
                let data = decode_or_err::<ContractDeployedData>(event, contract)?;
                Ok(DecodedRootModelEvent::ContractDeployed { event: event.clone(), kind, data })
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
            format!("Unexpected empty data for root model event `{}`", event.dst),
        )
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `RootModelEvent::TokenContractRegistered`.
pub struct TokenContractRegisteredData {
    pub token_contract_address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `RootModelEvent::ContractDeployed`.
pub struct ContractDeployedData {
    #[serde(rename = "self")]
    pub self_address: String,
}
