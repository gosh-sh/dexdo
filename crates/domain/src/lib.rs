// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use num_bigint::BigUint;
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
    PmpRejectedByOracle,
    EventCancelled,
}

impl CancelReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PmpRejectedByOracle => "PMP_REJECTED_BY_ORACLE",
            Self::EventCancelled => "EVENT_CANCELLED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PMP_REJECTED_BY_ORACLE" => Some(Self::PmpRejectedByOracle),
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

/// Maker fee rate, global on-chain (no per-market lookup).
/// Negative = maker rebate funded from the taker fee.
pub const MAKER_COMMISSION: &str = "-0.0003375";

/// Taker fee rate applied to trades, mirroring `TAKER_FEE_RATE` /
/// `FEE_DENOMINATOR` from `contracts/modifiers/modifiers.sol`. Always
/// non-negative.
pub const TAKER_COMMISSION: &str = "0.0004500";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub market_address: MarketAddress,
    /// Deterministic OrderBook address from `PMP.getOrderBookAddress()`,
    /// stamped on the first reconcile (the schema CHECK constraint pins
    /// `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL`).
    /// Always present on markets visible to the API — a NULL or blank value
    /// at this point is treated as `MarketInconsistent` →
    /// HTTP 503 by `assemble_market` / depth path. Clients gate trading
    /// availability on `status`, not on this field's presence.
    pub order_book_address: String,
    /// `0x`-prefixed hex of `PMP.getDetails().oracleListHash`, stamped
    /// by the market reconciler. Required by `placeOrder` / `placeBatch`
    /// chain calls; the public `/api/v1/markets` DTO does not surface
    /// it. Empty string on a reconciled row is treated as
    /// `MarketInconsistent` → HTTP 503 by the trading path.
    pub oracle_list_hash: String,
    pub market_name: MarketName,
    pub status: MarketStatus,
    pub quote_asset: String,
    pub token_type: i32,
    pub maker_commission: String,
    pub taker_commission: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderIdentity {
    Chain(String),
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OrderParts {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub order_id: String,
    pub client_order_id: String,
    pub price: String,
    pub orig_qty: String,
    pub executed_qty: String,
    pub status: OrderStatus,
    pub time_in_force: TimeInForce,
    pub order_type: OrderType,
    pub side: OrderSide,
    pub time: i64,
    pub update_time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Order {
    market_address: MarketAddress,
    symbol: Symbol,
    /// Chain-side order id as a decimal string. Empty when the row's
    /// `status` is `Rejected` — the chain never assigns an id to a
    /// rejected placement.
    order_id: String,
    client_order_id: String,
    price: String,
    orig_qty: String,
    executed_qty: String,
    status: OrderStatus,
    time_in_force: TimeInForce,
    order_type: OrderType,
    side: OrderSide,
    time: i64,
    update_time: i64,
}

impl Order {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        market_address: MarketAddress,
        symbol: Symbol,
        identity: OrderIdentity,
        client_order_id: String,
        price: String,
        orig_qty: String,
        executed_qty: String,
        status: OrderStatus,
        time_in_force: TimeInForce,
        order_type: OrderType,
        side: OrderSide,
        time: i64,
        update_time: i64,
    ) -> Result<Self, DomainError> {
        let order_id = match (status, identity) {
            (OrderStatus::Rejected, OrderIdentity::Rejected) => {
                if !decimal_string_is_zero(&executed_qty)? {
                    return Err(DomainError::MarketInconsistent);
                }
                String::new()
            }
            (OrderStatus::Rejected, OrderIdentity::Chain(_)) => {
                return Err(DomainError::MarketInconsistent);
            }
            (_, OrderIdentity::Rejected) => return Err(DomainError::MarketInconsistent),
            (_, OrderIdentity::Chain(order_id)) => {
                if order_id.trim().is_empty() {
                    return Err(DomainError::MarketInconsistent);
                }
                order_id
            }
        };

        // Quantities are validated by `decimal_string_*` below; `price` only
        // needs well-formedness here because no downstream check parses it.
        // The scaler upstream emits a decimal string from a numeric column,
        // so a parse failure here is a storage-invariant violation.
        decimal_string_validate(&price)?;
        // `orig_qty == 0` is structurally invalid for every status —
        // every projector path that builds an Order has a non-zero
        // `amount_initial` from the placement event. A zero here is a
        // chain-side bug or storage corruption and would otherwise
        // make `(NEW: executed == 0)` and `(FILLED: executed == orig)`
        // both trivially admit the same all-zeros row.
        if decimal_string_is_zero(&orig_qty)? {
            return Err(DomainError::MarketInconsistent);
        }
        let executed_is_zero = decimal_string_is_zero(&executed_qty)?;
        if !decimal_string_lte(&executed_qty, &orig_qty)? {
            return Err(DomainError::MarketInconsistent);
        }
        match status {
            OrderStatus::PendingNew | OrderStatus::PendingCancel => {
                return Err(DomainError::MarketInconsistent);
            }
            OrderStatus::New if !executed_is_zero => {
                return Err(DomainError::MarketInconsistent);
            }
            OrderStatus::PartiallyFilled
                if executed_is_zero || !decimal_string_lt(&executed_qty, &orig_qty)? =>
            {
                return Err(DomainError::MarketInconsistent);
            }
            OrderStatus::Filled
                if executed_is_zero || decimal_string_lt(&executed_qty, &orig_qty)? =>
            {
                return Err(DomainError::MarketInconsistent);
            }
            _ => {}
        }

        Ok(Self {
            market_address,
            symbol,
            order_id,
            client_order_id,
            price,
            orig_qty,
            executed_qty,
            status,
            time_in_force,
            order_type,
            side,
            time,
            update_time,
        })
    }

    pub fn market_address(&self) -> &MarketAddress {
        &self.market_address
    }

    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    pub fn order_id(&self) -> &str {
        &self.order_id
    }

    pub fn client_order_id(&self) -> &str {
        &self.client_order_id
    }

    pub fn price(&self) -> &str {
        &self.price
    }

    pub fn orig_qty(&self) -> &str {
        &self.orig_qty
    }

    pub fn executed_qty(&self) -> &str {
        &self.executed_qty
    }

    pub fn status(&self) -> OrderStatus {
        self.status
    }

    pub fn time_in_force(&self) -> TimeInForce {
        self.time_in_force
    }

    pub fn order_type(&self) -> OrderType {
        self.order_type
    }

    pub fn side(&self) -> OrderSide {
        self.side
    }

    pub fn time(&self) -> i64 {
        self.time
    }

    pub fn update_time(&self) -> i64 {
        self.update_time
    }

    pub fn into_parts(self) -> OrderParts {
        OrderParts {
            market_address: self.market_address,
            symbol: self.symbol,
            order_id: self.order_id,
            client_order_id: self.client_order_id,
            price: self.price,
            orig_qty: self.orig_qty,
            executed_qty: self.executed_qty,
            status: self.status,
            time_in_force: self.time_in_force,
            order_type: self.order_type,
            side: self.side,
            time: self.time,
            update_time: self.update_time,
        }
    }
}

pub fn decimal_string_is_zero(s: &str) -> Result<bool, DomainError> {
    let (value, _) = parse_decimal_string(s)?;
    Ok(value == BigUint::from(0_u8))
}

/// Parse-and-discard variant for fields that only need a well-formedness
/// check (no comparison or zero-test). Returns `MarketInconsistent` on
/// malformed input, mirroring the failure mode of the other
/// `decimal_string_*` helpers.
pub fn decimal_string_validate(s: &str) -> Result<(), DomainError> {
    parse_decimal_string(s).map(|_| ())
}

fn decimal_string_lte(left: &str, right: &str) -> Result<bool, DomainError> {
    Ok(decimal_string_cmp(left, right)? != std::cmp::Ordering::Greater)
}

fn decimal_string_lt(left: &str, right: &str) -> Result<bool, DomainError> {
    Ok(decimal_string_cmp(left, right)? == std::cmp::Ordering::Less)
}

fn decimal_string_cmp(left: &str, right: &str) -> Result<std::cmp::Ordering, DomainError> {
    let (mut left_value, left_scale) = parse_decimal_string(left)?;
    let (mut right_value, right_scale) = parse_decimal_string(right)?;
    if left_scale < right_scale {
        let shift =
            u32::try_from(right_scale - left_scale).map_err(|_| DomainError::MarketInconsistent)?;
        left_value *= BigUint::from(10_u8).pow(shift);
    } else if right_scale < left_scale {
        let shift =
            u32::try_from(left_scale - right_scale).map_err(|_| DomainError::MarketInconsistent)?;
        right_value *= BigUint::from(10_u8).pow(shift);
    }
    Ok(left_value.cmp(&right_value))
}

fn parse_decimal_string(s: &str) -> Result<(BigUint, usize), DomainError> {
    let normalized = s.trim();
    if normalized.is_empty() {
        return Err(DomainError::MarketInconsistent);
    }
    let (whole, fractional) = normalized.split_once('.').unwrap_or((normalized, ""));
    if whole.is_empty() && fractional.is_empty() {
        return Err(DomainError::MarketInconsistent);
    }
    if !whole.chars().chain(fractional.chars()).all(|c| c.is_ascii_digit()) {
        return Err(DomainError::MarketInconsistent);
    }
    let digits = format!("{whole}{fractional}");
    let value =
        BigUint::parse_bytes(digits.as_bytes(), 10).ok_or(DomainError::MarketInconsistent)?;
    Ok((value, fractional.len()))
}

/// One page of `/api/v1/oracles`. Pagination is by oracle; `next_cursor`
/// encodes the last retained oracle (see the repo's oracle cursor helpers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OraclesPage {
    pub oracles: Vec<OracleListing>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// One oracle with its event lists. Maps to api-spec `OracleEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleListing {
    pub name: String,
    pub address: String,
    pub event_lists: Vec<OracleEventListEntry>,
}

