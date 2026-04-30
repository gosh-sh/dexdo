// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketAddress(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketName(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenType {
    Nackl,
    Shell,
    Usdc,
    Unknown(String),
}

impl TokenType {
    pub fn as_code(&self) -> &str {
        match self {
            Self::Nackl => "NACKL",
            Self::Shell => "SHELL",
            Self::Usdc => "USDC",
            Self::Unknown(code) => code.as_str(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub outcome_id: u32,
    pub outcome_name: String,
    pub symbol: Symbol,
    pub price_precision: u8,
    pub quantity_precision: u8,
    pub tick_size: String,
    pub step_size: String,
    pub min_notional: String,
    pub max_batch_size: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub market_address: MarketAddress,
    pub market_name: MarketName,
    pub status: String,
    pub quote_asset: String,
    pub outcomes: Vec<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: String,
    pub quantity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthSnapshot {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub last_update_id: u64,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Oracle {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEventList {
    pub oracle_address: String,
    pub msg_id: String,
    pub address: String,
    pub list_index: Option<u128>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEvent {
    pub event_list_address: String,
    pub event_id: String,
    pub event_name: String,
    pub deadline: u64,
    pub outcome_names: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("mandatory parameter was not sent")]
    MissingParameter,
    #[error("invalid market or symbol")]
    InvalidMarketOrSymbol,
    #[error("authentication required")]
    AuthRequired,
    #[error("timestamp outside recvWindow")]
    TimestampOutsideRecvWindow,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("precision exceeds the maximum defined for this asset")]
    PrecisionExceeded,
    #[error("order would immediately fail validation")]
    OrderValidationFailed,
    #[error("unknown order")]
    UnknownOrder,
    #[error("unexpected domain error")]
    Unexpected,
}

impl DomainError {
    pub fn code(&self) -> i32 {
        match self {
            Self::Unexpected => -1000,
            Self::AuthRequired => -1002,
            Self::TimestampOutsideRecvWindow => -1021,
            Self::InvalidSignature => -1022,
            Self::MissingParameter => -1102,
            Self::PrecisionExceeded => -1111,
            Self::InvalidMarketOrSymbol => -1121,
            Self::OrderValidationFailed => -2010,
            Self::UnknownOrder => -2011,
        }
    }

    pub fn msg(&self) -> &'static str {
        match self {
            Self::Unexpected => "Unknown error.",
            Self::AuthRequired => "Authentication required.",
            Self::TimestampOutsideRecvWindow => "Timestamp outside recvWindow.",
            Self::InvalidSignature => "Invalid signature.",
            Self::MissingParameter => "Mandatory parameter was not sent.",
            Self::PrecisionExceeded => "Precision is over the maximum defined for this asset.",
            Self::InvalidMarketOrSymbol => "Invalid market or symbol.",
            Self::OrderValidationFailed => "Order would immediately fail validation.",
            Self::UnknownOrder => "Unknown order.",
        }
    }
}
