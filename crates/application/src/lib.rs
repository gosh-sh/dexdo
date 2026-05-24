// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;

use async_trait::async_trait;
use dodex_domain::encode_order_flags;
use dodex_domain::is_multiple_of;
use dodex_domain::lift_decimal;
use dodex_domain::notional_meets_minimum;
use dodex_domain::precision_within;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::Order;
use dodex_domain::OrderSide;
use dodex_domain::OrderStatus;
use dodex_domain::OrderType;
use dodex_domain::Outcome;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use dodex_domain::Symbol;
use dodex_domain::TimeInForce;
use num_bigint::BigUint;
use tracing::error;
use tracing::warn;
use uuid::Uuid;

/// Per-request authorization state assembled by the HMAC middleware and
/// consumed by handlers via the Salvo depot. Carries the resolved
/// account, its custodied trading PN (with decrypted signing key), and
/// the granted permissions. `pn_seckey` zeroes on drop.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub account_id: Uuid,
    pub api_key_id: i64,
    pub trading_pn: TradingPn,
    pub permissions: Vec<Permission>,
}

/// The custodied trading PN bound to an account. `pn_pubkey` and `pn_dih`
/// are decimal-encoded uint256 strings — the format `bee-dex` accepts
/// for chain-side calls.
#[derive(Debug, Clone)]
pub struct TradingPn {
    pub pn_address: String,
    pub pn_pubkey: String,
    pub pn_dih: String,
    pub pn_seckey: SensitiveBytes,
}

impl AuthContext {
    pub fn has_permission(&self, perm: Permission) -> bool {
        self.permissions.contains(&perm)
    }

    /// Enforce a required permission. Returns `DomainError::AuthRequired`
    /// when the key does not carry it; the api error layer maps that to
    /// `-1002 / 401` per `docs/api-spec.md`.
    pub fn require(&self, perm: Permission) -> Result<(), DomainError> {
        if self.has_permission(perm) {
            Ok(())
        } else {
            Err(DomainError::AuthRequired)
        }
    }
}

/// Inputs the HTTP layer hands to the authenticator. The service stays
/// thin: it extracts these fields out of the Salvo request and passes
/// them in unaltered. `raw_query_string` is canonicalized inside the
/// authenticator so the canonical/HMAC concern does not leak into the
/// service layer; `body` is the on-the-wire byte sequence (never
/// re-serialized JSON).
#[derive(Debug, Clone)]
pub struct AuthenticateRequest {
    pub api_key: String,
    pub timestamp_ms: i64,
    pub recv_window_ms: Option<u64>,
    pub signature_hex: String,
    pub raw_query_string: String,
    pub body: Vec<u8>,
    pub now_ms: i64,
}

/// Verifies one HMAC-authenticated request and resolves it to the
/// account's [`AuthContext`]. Matches the verification pipeline in
/// `docs/tech-specs/auth.md §Authentication`. Implementations are
/// expected to be cheap to clone (e.g. wrap a connection pool in
/// `Arc`) so the trait object can sit in app state.
#[async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, request: AuthenticateRequest) -> Result<AuthContext, DomainError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarketsSort {
    #[default]
    ResultStartAsc,
    CreatedAtDesc,
}

#[derive(Debug, Clone, Default)]
pub struct MarketsFilter {
    pub statuses: Vec<MarketStatus>,
    pub quote_asset: Option<String>,
    pub oracle_name: Option<String>,
    pub closing_before: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MarketsListing {
    pub filter: MarketsFilter,
    pub sort: MarketsSort,
    pub cursor: Option<String>,
    pub limit: u16,
    pub now: i64,
}

#[derive(Debug, Clone)]
pub enum MarketsRequest {
    One { market_address: MarketAddress, now: i64 },
    Listing(MarketsListing),
}

/// Slim projection the `DELETE /api/v1/order` path needs. Built by a
/// single SELECT joining `live_orders ⋈ markets ⋈ market_outcomes` with
/// the ownership predicate `live_orders.owner_pn_address = :pn_address`
/// baked into the where-clause — a miss collapses to
/// `DomainError::UnknownOrder` regardless of whether the orderId does
/// not exist, belongs to another account, is no longer OPEN, or the
/// `(marketAddress, symbol)` does not match the order's actual market.
/// That ambiguity is intentional: differentiating those cases would
/// leak the existence of orders the caller does not own.
#[derive(Debug, Clone)]
pub struct OrderForCancel {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: i32,
    pub market_status: MarketStatus,
    /// `live_orders.client_order_id`. NULL in the DB surfaces as `None`
    /// here; the handler renders it as the empty string per
    /// api-spec §Cancel Order.
    pub client_order_id: Option<String>,
}

/// Slim market+outcome projection the `POST /api/v1/order` path needs.
/// Built by a single SELECT joining `markets ⋈ market_outcomes`; the
/// oracle/event aggregation that `list_markets` performs is irrelevant
/// on the trading hot path. `status` is computed against the caller's
/// `now` so downstream validation can reject everything except
/// `MarketStatus::Trading` without a second round-trip.
#[derive(Debug, Clone)]
pub struct MarketForPlacement {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: i32,
    pub status: MarketStatus,
    pub outcome: Outcome,
}

/// Per-outcome metadata needed to render a market-balances row.
#[derive(Debug, Clone)]
pub struct BalanceOutcome {
    pub outcome_id: u32,
    pub symbol: Symbol,
    pub quantity_precision: u8,
}

/// Result of resolving `marketAddress` for a balances request. Contains
/// every chain-side field (`event_id`, `oracle_list_hash`, `token_type`)
/// needed to compute `stake_hash` plus the outcome list used to render
/// the response.
#[derive(Debug, Clone)]
pub struct MarketBalancesResolution {
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: i32,
    pub orderbook_address: String,
    pub num_outcomes: i32,
    pub outcomes: Vec<BalanceOutcome>,
}

#[async_trait]
pub trait MarketReadRepository: Send + Sync {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error>;

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error>;

    /// Resolve the `(marketAddress, symbol)` pair the trading path needs
    /// in a single SELECT — no oracle/event aggregation, no second
    /// outcome fetch. `now` lets the implementation compute the
    /// `MarketStatus` so the use case can fail closed without a separate
    /// `list_markets` call. Misses collapse to
    /// `DomainError::InvalidMarketOrSymbol`.
    async fn resolve_for_new_order(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        now: i64,
    ) -> Result<MarketForPlacement, anyhow::Error>;

    /// Resolve one open order owned by `owner_pn_address` together with
    /// the chain-side market fields needed for `PrivateNote.cancelOrder`,
    /// in a single SELECT. The ownership predicate is part of the
    /// where-clause: any miss (unknown id, wrong owner, wrong market,
    /// already closed) collapses to `DomainError::UnknownOrder` so error
    /// codes do not leak ownership.
    async fn resolve_for_cancel(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_id: u64,
        owner_pn_address: &str,
        now: i64,
    ) -> Result<OrderForCancel, anyhow::Error>;

    async fn list_orders(&self, query: &OrdersQuery) -> Result<OrdersPage, anyhow::Error>;