/// One event list owned by an oracle. Maps to api-spec `OracleEventList`.
/// `description` is required (`NOT NULL`): it is carried by every
/// `OracleEventListDeployed` event, so the public contract is a plain STRING.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEventListEntry {
    pub index: i64,
    pub address: String,
    pub description: String,
    pub events: Vec<OracleEventEntry>,
}

/// One available event offered by an event list. Maps to api-spec `OracleEvent`.
/// `event_id` is the `0x`-hex rendering (same as `/api/v1/markets`
/// `event.eventId`). `description` / `trust_address` are reconciler-filled and
/// may be NULL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleEventEntry {
    pub event_id: String,
    pub event_name: String,
    pub description: Option<String>,
    pub oracle_fee: OracleFee,
    pub deadline: i64,
    pub trust_address: Option<String>,
    pub outcomes: Vec<OracleOutcome>,
}

/// Fee required by an oracle for an event. `asset` is the literal `"SHELL"`
/// today; `amount` is the raw chain integer as a decimal string (unscaled).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleFee {
    pub asset: String,
    pub amount: String,
}

/// One outcome label of an event. Maps to api-spec `OracleOutcome`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleOutcome {
    pub outcome_id: u32,
    pub outcome_name: String,
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

/// Side of an order. Mirrors the public `side` enum from `api-spec.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "BUY" => Some(Self::Buy),
            "SELL" => Some(Self::Sell),
            _ => None,
        }
    }

    pub fn is_buy(&self) -> bool {
        matches!(self, Self::Buy)
    }
}

/// Order type. Mirrors the public `type` enum from `api-spec.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Limit,
    Market,
}

impl OrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Market => "MARKET",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "LIMIT" => Some(Self::Limit),
            "MARKET" => Some(Self::Market),
            _ => None,
        }
    }
}

/// Time-in-force. Mirrors the public `timeInForce` enum from `api-spec.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
    PostOnly,
}

impl TimeInForce {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gtc => "GTC",
            Self::Ioc => "IOC",
            Self::Fok => "FOK",
            Self::PostOnly => "POST_ONLY",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "GTC" => Some(Self::Gtc),
            "IOC" => Some(Self::Ioc),
            "FOK" => Some(Self::Fok),
            "POST_ONLY" => Some(Self::PostOnly),
            _ => None,
        }
    }
}

/// Order status. Mirrors the public `status` enum from `api-spec.md`.
///
/// `PendingNew` is the transitional state between the moment
/// `PrivateNote.placeOrder` accepts (synchronous return of
/// `dodex_chain::Dex::place_order`) and the moment `OrderBook.OrderPlaced`
/// projects into `live_orders` with a chain-assigned `orderId`. The
/// HTTP response to a successful `POST /api/v1/order` always carries
/// `PendingNew`; the indexer-projected row in `live_orders` then
/// surfaces as `NEW` through `GET /api/v1/orders`.
///
/// Variant declaration order is pinned by a regression test so the public
/// wire sequence stays deliberate. SQL status-filter composition uses
/// `QueryableOrderStatus`, not this enum.
///
/// `PendingCancel` is the analogous state for cancellation: the moment
/// `PrivateNote.cancelOrder` accepts and forwards to `OrderBook`, the
/// HTTP response carries `PendingCancel`; the book-side removal lands
/// asynchronously via `OrderBook.OrderCancelled`, after which the order
/// surfaces in `/api/v1/orders` as `Canceled` (or `Filled` if matching
/// raced the cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    PendingNew,
    New,
    PartiallyFilled,
    PendingCancel,
    Filled,
    Canceled,
    Rejected,
}

