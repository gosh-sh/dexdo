// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use zeroize::ZeroizeOnDrop;

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
    pub oracles: Vec<OracleEntry>,
}

/// One confirmation source for a `MarketEvent`. A PMP can confirm against
/// multiple `OracleEventList` contracts (see `PMPDeployed.oracleEventLists:
/// address[]`), each contributing a row here. `eventName`/`description` are
/// not duplicated — `eventId` is `hash(eventName, description, deadline,
/// outcomeNames)`, so all confirmation sources for the same `eventId` agree
/// on the shared event metadata by construction.
///
/// Fields are unprefixed (`name`/`address`/`fee`): the surrounding
/// `event.oracles[]` array provides the qualifier, so `oracleName` etc.
/// would be redundant on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEntry {
    pub name: Option<String>,
    pub address: Option<String>,
    pub fee: Option<String>,
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
    /// Deterministic OrderBook address from `PMP.getOrderBookAddress()`,
    /// stamped on the first reconcile (migration 0014 CHECK constraint
    /// pins `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT
    /// NULL`). Always present on markets visible to the API — a NULL or
    /// blank value at this point is treated as `MarketInconsistent` →
    /// HTTP 503 by `assemble_market` / depth path. Clients gate trading
    /// availability on `status`, not on this field's presence.
    pub order_book_address: String,
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
    /// Opaque chain-order cursor. Lex-comparable string sourced from
    /// `live_orders.last_chain_order` (which itself comes from the GraphQL
    /// gateway's `msg_chain_order` on every order event). Empty string when
    /// no order event has touched this `(orderbook, outcome)` pair yet.
    /// Clients should treat it as an opaque token: equality detects "no
    /// change", lex order detects "moved forward".
    pub last_update_id: String,
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

/// Access level associated with an api_key. Mirrors the public
/// `USER_DATA` / `TRADE` security levels in `docs/api-spec.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Permission {
    UserData,
    Trade,
}

impl Permission {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserData => "USER_DATA",
            Self::Trade => "TRADE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "USER_DATA" => Some(Self::UserData),
            "TRADE" => Some(Self::Trade),
            _ => None,
        }
    }
}

/// Byte buffer that zeroes its contents on drop. Used for plaintext
/// secrets (decrypted api_secret, decrypted pn_seckey) that must not
/// linger in memory after they are no longer needed. The `Debug`
/// implementation redacts the bytes so accidental `tracing` or `dbg!`
/// calls cannot leak the material.
#[derive(Clone, ZeroizeOnDrop)]
pub struct SensitiveBytes(Vec<u8>);

impl SensitiveBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SensitiveBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SensitiveBytes(<redacted, {} bytes>)", self.0.len())
    }
}

impl From<Vec<u8>> for SensitiveBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_as_str_round_trip() {
        for p in [Permission::UserData, Permission::Trade] {
            assert_eq!(Permission::parse(p.as_str()), Some(p));
        }
    }

    #[test]
    fn permission_parse_rejects_unknown() {
        assert_eq!(Permission::parse(""), None);
        assert_eq!(Permission::parse("user_data"), None); // case sensitive
        assert_eq!(Permission::parse("ADMIN"), None);
    }

    #[test]
    fn sensitive_bytes_debug_redacts() {
        // The secret bytes themselves MUST NOT appear in Debug output —
        // not in length-of-original form, not in any encoded form. A
        // distinctive plaintext makes a regression here obvious.
        let secret = b"super-secret-api-secret-xyz".to_vec();
        let s = SensitiveBytes::new(secret.clone());
        let dbg = format!("{s:?}");
        assert!(dbg.contains("redacted"), "expected redaction marker, got: {dbg}");
        assert!(!dbg.contains("super-secret"), "plaintext leaked into Debug: {dbg}");
        assert!(dbg.contains(&secret.len().to_string()));
    }

    #[test]
    fn sensitive_bytes_basic_accessors() {
        let s = SensitiveBytes::new(vec![1, 2, 3]);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
        assert_eq!(s.as_slice(), &[1, 2, 3]);

        let empty = SensitiveBytes::new(vec![]);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn sensitive_bytes_from_vec() {
        let s: SensitiveBytes = vec![0xab, 0xcd].into();
        assert_eq!(s.as_slice(), &[0xab, 0xcd]);
    }
}