    /// Resolve a market for the balances path: returns chain-side
    /// fields needed to compute `stake_hash` plus the outcome list
    /// used to render the response. Gated by
    /// `last_reconciled_at IS NOT NULL`. Misses collapse to
    /// `DomainError::InvalidMarketOrSymbol`.
    async fn resolve_market_for_balances(
        &self,
        market_address: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error>;

    /// Sum `amount_remaining` over OPEN SELL rows owned by `owner_pn`
    /// on `orderbook_address`, grouped by `outcome_id`. Returns a map
    /// keyed by `outcome_id` with raw uint128 values as decimal strings
    /// (scaled by the API). Missing outcomes default to "0" on the
    /// caller side.
    async fn sum_open_sell_remaining(
        &self,
        orderbook_address: &str,
        owner_pn_address: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + MarketReadRepository> MarketReadRepository for Arc<T> {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        (**self).list_markets(request).await
    }

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        (**self).get_depth(market_address, symbol, limit).await
    }

    async fn resolve_for_new_order(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        now: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
        (**self).resolve_for_new_order(market_address, symbol, now).await
    }

    async fn resolve_for_cancel(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_id: u64,
        owner_pn_address: &str,
        now: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        (**self).resolve_for_cancel(market_address, symbol, order_id, owner_pn_address, now).await
    }

    async fn list_orders(&self, query: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        (**self).list_orders(query).await
    }

    async fn resolve_market_for_balances(
        &self,
        market_address: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error> {
        (**self).resolve_market_for_balances(market_address).await
    }

    async fn sum_open_sell_remaining(
        &self,
        orderbook_address: &str,
        owner_pn_address: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        (**self).sum_open_sell_remaining(orderbook_address, owner_pn_address).await
    }
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub limit: u16,
}

pub const ORDERS_DEFAULT_LIMIT: u16 = 100;
pub const ORDERS_MAX_LIMIT: u16 = 500;

/// Order statuses queryable through `GET /api/v1/orders`. This deliberately
/// excludes write-side synthetic states (`PENDING_NEW`, `PENDING_CANCEL`) so
/// SQL predicate construction cannot accidentally admit them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum QueryableOrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

impl QueryableOrderStatus {
    pub fn as_public_status(self) -> OrderStatus {
        match self {
            Self::New => OrderStatus::New,
            Self::PartiallyFilled => OrderStatus::PartiallyFilled,
            Self::Filled => OrderStatus::Filled,
            Self::Canceled => OrderStatus::Canceled,
            Self::Rejected => OrderStatus::Rejected,
        }
    }
}

/// Non-empty subset of read-queryable statuses. The constructor is
/// crate-private and `from_csv` is the only way to build one from
/// outside, so `OrderStatusFilter::Only(_)` is non-empty by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyStatusSet(std::collections::BTreeSet<QueryableOrderStatus>);

impl NonEmptyStatusSet {
    fn new(set: std::collections::BTreeSet<QueryableOrderStatus>) -> Option<Self> {
        if set.is_empty() {
            None
        } else {
            Some(Self(set))
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &QueryableOrderStatus> + '_ {
        self.0.iter()
    }
}

/// Caller-supplied filter on order status. Either matches every row
/// (no `status` parameter on the request) or narrows to a non-empty
/// set of queryable tokens. Callers pattern-match on the variant
/// directly — `All` is plainly visible at the type level rather than
/// hidden behind an `is_all()` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatusFilter {
    /// No filter — every row passes.
    All,
    /// Filter to the listed statuses. The inner type's constructor is
    /// crate-private, so this variant is always non-empty.
    Only(NonEmptyStatusSet),
}

impl OrderStatusFilter {
    /// Parse the request `status` parameter. `None` or all-whitespace
    /// returns `All`; anything else is split on `,`, trimmed,
    /// de-duplicated, and matched against the allow-list. An unknown
    /// token (or `PENDING_NEW` / `PENDING_CANCEL`, which are write-side
    /// only) returns [`DomainError::InvalidParameter`].
    ///
    /// Whitespace-only input is treated as `All` by design. This is
    /// asymmetric with the `cursor` parameter — a whitespace-only
    /// `cursor` is rejected as `MissingParameter` — because the two
    /// parameters express different intents: `status` is an optional
    /// narrowing filter whose absence (any falsy form) trivially means
    /// "no filter applied", while `cursor` is an opaque server-issued
    /// token whose syntactic emptiness is always a client-side bug.
    /// See docs/api-spec.md#orders (Behavior section) for the public
    /// contract. Do not collapse the two parsers into a shared
    /// "blank-is-empty" helper.
    pub fn from_csv(raw: Option<&str>) -> Result<Self, DomainError> {
        let Some(value) = raw else {
            return Ok(Self::All);
        };
        let mut set = std::collections::BTreeSet::new();
        for token in value.split(',') {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            let status = match trimmed {
                "NEW" => QueryableOrderStatus::New,
                "PARTIALLY_FILLED" => QueryableOrderStatus::PartiallyFilled,
                "FILLED" => QueryableOrderStatus::Filled,
                "CANCELED" => QueryableOrderStatus::Canceled,
                "REJECTED" => QueryableOrderStatus::Rejected,
                _ => return Err(DomainError::InvalidParameter),
            };
            set.insert(status);
        }
        match NonEmptyStatusSet::new(set) {
            Some(non_empty) => Ok(Self::Only(non_empty)),
            None => Ok(Self::All),
        }
    }
}

/// Opaque pagination cursor for `/api/v1/orders`. The inner string is
/// the `placed_chain_order` of the last row returned by a previous
/// page; the server reads it as a lexicographic token via the strict
/// `<` predicate in [`PostgresReadModelRepository::list_orders`].
///
/// Both [`OrdersCursor::new`] (client input) and
/// [`OrdersCursor::from_db_token`] (storage token) trim surrounding
/// whitespace, reject blank values, and reject lengths above
/// [`MAX_CURSOR_LEN`]. They differ only in the error variant: a
/// blank or oversized client value surfaces as `MissingParameter`
/// (blank) or `InvalidParameter` (oversized); a corrupt stored
/// value surfaces as `Unexpected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrdersCursor(String);

/// Hard cap on the length of a cursor token after trimming. The
/// gateway-issued `msg_chain_order` is ~32-50 chars in practice; 128
/// keeps generous headroom while making a hostile 10 MB
/// `?cursor=AAA...` request fail before reaching the SQL layer (the
/// cursor binds as `$4::text` and Postgres performs the comparison
/// per scanned index entry).
pub const MAX_CURSOR_LEN: usize = 128;

impl OrdersCursor {
    /// Validating constructor for client input. Trims whitespace and
    /// rejects blank as [`DomainError::MissingParameter`]; rejects
    /// lengths above [`MAX_CURSOR_LEN`] as
    /// [`DomainError::InvalidParameter`] (the value is present, just
    /// malformed). Both maps surface to the documented `/api/v1/orders`
    /// error codes — see read-api.md §error mapping.
    pub fn new(raw: String) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainError::MissingParameter);
        }
        if trimmed.len() > MAX_CURSOR_LEN {
            return Err(DomainError::InvalidParameter);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn from_db_token(raw: String) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainError::Unexpected);
        }
        // Symmetric with `new`: a corrupt storage row with an
        // unbounded `placed_chain_order` would otherwise resurface on
        // the next page as a hostile-shaped cursor.
        if trimmed.len() > MAX_CURSOR_LEN {
            return Err(DomainError::Unexpected);
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// Page-size cap for `/api/v1/orders`. The constructor enforces
/// `1..=ORDERS_MAX_LIMIT`, lifting the "must be at least 1" invariant
/// into the type so the Postgres cursor builder's
/// `last() == Some` after `truncate(limit)` holds by construction
/// rather than by a runtime `expect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrdersLimit(u16);

impl OrdersLimit {
    /// Default page size when `limit` is absent on the request.
    /// Routes through `from_const` so the validating `assert!` runs at
    /// compile time — a future bump of `ORDERS_DEFAULT_LIMIT` above
    /// `ORDERS_MAX_LIMIT` would fail to build rather than producing an
    /// out-of-range default that bypasses the runtime guard in `new`.
    pub const DEFAULT: Self = Self::from_const(ORDERS_DEFAULT_LIMIT);

    /// Validating constructor for runtime input. Out-of-range values
    /// surface as `MissingParameter` — matches the public `-1102` error
    /// the HTTP handler emits.
    pub fn new(value: u16) -> Result<Self, DomainError> {
        if value == 0 || value > ORDERS_MAX_LIMIT {
            return Err(DomainError::MissingParameter);
        }
        Ok(Self(value))
    }

    /// Const constructor for statically-known values (test fixtures,
    /// derived defaults). The `assert!` is evaluated at compile time
    /// only when the call site is itself in a const context (a const
    /// item, another const fn, etc.); a non-const call site evaluates
    /// the assert at runtime and panics on out-of-range input. Use
    /// `OrdersLimit::new` for runtime construction with a typed error.
    pub const fn from_const(value: u16) -> Self {
        assert!(
            value >= 1 && value <= ORDERS_MAX_LIMIT,
            "OrdersLimit must be within 1..=ORDERS_MAX_LIMIT",
        );
        Self(value)
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct OrdersQuery {
    pub owner_pn_address: String,
    pub market: Option<OrdersMarketFilter>,
    pub status: OrderStatusFilter,
    pub limit: OrdersLimit,
    pub cursor: Option<OrdersCursor>,
}

#[derive(Debug, Clone)]
pub struct OrdersMarketFilter {
    market_address: MarketAddress,
    symbol: Symbol,
}

impl OrdersMarketFilter {
    pub fn pair(
        market_address: Option<MarketAddress>,
        symbol: Option<Symbol>,
    ) -> Result<Option<Self>, DomainError> {
        match (market_address, symbol) {
            (None, None) => Ok(None),
            (Some(market_address), Some(symbol)) => Ok(Some(Self { market_address, symbol })),
            _ => Err(DomainError::MissingParameter),
        }
    }

    pub fn market_address(&self) -> &MarketAddress {
        &self.market_address
    }

    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }
}

/// Result of `MarketReadRepository::list_orders`. All four combinations
/// of `(orders, next_cursor)` are legal:
///
/// - `(non-empty, Some)`: typical page with more results.
/// - `(non-empty, None)`: last page in the scan.
/// - `(empty, None)`: end of results (no rows in scope, or the cursor
///   advanced past every row).
/// - `(empty, Some)`: a `has_more=true` page in which every retained
///   row was filtered out by `order_from_row` (entire window is
///   corrupt). The cursor is built from the last retained row
///   *before* the filter pass — see read-api.md §SQL — so the client
///   can paginate through the corrupt window using the cursor without
///   ever re-reading the dropped rows. Surfacing `Unexpected` here
///   instead would strand the client at 500 with no usable cursor.
#[derive(Debug, Clone)]
pub struct OrdersPage {
    pub orders: Vec<Order>,
    pub next_cursor: Option<OrdersCursor>,
}

pub struct GetMarketsUseCase<R> {
    repo: R,
}

impl<R> GetMarketsUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetMarketsUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(&self, request: MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        self.repo.list_markets(&request).await
    }
}

pub struct GetDepthUseCase<R> {
    repo: R,
}

impl<R> GetDepthUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetDepthUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(&self, query: GetDepthQuery) -> Result<DepthSnapshot, anyhow::Error> {
        self.repo.get_depth(&query.market_address, &query.symbol, query.limit).await
    }
}

/// Input for `GetAccountUseCase`. Built by the HTTP layer from the
/// resolved auth context plus the request-entry timestamp.
#[derive(Debug, Clone)]
pub struct GetAccountInput {
    pub account_id: uuid::Uuid,
    pub pn_address: String,
    /// Unix milliseconds. Echoed as `updateTime` in the response.
    pub now_ms: i64,
}

pub struct GetAccountUseCase<P, R> {
    pn: P,
    refs: R,
}

impl<P, R> GetAccountUseCase<P, R> {
    pub fn new(pn: P, refs: R) -> Self {
        Self { pn, refs }
    }
}

impl<P, R> GetAccountUseCase<P, R>
where
    P: PnStateReader,
    R: ReferenceRepository,
{
    pub async fn execute(
        &self,
        input: GetAccountInput,
    ) -> Result<dodex_domain::AccountBalances, anyhow::Error> {
        let details = self.pn.get_details(&input.pn_address).await.map_err(|e| {
            warn!(error = ?e, pn = %input.pn_address, "get_details failed");
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        // Index locked_in_orders by token_type for O(1) lookup.
        let mut locked_by_tt: std::collections::HashMap<i32, String> =
            std::collections::HashMap::new();
        for (tt, amount) in &details.locked_in_orders {
            locked_by_tt.insert(*tt, amount.clone());
        }

        let mut rows: Vec<dodex_domain::AssetBalance> = Vec::with_capacity(details.balance.len());
        for (tt, raw_free) in &details.balance {
            let token = self
                .refs
                .lookup_ref_token(*tt)
                .await?
                .ok_or_else(|| {
                    warn!(token_type = tt, "balance carries unknown token_type");
                    anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
                })?;
            let raw_locked =
                locked_by_tt.get(tt).cloned().unwrap_or_else(|| "0".to_string());
            rows.push(dodex_domain::AssetBalance {
                asset: token.token_code.clone(),
                free: scale_decimal(raw_free, token.decimals),
                locked: scale_decimal(&raw_locked, token.decimals),
            });
        }
        rows.sort_by(|a, b| a.asset.cmp(&b.asset));

        Ok(dodex_domain::AccountBalances {
            account_id: input.account_id,
            update_time_ms: input.now_ms,
            balances: rows,
        })
    }
}

/// Scale a non-negative integer-decimal string `raw` (the smallest-unit
/// uint representation) to a fixed-point decimal with `decimals` digits
/// to the right of the point.
///
/// Non-zero inputs are padded to exactly `decimals` fractional digits
/// (e.g. `"10000000000"` with `decimals=9` → `"10.000000000"`,
/// `"1"` → `"0.000000001"`). The literal zero case (`raw == "0"` or
/// empty) short-circuits to bare `"0"`; clients SHOULD treat `"0"`
/// and `"0.000000"` as equivalent. `decimals == 0` returns `raw`
/// unchanged.
fn scale_decimal(raw: &str, decimals: u8) -> String {
    if raw == "0" || raw.is_empty() {
        return "0".to_string();
    }
    let d = decimals as usize;
    if d == 0 {
        return raw.to_string();
    }
    if raw.len() <= d {
        let padded = "0".repeat(d - raw.len()) + raw;
        format!("0.{padded}")
    } else {
        let split = raw.len() - d;
        format!("{}.{}", &raw[..split], &raw[split..])
    }
}

/// Input shape for `CreateOrderUseCase`. The HTTP layer parses
/// `POST /api/v1/order` body + `AuthContext` + clock into this struct.
/// All decimal fields stay as strings — exact-decimal validation runs
/// inside the use case via `dodex_domain` helpers; floats are never
/// involved.
#[derive(Debug, Clone)]
pub struct NewOrderInput {
    pub trading_pn: TradingPn,
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub side: OrderSide,
    /// Outcome-token amount for LIMIT and for `MARKET SELL`; quote-asset
    /// spend amount for `MARKET BUY` per
    /// [api-spec §New Order](../../docs/api-spec.md#new-order).
    pub quantity: String,
    /// Required for `LIMIT`; rejected for `MARKET`.
    pub price: Option<String>,
    pub order_type: OrderType,
    pub time_in_force: Option<TimeInForce>,
    /// Optional client-supplied id; absence triggers backend generation.
    pub client_order_id: Option<String>,
    /// Unix seconds. Used both for status derivation and as the
    /// `serverTime`-style anchor for the response.
    pub now_seconds: i64,
    /// Unix milliseconds. Returned to the client as `transactTime`.
    pub now_ms: i64,
}

/// Chain-shaped payload handed to `ChainOrderSender`. All numeric
/// fields are decimal strings sized for the on-chain ABI:
/// - `price_raw`: uint256 in the contract's tick units (lifted by
///   `pricePrecision`); `"0"` for `MARKET`.
/// - `amount_raw`: uint128 lifted by `quantityPrecision`. The scale
///   is the same regardless of side or type; only the unit it
///   represents differs — outcome-token amount on LIMIT and MARKET
///   SELL, quote-asset spend amount on MARKET BUY (per [api-spec
///   §New Order](../../docs/api-spec.md#new-order)).
/// - `client_order_id`: decimal string. ABI accepts uint128 but the
///   serialization path through `serde_json::json!` rejects values
///   above `u64::MAX` (no `arbitrary_precision` feature upstream), so
///   the use case validates this as `u64::from_str`. See
///   [write-api.md §clientOrderId generation] for the rationale.
#[derive(Debug, Clone)]
pub struct NewOrderPayload {
    pub pn_address: String,
    /// Decimal-encoded `uint256` public half of the trading-PN keypair.
    /// `BeeDexChainSender` re-encodes it as hex for `KeyPair.public`.
    pub pn_pubkey: String,
    pub pn_seckey: SensitiveBytes,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub outcome_id: u32,
    pub is_buy: bool,
    pub price_raw: String,
    pub amount_raw: String,
    pub flags: u8,
    pub client_order_id: String,
}

/// Output of `CreateOrderUseCase`. The HTTP response shape for
/// `POST /api/v1/order` is intentionally minimal — see
/// `docs/tech-specs/write-api.md §Response` for the rationale; the
/// only fact the use case contributes that the handler does not
/// already have is the resolved `clientOrderId` (caller-supplied or
/// backend-generated).
#[derive(Debug, Clone)]
pub struct SubmittedOrder {
    pub client_order_id: String,
}

/// Input shape for `CancelOrderUseCase`. The HTTP layer parses
/// `DELETE /api/v1/order` query string + `AuthContext` + clock into
/// this struct. `order_id` is already parsed as `u64` — overflow is
/// rejected at the HTTP boundary so the use case never sees out-of-range
/// values (see `docs/tech-specs/write-api.md §Request parsing`).
#[derive(Debug, Clone)]
pub struct CancelOrderInput {
    pub trading_pn: TradingPn,
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub order_id: u64,
    /// Unix seconds. Used for status derivation in
    /// `resolve_for_cancel` and as the `serverTime`-style anchor.
    pub now_seconds: i64,
    /// Unix milliseconds. Returned to the client as `transactTime`.
    pub now_ms: i64,
}

/// Chain-shaped payload handed to `ChainOrderSender::cancel_order`.
/// Parallel in shape to `NewOrderPayload`, but the ABI is narrower —
/// `PrivateNote.cancelOrder` takes only event/oracle/token coordinates
/// plus the chain-assigned `orderId`. No price, amount, or flags are
/// involved on cancel.
#[derive(Debug, Clone)]
pub struct CancelOrderPayload {
    pub pn_address: String,
    /// Decimal-encoded `uint256` public half of the trading-PN keypair.
    pub pn_pubkey: String,
    pub pn_seckey: SensitiveBytes,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub order_id: u64,
}

/// Output of `CancelOrderUseCase`. Carries the `clientOrderId` resolved
/// from `live_orders` (or `None` if the order was placed without one)
/// so the handler can echo it in the response per api-spec §Cancel
/// Order. `orderId` is not duplicated here — the handler already has it
/// from the request.
#[derive(Debug, Clone)]
pub struct CancelledOrder {
    pub client_order_id: Option<String>,
}

/// One body item of `POST /api/v1/batchOrders`. Validation rules are
/// identical to `POST /api/v1/order` — both paths share
/// `validate_and_encode_order_item`.
#[derive(Debug, Clone)]
pub struct BatchOrderInputItem {
    pub side: OrderSide,
    pub quantity: String,
    pub price: Option<String>,
    pub order_type: OrderType,
    pub time_in_force: Option<TimeInForce>,
    pub client_order_id: Option<String>,
}

/// Input shape for `CreateBatchOrdersUseCase`. One `(marketAddress,
/// symbol)` per request: the chain ABI accepts only one `(eventId,
/// oracleListHash, tokenType)` per batch, so all items share the same
/// market/outcome resolution.
#[derive(Debug, Clone)]
pub struct CreateBatchOrdersInput {
    pub trading_pn: TradingPn,
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub orders: Vec<BatchOrderInputItem>,
    pub now_seconds: i64,
    pub now_ms: i64,
}

/// Per-item chain-shaped fields. Split from the batch-level
/// `(eventId, oracleListHash, tokenType)` because the chain ABI carries
/// those at the batch level and only these fields per order.
#[derive(Debug, Clone)]
pub struct BatchOrderPayloadItem {
    pub outcome_id: u32,
    pub is_buy: bool,
    pub price_raw: String,
    pub amount_raw: String,
    pub flags: u8,
    pub client_order_id: String,
}

/// Chain-shaped payload handed to `ChainOrderSender::submit_batch_order`.
/// Maps directly to `ackinacki-kit::PrivateNote::place_batch`
/// (`ParamsOfPlaceBatch`).
#[derive(Debug, Clone)]
pub struct NewBatchOrderPayload {
    pub pn_address: String,
    pub pn_pubkey: String,
    pub pn_seckey: SensitiveBytes,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub orders: Vec<BatchOrderPayloadItem>,
}

/// Output of `CreateBatchOrdersUseCase`. Request order is preserved so
/// the HTTP layer can pair each `SubmittedOrder` with the input item by
/// position — see
/// [api-spec §New Batch Orders](../../docs/api-spec.md#new-batch-orders).
#[derive(Debug, Clone)]
pub struct SubmittedBatchOrders {
    pub items: Vec<SubmittedOrder>,
}

/// Dispatch a `PrivateNote.placeOrder` external message to chain.
/// Returns once `bee_dex` has observed the chain's execution of
/// `PrivateNote.placeOrder` — so PrivateNote-side `require(...)`
/// failures (`ERR_NOTE_BUSY`, `ERR_LOW_VALUE`, `ERR_INVALID_OUTCOME_ID`,
/// etc.) come back as typed `DomainError`s here. Only
/// `OrderBook.Rejected` remains async (it fires from the internal
/// message `placeOrder` enqueues, in a separate transaction this
/// future cannot observe) and is surfaced through indexer projection
/// — see [write-api.md §Failure surface](../../docs/tech-specs/write-api.md#failure-surface)
/// for the canonical three-class split.
#[async_trait]
pub trait ChainOrderSender: Send + Sync {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError>;

    /// Dispatch a `PrivateNote.cancelOrder` external message to chain.
    /// Returns once `bee_dex` has observed PN's execution of
    /// `cancelOrder` — the only PN-side reject mapped here is
    /// `ERR_NOTE_BUSY` → `OrderPnBusy`. OrderBook-side outcomes (silent
    /// no-op on owner mismatch / already-closed, queue overflow
    /// `Rejected`) are asynchronous and surface through the indexer; see
    /// `docs/tech-specs/write-api.md §DELETE failure surface`.
    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError>;

    /// Dispatch a `PrivateNote.placeBatch` external message to chain.
    /// `placeBatch` is atomic on chain: every item is re-validated and
    /// any failed `require(...)` reverts the whole batch — none of the
    /// items land. The same chain `_busy` lock applies as for
    /// `placeOrder`, so a busy PN surfaces here as `OrderPnBusy` too.
    /// New chain exit codes batches can raise:
    /// `129 ERR_INVALID_PARAMS` (intra-batch clientOrderId collision),
    /// `161 ERR_BATCH_TOO_LARGE`, `162 ERR_EMPTY_BATCH` (both as
    /// defence-in-depth — the use case pre-checks these), and
    /// `168 ERR_NOTIONAL_OVERFLOW`.
    async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError>;
}

#[async_trait]
impl<T: ?Sized + ChainOrderSender> ChainOrderSender for Arc<T> {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        (**self).submit_order(payload).await
    }

    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
        (**self).cancel_order(payload).await
    }

    async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError> {
        (**self).submit_batch_order(payload).await
    }
}

// ─────────────────────────────────────────────────────────────────────
// Balances ports + value objects (NODE-3445).
// ─────────────────────────────────────────────────────────────────────

/// One row of `ref_tokens` exposed to the application layer.
#[derive(Debug, Clone)]
pub struct RefToken {
    pub token_type: i32,
    pub token_code: String,
    pub decimals: u8,
}

/// Source of `ref_tokens` lookups. Kept as a separate port from
/// `MarketReadRepository` because callers (use cases) need only
/// `lookup_ref_token` and dragging in the heavy MarketRead surface
/// would coupling-test downstream traits unnecessarily.
#[async_trait]
pub trait ReferenceRepository: Send + Sync {
    /// Returns `None` for an unknown `token_type`. Use cases turn `None`
    /// into `DomainError::MarketInconsistent` (the indexer ships with
    /// the canonical set; an unknown type is read-model corruption).
    async fn lookup_ref_token(
        &self,
        token_type: i32,
    ) -> Result<Option<RefToken>, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + ReferenceRepository> ReferenceRepository for Arc<T> {
    async fn lookup_ref_token(
        &self,
        token_type: i32,
    ) -> Result<Option<RefToken>, anyhow::Error> {
        (**self).lookup_ref_token(token_type).await
    }
}

/// Detokenized `PrivateNote.getDetails()` output (only the fields the
/// account-balance path needs). `balance` and `locked_in_orders` mirror
/// the on-chain `map(uint32 → uint128)` shape as `(token_type, raw_uint128_decimal_string)`
/// pairs — the use case scales them with `RefToken.decimals` before
/// returning to the API. The raw amounts stay as strings because
/// `serde_json` cannot round-trip `u128` safely.
#[derive(Debug, Clone)]
pub struct PnDetails {
    pub balance: Vec<(i32, String)>,
    pub locked_in_orders: Vec<(i32, String)>,
}

/// Detokenized `PrivateNote._stakes(hash)` value object — only the three
/// outcome-amount arrays. Arrays are indexed by `outcome_id`. Empty
/// arrays mean "no stake key for this market" (TVM auto-getter returns
/// a zero-default struct).
#[derive(Debug, Clone, Default)]
pub struct PnStake {
    pub amount: Vec<String>,
    pub debt_amount: Vec<String>,
    pub coupons_amount: Vec<String>,
}

/// On-demand reader for `PrivateNote` chain state. Implementations
/// fetch the PN BOC from the GraphQL gateway and run the requested
/// off-chain getter; failures (gateway down, account absent, ABI
/// mismatch) bubble up as `anyhow::Error` which the use case lifts
/// to `DomainError::MarketInconsistent`.
#[async_trait]
pub trait PnStateReader: Send + Sync {
    /// Run `getDetails()` against the PN at `pn_address`.
    async fn get_details(&self, pn_address: &str) -> Result<PnDetails, anyhow::Error>;