impl OrderStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PendingNew => "PENDING_NEW",
            Self::New => "NEW",
            Self::PartiallyFilled => "PARTIALLY_FILLED",
            Self::PendingCancel => "PENDING_CANCEL",
            Self::Filled => "FILLED",
            Self::Canceled => "CANCELED",
            Self::Rejected => "REJECTED",
        }
    }
}

/// Chain-side flag bits packed into `placeOrder.flags` (uint8). Layout
/// matches `contracts/modifiers/modifiers.sol`.
pub const FLAG_IOC: u8 = 0x01;
pub const FLAG_FOK: u8 = 0x02;
pub const FLAG_MARKET: u8 = 0x04;
pub const FLAG_POST_ONLY: u8 = 0x08;

/// Encode `type` × `timeInForce` into the on-chain `uint8 flags` value
/// per spec §Flags. Accepts `Option<TimeInForce>` because the public API
/// ignores `timeInForce` on `MARKET` orders — passing `None` for `MARKET`
/// is the canonical "no TIF was given" path. Returns
/// `DomainError::InvalidParameter` for combinations the spec rejects.
pub fn encode_order_flags(
    order_type: OrderType,
    time_in_force: Option<TimeInForce>,
) -> Result<u8, DomainError> {
    match order_type {
        OrderType::Limit => match time_in_force.unwrap_or(TimeInForce::Gtc) {
            TimeInForce::Gtc => Ok(0),
            TimeInForce::Ioc => Ok(FLAG_IOC),
            TimeInForce::Fok => Ok(FLAG_FOK),
            TimeInForce::PostOnly => Ok(FLAG_POST_ONLY),
        },
        // MARKET has IOC semantics by construction. Explicit GTC / FOK /
        // POST_ONLY is rejected per the api-spec — `timeInForce` is
        // LIMIT-only.
        OrderType::Market => match time_in_force {
            None | Some(TimeInForce::Ioc) => Ok(FLAG_MARKET),
            Some(_) => Err(DomainError::InvalidParameter),
        },
    }
}

/// Cap on the fractional digit count any decimal string is allowed to
/// carry through validation. Far above any precision exposed by
/// `market_outcomes.price_precision` / `quantity_precision`; the cap
/// exists so a pathologically long input cannot blow up the scaling
/// `pow()` step.
///
/// Note: this domain layer is precision-aware (BigUint) and does
/// not impose its own u64 ceiling on the lifted value, but the
/// downstream chain submission path eventually serializes
/// `amount: u128` and `client_order_id: u128` through
/// `serde_json::json!` (in `ackinacki-kit`) without the
/// `arbitrary_precision` feature. Values above `u64::MAX` panic
/// there. For realistic `(quantity_precision, quantity)` pairs the
/// lifted amount stays well within u64 (NACKL: precision=9, max
/// market ≈ 10^15 ≪ 2^64), so we do not enforce an extra check
/// here — but an operator-misconfigured `market_outcomes` with
/// e.g. `quantity_precision=20` and a tiny `step_size` could push
/// the lifted amount past `u64::MAX` and trigger the same panic
/// as the historic coid bug. See
/// `docs/tech-specs/write-api.md §clientOrderId generation`.
const MAX_DECIMAL_DIGITS: u8 = 38;

/// Parse a non-negative decimal string into a scaled `BigUint`, rejecting
/// inputs with more fractional digits than `max_decimals`. Returns
/// `(scaled, actual_decimals)` such that `scaled == value * 10.pow(actual_decimals)`.
/// Used as the building block for tick/step/notional checks at exact precision.
pub fn parse_positive_decimal(value: &str, max_decimals: u8) -> Result<(BigUint, u8), DomainError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(DomainError::MissingParameter);
    }
    if !trimmed.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(DomainError::InvalidParameter);
    }
    let (int_part, frac_part) = trimmed.split_once('.').unwrap_or((trimmed, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(DomainError::InvalidParameter);
    }
    let decimals = frac_part.len();
    if decimals > max_decimals as usize {
        return Err(DomainError::PrecisionExceeded);
    }
    let combined: String = format!("{int_part}{frac_part}");
    let scaled =
        BigUint::parse_bytes(combined.as_bytes(), 10).ok_or(DomainError::InvalidParameter)?;
    Ok((scaled, decimals as u8))
}

/// On-chain price scale. The OrderBook contract stores `price` in basis
/// points with `FULL_PERCENT = 10_000` (= 10^4), so a probability price like
/// "0.488" must be encoded as `4880` bps. This is the chain price scale and is
/// deliberately distinct from the display `price_precision` (which only bounds
/// the input's fractional digits): the same price is shown to clients at
/// `price_precision` decimals but must be lifted to basis points for the chain.
pub const PRICE_BPS_DECIMALS: u8 = 4;

// The price scale is `FULL_PERCENT = 10_000` by definition; pin the exponent to
// that literal so the two cannot drift apart silently.
const _: () = assert!(10u128.pow(PRICE_BPS_DECIMALS as u32) == 10_000);

/// Lift a decimal string to `BigUint` at exactly `target_decimals` fractional
/// digits. Fails with `PrecisionExceeded` when the input carries more
/// fractional digits than the target.
pub fn lift_decimal(value: &str, target_decimals: u8) -> Result<BigUint, DomainError> {
    let (scaled, actual) = parse_positive_decimal(value, target_decimals)?;
    let pad = (target_decimals - actual) as u32;
    Ok(scaled * BigUint::from(10u32).pow(pad))
}

/// Divide a non-negative integer string by `10^k` by dropping the last `k`
/// digits. Steps a chain-scale integer (price in basis points, amount in token
/// atoms) down to a coarser display grid before fixed-point formatting; the
/// inverse direction of [`lift_decimal`].
///
/// The drop is lossless only when the `k` dropped digits are all zero. For an
/// on-grid chain value that holds by construction — `price` is a `TICK_SIZE`
/// multiple, `amount` a lot multiple — but only down to the grid the lattice
/// guarantees (one zero digit per `TICK_SIZE` — 10 bps for current tokens —
/// and `log10(LOT_SIZE)` per lot). A nonzero dropped digit therefore means the value is off that grid (a
/// display precision coarser than the lattice, or a raw value the chain would
/// never have accepted): surface it as `MarketInconsistent` rather than return
/// a confidently-wrong rounded value. A non-digit input is read-model
/// corruption and surfaces the same way.
pub fn descale_pow10(raw: &str, k: usize) -> Result<String, DomainError> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DomainError::MarketInconsistent);
    }
    if k == 0 {
        return Ok(raw.to_string());
    }
    let keep = raw.len().saturating_sub(k);
    let (head, dropped) = raw.split_at(keep);
    if dropped.bytes().any(|b| b != b'0') {
        return Err(DomainError::MarketInconsistent);
    }
    if head.is_empty() {
        return Ok("0".to_string());
    }
    Ok(head.to_string())
}

