use std::fmt::Display;

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
/// External events emitted by `RootOracle`.
pub enum RootOracleEvent {
    OracleDeployed = 136,
}

impl TryFrom<String> for RootOracleEvent {
    type Error = KitError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let cleaned = value.replace(":", "");
        let number = u128::from_str_radix(&cleaned, 16).map_err(|e| {
            KitError::new(
                KitModule::Event,
                KitErrorCode::Parse,
                format!("Parse root oracle event `{cleaned}` into u128 ({e})"),
            )
        })?;

        match number {
            136 => Ok(RootOracleEvent::OracleDeployed),
            _ => Err(KitError::new(
                KitModule::Event,
                KitErrorCode::UnknownEvent,
                format!("Unknown root oracle event `{cleaned}`"),
            )),
        }
    }
}

impl Display for RootOracleEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, ":{:064x}", *self as u128)
    }
}

impl RootOracleEvent {
    pub fn to_address(&self) -> String {
        format!("0:{:064x}", *self as u128)
    }
}

/// Typed decoded `RootOracle` external event.
pub enum DecodedRootOracleEvent {
    OracleDeployed { event: Event, kind: RootOracleEvent, data: OracleDeployedData },
}

impl FromEvent for DecodedRootOracleEvent {
    fn from_event(event: &Event, contract: &impl DecodeMessage) -> KitResult<Self> {
        let kind = RootOracleEvent::try_from(event.dst.clone())?;
        match kind {
            RootOracleEvent::OracleDeployed => {
                let decoded = event.decode::<OracleDeployedData>(contract)?;
                let data = decoded.ok_or_else(|| {
                    KitError::new(
                        KitModule::Event,
                        KitErrorCode::EmptyData,
                        format!("Unexpected empty data for root oracle event `{}`", event.dst),
                    )
                })?;
                Ok(DecodedRootOracleEvent::OracleDeployed { event: event.clone(), kind, data })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
/// Payload of `RootOracleEvent::OracleDeployed`.
pub struct OracleDeployedData {
    pub oracle: String,
    pub pubkey: String,
    pub name: String,
}
