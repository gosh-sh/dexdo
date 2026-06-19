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

/// External event IDs are defined in `dex/modifiers/modifiers.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `OracleEventList`.
pub enum OracleEventListEvent {
    EventConfirmed = 106,
    DescriptionUpdated = 107,
    EventAdded = 133,
}

impl TryFrom<String> for OracleEventListEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse oracle event list event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            106 => Ok(OracleEventListEvent::EventConfirmed),
            107 => Ok(OracleEventListEvent::DescriptionUpdated),
            133 => Ok(OracleEventListEvent::EventAdded),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown oracle event list event `{cleaned}`"),
            )),
        }
    }
}

impl Display for OracleEventListEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl OracleEventListEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `OracleEventList` external event.
pub enum DecodedOracleEventListEvent {
    EventAdded { event: Event, kind: OracleEventListEvent, data: EventAddedData },
    EventConfirmed { event: Event, kind: OracleEventListEvent, data: EventConfirmedData },
    DescriptionUpdated { event: Event, kind: OracleEventListEvent, data: DescriptionUpdatedData },
}

impl FromEvent for DecodedOracleEventListEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = OracleEventListEvent::try_from(event.dst.clone())?;
        match kind {
            OracleEventListEvent::EventAdded => {
                let decoded = event.decode::<EventAddedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!(
                            "Unexpected empty data for oracle event list event `{}`",
                            event.dst
                        ),
                    )
                })?;
                Ok(DecodedOracleEventListEvent::EventAdded { event: event.clone(), kind, data })
            }
            OracleEventListEvent::EventConfirmed => {
                let decoded = event.decode::<EventConfirmedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!(
                            "Unexpected empty data for oracle event list event `{}`",
                            event.dst
                        ),
                    )
                })?;
                Ok(DecodedOracleEventListEvent::EventConfirmed { event: event.clone(), kind, data })
            }
            OracleEventListEvent::DescriptionUpdated => {
                let decoded = event.decode::<DescriptionUpdatedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!(
                            "Unexpected empty data for oracle event list event `{}`",
                            event.dst
                        ),
                    )
                })?;
                Ok(DecodedOracleEventListEvent::DescriptionUpdated {
                    event: event.clone(),
                    kind,
                    data,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OracleEventListEvent::EventAdded`.
pub struct EventAddedData {
    pub event_id: String,
    pub event_name: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub oracle_fee: u128,
    #[serde(deserialize_with = "deserialize_u64")]
    pub deadline: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Payload of `OracleEventListEvent::EventConfirmed`.
pub struct EventConfirmedData {
    pub event_id: String,
    pub pmp_address: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `OracleEventListEvent::DescriptionUpdated`.
pub struct DescriptionUpdatedData {
    pub description: String,
}