    /// Run `_stakes` (no args — TVM Solidity public-mapping auto-getter
    /// returns the whole map). Implementation locates the matching key
    /// by `stake_hash`; returns `None` when the key is absent.
    async fn get_stake(
        &self,
        pn_address: &str,
        stake_hash: &str,
    ) -> Result<Option<PnStake>, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + PnStateReader> PnStateReader for Arc<T> {
    async fn get_details(&self, pn_address: &str) -> Result<PnDetails, anyhow::Error> {
        (**self).get_details(pn_address).await
    }

    async fn get_stake(
        &self,
        pn_address: &str,
        stake_hash: &str,
    ) -> Result<Option<PnStake>, anyhow::Error> {
        (**self).get_stake(pn_address, stake_hash).await
    }
}

/// Orchestrates `POST /api/v1/order`: resolves market, derives status,
/// validates input per spec §Input validation, encodes flags, builds the
/// chain payload, dispatches through `ChainOrderSender`, and returns
/// values the HTTP layer needs to assemble the response. The use case
/// is generic over the repo and sender so tests can substitute fakes.
pub struct CreateOrderUseCase<R, S> {
    repo: R,
    sender: S,
}

impl<R, S> CreateOrderUseCase<R, S> {
    pub fn new(repo: R, sender: S) -> Self {
        Self { repo, sender }
    }
}

impl<R, S> CreateOrderUseCase<R, S>
where
    R: MarketReadRepository,
    S: ChainOrderSender,
{
    pub async fn execute(&self, input: NewOrderInput) -> Result<SubmittedOrder, DomainError> {
        let MarketForPlacement { event_id, oracle_list_hash, token_type, status, outcome } = self
            .repo
            .resolve_for_new_order(&input.market_address, &input.symbol, input.now_seconds)
            .await
            .map_err(|err| {
                // The repo returns `anyhow::Error` so its inner failures can
                // be typed (`InvalidMarketOrSymbol` for a miss,
                // `MarketInconsistent` for blank orderbook etc.) or raw I/O.
                // Downcast preserves the typed variant; everything else is
                // an unexpected internal error — log the underlying cause
                // so a 500 on the trading path leaves a breadcrumb for ops
                // (matches the logging discipline of `get_markets` /
                // `get_depth` in `services/api`).
                if let Some(domain) = err.downcast_ref::<DomainError>() {
                    return *domain;
                }
                error!(?err, market_address = %input.market_address.0, "resolve_for_new_order failed (non-domain)");
                DomainError::Unexpected
            })?;

        if status != MarketStatus::Trading {
            return Err(DomainError::OrderValidationFailed);
        }

        // Read-side `assemble_market` deliberately renders a NULL
        // `oracle_list_hash` as the empty string so that read endpoints
        // (which do not surface the field) stay available for an
        // otherwise-valid market. The trading path is where it actually
        // matters — fail closed with 503 here, mirroring the
        // `orderbook_address` invariant.
        if oracle_list_hash.is_empty() {
            return Err(DomainError::MarketInconsistent);
        }

        let encoded = validate_and_encode_order_item(
            input.side,
            &input.quantity,
            input.price.as_deref(),
            input.order_type,
            input.time_in_force,
            input.client_order_id.as_deref(),
            &outcome,
        )?;

        // `markets.token_type` is `integer` in Postgres (signed), but the
        // on-chain `PrivateNote.placeOrder` ABI is `uint32`. The
        // reconciler only ever writes values pulled from
        // `PMP.getDetails()`, so a negative here would mean the DB row
        // was corrupted post-reconcile — fail closed with 503 instead
        // of pushing a sign-folded value to chain.
        let token_type = u32::try_from(token_type).map_err(|_| DomainError::MarketInconsistent)?;

        let payload = NewOrderPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            outcome_id: encoded.outcome_id,
            is_buy: encoded.is_buy,
            price_raw: encoded.price_raw,
            amount_raw: encoded.amount_raw,
            flags: encoded.flags,
            client_order_id: encoded.client_order_id.clone(),
        };
        self.sender.submit_order(payload).await?;

        Ok(SubmittedOrder { client_order_id: encoded.client_order_id })
    }
}

/// Generate a fresh `clientOrderId`. Decimal string of a `uint64`
/// random value (low 64 bits of `Uuid::new_v4()`), bounded by the
/// upstream serialization constraint documented in
/// `docs/tech-specs/write-api.md §clientOrderId generation`.
fn generate_client_order_id() -> String {
    (Uuid::new_v4().as_u128() as u64).to_string()
}

/// Run the per-order validation and chain encoding that both
/// `POST /api/v1/order` and `POST /api/v1/batchOrders` apply identically
/// per [api-spec §Validation Rules]. Single-order callers wrap the
/// result with chain-level fields into a `NewOrderPayload`; batch
/// callers collect a `Vec<BatchOrderPayloadItem>` into
/// `NewBatchOrderPayload`.
fn validate_and_encode_order_item(
    side: OrderSide,
    quantity: &str,
    price: Option<&str>,
    order_type: OrderType,
    time_in_force: Option<TimeInForce>,
    client_order_id: Option<&str>,
    outcome: &Outcome,
) -> Result<BatchOrderPayloadItem, DomainError> {
    let flags = encode_order_flags(order_type, time_in_force)?;

    // `price` is required for LIMIT and rejected for MARKET per
    // api-spec §New Order. Resolve the field-presence + order-type
    // matrix once, into an `Option<&str>` the rest of the function
    // can reference without re-checking.
    let price_input: Option<&str> = match (order_type, price) {
        (OrderType::Limit, Some(p)) => Some(p),
        (OrderType::Limit, None) => return Err(DomainError::MissingParameter),
        (OrderType::Market, None) => None,
        (OrderType::Market, Some(_)) => return Err(DomainError::InvalidParameter),
    };

    let price_raw = match price_input {
        Some(p) => {
            precision_within(p, outcome.price_precision)?;
            if !is_multiple_of(p, &outcome.tick_size)? {
                return Err(DomainError::PrecisionExceeded);
            }
            lift_decimal(p, outcome.price_precision)?.to_str_radix(10)
        }
        None => "0".to_string(),
    };

    precision_within(quantity, outcome.quantity_precision)?;
    if !is_multiple_of(quantity, &outcome.step_size)? {
        return Err(DomainError::PrecisionExceeded);
    }
    let amount_lifted = lift_decimal(quantity, outcome.quantity_precision)?;
    // Strictly-positive invariant. `quantity == "0"` survives
    // `precision_within` (no fractional digits) and `is_multiple_of`
    // (zero is a multiple of every non-zero step), and the
    // MARKET-SELL branch below skips the notional check that
    // implicitly catches it for LIMIT and MARKET-BUY. Without this
    // gate the chain would reject with `ERR_LOW_VALUE` (102) — a
    // wasted round-trip and avoidable contention with the per-PN
    // `_busy` lock for the legitimate next submission.
    if amount_lifted == BigUint::from(0u32) {
        return Err(DomainError::OrderValidationFailed);
    }
    // SDK serialization ceiling. `PrivateNote.placeOrder.amount` and
    // `placeBatch.orders[i].amount` are `uint128` at the chain ABI,
    // but the upstream `bee_dex` → `ackinacki-kit` → `serde_json::json!`
    // path panics on `u128 > u64::MAX` for the same reason
    // `clientOrderId` is capped — see
    // `docs/tech-specs/write-api.md §clientOrderId generation`.
    if amount_lifted > BigUint::from(u64::MAX) {
        return Err(DomainError::OrderValidationFailed);
    }
    let amount_raw = amount_lifted.to_str_radix(10);

    // Notional check splits per (type, side) per spec validation
    // table. `price_input` carries the validated LIMIT price (or
    // `None` for MARKET); the MARKET-SELL branch has no spec rule.
    match (order_type, side, price_input) {
        (OrderType::Limit, _, Some(p)) => {
            if !notional_meets_minimum(p, quantity, &outcome.min_notional)? {
                return Err(DomainError::OrderValidationFailed);
            }
        }
        (OrderType::Market, OrderSide::Buy, _) => {
            // MARKET BUY: `quantity` is the quote-asset spend amount,
            // compared directly against `minNotional`.
            if !notional_meets_minimum("1", quantity, &outcome.min_notional)? {
                return Err(DomainError::OrderValidationFailed);
            }
        }
        (OrderType::Market, OrderSide::Sell, _) => {
            // api-spec doesn't list a notional rule for MARKET SELL;
            // the chain enforces its own MIN_ORDER_NOTIONAL.
        }
        // The (Limit, None) and (Market, Some) cases above already
        // returned, so this arm is structurally unreachable. Collapse
        // to `Unexpected` (500) rather than `panic!` so a future
        // refactor that broke the invariant could not turn into an
        // opaque crash in the request handler. Log the breach so the
        // 500 carries a breadcrumb instead of being a bare wire error.
        (OrderType::Limit, _, None) => {
            error!("validate_and_encode_order_item: (Limit, _, None) reached — price_input resolution invariant (Limit orders carry Some(price)) drifted");
            return Err(DomainError::Unexpected);
        }
    }

    // Caller-supplied `newOrderClientId` is bounded at `u64::MAX`
    // by the upstream serialization constraint documented in
    // `docs/tech-specs/write-api.md §clientOrderId generation`.
    let client_order_id = match client_order_id {
        Some(raw) => {
            raw.parse::<u64>().map_err(|_| DomainError::InvalidParameter)?;
            raw.to_string()
        }
        None => generate_client_order_id(),
    };

    Ok(BatchOrderPayloadItem {
        outcome_id: outcome.outcome_id,
        is_buy: side.is_buy(),
        price_raw,
        amount_raw,
        flags,
        client_order_id,
    })
}

/// Orchestrates `DELETE /api/v1/order`: resolves the caller-owned open
/// order through one SELECT, validates market `status == TRADING`,
/// builds the chain payload, dispatches `PrivateNote.cancelOrder`, and
/// returns the `clientOrderId` to echo. The chain-side effects on
/// `OrderBook` are asynchronous; only the PN-side accept/reject is
/// surfaced synchronously.
pub struct CancelOrderUseCase<R, S> {
    repo: R,
    sender: S,
}

impl<R, S> CancelOrderUseCase<R, S> {
    pub fn new(repo: R, sender: S) -> Self {
        Self { repo, sender }
    }
}

impl<R, S> CancelOrderUseCase<R, S>
where
    R: MarketReadRepository,
    S: ChainOrderSender,
{
    pub async fn execute(&self, input: CancelOrderInput) -> Result<CancelledOrder, DomainError> {
        let OrderForCancel { event_id, oracle_list_hash, token_type, market_status, client_order_id } =
            self.repo
                .resolve_for_cancel(
                    &input.market_address,
                    &input.symbol,
                    input.order_id,
                    &input.trading_pn.pn_address,
                    input.now_seconds,
                )
                .await
                .map_err(|err| {
                    if let Some(domain) = err.downcast_ref::<DomainError>() {
                        return *domain;
                    }
                    error!(?err, market_address = %input.market_address.0, "resolve_for_cancel failed (non-domain)");
                    DomainError::Unexpected
                })?;

        if market_status != MarketStatus::Trading {
            return Err(DomainError::OrderValidationFailed);
        }

        // Same fail-closed invariant as POST: a reconciled market is
        // expected to carry `oracle_list_hash`; the chain ABI requires
        // it. Blank means the read-model is internally inconsistent —
        // 503 lets the indexer catch up rather than pushing a
        // zero-hash submission.
        if oracle_list_hash.is_empty() {
            return Err(DomainError::MarketInconsistent);
        }

        let token_type = u32::try_from(token_type).map_err(|_| DomainError::MarketInconsistent)?;

        let payload = CancelOrderPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            order_id: input.order_id,
        };
        self.sender.cancel_order(payload).await?;

        Ok(CancelledOrder { client_order_id })
    }
}

/// Orchestrates `POST /api/v1/batchOrders`: resolves market+outcome
/// once, validates the request shape (non-empty,
/// `len ≤ outcome.max_batch_size`), runs the same per-item validation
/// chain `POST /api/v1/order` uses (any failure rejects the whole
/// batch), and dispatches a single `PrivateNote.placeBatch` call. The
/// chain itself enforces all-or-nothing — if any item fails on-chain
/// the entire `placeBatch` reverts.
pub struct CreateBatchOrdersUseCase<R, S> {
    repo: R,
    sender: S,
}

impl<R, S> CreateBatchOrdersUseCase<R, S> {
    pub fn new(repo: R, sender: S) -> Self {
        Self { repo, sender }
    }
}

impl<R, S> CreateBatchOrdersUseCase<R, S>
where
    R: MarketReadRepository,
    S: ChainOrderSender,
{
    pub async fn execute(
        &self,
        input: CreateBatchOrdersInput,
    ) -> Result<SubmittedBatchOrders, DomainError> {
        // Empty batch is a client-shape error. The chain enforces the
        // same (162 `ERR_EMPTY_BATCH`) but failing fast saves a round-trip
        // and avoids needlessly contending for the per-PN `_busy` lock.
        // `phase = "shape"` + `orders_len = 0` lets ops query the single
        // substring `batchOrders rejected` and disambiguate this empty
        // case from the symmetric oversize reject below — both map to
        // -1130 on the wire.
        if input.orders.is_empty() {
            warn!(phase = "shape", orders_len = 0, "batchOrders rejected");
            return Err(DomainError::InvalidParameter);
        }

        let MarketForPlacement { event_id, oracle_list_hash, token_type, status, outcome } = self
            .repo
            .resolve_for_new_order(&input.market_address, &input.symbol, input.now_seconds)
            .await
            .map_err(|err| {
                if let Some(domain) = err.downcast_ref::<DomainError>() {
                    return *domain;
                }
                error!(
                    ?err,
                    market_address = %input.market_address.0,
                    "resolve_for_new_order failed (non-domain)",
                );
                DomainError::Unexpected
            })?;

        if status != MarketStatus::Trading {
            return Err(DomainError::OrderValidationFailed);
        }
        if oracle_list_hash.is_empty() {
            return Err(DomainError::MarketInconsistent);
        }
        // Per-outcome cap. Authoritative source is `/api/v1/markets`
        // (`outcome.max_batch_size`); the chain enforces the same
        // (161 `ERR_BATCH_TOO_LARGE`). Reject locally so a misbehaving
        // client gets `-1130 / 400` instead of paying a chain
        // round-trip on a doomed batch.
        if input.orders.len() > outcome.max_batch_size as usize {
            warn!(
                phase = "shape",
                orders_len = input.orders.len(),
                max_batch_size = outcome.max_batch_size,
                "batchOrders rejected",
            );
            return Err(DomainError::InvalidParameter);
        }

        let token_type = u32::try_from(token_type).map_err(|_| DomainError::MarketInconsistent)?;

        let mut encoded_orders = Vec::with_capacity(input.orders.len());
        let mut submitted_items = Vec::with_capacity(input.orders.len());
        for (item_index, item) in input.orders.into_iter().enumerate() {
            // First per-item failure short-circuits the whole batch —
            // matches the chain's atomic placeBatch semantics. The
            // caller can re-submit with the offending entry removed or
            // corrected. `item_index` is logged so ops can correlate
            // the per-item `DomainError` (whatever code it maps to)
            // against the offending position; the wire shape stays
            // one error object per api-spec.
            let encoded = validate_and_encode_order_item(
                item.side,
                &item.quantity,
                item.price.as_deref(),
                item.order_type,
                item.time_in_force,
                item.client_order_id.as_deref(),
                &outcome,
            )
            .inspect_err(|err| {
                warn!(phase = "validate", item_index, ?err, "batchOrders rejected");
            })?;
            submitted_items
                .push(SubmittedOrder { client_order_id: encoded.client_order_id.clone() });
            encoded_orders.push(encoded);
        }

        let payload = NewBatchOrderPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            orders: encoded_orders,
        };
        self.sender.submit_batch_order(payload).await?;

        Ok(SubmittedBatchOrders { items: submitted_items })
    }
}

