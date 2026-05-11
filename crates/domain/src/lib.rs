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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketStatus {
    Pending,
    Upcoming,
    Staking,
    AwaitingFreeze,
    Trading,
    Resolving,
    Resolved,
    Cancelled,
    Expired,
}

impl MarketStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Upcoming => "UPCOMING",
            Self::Staking => "STAKING",
            Self::AwaitingFreeze => "AWAITING_FREEZE",
            Self::Trading => "TRADING",
            Self::Resolving => "RESOLVING",
            Self::Resolved => "RESOLVED",
            Self::Cancelled => "CANCELLED",
            Self::Expired => "EXPIRED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(Self::Pending),
            "UPCOMING" => Some(Self::Upcoming),
            "STAKING" => Some(Self::Staking),
            "AWAITING_FREEZE" => Some(Self::AwaitingFreeze),
            "TRADING" => Some(Self::Trading),
            "RESOLVING" => Some(Self::Resolving),
            "RESOLVED" => Some(Self::Resolved),
            "CANCELLED" => Some(Self::Cancelled),
            "EXPIRED" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timings {
    pub stake_start: i64,
    pub stake_end: i64,
    pub result_start: i64,
    pub result_end: i64,
    pub frozen_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketEvent {
    pub event_id: String,
    pub event_name: Option<String>,
    pub description: Option<String>,
    pub oracle_name: Option<String>,
    pub oracle_address: Option<String>,
    pub oracle_fee: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalKind {
    Resolved,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CancelReason {
    PmpCancelled,
    EventCancelled,
}

impl CancelReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PmpCancelled => "PMP_CANCELLED",
            Self::EventCancelled => "EVENT_CANCELLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PMP_CANCELLED" => Some(Self::PmpCancelled),
            "EVENT_CANCELLED" => Some(Self::EventCancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Terminal {
    pub kind: TerminalKind,
    pub at: i64,
    pub resolved_outcome_id: Option<u32>,
    pub cancel_reason: Option<CancelReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub market_address: MarketAddress,
    /// `None` until the on-chain OrderBook is deployed for this PMP. The
    /// reconciler can populate every other PMP-state field via `getDetails`
    /// before the OrderBook contract exists on-chain, so this stays nullable
    /// in the public contract; clients gate trading availability on `status`.
    pub order_book_address: Option<String>,
    pub market_name: MarketName,
    pub status: MarketStatus,
    pub quote_asset: String,
    pub token_type: i32,
    pub created_at: i64,
    pub timings: Option<Timings>,
    pub event: MarketEvent,
    pub terminal: Option<Terminal>,
    pub outcomes: Vec<Outcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketsPage {
    pub markets: Vec<Market>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DomainError {
    #[error("mandatory parameter was not sent")]
    MissingParameter,
    #[error("invalid value for a query parameter")]
    InvalidParameter,
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
    /// The read-model row violates a tech-spec invariant (e.g. RESOLVED with
    /// `frozenAt = null`, CANCELLED with `cancelReason = null`). Per
    /// docs/tech-spec.md:113 these rows MUST be rejected rather than
    /// serialized — the indexer is mid-replay and a consistent view is not
    /// available yet. Surfaces as a 503 so clients know to retry.
    #[error("market read-model is temporarily inconsistent")]
    MarketInconsistent,
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
            Self::InvalidParameter => -1130,
            Self::MarketInconsistent => -1500,
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
            Self::InvalidParameter => "Invalid value for a query parameter.",
            Self::MarketInconsistent => "Market data is temporarily inconsistent.",
            Self::OrderValidationFailed => "Order would immediately fail validation.",
            Self::UnknownOrder => "Unknown order.",
        }
    }
}
