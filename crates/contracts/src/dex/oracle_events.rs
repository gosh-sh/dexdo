use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::error::KitError;
use ackinacki_kit::contracts::error::KitErrorCode;
use ackinacki_kit::contracts::error::KitModule;
use ackinacki_kit::contracts::event::Event;
use ackinacki_kit::contracts::traits::DecodeMessage;
use ackinacki_kit::contracts::traits::FromEvent;
use ackinacki_kit::contracts::KitResult;
use serde::Deserialize;

/// External event IDs are defined in `dex/modifiers/modifiers.sol`.
/// Note: `OracleEventListDeployed` is emitted with `ORACLE_DEPLOYED` (104) in `Oracle.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `Oracle`.
pub enum OracleEvent {
    OracleEventListDeployed = 104,
    EventPublished = 134,
}

impl TryFrom<String> for OracleEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse oracle event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            104 => Ok(OracleEvent::OracleEventListDeployed),
            134 => Ok(OracleEvent::EventPublished),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown oracle event `{cleaned}`"),
            )),
        }
    }
}

impl Display for OracleEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl OracleEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `Oracle` external event.
pub enum DecodedOracleEvent {
    OracleEventListDeployed { event: Event, kind: OracleEvent, data: OracleEventListDeployedData },
    EventPublished { event: Event, kind: OracleEvent, data: EventPublishedData },
}

impl FromEvent for DecodedOracleEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = OracleEvent::try_from(event.dst.clone())?;
        match kind {
            OracleEvent::OracleEventListDeployed => {
                let decoded = event.decode::<OracleEventListDeployedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for oracle event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedOracleEvent::OracleEventListDeployed { event: event.clone(), kind, data })
            }
            OracleEvent::EventPublished => {
                let decoded = event.decode::<EventPublishedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for oracle event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedOracleEvent::EventPublished { event: event.clone(), kind, data })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `OracleEvent::OracleEventListDeployed`.
pub struct OracleEventListDeployedData {
    #[serde(rename = "eventListAddress")]
    pub event_list_address: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub index: u128,
    /// Human-readable description of the deployed list.
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OracleEvent::EventPublished`.
pub struct EventPublishedData {
    pub event_id: String,
    pub event_name: String,
}