/// Inputs to `GetOrdersUseCase::execute`, mirroring the shape of
/// [`NewOrderInput`] for symmetry across read/write use cases. The
/// HTTP handler is the only intended constructor: it owns the
/// `AuthContext` and passes a clone of `ctx.trading_pn.pn_address`
/// here. The CSV `status` / `cursor` strings are raw request values;
/// validation happens inside `execute`.
pub struct GetOrdersInput {
    pub owner_pn_address: String,
    pub market_filter: Option<OrdersMarketFilter>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

pub struct GetOrdersUseCase<R> {
    repo: R,
}

impl<R> GetOrdersUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetOrdersUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(&self, input: GetOrdersInput) -> Result<OrdersPage, anyhow::Error> {
        let status = OrderStatusFilter::from_csv(input.status.as_deref())
            .map_err(|err| anyhow::anyhow!(err))?;

        let limit = match input.limit {
            None => OrdersLimit::DEFAULT,
            Some(v) => {
                // u16::try_from rejects negative and over-u16 inputs;
                // OrdersLimit::new then enforces the [1, MAX] bound. Both
                // failure modes collapse to `MissingParameter` (the public
                // `-1102` error per api-spec.md).
                let raw =
                    u16::try_from(v).map_err(|_| anyhow::anyhow!(DomainError::MissingParameter))?;
                OrdersLimit::new(raw).map_err(|err| anyhow::anyhow!(err))?
            }
        };

