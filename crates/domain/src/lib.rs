// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketId(pub String);

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
    pub market_id: MarketId,
    pub name: String,
    pub status: String,
    pub quote_asset: String,
    pub market_address: String,
    pub outcomes: Vec<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: String,
    pub quantity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthSnapshot {
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
    #[error("invalid symbol")]
    InvalidSymbol,
    #[error("unexpected domain error")]
    Unexpected,
}