fn count_decimals(value: &str) -> Result<u8, DomainError> {
    parse_positive_decimal(value, MAX_DECIMAL_DIGITS).map(|(_, dp)| dp)
}

/// Check that the input has no more fractional digits than `max_decimals`.
/// A convenience wrapper around `parse_positive_decimal` used by
/// `pricePrecision` / `quantityPrecision` rules in [api-spec §Validation
/// Rules].
pub fn precision_within(value: &str, max_decimals: u8) -> Result<(), DomainError> {
    parse_positive_decimal(value, max_decimals).map(|_| ())
}

/// Check that `value` is a non-negative multiple of `step` (both decimal
/// strings). Lifts both to the common precision `max(value_dp, step_dp)`
/// so a value with stricter precision than `step` is correctly compared
/// rather than rejected as over-precise — the `precision_within` check
/// is the right place to enforce digit count.
pub fn is_multiple_of(value: &str, step: &str) -> Result<bool, DomainError> {
    let value_dp = count_decimals(value)?;
    let step_dp = count_decimals(step)?;
    let scale = value_dp.max(step_dp);
    let v = lift_decimal(value, scale)?;
    let s = lift_decimal(step, scale)?;
    let zero = BigUint::from(0u32);
    if s == zero {
        return Err(DomainError::InvalidParameter);
    }
    Ok((v % s) == zero)
}

/// Render a previously-validated decimal string with exactly
/// `target_decimals` fractional digits, padding with trailing zeros.
/// Used to format response fields (`price`, `origQty`, `executedQty`)
/// at the outcome's `pricePrecision` / `quantityPrecision` so clients
/// see a stable shape regardless of how the request was written.
pub fn normalize_decimal(value: &str, target_decimals: u8) -> Result<String, DomainError> {
    let scaled = lift_decimal(value, target_decimals)?;
    if target_decimals == 0 {
        return Ok(scaled.to_str_radix(10));
    }
    let divisor = BigUint::from(10u32).pow(target_decimals as u32);
    let int_part = &scaled / &divisor;
    let frac_part = &scaled % &divisor;
    let frac_str = frac_part.to_str_radix(10);
    let width = target_decimals as usize;
    Ok(format!("{}.{:0>width$}", int_part.to_str_radix(10), frac_str, width = width))
}

/// Check `price * quantity >= min_notional`, all three as decimal
/// strings. Arithmetic is exact in `BigUint` at the common scale
/// `max(price_dp + qty_dp, min_dp)`; no float precision loss.
pub fn notional_meets_minimum(
    price: &str,
    quantity: &str,
    min_notional: &str,
) -> Result<bool, DomainError> {
    let price_dp = count_decimals(price)?;
    let qty_dp = count_decimals(quantity)?;
    let min_dp = count_decimals(min_notional)?;

    let p = lift_decimal(price, price_dp)?;
    let q = lift_decimal(quantity, qty_dp)?;
    let product = p * q;
    let product_dp = price_dp + qty_dp;

    let common_dp = product_dp.max(min_dp);
    let pad = (common_dp - product_dp) as u32;
    let product_scaled = product * BigUint::from(10u32).pow(pad);
    let min_scaled = lift_decimal(min_notional, common_dp)?;

    Ok(product_scaled >= min_scaled)
}

/// Byte buffer that zeroes its contents on drop. Used for plaintext
/// secrets (decrypted api_secret, decrypted pn_seckey) that must not
/// linger in memory after they are no longer needed. The `Debug`
/// implementation redacts the bytes so accidental `tracing` or `dbg!`
/// calls cannot leak the material.
#[derive(Clone, ZeroizeOnDrop)]
pub struct SensitiveBytes(Vec<u8>);

/// Required byte length of an ed25519 secret key — fixed by the
/// chain ABI (`tvm_client::crypto::KeyPair.secret` is a 32-byte hex
/// string upstream). Pinning the length at the construction boundary
/// (`SensitiveBytes::seckey`) means a corrupted `accounts.pn_seckey_enc`
/// row or a future migration that drifts the encoding cannot smuggle
/// a wrong-sized buffer into the chain sender.
pub const PN_SECKEY_BYTE_LEN: usize = 32;

impl SensitiveBytes {
    /// General-purpose constructor. Used for variable-length plaintext
    /// (`api_secret` is HMAC key material with no fixed length under
    /// the spec). Use [`SensitiveBytes::seckey`] for trading-PN secret
    /// keys so the 32-byte invariant is checked at the construction
    /// boundary, not at the chain ABI.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Wrap a freshly-decrypted ed25519 PN secret key. Enforces
    /// exactly `PN_SECKEY_BYTE_LEN` bytes; a wrong-sized buffer
    /// surfaces as `DomainError::Unexpected` so the auth pipeline
    /// fails closed before the bytes reach `DexChainSender`,
    /// where a `hex::encode` of a short key would silently produce
    /// a key that the chain rejects with an unmappable error.
    pub fn seckey(bytes: Vec<u8>) -> Result<Self, DomainError> {
        if bytes.len() != PN_SECKEY_BYTE_LEN {
            return Err(DomainError::Unexpected);
        }
        Ok(Self(bytes))
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
    /// Binance-compatible `-1102`. Some handlers also use this for a
    /// syntactically present parameter whose numeric value is outside the
    /// accepted range; the wire message intentionally stays Binance-shaped.
    #[error("mandatory parameter was not sent")]
    MissingParameter,
    #[error("invalid value for a query parameter")]
    InvalidParameter,
    #[error("invalid market or symbol")]
    InvalidMarketOrSymbol,
    #[error("authentication required")]
    AuthRequired,
    #[error("required auth parameter missing")]
    AuthEnvelopeIncomplete,
    #[error("request body too large")]
    RequestTooLarge,
    #[error("timestamp outside recvWindow")]
    TimestampOutsideRecvWindow,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("precision exceeds the maximum defined for this asset")]
    PrecisionExceeded,
    #[error("order would immediately fail validation")]
    OrderValidationFailed,
    /// The trading PN is mid-`placeOrder` (chain `ERR_NOTE_BUSY`):
    /// only one external `placeOrder` per PN can be in flight at a
    /// time. Distinct from `OrderValidationFailed` because the order
    /// itself is fine — the caller just needs to retry once the
    /// previous `onOrderPlaced` callback has cleared `_busy`.
    #[error("trading note busy with a previous order")]
    OrderPnBusy,
    #[error("unknown order")]
    UnknownOrder,
    /// The caller's PrivateNote contract is not deployed yet at the
    /// resolved address. Operationally distinct from gateway flap or PN
    /// state parsing failure — the address is well-formed and the
    /// account is reachable, the BOC just isn't there. Surfaces as 404
    /// so clients can offer "deploy your account" rather than retry.
    #[error("account not deployed")]
    AccountNotDeployed,
    /// The read-model row violates a tech-spec invariant (e.g. RESOLVED with
    /// `frozenAt = null`, CANCELLED with `cancelReason = null`). Per the
    /// invariant-checking contract in `docs/tech-specs/read-api.md`
    /// these rows MUST be rejected rather than serialized — the
    /// indexer is mid-replay and a consistent view is not available
    /// yet. Surfaces as a 503 so clients know to retry.
    #[error("market read-model is temporarily inconsistent")]
    MarketInconsistent,
    /// The request exceeded the per-handler wall-clock budget enforced
    /// by the API's `request_timeout` hoop. Typically means a chain
    /// submission or downstream call hung past the configured slack.
    /// Mirrors the Binance `-1007 / 504 TIMEOUT` shape.
    ///
    /// Idempotency on retry depends on the path:
    /// - place / place-batch: retry with the same `clientOrderId` so a
    ///   successfully-landed chain message is not duplicated — the
    ///   chain dedupes by `(pn, coid)`.
    /// - cancel / cancel-batch: retry with the same chain-assigned
    ///   `orderId` (or `orderId[]`); cancelling an already-cancelled
    ///   id is a no-op on the chain. `clientOrderId` is not a cancel
    ///   key — the API resolves chain `orderId` upstream of dispatch.
    #[error("request timed out before completion")]
    RequestTimeout,
    #[error("unexpected domain error")]
    Unexpected,
}