        let cursor = match input.cursor {
            None => None,
            Some(raw) => Some(OrdersCursor::new(raw).map_err(|err| anyhow::anyhow!(err))?),
        };

        self.repo
            .list_orders(&OrdersQuery {
                owner_pn_address: input.owner_pn_address,
                market: input.market_filter,
                status,
                limit,
                cursor,
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_with(perms: Vec<Permission>) -> AuthContext {
        AuthContext {
            account_id: Uuid::nil(),
            api_key_id: 0,
            trading_pn: TradingPn {
                pn_address: "0:test".into(),
                pn_pubkey: "0".into(),
                pn_dih: "0".into(),
                pn_seckey: SensitiveBytes::new(vec![]),
            },
            permissions: perms,
        }
    }

    #[test]
    fn require_grants_when_present() {
        let ctx = context_with(vec![Permission::UserData, Permission::Trade]);
        assert!(ctx.require(Permission::UserData).is_ok());
        assert!(ctx.require(Permission::Trade).is_ok());
    }

    #[test]
    fn require_rejects_when_absent() {
        let ctx = context_with(vec![Permission::UserData]);
        let err = ctx.require(Permission::Trade).unwrap_err();
        assert_eq!(err, DomainError::AuthRequired);
    }

    #[test]
    fn require_rejects_when_empty() {
        // A key issued with no permissions should fail every check — even
        // USER_DATA. This protects /account/ endpoints from a misconfigured
        // empty-permission key being silently allowed.
        let ctx = context_with(vec![]);
        assert!(ctx.require(Permission::UserData).is_err());
        assert!(ctx.require(Permission::Trade).is_err());
    }

    // ---- CreateOrderUseCase ----

    use std::sync::Mutex;

    use dodex_domain::Market;
    use dodex_domain::MarketEvent;
    use dodex_domain::MarketName;
    use dodex_domain::Outcome;
    use dodex_domain::FLAG_MARKET;

    #[derive(Clone)]
    struct FakeCancelableOrder {
        order_id: u64,
        owner_pn_address: String,
        client_order_id: Option<String>,
    }

    struct FakeRepo {
        market: Option<Market>,
        cancelable_order: Option<FakeCancelableOrder>,
        orders_response: OrdersPage,
        recorded_orders_queries: Mutex<Vec<OrdersQuery>>,
    }

    fn empty_orders_page() -> OrdersPage {
        OrdersPage { orders: vec![], next_cursor: None }
    }

    impl FakeRepo {
        fn with(market: Market) -> Self {
            Self {
                market: Some(market),
                cancelable_order: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn empty() -> Self {
            Self {
                market: None,
                cancelable_order: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn with_cancelable_order(market: Market, order: FakeCancelableOrder) -> Self {
            Self {
                market: Some(market),
                cancelable_order: Some(order),
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn recorded_orders_queries(&self) -> Vec<OrdersQuery> {
            self.recorded_orders_queries.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MarketReadRepository for FakeRepo {
        async fn list_markets(&self, _: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
            Ok(MarketsPage {
                markets: self.market.clone().into_iter().collect(),
                next_cursor: None,
                has_more: false,
            })
        }

        async fn get_depth(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: u16,
        ) -> Result<DepthSnapshot, anyhow::Error> {
            unimplemented!("get_depth is not exercised by the order use case")
        }

        async fn resolve_for_new_order(
            &self,
            _: &MarketAddress,
            symbol: &Symbol,
            _: i64,
        ) -> Result<MarketForPlacement, anyhow::Error> {
            // Tests construct a fully-populated `Market` and let this
            // adapter project it down to the slim shape the use case
            // actually consumes. Both miss paths (no market, no symbol
            // within market) collapse to `InvalidMarketOrSymbol` the
            // same way the Postgres impl does.
            let Some(market) = self.market.clone() else {
                return Err(anyhow::anyhow!(DomainError::InvalidMarketOrSymbol));
            };
            let outcome = market
                .outcomes
                .iter()
                .find(|o| o.symbol == *symbol)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!(DomainError::InvalidMarketOrSymbol))?;
            Ok(MarketForPlacement {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type: market.token_type,
                status: market.status,
                outcome,
            })
        }

        async fn resolve_for_cancel(
            &self,
            _: &MarketAddress,
            symbol: &Symbol,
            order_id: u64,
            owner_pn_address: &str,
            _: i64,
        ) -> Result<OrderForCancel, anyhow::Error> {
            // Same shape as the Postgres impl: any predicate miss
            // (market/symbol/order/owner) collapses to `UnknownOrder`
            // so callers cannot distinguish "wrong owner" from
            // "no such order" through error codes.
            let unknown = || anyhow::anyhow!(DomainError::UnknownOrder);
            let market = self.market.clone().ok_or_else(unknown)?;
            if !market.outcomes.iter().any(|o| o.symbol == *symbol) {
                return Err(unknown());
            }
            let order = self.cancelable_order.clone().ok_or_else(unknown)?;
            if order.order_id != order_id || order.owner_pn_address != owner_pn_address {
                return Err(unknown());
            }
            Ok(OrderForCancel {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type: market.token_type,
                market_status: market.status,
                client_order_id: order.client_order_id,
            })
        }

        async fn list_orders(&self, query: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
            self.recorded_orders_queries.lock().unwrap().push(query.clone());
            Ok(self.orders_response.clone())
        }

        async fn resolve_market_for_balances(
            &self,
            _: &MarketAddress,
        ) -> Result<MarketBalancesResolution, anyhow::Error> {
            unimplemented!("resolve_market_for_balances not exercised by FakeRepo")
        }

        async fn sum_open_sell_remaining(
            &self,
            _: &str,
            _: &str,
        ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
            unimplemented!("sum_open_sell_remaining not exercised by FakeRepo")
        }
    }

    struct FakeSender {
        recorded: Mutex<Vec<NewOrderPayload>>,
        recorded_cancels: Mutex<Vec<CancelOrderPayload>>,
        recorded_batches: Mutex<Vec<NewBatchOrderPayload>>,
        fail_with: Option<DomainError>,
    }

    impl FakeSender {
        fn ok() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                recorded_batches: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(err: DomainError) -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                recorded_batches: Mutex::new(Vec::new()),
                fail_with: Some(err),
            }
        }

        fn calls(&self) -> Vec<NewOrderPayload> {
            self.recorded.lock().unwrap().clone()
        }

        fn cancel_calls(&self) -> Vec<CancelOrderPayload> {
            self.recorded_cancels.lock().unwrap().clone()
        }

        fn batch_calls(&self) -> Vec<NewBatchOrderPayload> {
            self.recorded_batches.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChainOrderSender for FakeSender {
        async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded.lock().unwrap().push(payload);
            Ok(())
        }

        async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded_cancels.lock().unwrap().push(payload);
            Ok(())
        }

        async fn submit_batch_order(
            &self,
            payload: NewBatchOrderPayload,
        ) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded_batches.lock().unwrap().push(payload);
            Ok(())
        }
    }

    fn test_outcome(symbol: &str) -> Outcome {
        Outcome {
            outcome_id: 1,
            outcome_name: "YES".into(),
            symbol: Symbol(symbol.into()),
            price_precision: 3,
            quantity_precision: 6,
            tick_size: "0.001".into(),
            step_size: "0.000001".into(),
            // 0.5 not 1: the base test scenario uses price=0.615,
            // quantity=1.5 with notional 0.9225, so a 1.0 threshold
            // would make every base case fail spuriously on notional.
            // Tests that exercise the notional rule override this.
            min_notional: "0.5".into(),
            max_batch_size: 5,
        }
    }

    fn trading_market(symbol: &str) -> Market {
        Market {
            market_address: MarketAddress("0:market".into()),
            order_book_address: "0:ob".into(),
            oracle_list_hash: "0xdead".into(),
            market_name: MarketName("PM".into()),
            status: MarketStatus::Trading,
            quote_asset: "NACKL".into(),
            token_type: 1,
            created_at: 0,
            timings: None,
            event: MarketEvent {
                event_id: "0xevent".into(),
                event_name: None,
                description: None,
                oracles: vec![],
            },
            terminal: None,
            outcomes: vec![test_outcome(symbol)],
        }
    }

    fn base_input(symbol: &str) -> NewOrderInput {
        NewOrderInput {
            trading_pn: TradingPn {
                pn_address: "0:pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            market_address: MarketAddress("0:market".into()),
            symbol: Symbol(symbol.into()),
            side: OrderSide::Buy,
            quantity: "1.5".into(),
            price: Some("0.615".into()),
            order_type: OrderType::Limit,
            time_in_force: Some(TimeInForce::Gtc),
            client_order_id: Some("42".into()),
            now_seconds: 1_000,
            now_ms: 1_000_000,
        }
    }

    #[tokio::test]
    async fn create_order_happy_path_buy_limit_gtc() {
        let market = trading_market("PM-YES");
        let repo = FakeRepo::with(market);
        let sender = FakeSender::ok();
        let uc = CreateOrderUseCase::new(repo, sender);

        let out = uc.execute(base_input("PM-YES")).await.expect("happy path");

        // The use case contributes one thing the handler does not
        // already have: the resolved `clientOrderId`. Sender-payload
        // assertions live in the next test
        // (`create_order_sender_payload_matches_request`) which owns
        // a concrete `Arc<FakeSender>` reference for inspection.
        assert_eq!(out.client_order_id, "42");
    }

    #[tokio::test]
    async fn create_order_sender_payload_matches_request() {
        // Captures the on-chain payload shape the use case constructs.
        // A regression here would mis-bind fields between the API
        // request and `ParamsOfPlaceOrder` — silent corruption that
        // unit tests are the only line of defence against.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender.clone());

        uc.execute(base_input("PM-YES")).await.unwrap();

        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        let p = &calls[0];
        assert_eq!(p.pn_address, "0:pn");
        assert_eq!(p.pn_pubkey, "1");
        assert_eq!(p.event_id, "0xevent");
        assert_eq!(p.oracle_list_hash, "0xdead");
        assert_eq!(p.token_type, 1);
        assert_eq!(p.outcome_id, 1);
        assert!(p.is_buy);
        // 0.615 lifted by price_precision=3 -> 615
        assert_eq!(p.price_raw, "615");
        // 1.5 lifted by quantity_precision=6 -> 1_500_000
        assert_eq!(p.amount_raw, "1500000");
        // LIMIT + GTC = flags 0x00
        assert_eq!(p.flags, 0);
        assert_eq!(p.client_order_id, "42");
    }

    #[tokio::test]
    async fn create_order_market_not_found() {
        let uc = CreateOrderUseCase::new(FakeRepo::empty(), FakeSender::ok());
        let err = uc.execute(base_input("PM-YES")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidMarketOrSymbol);
    }

    #[tokio::test]
    async fn create_order_symbol_not_found_in_market() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.symbol = Symbol("PM-NOPE".into());
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidMarketOrSymbol);
    }

    #[tokio::test]
    async fn create_order_rejects_non_trading_status() {
        let mut market = trading_market("PM-YES");
        market.status = MarketStatus::Resolving;
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err = uc.execute(base_input("PM-YES")).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn create_order_limit_requires_price() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.price = None;
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[tokio::test]
    async fn create_order_market_rejects_explicit_price() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.order_type = OrderType::Market;
        input.time_in_force = None;
        // price still set → invalid combination
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[tokio::test]
    async fn create_order_rejects_excess_price_precision() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.price = Some("0.6155".into()); // 4 dp > pricePrecision=3
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::PrecisionExceeded);
    }

    #[tokio::test]
    async fn create_order_rejects_non_tick_multiple() {
        // tick_size = 0.001; price 0.6151 is finer than the lattice (would
        // need tickSize=0.0001 to be valid). But 0.6151 has 4 dp > 3, so
        // it'd fail precision first. Use a precision-matching but
        // non-multiple value: tick = 0.003 and price = 0.001 — change the
        // outcome tick to 0.003.
        let mut market = trading_market("PM-YES");
        market.outcomes[0].tick_size = "0.003".into();
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.price = Some("0.001".into()); // 0.001 is not a multiple of 0.003
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::PrecisionExceeded);
    }

    #[tokio::test]
    async fn create_order_rejects_below_min_notional() {
        let mut market = trading_market("PM-YES");
        market.outcomes[0].min_notional = "100".into(); // notional below price*qty=0.9225
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err = uc.execute(base_input("PM-YES")).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn create_order_generates_client_order_id_when_absent() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.client_order_id = None;
        let out = uc.execute(input).await.unwrap();
        // 128-bit value rendered in decimal — non-empty, all digits, and
        // not the test-fixture's literal "42".
        assert!(!out.client_order_id.is_empty());
        assert!(out.client_order_id.chars().all(|c| c.is_ascii_digit()));
        assert_ne!(out.client_order_id, "42");
    }

    #[tokio::test]
    async fn create_order_propagates_sender_transport_failure() {
        let market = trading_market("PM-YES");
        let sender = FakeSender::failing(DomainError::Unexpected);
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender);
        let err = uc.execute(base_input("PM-YES")).await.unwrap_err();
        assert_eq!(err, DomainError::Unexpected);
    }

    #[tokio::test]
    async fn create_order_rejects_market_with_empty_oracle_list_hash() {
        // A reconciled market whose `oracle_list_hash` is missing
        // breaks `placeOrder` on chain (it would send an invalid PMP
        // key). The read endpoints stay available for that market
        // (they don't surface the field), but the trading path must
        // fail closed before submitting.
        let mut market = trading_market("PM-YES");
        market.oracle_list_hash = String::new();
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err = uc.execute(base_input("PM-YES")).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[tokio::test]
    async fn create_order_rejects_client_order_id_overflowing_u64() {
        // The chain ABI is `uint128`, but the serialization path
        // (`bee_dex` → `ackinacki-kit` → `serde_json::json!` without
        // arbitrary_precision) rejects `u128 > u64::MAX` with a panic.
        // Until the SDK supports arbitrary precision, the public
        // surface is bounded at u64. A caller who supplies
        // `u64::MAX + 1` must surface as -1130 / 400 — not as the
        // -1000 / 500 the worker panic would otherwise produce.
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        // u64::MAX + 1 = 18_446_744_073_709_551_616
        input.client_order_id = Some("18446744073709551616".into());
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[tokio::test]
    async fn create_order_rejects_non_numeric_client_order_id() {
        let market = trading_market("PM-YES");
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let mut input = base_input("PM-YES");
        input.client_order_id = Some("not-a-number".into());
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[tokio::test]
    async fn create_order_market_sell_rejects_zero_quantity() {
        // Regression: MARKET SELL skips the notional check that
        // implicitly catches qty=0 on LIMIT / MARKET BUY, so without
        // the explicit `amount_lifted > 0` gate this would reach the
        // chain sender and pay an `ERR_LOW_VALUE` round-trip (plus
        // contention with the per-PN `_busy` lock).
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender.clone());
        let mut input = base_input("PM-YES");
        input.order_type = OrderType::Market;
        input.side = OrderSide::Sell;
        input.time_in_force = None;
        input.price = None;
        input.quantity = "0".into();
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        // The sender MUST NOT have been touched — gate is upstream.
        assert!(sender.calls().is_empty(), "chain sender hit despite zero-qty reject");
    }

    #[tokio::test]
    async fn create_order_limit_rejects_zero_quantity() {
        // Symmetric pin: LIMIT qty=0 already failed historically via
        // the notional check (0 * price < min_notional). With the new
        // explicit gate, the result is the same shape but the failure
        // happens earlier in the validation chain. Lock the outcome
        // so a future refactor that reorders or weakens either gate
        // can't silently let zero-qty through.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender.clone());
        let mut input = base_input("PM-YES");
        input.quantity = "0".into();
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn create_order_rejects_quantity_exceeding_u64() {
        // Regression: the *effective* ceiling on `amount` is
        // `u64::MAX`, not `u128::MAX`, because the upstream
        // `serde_json::json!` path in `ackinacki-kit` panics above
        // u64 (same SDK constraint that bounds `clientOrderId` —
        // see write-api.md §clientOrderId generation). Pin a value
        // strictly inside the (u64::MAX, u128::MAX) gap so a future
        // relaxation of the gate to u128 would re-open the 500 path
        // and trip this test.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender.clone());
        let mut input = base_input("PM-YES");
        // u64::MAX = 18_446_744_073_709_551_615. After lift by
        // quantity_precision=6, an input quantity of
        // "18446744073709.551616" lifts to u64::MAX + 1 — fits in
        // u128 (sender would not 500), but the SDK ceiling rejects.
        input.quantity = "18446744073709.551616".into();
        // Strip price to a small value so the LIMIT notional check
        // does not short-circuit the amount gate first.
        input.price = Some("0.001".into());
        let err = uc.execute(input).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.calls().is_empty(), "chain sender hit despite over-ceiling qty");
    }

    #[tokio::test]
    async fn create_order_accepts_quantity_at_u64_max() {
        // Boundary pin counterpart: a quantity whose lifted value is
        // exactly `u64::MAX` must still pass the gate. Catches a
        // future off-by-one (e.g. `>=` instead of `>` on the
        // comparison) that would reject the boundary value.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateOrderUseCase::new(FakeRepo::with(market), sender.clone());
        let mut input = base_input("PM-YES");
        // u64::MAX = 18_446_744_073_709_551_615.
        input.quantity = "18446744073709.551615".into();
        input.price = Some("0.001".into());
        uc.execute(input).await.expect("boundary qty must pass");
        assert_eq!(sender.calls().len(), 1);
        assert_eq!(sender.calls()[0].amount_raw, u64::MAX.to_string());
    }

    #[test]
    fn status_filter_parses_csv_and_dedups() {
        let filter =
            OrderStatusFilter::from_csv(Some("NEW, FILLED ,NEW, CANCELED")).expect("valid CSV");
        let OrderStatusFilter::Only(set) = filter else {
            panic!("expected Only, got All");
        };
        let canonical: Vec<_> = set.iter().copied().collect();
        assert_eq!(
            canonical,
            vec![
                QueryableOrderStatus::New,
                QueryableOrderStatus::Filled,
                QueryableOrderStatus::Canceled,
            ]
        );
    }

    #[test]
    fn status_filter_iteration_orders_all_public_read_statuses_exhaustively() {
        fn permutations(values: Vec<&'static str>) -> Vec<Vec<&'static str>> {
            if values.is_empty() {
                return vec![Vec::new()];
            }
            let mut out = Vec::new();
            for (idx, value) in values.iter().enumerate() {
                let mut rest = values.clone();
                rest.remove(idx);
                for mut tail in permutations(rest) {
                    let mut next = vec![*value];
                    next.append(&mut tail);
                    out.push(next);
                }
            }
            out
        }

        let statuses = ["REJECTED", "CANCELED", "FILLED", "PARTIALLY_FILLED", "NEW"];
        for permutation in permutations(statuses.to_vec()) {
            let csv = permutation.join(",");
            let filter = OrderStatusFilter::from_csv(Some(&csv)).expect("valid exhaustive CSV");
            let OrderStatusFilter::Only(set) = filter else {
                panic!("expected Only for exhaustive CSV {csv}");
            };
            let canonical: Vec<_> = set.iter().copied().collect();
            assert_eq!(
                canonical,
                vec![
                    QueryableOrderStatus::New,
                    QueryableOrderStatus::PartiallyFilled,
                    QueryableOrderStatus::Filled,
                    QueryableOrderStatus::Canceled,
                    QueryableOrderStatus::Rejected,
                ],
                "canonical order changed for input {csv}"
            );
        }
    }

    /// `(empty orders, Some(cursor))` is the corrupt-window page —
    /// every row in a `has_more=true` page was filtered by
    /// `order_from_row`. The cursor still advances past the dropped
    /// rows so the client can paginate through. A previous version
    /// of this codebase rejected this state as `Unexpected` and
    /// stranded the client at 500; this test pins that the shape
    /// constructs cleanly.
    #[test]
    fn orders_page_allows_empty_with_cursor_for_corrupt_window() {
        let cursor = OrdersCursor::new("token".into()).expect("valid cursor");
        let page = OrdersPage { orders: vec![], next_cursor: Some(cursor.clone()) };
        assert!(page.orders.is_empty());
        assert_eq!(page.next_cursor, Some(cursor));
    }

    #[test]
    fn status_filter_treats_absent_and_empty_as_all() {
        assert_eq!(OrderStatusFilter::from_csv(None).expect("absent"), OrderStatusFilter::All);
        assert_eq!(
            OrderStatusFilter::from_csv(Some("   ")).expect("blank"),
            OrderStatusFilter::All,
        );
    }

    #[test]
    fn orders_market_filter_pair_rejects_half_filters() {
        assert!(OrdersMarketFilter::pair(None, None).expect("absent pair").is_none());

        assert_eq!(
            OrdersMarketFilter::pair(Some(MarketAddress("0:market".into())), None)
                .expect_err("market without symbol rejected"),
            DomainError::MissingParameter
        );
        assert_eq!(
            OrdersMarketFilter::pair(None, Some(Symbol("YES".into())))
                .expect_err("symbol without market rejected"),
            DomainError::MissingParameter
        );

        let filter = OrdersMarketFilter::pair(
            Some(MarketAddress("0:market".into())),
            Some(Symbol("YES".into())),
        )
        .expect("complete pair accepted")
        .expect("filter present");
        assert_eq!(filter.market_address().0, "0:market");
        assert_eq!(filter.symbol().0, "YES");
    }

    #[test]
    fn status_filter_rejects_unknown_token() {
        let err = OrderStatusFilter::from_csv(Some("NEW,SUPER_FILLED")).expect_err("unknown token");
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[test]
    fn orders_cursor_trims_surrounding_whitespace() {
        // The cursor is server-issued (`placed_chain_order` of the last
        // row of the previous page) and round-trips via the client. We
        // trim so a client that re-emits the value with stray padding
        // is forgiven; after trimming, the value must match a server-issued
        // token or the strict `<` predicate in `list_orders` advances the
        // cursor to the wrong row and pagination silently breaks.
        let cursor = OrdersCursor::new("  003  ".into()).expect("trims and accepts");
        assert_eq!(cursor.as_str(), "003");
    }

    #[test]
    fn orders_cursor_rejects_blank_db_token_as_unexpected() {
        assert_eq!(
            OrdersCursor::from_db_token("   ".into()).expect_err("blank DB token rejected"),
            DomainError::Unexpected
        );
    }

    #[test]
    fn orders_cursor_rejects_blank() {
        assert_eq!(
            OrdersCursor::new("   ".into()).expect_err("blank rejected"),
            DomainError::MissingParameter
        );
        assert_eq!(
            OrdersCursor::new(String::new()).expect_err("empty rejected"),
            DomainError::MissingParameter
        );
    }

    /// A client could otherwise post a 10 MB `?cursor=…` and the value
    /// would bind as `$cursor::text` for the `placed_chain_order < $cursor`
    /// comparison, costing per-row CPU at the Postgres side.
    /// `MAX_CURSOR_LEN` is the read-path defence.
    #[test]
    fn orders_cursor_rejects_oversized_client_input() {
        let over = "A".repeat(MAX_CURSOR_LEN + 1);
        assert_eq!(
            OrdersCursor::new(over).expect_err("oversized client cursor rejected"),
            DomainError::InvalidParameter,
        );
        let at_cap = "A".repeat(MAX_CURSOR_LEN);
        OrdersCursor::new(at_cap).expect("exactly MAX_CURSOR_LEN is accepted");
    }

    /// Symmetric guard at the storage boundary: a corrupt
    /// `placed_chain_order` cell with an oversized value would
    /// otherwise resurface on the next page as a hostile-shaped cursor
    /// the client never typed.
    #[test]
    fn orders_cursor_rejects_oversized_db_token_as_unexpected() {
        let over = "A".repeat(MAX_CURSOR_LEN + 1);
        assert_eq!(
            OrdersCursor::from_db_token(over).expect_err("oversized DB token rejected"),
            DomainError::Unexpected,
        );
    }

    #[test]
    fn status_filter_rejects_pending_states() {
        // PendingNew and PendingCancel are write-side synthetic statuses
        // and must not be accepted as a /orders filter — neither appears
        // on a live_orders row.
        let err =
            OrderStatusFilter::from_csv(Some("PENDING_NEW")).expect_err("pending_new rejected");
        assert_eq!(err, DomainError::InvalidParameter);
        let err = OrderStatusFilter::from_csv(Some("PENDING_CANCEL"))
            .expect_err("pending_cancel rejected");
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[test]
    fn generated_client_order_id_fits_in_u64() {
        // The generator MUST stay inside u64: `bee_dex` / `serde_json`
        // panic on serialize for values above u64::MAX, so a
        // `Uuid::new_v4().as_u128()` regression would crash the worker
        // ~50 % of the time. 256 samples is more than enough to
        // surface that regression.
        for _ in 0..256 {
            let coid = generate_client_order_id();
            assert!(
                coid.parse::<u64>().is_ok(),
                "generated coid {coid:?} does not fit in u64 — would panic in bee_dex::Dex::place_order",
            );
        }
    }

    // ---- CancelOrderUseCase ----

    fn base_cancel_input(symbol: &str, order_id: u64) -> CancelOrderInput {
        CancelOrderInput {
            trading_pn: TradingPn {
                pn_address: "0:pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            market_address: MarketAddress("0:market".into()),
            symbol: Symbol(symbol.into()),
            order_id,
            now_seconds: 1_000,
            now_ms: 1_000_000,
        }
    }

    #[tokio::test]
    async fn cancel_order_happy_path() {
        let market = trading_market("PM-YES");
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: Some("42".into()),
        };
        let sender = Arc::new(FakeSender::ok());
        let uc =
            CancelOrderUseCase::new(FakeRepo::with_cancelable_order(market, order), sender.clone());

        let out = uc.execute(base_cancel_input("PM-YES", 123)).await.expect("happy path");
        assert_eq!(out.client_order_id, Some("42".into()));

        let calls = sender.cancel_calls();
        assert_eq!(calls.len(), 1);
        let p = &calls[0];
        assert_eq!(p.pn_address, "0:pn");
        assert_eq!(p.event_id, "0xevent");
        assert_eq!(p.oracle_list_hash, "0xdead");
        assert_eq!(p.token_type, 1);
        assert_eq!(p.order_id, 123);
    }

    #[tokio::test]
    async fn cancel_order_echoes_empty_client_order_id_when_absent() {
        // Orders placed without `newOrderClientId` come back as
        // `client_order_id: None`; the use case must propagate the
        // absence rather than fabricating a value, so the HTTP layer
        // can render the spec-mandated empty string.
        let market = trading_market("PM-YES");
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            Arc::new(FakeSender::ok()),
        );

        let out = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap();
        assert_eq!(out.client_order_id, None);
    }

    #[tokio::test]
    async fn cancel_order_unknown_when_market_missing() {
        let uc = CancelOrderUseCase::new(FakeRepo::empty(), FakeSender::ok());
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
    }

    #[tokio::test]
    async fn cancel_order_unknown_when_symbol_mismatch() {
        let market = trading_market("PM-YES");
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            FakeSender::ok(),
        );
        let err = uc.execute(base_cancel_input("PM-NOPE", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
    }

    #[tokio::test]
    async fn cancel_order_unknown_when_order_missing() {
        // Market resolves, but no live_orders row for the caller —
        // surfaces as UnknownOrder, never as MissingParameter.
        let market = trading_market("PM-YES");
        let uc = CancelOrderUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
    }

    #[tokio::test]
    async fn cancel_order_unknown_when_owner_mismatch() {
        // Wrong-owner case MUST NOT differ from "no such order" — the
        // existence of another account's order would otherwise leak
        // through the error code.
        let market = trading_market("PM-YES");
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:someone-else".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            FakeSender::ok(),
        );
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
    }

    #[tokio::test]
    async fn cancel_order_rejects_non_trading_status() {
        let mut market = trading_market("PM-YES");
        market.status = MarketStatus::Resolving;
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            FakeSender::ok(),
        );
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn cancel_order_rejects_blank_oracle_list_hash() {
        let mut market = trading_market("PM-YES");
        market.oracle_list_hash = String::new();
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            FakeSender::ok(),
        );
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[tokio::test]
    async fn cancel_order_propagates_sender_pn_busy() {
        let market = trading_market("PM-YES");
        let order = FakeCancelableOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: None,
        };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_cancelable_order(market, order),
            FakeSender::failing(DomainError::OrderPnBusy),
        );
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::OrderPnBusy);
    }

    fn base_get_orders_input() -> GetOrdersInput {
        GetOrdersInput {
            owner_pn_address: "0:pn".into(),
            market_filter: None,
            status: None,
            limit: None,
            cursor: None,
        }
    }

    /// Pin the wiring from `GetOrdersInput` to `OrdersQuery`: every
    /// caller-supplied field (owner, market_filter, status CSV, limit,
    /// cursor) must reach the repository unchanged after parsing. A
    /// regression that drops one — e.g. forgetting to forward
    /// `input.market_filter` into `OrdersQuery::market` — would silently
    /// widen the result set, and the HTTP suite is the only thing that
    /// catches it today.
    #[tokio::test]
    async fn get_orders_propagates_inputs_to_repo_query() {
        let filter = OrdersMarketFilter::pair(
            Some(MarketAddress("0:market".into())),
            Some(Symbol("PM-YES".into())),
        )
        .expect("valid pair")
        .expect("non-empty pair");

        let repo = Arc::new(FakeRepo::empty());
        let uc = GetOrdersUseCase::new(repo.clone());
        uc.execute(GetOrdersInput {
            owner_pn_address: "0:owner".into(),
            market_filter: Some(filter.clone()),
            status: Some("NEW,FILLED".into()),
            limit: Some(50),
            cursor: Some("  abc123  ".into()),
        })
        .await
        .expect("ok");

        let queries = repo.recorded_orders_queries();
        assert_eq!(queries.len(), 1, "list_orders called exactly once");
        let q = &queries[0];
        assert_eq!(q.owner_pn_address, "0:owner");
        let captured = q.market.as_ref().expect("market filter forwarded");
        assert_eq!(captured.market_address().0, filter.market_address().0);
        assert_eq!(captured.symbol().0, filter.symbol().0);
        assert_eq!(
            q.status,
            OrderStatusFilter::from_csv(Some("NEW,FILLED")).expect("valid status CSV"),
            "status CSV must be parsed and forwarded"
        );
        assert_eq!(q.limit, OrdersLimit::from_const(50));
        // OrdersCursor::new trims surrounding whitespace; the trimmed
        // value must reach the repo verbatim.
        assert_eq!(q.cursor.as_ref().expect("cursor forwarded").as_str(), "abc123");
    }

    /// Absent `limit` must collapse to `OrdersLimit::DEFAULT`. Combined
    /// with the type-level `1..=ORDERS_MAX_LIMIT` invariant on
    /// `OrdersLimit`, this is what guarantees the Postgres cursor
    /// builder's `last() == Some` after `truncate(limit)`.
    #[tokio::test]
    async fn get_orders_defaults_limit_when_absent() {
        let repo = Arc::new(FakeRepo::empty());
        let uc = GetOrdersUseCase::new(repo.clone());
        uc.execute(base_get_orders_input()).await.expect("ok");

        let q = repo.recorded_orders_queries().pop().expect("one call");
        assert_eq!(q.limit, OrdersLimit::DEFAULT);
    }

    /// Out-of-range `limit` (0, > `ORDERS_MAX_LIMIT`, negative) must trip
    /// `MissingParameter` before the repository is touched. The type-level
    /// invariant on `OrdersLimit` makes the panic at
    /// `expect("has_more implies non-empty page")` unreachable by
    /// construction; this test pins the wiring that maps a non-HTTP
    /// caller's bad input to `MissingParameter` instead of letting it
    /// reach the repository.
    #[tokio::test]
    async fn get_orders_rejects_limit_out_of_range() {
        for bad_limit in [0_i64, -1, i64::from(ORDERS_MAX_LIMIT) + 1, i64::MAX] {
            let repo = Arc::new(FakeRepo::empty());
            let uc = GetOrdersUseCase::new(repo.clone());
            let err = uc
                .execute(GetOrdersInput { limit: Some(bad_limit), ..base_get_orders_input() })
                .await
                .expect_err("out-of-range limit must fail");
            let domain = err
                .downcast_ref::<DomainError>()
                .expect("out-of-range limit surfaces as typed DomainError");
            assert_eq!(*domain, DomainError::MissingParameter, "limit={bad_limit}");
            assert!(
                repo.recorded_orders_queries().is_empty(),
                "repo must not be touched for invalid limit={bad_limit}",
            );
        }
    }

    /// Blank / whitespace-only `cursor` must trip `MissingParameter`
    /// before the repository is touched. Mirrors the strict contract on
    /// `OrdersCursor::new` and the HTTP-layer test for blank cursor —
    /// covering the application layer closes the gap where a non-HTTP
    /// caller could pass `Some("  ")` directly.
    #[tokio::test]
    async fn get_orders_rejects_blank_cursor() {
        for blank in ["", "   ", "\t\n"] {
            let repo = Arc::new(FakeRepo::empty());
            let uc = GetOrdersUseCase::new(repo.clone());
            let err = uc
                .execute(GetOrdersInput { cursor: Some(blank.into()), ..base_get_orders_input() })
                .await
                .expect_err("blank cursor must fail");
            let domain = err
                .downcast_ref::<DomainError>()
                .expect("blank cursor surfaces as typed DomainError");
            assert_eq!(*domain, DomainError::MissingParameter, "cursor={blank:?}");
            assert!(
                repo.recorded_orders_queries().is_empty(),
                "repo must not be touched for blank cursor={blank:?}",
            );
        }
    }

    // ---- CreateBatchOrdersUseCase ----

    fn batch_item(coid: Option<&str>) -> BatchOrderInputItem {
        BatchOrderInputItem {
            side: OrderSide::Buy,
            quantity: "1.5".into(),
            price: Some("0.615".into()),
            order_type: OrderType::Limit,
            time_in_force: Some(TimeInForce::Gtc),
            client_order_id: coid.map(|s| s.to_string()),
        }
    }

    fn base_batch_input(symbol: &str, orders: Vec<BatchOrderInputItem>) -> CreateBatchOrdersInput {
        CreateBatchOrdersInput {
            trading_pn: TradingPn {
                pn_address: "0:pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            market_address: MarketAddress("0:market".into()),
            symbol: Symbol(symbol.into()),
            orders,
            now_seconds: 1_000,
            now_ms: 1_000_000,
        }
    }

    #[tokio::test]
    async fn create_batch_orders_happy_path_two_items() {
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let input =
            base_batch_input("PM-YES", vec![batch_item(Some("11")), batch_item(Some("22"))]);
        let out = uc.execute(input).await.expect("happy path");

        assert_eq!(out.items.len(), 2);
        assert_eq!(out.items[0].client_order_id, "11");
        assert_eq!(out.items[1].client_order_id, "22");

        let batches = sender.batch_calls();
        assert_eq!(batches.len(), 1);
        let payload = &batches[0];
        assert_eq!(payload.pn_address, "0:pn");
        assert_eq!(payload.event_id, "0xevent");
        assert_eq!(payload.oracle_list_hash, "0xdead");
        assert_eq!(payload.token_type, 1);
        assert_eq!(payload.orders.len(), 2);
        assert_eq!(payload.orders[0].client_order_id, "11");
        assert_eq!(payload.orders[1].client_order_id, "22");
        assert_eq!(payload.orders[0].outcome_id, 1);
        assert_eq!(payload.orders[0].price_raw, "615");
        assert_eq!(payload.orders[0].amount_raw, "1500000");
        assert_eq!(payload.orders[0].flags, 0);
        assert!(payload.orders[0].is_buy);
    }

    #[tokio::test]
    async fn create_batch_orders_rejects_empty_batch() {
        // Pre-flight: an empty `orders[]` reaches the chain as
        // ERR_EMPTY_BATCH (162); failing here saves the round-trip
        // and avoids contending for the per-PN _busy lock.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let err = uc.execute(base_batch_input("PM-YES", vec![])).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        // Empty batch must short-circuit BEFORE market resolution and
        // the sender — otherwise we'd waste a chain call.
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_rejects_above_max_batch_size() {
        // `max_batch_size + 1` items must fail locally with -1130 instead
        // of paying a chain ERR_BATCH_TOO_LARGE round-trip.
        let market = trading_market("PM-YES");
        let max = test_outcome("PM-YES").max_batch_size as usize;
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let orders = (0..=max).map(|i| batch_item(Some(&i.to_string()))).collect();
        let err = uc.execute(base_batch_input("PM-YES", orders)).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_accepts_exactly_max_batch_size() {
        // Boundary pin: `outcome.max_batch_size` items must succeed.
        // Catches a future off-by-one (e.g. `>=` instead of `>`) that
        // would reject the boundary value.
        let market = trading_market("PM-YES");
        let max = test_outcome("PM-YES").max_batch_size as usize;
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let orders: Vec<_> = (0..max).map(|i| batch_item(Some(&i.to_string()))).collect();
        let out = uc.execute(base_batch_input("PM-YES", orders)).await.expect("max size accepted");
        assert_eq!(out.items.len(), max);
        assert_eq!(sender.batch_calls()[0].orders.len(), max);
    }

    #[tokio::test]
    async fn create_batch_orders_market_not_found() {
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::empty(), FakeSender::ok());
        let err =
            uc.execute(base_batch_input("PM-YES", vec![batch_item(Some("1"))])).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidMarketOrSymbol);
    }

    #[tokio::test]
    async fn create_batch_orders_rejects_non_trading_status() {
        let mut market = trading_market("PM-YES");
        market.status = MarketStatus::Resolving;
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err =
            uc.execute(base_batch_input("PM-YES", vec![batch_item(Some("1"))])).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn create_batch_orders_rejects_blank_oracle_list_hash() {
        let mut market = trading_market("PM-YES");
        market.oracle_list_hash = String::new();
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), FakeSender::ok());
        let err =
            uc.execute(base_batch_input("PM-YES", vec![batch_item(Some("1"))])).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_failure_aborts_whole_batch() {
        // First item is fine; second has 4-dp price against
        // pricePrecision=3. The whole batch must fail with the first
        // observed validation error and the sender MUST NOT be hit —
        // chain placeBatch is atomic, so half-submitting locally
        // would diverge from the chain contract.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("2"));
        bad.price = Some("0.6155".into()); // 4 dp > pricePrecision=3
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::PrecisionExceeded);
        assert!(sender.batch_calls().is_empty(), "chain sender hit despite per-item reject");
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_zero_quantity_aborts_whole_batch() {
        // `quantity == "0"` passes `precision_within` and
        // `is_multiple_of` (zero is a multiple of every non-zero
        // step), so the explicit strictly-positive gate in
        // `validate_and_encode_order_item` is the only thing that
        // catches it on the LIMIT path. Pin that the batch loop
        // runs that gate per item and short-circuits before the
        // chain.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("2"));
        bad.quantity = "0".into();
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_propagates_sender_pn_busy() {
        let market = trading_market("PM-YES");
        let uc = CreateBatchOrdersUseCase::new(
            FakeRepo::with(market),
            FakeSender::failing(DomainError::OrderPnBusy),
        );
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), batch_item(Some("2"))]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::OrderPnBusy);
    }

    #[tokio::test]
    async fn create_batch_orders_generates_client_order_id_when_absent() {
        // Smoke: each item without `newOrderClientId` gets a fresh
        // u64-bounded decimal id, fed straight into the chain payload.
        // Statistical uniqueness comes from the generator (Uuid::new_v4
        // -> u128 -> u64), not from this two-sample check; the
        // `assert_ne!` below catches the degenerate case where both
        // items pick up the same constant.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let input = base_batch_input("PM-YES", vec![batch_item(None), batch_item(None)]);
        let out = uc.execute(input).await.unwrap();

        let coid_a = &out.items[0].client_order_id;
        let coid_b = &out.items[1].client_order_id;
        for coid in [coid_a, coid_b] {
            assert!(!coid.is_empty());
            assert!(coid.chars().all(|c| c.is_ascii_digit()));
            assert!(coid.parse::<u64>().is_ok());
        }
        assert_ne!(coid_a, coid_b);
        // Same ids must appear in the chain payload.
        let payload = &sender.batch_calls()[0];
        assert_eq!(payload.orders[0].client_order_id, *coid_a);
        assert_eq!(payload.orders[1].client_order_id, *coid_b);
    }

    #[tokio::test]
    async fn create_batch_orders_first_item_failure_aborts_whole_batch() {
        // A bad item at index 0 must abort the whole batch — guards
        // against a future `enumerate().skip(N)` regression that
        // would silently skip index 0 validation.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("1"));
        bad.price = Some("0.6155".into()); // 4 dp > pricePrecision=3
        let err = uc
            .execute(base_batch_input("PM-YES", vec![bad, batch_item(Some("2"))]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::PrecisionExceeded);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_market_buy_happy_path() {
        // A well-formed MARKET-BUY item must flow through
        // `submit_batch_order` and land as a chain payload with
        // `FLAG_MARKET` set and `price_raw == "0"`.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut item = batch_item(Some("1"));
        item.order_type = OrderType::Market;
        item.side = OrderSide::Buy;
        item.time_in_force = None;
        item.price = None;
        item.quantity = "5".into(); // notional 5 > min_notional 0.5
        let out = uc.execute(base_batch_input("PM-YES", vec![item])).await.expect("market happy");
        assert_eq!(out.items.len(), 1);

        let calls = sender.batch_calls();
        assert_eq!(calls.len(), 1);
        let payload = &calls[0].orders[0];
        assert!(payload.is_buy);
        // MARKET items carry `price_raw = "0"` per the encode helper.
        assert_eq!(payload.price_raw, "0");
        // Pin FLAG_MARKET specifically — `assert_ne!(flags, 0)`
        // would accept any stray TIF bit.
        assert!(payload.flags & FLAG_MARKET != 0, "flags=0x{:02x}", payload.flags);
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_market_buy_below_min_notional_aborts_whole_batch() {
        // MARKET-BUY notional arm: `quantity` is the quote-asset spend,
        // compared directly to `min_notional`. Raise the threshold so
        // the base 1.5 spend underflows.
        let mut market = trading_market("PM-YES");
        market.outcomes[0].min_notional = "10".into();
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("2"));
        bad.order_type = OrderType::Market;
        bad.side = OrderSide::Buy;
        bad.time_in_force = None;
        bad.price = None;
        bad.quantity = "1.5".into(); // below min_notional=10
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_market_sell_zero_quantity_aborts_whole_batch() {
        // MARKET SELL skips the notional check, so the explicit
        // `amount_lifted > 0` gate is the only thing standing between
        // qty=0 and a chain `ERR_LOW_VALUE` round-trip. Pin that the
        // batch loop runs that gate per item.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("2"));
        bad.order_type = OrderType::Market;
        bad.side = OrderSide::Sell;
        bad.time_in_force = None;
        bad.price = None;
        bad.quantity = "0".into();
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_quantity_exceeding_u64_aborts_whole_batch() {
        // The effective ceiling on `amount` is `u64::MAX` — the upstream
        // SDK serialisation path panics above. Without the local gate
        // the chain sender would 500 on serialise. Pin per-item.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(Some("2"));
        // u64::MAX = 18_446_744_073_709_551_615. With
        // quantity_precision=6, "18446744073709.551616" lifts to
        // u64::MAX + 1 — strictly inside (u64::MAX, u128::MAX).
        bad.quantity = "18446744073709.551616".into();
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.batch_calls().is_empty());
    }

    #[tokio::test]
    async fn create_batch_orders_per_item_client_order_id_overflowing_u64_aborts_whole_batch() {
        // Caller-supplied coid > u64::MAX must surface as -1130 before
        // hitting the SDK's panic-on-u128 serialize path. Pin per-item.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CreateBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let mut bad = batch_item(None);
        // u64::MAX + 1
        bad.client_order_id = Some("18446744073709551616".into());
        let err = uc
            .execute(base_batch_input("PM-YES", vec![batch_item(Some("1")), bad]))
            .await
            .unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.batch_calls().is_empty());
    }
}

#[cfg(test)]
mod get_account_use_case_tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct StubPn {
        details: Result<PnDetails, String>,
    }

    #[async_trait]
    impl PnStateReader for StubPn {
        async fn get_details(&self, _pn_address: &str) -> anyhow::Result<PnDetails> {
            self.details.clone().map_err(|e| anyhow::anyhow!(e))
        }
        async fn get_stake(
            &self,
            _pn: &str,
            _hash: &str,
        ) -> anyhow::Result<Option<PnStake>> {
            unreachable!("get_account never calls get_stake")
        }
    }

    struct StubRefs {
        rows: Mutex<std::collections::HashMap<i32, RefToken>>,
    }

    #[async_trait]
    impl ReferenceRepository for StubRefs {
        async fn lookup_ref_token(&self, t: i32) -> anyhow::Result<Option<RefToken>> {
            Ok(self.rows.lock().unwrap().get(&t).cloned())
        }
    }

    fn make_refs() -> StubRefs {
        let mut m = std::collections::HashMap::new();
        m.insert(1, RefToken { token_type: 1, token_code: "NACKL".into(), decimals: 9 });
        m.insert(3, RefToken { token_type: 3, token_code: "USDC".into(), decimals: 6 });
        StubRefs { rows: Mutex::new(m) }
    }

    #[tokio::test]
    async fn renders_two_assets_sorted_by_code_asc() {
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![
                    (3, "25000000000".to_string()), // 25_000 USDC at 6 decimals
                    (1, "10000000000".to_string()), // 10 NACKL at 9 decimals
                ],
                locked_in_orders: vec![
                    (3, "3750000000".to_string()),  // 3_750 USDC locked
                    (1, "1500000000".to_string()),  // 1.5 NACKL locked
                ],
            }),
        };
        let uc = GetAccountUseCase::new(pn, make_refs());
        let out = uc
            .execute(GetAccountInput {
                account_id: uuid::Uuid::nil(),
                pn_address: "0:pn".into(),
                now_ms: 1710000000000,
            })
            .await
            .expect("ok");
        assert_eq!(out.balances.len(), 2);
        assert_eq!(out.balances[0].asset, "NACKL");
        assert_eq!(out.balances[0].free, "10.000000000");
        assert_eq!(out.balances[0].locked, "1.500000000");
        assert_eq!(out.balances[1].asset, "USDC");
        assert_eq!(out.balances[1].free, "25000.000000");
        assert_eq!(out.balances[1].locked, "3750.000000");
        assert_eq!(out.update_time_ms, 1710000000000);
    }

