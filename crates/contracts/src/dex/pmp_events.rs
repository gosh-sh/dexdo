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

/// External event IDs are defined in `dex/modifiers/modifiers.sol`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u128)]
/// External events emitted by `PMP`.
pub enum PmpEvent {
    StakeAccepted = 118,
    ApprovedByOracle = 119,
    Resolved = 120,
    ClaimProcessed = 121,
    NetworkFeeBurned = 122,
    TimingsSet = 124,
    NumOutcomesSet = 125,
    EventCancelled = 126,
    PmpRejected = 132,
    CreatorFeeCollected = 137,
    PoolsFrozen = 140,
    SplitProcessed = 141,
    MergeProcessed = 142,
}

impl TryFrom<String> for PmpEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse PMP event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            118 => Ok(PmpEvent::StakeAccepted),
            119 => Ok(PmpEvent::ApprovedByOracle),
            120 => Ok(PmpEvent::Resolved),
            121 => Ok(PmpEvent::ClaimProcessed),
            122 => Ok(PmpEvent::NetworkFeeBurned),
            124 => Ok(PmpEvent::TimingsSet),
            125 => Ok(PmpEvent::NumOutcomesSet),
            126 => Ok(PmpEvent::EventCancelled),
            132 => Ok(PmpEvent::PmpRejected),
            137 => Ok(PmpEvent::CreatorFeeCollected),
            140 => Ok(PmpEvent::PoolsFrozen),
            141 => Ok(PmpEvent::SplitProcessed),
            142 => Ok(PmpEvent::MergeProcessed),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown PMP event `{cleaned}`"),
            )),
        }
    }
}

impl Display for PmpEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl PmpEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `PMP` external event.
pub enum DecodedPmpEvent {
    StakeAccepted { event: Event, kind: PmpEvent, data: StakeAcceptedData },
    ApprovedByOracle { event: Event, kind: PmpEvent, data: ApprovedByOracleData },
    Resolved { event: Event, kind: PmpEvent, data: ResolvedData },
    ClaimProcessed { event: Event, kind: PmpEvent, data: ClaimProcessedData },
    NetworkFeeBurned { event: Event, kind: PmpEvent, data: NetworkFeeBurnedData },
    TimingsSet { event: Event, kind: PmpEvent, data: TimingsSetData },
    NumOutcomesSet { event: Event, kind: PmpEvent, data: NumOutcomesSetData },
    EventCancelled { event: Event, kind: PmpEvent },
    PmpRejected { event: Event, kind: PmpEvent },
    CreatorFeeCollected { event: Event, kind: PmpEvent, data: CreatorFeeCollectedData },
    PoolsFrozen { event: Event, kind: PmpEvent, data: PoolsFrozenData },
    SplitProcessed { event: Event, kind: PmpEvent, data: SplitProcessedData },
    MergeProcessed { event: Event, kind: PmpEvent, data: MergeProcessedData },
}

impl FromEvent for DecodedPmpEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = PmpEvent::try_from(event.dst.clone())?;
        match kind {
            PmpEvent::StakeAccepted => {
                let decoded = event.decode::<StakeAcceptedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::StakeAccepted { event: event.clone(), kind, data })
            }
            PmpEvent::ApprovedByOracle => {
                let decoded = event.decode::<ApprovedByOracleData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::ApprovedByOracle { event: event.clone(), kind, data })
            }
            PmpEvent::Resolved => {
                let decoded = event.decode::<ResolvedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::Resolved { event: event.clone(), kind, data })
            }
            PmpEvent::ClaimProcessed => {
                let decoded = event.decode::<ClaimProcessedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::ClaimProcessed { event: event.clone(), kind, data })
            }
            PmpEvent::NetworkFeeBurned => {
                let decoded = event.decode::<NetworkFeeBurnedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::NetworkFeeBurned { event: event.clone(), kind, data })
            }
            PmpEvent::TimingsSet => {
                let decoded = event.decode::<TimingsSetData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::TimingsSet { event: event.clone(), kind, data })
            }
            PmpEvent::NumOutcomesSet => {
                let decoded = event.decode::<NumOutcomesSetData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::NumOutcomesSet { event: event.clone(), kind, data })
            }
            PmpEvent::EventCancelled => {
                Ok(DecodedPmpEvent::EventCancelled { event: event.clone(), kind })
            }
            PmpEvent::PmpRejected => {
                Ok(DecodedPmpEvent::PmpRejected { event: event.clone(), kind })
            }
            PmpEvent::CreatorFeeCollected => {
                let decoded = event.decode::<CreatorFeeCollectedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::CreatorFeeCollected { event: event.clone(), kind, data })
            }
            PmpEvent::PoolsFrozen => {
                let decoded = event.decode::<PoolsFrozenData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::PoolsFrozen { event: event.clone(), kind, data })
            }
            PmpEvent::SplitProcessed => {
                let decoded = event.decode::<SplitProcessedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::SplitProcessed { event: event.clone(), kind, data })
            }
            PmpEvent::MergeProcessed => {
                let decoded = event.decode::<MergeProcessedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for PMP event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedPmpEvent::MergeProcessed { event: event.clone(), kind, data })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::StakeAccepted`.
pub struct StakeAcceptedData {
    pub note: String,
    #[serde(rename = "outcomeId", deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
    #[serde(rename = "betType", deserialize_with = "deserialize_u8")]
    pub bet_type: u8,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::ApprovedByOracle`.
pub struct ApprovedByOracleData {
    #[serde(rename = "oracleEventList")]
    pub oracle_event_list: String,
    #[serde(rename = "oraclePubkey")]
    pub oracle_pubkey: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::Resolved`.
pub struct ResolvedData {
    #[serde(rename = "outcomeId", deserialize_with = "deserialize_u32")]
    pub outcome_id: u32,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::ClaimProcessed`.
pub struct ClaimProcessedData {
    pub note: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub payout: u128,
    pub win: bool,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::NetworkFeeBurned`.
pub struct NetworkFeeBurnedData {
    #[serde(deserialize_with = "deserialize_u64")]
    pub amount: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::TimingsSet`.
pub struct TimingsSetData {
    #[serde(rename = "stakeStart", deserialize_with = "deserialize_u64")]
    pub stake_start: u64,
    #[serde(rename = "stakeEnd", deserialize_with = "deserialize_u64")]
    pub stake_end: u64,
    #[serde(rename = "resultStart", deserialize_with = "deserialize_u64")]
    pub result_start: u64,
    #[serde(rename = "resultEnd", deserialize_with = "deserialize_u64")]
    pub result_end: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::NumOutcomesSet`.
pub struct NumOutcomesSetData {
    #[serde(rename = "numOutcomes", deserialize_with = "deserialize_u32")]
    pub num_outcomes: u32,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::CreatorFeeCollected`.
pub struct CreatorFeeCollectedData {
    #[serde(deserialize_with = "deserialize_u128")]
    pub fee: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::PoolsFrozen`.
pub struct PoolsFrozenData {
    #[serde(rename = "baseTotalPool", deserialize_with = "deserialize_u128")]
    pub base_total_pool: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::SplitProcessed`.
pub struct SplitProcessedData {
    pub note: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub collateral: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `PmpEvent::MergeProcessed`.
pub struct MergeProcessedData {
    pub note: String,
    #[serde(deserialize_with = "deserialize_u128")]
    pub collateral: u128,
}