impl DomainError {
    pub fn code(&self) -> i32 {
        match self {
            Self::Unexpected => -1000,
            Self::AuthRequired => -1002,
            Self::AuthEnvelopeIncomplete => -1003,
            Self::RequestTimeout => -1007,
            Self::RequestTooLarge => -1009,
            Self::TimestampOutsideRecvWindow => -1021,
            Self::InvalidSignature => -1022,
            Self::MissingParameter => -1102,
            Self::PrecisionExceeded => -1111,
            Self::InvalidMarketOrSymbol => -1121,
            Self::InvalidParameter => -1130,
            Self::MarketInconsistent => -1500,
            Self::OrderValidationFailed => -2010,
            Self::UnknownOrder => -2011,
            Self::AccountNotDeployed => -2013,
            Self::OrderPnBusy => -2014,
        }
    }

    pub fn msg(&self) -> &'static str {
        match self {
            Self::Unexpected => "Unknown error.",
            Self::AuthRequired => "Authentication required.",
            Self::AuthEnvelopeIncomplete => "Required auth parameter missing.",
            Self::RequestTimeout => "Request timed out before completion.",
            Self::RequestTooLarge => "Request body too large.",
            Self::TimestampOutsideRecvWindow => "Timestamp outside recvWindow.",
            Self::InvalidSignature => "Invalid signature.",
            Self::MissingParameter => "Mandatory parameter was not sent.",
            Self::PrecisionExceeded => "Precision is over the maximum defined for this asset.",
            Self::InvalidMarketOrSymbol => "Invalid market or symbol.",
            Self::InvalidParameter => "Invalid value for a query parameter.",
            Self::MarketInconsistent => "Market data is temporarily inconsistent.",
            Self::OrderValidationFailed => "Order would immediately fail validation.",
            Self::UnknownOrder => "Unknown order.",
            Self::AccountNotDeployed => "Account not deployed.",
            Self::OrderPnBusy => "Trading note busy with a previous order; retry shortly.",
        }
    }
}

/// One collateral-asset row in `GET /api/v1/account` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetBalance {
    pub asset: String,
    pub free: String,
    pub locked: String,
}

/// Full response shape for `GET /api/v1/account`. The HTTP layer maps
/// this into the wire envelope; this type lives in `domain` so the
/// use case can return a typed value rather than untyped json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBalances {
    pub account_id: uuid::Uuid,
    pub update_time_ms: i64,
    pub balances: Vec<AssetBalance>,
}

/// One outcome row in `GET /api/v1/account/balances` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutcomeBalance {
    pub outcome_id: u32,
    pub symbol: Symbol,
    pub free: String,
    pub locked_in_orders: String,
}

