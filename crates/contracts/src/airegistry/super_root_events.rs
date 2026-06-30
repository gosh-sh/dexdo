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
/// External events emitted by `SuperRoot`.
pub enum SuperRootEvent {
    RootRegistered = 700,
}

impl TryFrom<String> for SuperRootEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse super root event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            700 => Ok(SuperRootEvent::RootRegistered),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown super root event `{cleaned}`"),
            )),
        }
    }
}

impl Display for SuperRootEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl SuperRootEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `SuperRoot` external event.
pub enum DecodedSuperRootEvent {
    RootRegistered { event: Event, kind: SuperRootEvent, data: RootRegisteredData },
}

impl FromEvent for DecodedSuperRootEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = SuperRootEvent::try_from(event.dst.clone())?;
        match kind {
            SuperRootEvent::RootRegistered => {
                let data = decode_or_err::<RootRegisteredData>(event, contract)?;
                Ok(DecodedSuperRootEvent::RootRegistered { event: event.clone(), kind, data })
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
            format!("Unexpected empty data for super root event `{}`", event.dst),
        )
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `SuperRootEvent::RootRegistered`.
pub struct RootRegisteredData {
    pub root_address: String,
}
