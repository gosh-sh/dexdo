use std::collections::HashMap;
use std::fmt::Display;

use ackinacki_kit::contracts::deserialize::deserialize_u128;
use ackinacki_kit::contracts::deserialize::deserialize_u128_map;
use ackinacki_kit::contracts::deserialize::deserialize_u32;
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
/// External events emitted by `RootPN`.
pub enum RootPnEvent {
    PrivateNoteDeployed = 101,
    NullifierDeployed = 102,
    VoucherGenerated = 135,
    TokensWithdrawn = 154,
    ProtocolFeeCollected = 155,
    ProtocolFeeWithdrawn = 156,
}

impl TryFrom<String> for RootPnEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse root pn event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            101 => Ok(RootPnEvent::PrivateNoteDeployed),
            102 => Ok(RootPnEvent::NullifierDeployed),
            135 => Ok(RootPnEvent::VoucherGenerated),
            154 => Ok(RootPnEvent::TokensWithdrawn),
            155 => Ok(RootPnEvent::ProtocolFeeCollected),
            156 => Ok(RootPnEvent::ProtocolFeeWithdrawn),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown root pn event `{cleaned}`"),
            )),
        }
    }
}

impl Display for RootPnEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl RootPnEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `RootPN` external event.
pub enum DecodedRootPnEvent {
    PrivateNoteDeployed { event: Event, kind: RootPnEvent, data: PrivateNoteDeployedData },
    NullifierDeployed { event: Event, kind: RootPnEvent, data: NullifierDeployedData },
    VoucherGenerated { event: Event, kind: RootPnEvent, data: VoucherGeneratedData },
    TokensWithdrawn { event: Event, kind: RootPnEvent, data: TokensWithdrawnData },
    ProtocolFeeCollected { event: Event, kind: RootPnEvent, data: ProtocolFeeCollectedData },
    ProtocolFeeWithdrawn { event: Event, kind: RootPnEvent, data: ProtocolFeeWithdrawnData },
}

impl FromEvent for DecodedRootPnEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = RootPnEvent::try_from(event.dst.clone())?;

        match kind {
            RootPnEvent::PrivateNoteDeployed => {
                let decoded = event.decode::<PrivateNoteDeployedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::PrivateNoteDeployed { event: event.clone(), kind, data })
            }
            RootPnEvent::NullifierDeployed => {
                let decoded = event.decode::<NullifierDeployedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::NullifierDeployed { event: event.clone(), kind, data })
            }
            RootPnEvent::VoucherGenerated => {
                let decoded = event.decode::<VoucherGeneratedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::VoucherGenerated { event: event.clone(), kind, data })
            }
            RootPnEvent::TokensWithdrawn => {
                let decoded = event.decode::<TokensWithdrawnData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::TokensWithdrawn { event: event.clone(), kind, data })
            }
            RootPnEvent::ProtocolFeeCollected => {
                let decoded = event.decode::<ProtocolFeeCollectedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::ProtocolFeeCollected { event: event.clone(), kind, data })
            }
            RootPnEvent::ProtocolFeeWithdrawn => {
                let decoded = event.decode::<ProtocolFeeWithdrawnData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root pn event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootPnEvent::ProtocolFeeWithdrawn { event: event.clone(), kind, data })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::PrivateNoteDeployed`.
pub struct PrivateNoteDeployedData {
    #[serde(rename = "depositIdentifierHash")]
    pub deposit_identifier_hash: String,
    #[serde(rename = "noteAddress")]
    pub note_address: String,
    #[serde(rename = "initialBalance", deserialize_with = "deserialize_u128")]
    pub initial_balance: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::NullifierDeployed`.
pub struct NullifierDeployedData {
    #[serde(rename = "nullifierAddress")]
    pub nullifier_address: String,
    #[serde(deserialize_with = "deserialize_u64")]
    pub value: u64,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::VoucherGenerated`.
pub struct VoucherGeneratedData {
    #[serde(rename = "skUCommit")]
    pub sk_u_commit: String,
    #[serde(rename = "voucherNominal")]
    pub voucher_nominal: String,
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::TokensWithdrawn`.
pub struct TokensWithdrawnData {
    /// `tokenType → value` for every balance withdrawn in the call.
    #[serde(deserialize_with = "deserialize_u128_map")]
    pub amounts: HashMap<String, u128>,
    /// The PrivateNote that initiated the withdraw (`msg.sender`).
    #[serde(rename = "noteAddress")]
    pub note_address: String,
    /// Destination wallet the tokens were sent to.
    pub to: String,
    #[serde(rename = "dapp_id")]
    pub dapp_id: String,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::ProtocolFeeCollected`.
pub struct ProtocolFeeCollectedData {
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootPnEvent::ProtocolFeeWithdrawn`.
pub struct ProtocolFeeWithdrawnData {
    pub to: String,
    #[serde(rename = "dapp_id")]
    pub dapp_id: String,
    #[serde(rename = "tokenType", deserialize_with = "deserialize_u32")]
    pub token_type: u32,
    #[serde(deserialize_with = "deserialize_u128")]
    pub amount: u128,
}