/// Full response shape for `GET /api/v1/account/balances`. Sorted by
/// `outcome_id` ASC; length equals the market's outcome count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketBalances {
    pub market_address: MarketAddress,
    pub update_time_ms: i64,
    pub balances: Vec<OutcomeBalance>,
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

    #[test]
    fn sensitive_bytes_seckey_accepts_32_bytes() {
        let bytes = vec![0u8; PN_SECKEY_BYTE_LEN];
        let s = SensitiveBytes::seckey(bytes.clone()).expect("32-byte seckey accepted");
        assert_eq!(s.len(), PN_SECKEY_BYTE_LEN);
        assert_eq!(s.as_slice(), bytes.as_slice());
    }

    #[test]
    fn sensitive_bytes_seckey_rejects_wrong_length() {
        // ed25519 seckey is exactly 32 bytes; auth.rs decrypts from
        // `accounts.pn_seckey_enc`, and a row whose plaintext length
        // drifted from 32 is a service-state bug, not a user error.
        // Any deviation must surface as `Unexpected` (-1000/500) so
        // the auth pipeline fails closed before the bytes reach
        // `DexChainSender::submit_order`.
        for bad_len in [0usize, 1, 16, 31, 33, 64] {
            let err = SensitiveBytes::seckey(vec![0u8; bad_len])
                .expect_err("wrong-length seckey must be rejected at construction");
            assert_eq!(err, DomainError::Unexpected, "len {bad_len}: wrong variant");
        }
    }

    #[test]
    fn order_side_round_trip() {
        for s in [OrderSide::Buy, OrderSide::Sell] {
            assert_eq!(OrderSide::parse(s.as_str()), Some(s));
        }
        assert!(OrderSide::Buy.is_buy());
        assert!(!OrderSide::Sell.is_buy());
    }

    #[test]
    fn order_side_parse_rejects_unknown() {
        // Case sensitivity matters — the api-spec writes BUY/SELL in
        // upper case and we should not silently accept lower-case input.
        assert_eq!(OrderSide::parse(""), None);
        assert_eq!(OrderSide::parse("buy"), None);
        assert_eq!(OrderSide::parse("HOLD"), None);
    }

    #[test]
    fn order_status_declaration_order_pins_read_api_canonical_order() {
        let mut statuses = vec![
            OrderStatus::Rejected,
            OrderStatus::Canceled,
            OrderStatus::Filled,
            OrderStatus::PendingCancel,
            OrderStatus::PartiallyFilled,
            OrderStatus::New,
            OrderStatus::PendingNew,
        ];
        statuses.sort();
        assert_eq!(
            statuses,
            vec![
                OrderStatus::PendingNew,
                OrderStatus::New,
                OrderStatus::PartiallyFilled,
                OrderStatus::PendingCancel,
                OrderStatus::Filled,
                OrderStatus::Canceled,
                OrderStatus::Rejected,
            ]
        );
    }

    /// `orig_qty == 0` is structurally invalid for every status — the
    /// infrastructure-level test only exercises the OPEN path (where
    /// `order_from_row` already drops the row before `Order::new`
    /// runs). Pin the domain-side guard directly for the non-OPEN
    /// terminals.
    #[test]
    fn order_constructor_rejects_zero_orig_qty_on_terminal_statuses() {
        for status in [OrderStatus::Filled, OrderStatus::Canceled] {
            let err = Order::new(
                MarketAddress("0:market".into()),
                Symbol("SYM".into()),
                OrderIdentity::Chain("1".into()),
                String::new(),
                "1.000".into(),
                "0".into(),
                "0".into(),
                status,
                TimeInForce::Gtc,
                OrderType::Limit,
                OrderSide::Buy,
                1,
                1,
            )
            .expect_err("orig_qty == 0 must be rejected for {status:?}");
            assert_eq!(err, DomainError::MarketInconsistent, "status={status:?}");
        }
        // Rejected: pairs with OrderIdentity::Rejected (the chain
        // never assigns an id), and orig_qty=0 must still be rejected
        // by the same guard.
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Rejected,
            String::new(),
            "1.000".into(),
            "0".into(),
            "0".into(),
            OrderStatus::Rejected,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("orig_qty == 0 must be rejected for REJECTED");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_rejects_filled_with_zero_executed_qty() {
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Chain("1".into()),
            String::new(),
            "1.00".into(),
            "1.00".into(),
            "0.00".into(),
            OrderStatus::Filled,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("FILLED with zero executed quantity rejected");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_rejects_rejected_with_chain_identity() {
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Chain("123".into()),
            String::new(),
            "1.00".into(),
            "1.00".into(),
            "0.00".into(),
            OrderStatus::Rejected,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("REJECTED with chain identity rejected");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_rejects_pending_read_model_statuses() {
        for status in [OrderStatus::PendingNew, OrderStatus::PendingCancel] {
            let err = Order::new(
                MarketAddress("0:market".into()),
                Symbol("SYM".into()),
                OrderIdentity::Chain("123".into()),
                String::new(),
                "1.00".into(),
                "1.00".into(),
                "0.00".into(),
                status,
                TimeInForce::Gtc,
                OrderType::Limit,
                OrderSide::Buy,
                1,
                1,
            )
            .expect_err("pending statuses cannot appear in live_orders read DTOs");
            assert_eq!(err, DomainError::MarketInconsistent);
        }
    }

    #[test]
    fn order_constructor_rejects_executed_qty_above_orig_qty() {
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Chain("123".into()),
            String::new(),
            "1.00".into(),
            "1.00".into(),
            "1.01".into(),
            OrderStatus::PartiallyFilled,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("executed quantity cannot exceed original quantity");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_rejects_new_with_nonzero_executed_qty() {
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Chain("123".into()),
            String::new(),
            "1.00".into(),
            "1.00".into(),
            "0.01".into(),
            OrderStatus::New,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("NEW requires zero executed quantity");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_rejects_partially_filled_boundary_quantities() {
        for executed_qty in ["0.00", "1.00"] {
            let err = Order::new(
                MarketAddress("0:market".into()),
                Symbol("SYM".into()),
                OrderIdentity::Chain("123".into()),
                String::new(),
                "1.00".into(),
                "1.00".into(),
                executed_qty.into(),
                OrderStatus::PartiallyFilled,
                TimeInForce::Gtc,
                OrderType::Limit,
                OrderSide::Buy,
                1,
                1,
            )
            .expect_err("PARTIALLY_FILLED requires 0 < executed quantity < original quantity");
            assert_eq!(err, DomainError::MarketInconsistent);
        }
    }

    #[test]
    fn order_constructor_rejects_filled_below_orig_qty() {
        let err = Order::new(
            MarketAddress("0:market".into()),
            Symbol("SYM".into()),
            OrderIdentity::Chain("123".into()),
            String::new(),
            "1.00".into(),
            "1.00".into(),
            "0.99".into(),
            OrderStatus::Filled,
            TimeInForce::Gtc,
            OrderType::Limit,
            OrderSide::Buy,
            1,
            1,
        )
        .expect_err("FILLED requires executed quantity to equal original quantity");
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[test]
    fn order_constructor_accepts_public_status_quantity_shapes() {
        for (status, executed_qty) in [
            (OrderStatus::New, "0.00"),
            (OrderStatus::PartiallyFilled, "0.50"),
            (OrderStatus::Filled, "1.00"),
        ] {
            Order::new(
                MarketAddress("0:market".into()),
                Symbol("SYM".into()),
                OrderIdentity::Chain("123".into()),
                String::new(),
                "1.00".into(),
                "1.00".into(),
                executed_qty.into(),
                status,
                TimeInForce::Gtc,
                OrderType::Limit,
                OrderSide::Buy,
                1,
                1,
            )
            .expect("valid status/executed/orig quantity shape accepted");
        }
    }

    #[test]
    fn order_type_round_trip() {
        for t in [OrderType::Limit, OrderType::Market] {
            assert_eq!(OrderType::parse(t.as_str()), Some(t));
        }
    }

    #[test]
    fn order_type_parse_rejects_unknown() {
        assert_eq!(OrderType::parse(""), None);
        assert_eq!(OrderType::parse("limit"), None);
        assert_eq!(OrderType::parse("STOP_LIMIT"), None);
    }

    #[test]
    fn time_in_force_round_trip() {
        for t in [TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok, TimeInForce::PostOnly] {
            assert_eq!(TimeInForce::parse(t.as_str()), Some(t));
        }
    }

    #[test]
    fn time_in_force_parse_rejects_unknown() {
        assert_eq!(TimeInForce::parse(""), None);
        assert_eq!(TimeInForce::parse("gtc"), None);
        assert_eq!(TimeInForce::parse("DAY"), None);
        // POST_ONLY is the canonical form — not POSTONLY, not POST-ONLY.
        assert_eq!(TimeInForce::parse("POSTONLY"), None);
    }

    #[test]
    fn flag_bits_are_distinct_powers_of_two() {
        // A regression here would corrupt every order placed — two flag
        // bits overlapping means the encoder silently emits the wrong
        // semantics. The chain would reject (or worse, accept and act
        // on the wrong meaning).
        let all = [FLAG_IOC, FLAG_FOK, FLAG_MARKET, FLAG_POST_ONLY];
        for &bit in &all {
            assert!(bit.is_power_of_two(), "flag {bit:#x} is not a single bit");
        }
        for (i, &a) in all.iter().enumerate() {
            for &b in &all[i + 1..] {
                assert_eq!(a & b, 0, "flags {a:#x} and {b:#x} share a bit");
            }
        }
    }

    #[test]
    fn encode_flags_limit_table() {
        // The five rows in spec §Flags must encode to exactly these bits.
        // Locking the table down with a test means a future "small
        // tweak" cannot quietly remap LIMIT/GTC to FLAG_IOC etc.
        assert_eq!(encode_order_flags(OrderType::Limit, Some(TimeInForce::Gtc)), Ok(0x00));
        assert_eq!(encode_order_flags(OrderType::Limit, Some(TimeInForce::Ioc)), Ok(FLAG_IOC));
        assert_eq!(encode_order_flags(OrderType::Limit, Some(TimeInForce::Fok)), Ok(FLAG_FOK));
        assert_eq!(
            encode_order_flags(OrderType::Limit, Some(TimeInForce::PostOnly)),
            Ok(FLAG_POST_ONLY),
        );
    }

    #[test]
    fn encode_flags_limit_defaults_to_gtc_when_tif_absent() {
        // The api-spec defaults `timeInForce` to GTC for LIMIT. The
        // handler may surface this as `None` (request didn't include
        // the field), so the encoder owns the default.
        assert_eq!(encode_order_flags(OrderType::Limit, None), Ok(0x00));
    }

    #[test]
    fn encode_flags_market_accepts_ioc_or_absent_tif() {
        // The api-spec describes `timeInForce` as ignored for MARKET.
        // Concretely that means two callers are valid: one that omits
        // the field (None) and one that explicitly sends IOC, which
        // matches MARKET's actual on-chain semantics.
        assert_eq!(encode_order_flags(OrderType::Market, None), Ok(FLAG_MARKET));
        assert_eq!(encode_order_flags(OrderType::Market, Some(TimeInForce::Ioc)), Ok(FLAG_MARKET),);
    }

    #[test]
    fn encode_flags_rejects_market_with_resting_or_postonly_tif() {
        // MARKET orders never rest, so GTC/FOK make no sense; POST_ONLY
        // is the opposite of MARKET semantically. Spec rejects all three.
        for bad in [TimeInForce::Gtc, TimeInForce::Fok, TimeInForce::PostOnly] {
            assert_eq!(
                encode_order_flags(OrderType::Market, Some(bad)),
                Err(DomainError::InvalidParameter),
                "MARKET + {} should be rejected as InvalidParameter",
                bad.as_str(),
            );
        }
    }

    #[test]
    fn parse_decimal_accepts_canonical_forms() {
        assert_eq!(parse_positive_decimal("0", 0).unwrap(), (BigUint::from(0u32), 0));
        assert_eq!(parse_positive_decimal("0.000", 3).unwrap(), (BigUint::from(0u32), 3));
        assert_eq!(parse_positive_decimal("0.615", 3).unwrap(), (BigUint::from(615u32), 3));
        assert_eq!(parse_positive_decimal("1500", 6).unwrap(), (BigUint::from(1500u32), 0));
        assert_eq!(
            parse_positive_decimal("123.456789", 6).unwrap(),
            (BigUint::from(123456789u32), 6),
        );
    }

    #[test]
    fn parse_decimal_tolerates_partial_dot_forms() {
        // The api-spec doesn't pin the wire grammar tightly. Accept
        // ".5" as "0.5" and "5." as "5"; this matches how most JSON
        // serializers emit decimals.
        assert_eq!(parse_positive_decimal(".5", 1).unwrap(), (BigUint::from(5u32), 1));
        assert_eq!(parse_positive_decimal("5.", 0).unwrap(), (BigUint::from(5u32), 0));
    }

    #[test]
    fn parse_decimal_rejects_garbage() {
        // Empty -> MissingParameter (caller should not even get here,
        // but defence in depth).
        assert_eq!(parse_positive_decimal("", 3), Err(DomainError::MissingParameter));
        assert_eq!(parse_positive_decimal("   ", 3), Err(DomainError::MissingParameter));
        // Signs, exponents, hex, locale separators — all rejected as
        // InvalidParameter so the client gets `-1130` not `-1111`.
        for s in ["-1", "+1", "1e3", "0xff", "1,5", "1 5", "abc"] {
            assert_eq!(
                parse_positive_decimal(s, 3),
                Err(DomainError::InvalidParameter),
                "expected InvalidParameter for {s:?}",
            );
        }
        // Lone dot has no integer or fractional half.
        assert_eq!(parse_positive_decimal(".", 3), Err(DomainError::InvalidParameter));
    }

    #[test]
    fn parse_decimal_rejects_excess_precision() {
        // The `max_decimals` cap is the whole point — this is the spec
        // rule "price decimals ≤ pricePrecision".
        assert_eq!(parse_positive_decimal("0.6150", 3), Err(DomainError::PrecisionExceeded),);
        assert_eq!(parse_positive_decimal("1.5", 0), Err(DomainError::PrecisionExceeded),);
    }

    #[test]
    fn precision_within_matches_parser() {
        assert!(precision_within("0.615", 3).is_ok());
        assert_eq!(precision_within("0.6150", 3), Err(DomainError::PrecisionExceeded));
    }

    #[test]
    fn lift_decimal_scales_correctly() {
        assert_eq!(lift_decimal("1", 3).unwrap(), BigUint::from(1000u32));
        assert_eq!(lift_decimal("0.5", 3).unwrap(), BigUint::from(500u32));
        assert_eq!(lift_decimal("0.615", 3).unwrap(), BigUint::from(615u32));
        assert_eq!(lift_decimal("123.45", 4).unwrap(), BigUint::from(1234500u32));
    }

    #[test]
    fn descale_pow10_drops_exact_trailing_zeros() {
        assert_eq!(descale_pow10("4880", 1).unwrap(), "488"); // bps → 0.001 price grid
        assert_eq!(descale_pow10("10000000", 4).unwrap(), "1000"); // USDC atoms → 0.01 qty grid
        assert_eq!(descale_pow10("488", 0).unwrap(), "488");
        assert_eq!(descale_pow10("0", 3).unwrap(), "0"); // whole value is zero
        assert_eq!(descale_pow10("1000", 3).unwrap(), "1"); // dropped "000" are all zero
    }

    #[test]
    fn descale_pow10_rejects_non_digit() {
        assert_eq!(descale_pow10("4.88", 1), Err(DomainError::MarketInconsistent));
        assert_eq!(descale_pow10("", 1), Err(DomainError::MarketInconsistent));
    }

    #[test]
    fn descale_pow10_rejects_nonzero_dropped_digits() {
        // A nonzero digit inside the dropped tail means the value is off the
        // chain's tick / lot grid — fail closed instead of rounding.
        assert_eq!(descale_pow10("4885", 1), Err(DomainError::MarketInconsistent)); // off TICK_SIZE
        assert_eq!(descale_pow10("10000001", 4), Err(DomainError::MarketInconsistent)); // off lot
        assert_eq!(descale_pow10("5", 1), Err(DomainError::MarketInconsistent));
        // whole value dropped, nonzero
    }

    #[test]
    fn lift_then_descale_round_trips_on_contract_grid() {
        // Encode ↔ decode are inverse on the contract grid (real numbers):
        // price "0.488" -> 4880 bps -> 488 (== the 0.001 display grid int).
        let bps = lift_decimal("0.488", PRICE_BPS_DECIMALS).unwrap().to_str_radix(10);
        assert_eq!(bps, "4880");
        assert_eq!(
            descale_pow10(&bps, (PRICE_BPS_DECIMALS - 3) as usize).unwrap(),
            lift_decimal("0.488", 3).unwrap().to_str_radix(10), // "488"
        );
        // amount "10" -> 10_000_000 USDC atoms -> 1000 (== the 0.01 display grid int).
        let atoms = lift_decimal("10", 6).unwrap().to_str_radix(10);
        assert_eq!(atoms, "10000000");
        assert_eq!(
            descale_pow10(&atoms, (6 - 2) as usize).unwrap(),
            lift_decimal("10", 2).unwrap().to_str_radix(10), // "1000"
        );
    }

    #[test]
    fn is_multiple_of_handles_step_boundaries() {
        // Spec rule: "price is a multiple of tickSize". Cover both
        // success and the obvious failure modes.
        assert!(is_multiple_of("0.615", "0.001").unwrap());
        assert!(is_multiple_of("0.6", "0.1").unwrap());
        assert!(is_multiple_of("1", "0.1").unwrap()); // integer is multiple of fractional step
        assert!(!is_multiple_of("0.6155", "0.001").unwrap()); // finer than step
        assert!(!is_multiple_of("0.7", "0.3").unwrap()); // 0.7 / 0.3 has remainder
    }

    #[test]
    fn is_multiple_of_with_value_more_precise_than_step() {
        // 0.61 vs step 0.1: value is finer than the step, so it cannot
        // be an exact multiple. Must return `Ok(false)`, not
        // `PrecisionExceeded` — the precision check belongs to a
        // separate `precision_within` call against `pricePrecision`,
        // not against tickSize's own precision.
        assert_eq!(is_multiple_of("0.61", "0.1"), Ok(false));
    }

    #[test]
    fn is_multiple_of_rejects_zero_step() {
        // A configuration bug rather than a client-input bug; the
        // 400 reply nudges ops to look at `market_outcomes`.
        assert_eq!(is_multiple_of("1", "0"), Err(DomainError::InvalidParameter));
    }

    #[test]
    fn notional_meets_minimum_exact_decimal_arithmetic() {
        // 0.615 * 1.5 = 0.9225, which is below a min_notional of 1 but
        // above 0.9. Floating-point would mis-round on the boundary;
        // BigUint comparison is exact.
        assert!(!notional_meets_minimum("0.615", "1.5", "1").unwrap());
        assert!(notional_meets_minimum("0.615", "1.5", "0.9").unwrap());
        // Exact equality counts as meeting the minimum.
        assert!(notional_meets_minimum("0.5", "2", "1").unwrap());
    }

    #[test]
    fn normalize_decimal_pads_to_target_precision() {
        assert_eq!(normalize_decimal("1.5", 6).unwrap(), "1.500000");
        assert_eq!(normalize_decimal("0.615", 3).unwrap(), "0.615");
        assert_eq!(normalize_decimal("0", 6).unwrap(), "0.000000");
        assert_eq!(normalize_decimal("100", 2).unwrap(), "100.00");
    }

    #[test]
    fn normalize_decimal_precision_zero_omits_dot() {
        // pricePrecision = 0 markets (if any) shouldn't get a trailing
        // dot in the response. Match Postgres NUMERIC formatting.
        assert_eq!(normalize_decimal("42", 0).unwrap(), "42");
        assert_eq!(normalize_decimal("0", 0).unwrap(), "0");
    }

    #[test]
    fn normalize_decimal_rejects_excess_precision() {
        assert_eq!(normalize_decimal("0.6155", 3), Err(DomainError::PrecisionExceeded));
    }

    #[test]
    fn notional_meets_minimum_handles_disparate_precisions() {
        // price has 3 dp, qty has 2 dp, min has 0 dp; the helper must
        // align scales before comparing.
        assert!(notional_meets_minimum("10.000", "0.50", "5").unwrap());
        assert!(!notional_meets_minimum("10.000", "0.49", "5").unwrap());
    }

    #[test]
    fn encode_flags_market_bit_is_set() {
        // FLAG_MARKET (0x04) must always be set on MARKET-encoded flags,
        // never on LIMIT-encoded ones. The chain branches on this bit
        // for the `cost = amount` vs `cost = amount * price` decision
        // (PrivateNote.sol:1210-1215); a leak in either direction would
        // mis-lock collateral.
        let market = encode_order_flags(OrderType::Market, None).unwrap();
        assert_ne!(market & FLAG_MARKET, 0);

        for tif in [TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok, TimeInForce::PostOnly] {
            let limit = encode_order_flags(OrderType::Limit, Some(tif)).unwrap();
            assert_eq!(
                limit & FLAG_MARKET,
                0,
                "LIMIT/{} must not have FLAG_MARKET set",
                tif.as_str(),
            );
        }
    }

    #[test]
    fn asset_balance_json_uses_camel_case() {
        let b = AssetBalance {
            asset: "USDC".to_string(),
            free: "25000.00".to_string(),
            locked: "3750.00".to_string(),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["asset"], "USDC");
        assert_eq!(v["free"], "25000.00");
        assert_eq!(v["locked"], "3750.00");
    }

    #[test]
    fn outcome_balance_json_uses_camel_case() {
        let b = OutcomeBalance {
            outcome_id: 1,
            symbol: Symbol("PM-X-YES".to_string()),
            free: "5.50".to_string(),
            locked_in_orders: "1000.00".to_string(),
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["outcomeId"], 1);
        assert_eq!(v["symbol"], "PM-X-YES");
        assert_eq!(v["free"], "5.50");
        assert_eq!(v["lockedInOrders"], "1000.00");
    }
}