    #[tokio::test]
    async fn locked_defaults_to_zero_when_key_absent_in_locked_map() {
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![(1, "5000000000".to_string())], // 5 NACKL
                locked_in_orders: vec![],                     // empty
            }),
        };
        let uc = GetAccountUseCase::new(pn, make_refs());
        let out = uc
            .execute(GetAccountInput {
                account_id: uuid::Uuid::nil(),
                pn_address: "0:pn".into(),
                now_ms: 0,
            })
            .await
            .expect("ok");
        assert_eq!(out.balances[0].free, "5.000000000");
        assert_eq!(out.balances[0].locked, "0");
    }

    #[tokio::test]
    async fn unknown_token_type_yields_market_inconsistent() {
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![(99, "1".to_string())],
                locked_in_orders: vec![],
            }),
        };
        let uc = GetAccountUseCase::new(pn, make_refs());
        let err = uc
            .execute(GetAccountInput {
                account_id: uuid::Uuid::nil(),
                pn_address: "0:pn".into(),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn pn_reader_failure_yields_market_inconsistent() {
        let pn = StubPn { details: Err("gateway down".into()) };
        let uc = GetAccountUseCase::new(pn, make_refs());
        let err = uc
            .execute(GetAccountInput {
                account_id: uuid::Uuid::nil(),
                pn_address: "0:pn".into(),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[test]
    fn scale_decimal_pads_to_full_precision() {
        assert_eq!(scale_decimal("10000000000", 9), "10.000000000");
        assert_eq!(scale_decimal("1500000000", 9), "1.500000000");
        assert_eq!(scale_decimal("1", 9), "0.000000001");
        assert_eq!(scale_decimal("0", 9), "0");
        assert_eq!(scale_decimal("", 9), "0");
        assert_eq!(scale_decimal("25000000000", 6), "25000.000000");
        assert_eq!(scale_decimal("42", 0), "42");
    }
}

#[cfg(test)]
mod balances_port_tests {
    use super::*;

    // Existence-only test: this compiles iff the trait is dyn-compatible
    // and the value-object fields have the expected names/types. We
    // exercise behaviour via the use case tests in Task 3.
    #[test]
    fn pn_state_reader_is_dyn_compatible() {
        fn _accepts_dyn(_: &dyn PnStateReader) {}
    }

    #[test]
    fn reference_repository_is_dyn_compatible() {
        fn _accepts_dyn(_: &dyn ReferenceRepository) {}
    }

    #[test]
    fn pn_details_constructs_with_named_fields() {
        let _ = PnDetails {
            balance: vec![(1, "100".to_string())],
            locked_in_orders: vec![(1, "10".to_string())],
        };
    }

    #[test]
    fn pn_stake_constructs_with_arrays() {
        let _ = PnStake {
            amount: vec!["1".to_string(), "2".to_string()],
            debt_amount: vec!["0".to_string(), "0".to_string()],
            coupons_amount: vec!["0".to_string(), "0".to_string()],
        };
    }

    #[test]
    fn market_balances_resolution_constructs() {
        let _ = MarketBalancesResolution {
            event_id: "12345".to_string(),
            oracle_list_hash: "67890".to_string(),
            token_type: 1,
            orderbook_address: "0:orderbook".to_string(),
            num_outcomes: 2,
            outcomes: vec![
                BalanceOutcome {
                    outcome_id: 0,
                    symbol: Symbol("X-NO".to_string()),
                    quantity_precision: 2,
                },
                BalanceOutcome {
                    outcome_id: 1,
                    symbol: Symbol("X-YES".to_string()),
                    quantity_precision: 2,
                },
            ],
        };
    }
}
