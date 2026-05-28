// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashMap;
use std::collections::HashSet;
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
/// are decimal-encoded uint256 strings — the format the chain ABI accepts
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
    /// Validated as non-negative at the repo boundary (the DB column is
    /// `integer` but the chain ABI is `uint32`); callers can use it
    /// directly as `u32` without a secondary cast.
    pub token_type: u32,
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
    /// Validated as non-negative at the repo boundary (the DB column is
    /// `integer` but the chain ABI is `uint32`); callers can use it
    /// directly as `u32` without a secondary cast.
    pub token_type: u32,
    pub status: MarketStatus,
    pub outcome: Outcome,
}

/// Slim market projection the `POST /api/v1/buyFullSet` path needs.
/// No `market_outcomes` join — splitFullSet is a market-level operation
/// (the chain produces one outcome token of every outcome from the
/// `collateral`), so no symbol resolution is involved. `status` is
/// computed against the caller's `now` so downstream validation can
/// gate on `AWAITING_FREEZE | TRADING` without a second round-trip per
/// [api-spec §Buy Full Set](../../docs/api-spec.md#buy-full-set).
#[derive(Debug, Clone)]
pub struct MarketForBuyFullSet {
    pub event_id: String,
    pub oracle_list_hash: String,
    /// Validated as non-negative at the repo boundary (the DB column is
    /// `integer` but the chain ABI is `uint32`); callers can use it
    /// directly as `u32` without a secondary cast. Doubles as the
    /// `tokenType` slot of `ParamsOfSplitFullSet` and as the lookup key
    /// for the quote asset's on-chain `decimals` via
    /// [`ReferenceRepository::lookup_ref_token`].
    pub token_type: u32,
    pub status: MarketStatus,
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
    /// Already validated as non-negative at the repo boundary
    /// (`try_into().map_err(MarketInconsistent)`); callers can use it
    /// directly as `u32` without a secondary cast.
    pub token_type: u32,
    pub orderbook_address: String,
    /// Number of outcomes for this market. `u32` because outcome counts
    /// are non-negative; the Postgres `integer` column is cast at the
    /// repo boundary (negative DB values → `MarketInconsistent`).
    pub num_outcomes: u32,
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

    /// Resolve multiple open orders owned by `owner_pn_address` on a
    /// single `(market_address, symbol)`. Returns matched rows in a
    /// `HashMap<u64, OrderForCancelBatch>` keyed by chain `order_id`.
    ///
    /// **Trait contract — every impl owes:** every key in
    /// `orders` is an element of the caller's `order_ids[]` slice.
    /// The Postgres impl enforces this with
    /// `WHERE lo.order_id = ANY($3)`; the natural HashMap uniqueness
    /// plus the `(orderbook_address, order_id)` primary key
    /// guarantees no key collisions.
    ///
    /// **Result shape:**
    /// - `Ok(None)` — zero matches. Use case maps to `UnknownOrder`.
    /// - `Ok(Some(r))` with `r.orders.len() == order_ids.len()` —
    ///   full match (by contract every key is in input; pigeonhole
    ///   gives an exact set match).
    /// - `Ok(Some(r))` with `r.orders.len() < order_ids.len()` —
    ///   partial shortfall (one or more input ids did not match).
    ///   Use case maps to `UnknownOrder`.
    /// - `Err(_)` — non-domain SELECT failure via `anyhow`.
    ///
    /// **Wrapping fields:** `event_id`, `oracle_list_hash`,
    /// `token_type`, and `market_status` are projected from the
    /// JOINed `markets` row onto `CancelBatchResolution`, evaluated
    /// at the caller-provided `now`. All four are constant by SELECT
    /// construction (filter pins one `(pmp_address, symbol)`). The
    /// use case re-checks `market_status == Trading` post-SELECT to
    /// close the race between `resolve_for_new_order`'s earlier
    /// snapshot and this bulk one — a reconciler commit between the
    /// two MVCC snapshots rejects with `OrderValidationFailed`
    /// before chain dispatch.
    async fn resolve_for_cancel_batch(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_ids: &[u64],
        owner_pn_address: &str,
        now: i64,
    ) -> Result<Option<CancelBatchResolution>, anyhow::Error>;

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

    /// Resolve `marketAddress` for `POST /api/v1/buyFullSet`. Returns
    /// chain identity (`event_id`, `oracle_list_hash`, `token_type`)
    /// plus the `MarketStatus` derived against `now`. Gated by
    /// `last_reconciled_at IS NOT NULL`; misses collapse to
    /// `DomainError::InvalidMarketOrSymbol`. No outcome join — the
    /// splitFullSet ABI operates at the market level.
    async fn resolve_for_buy_full_set(
        &self,
        market_address: &MarketAddress,
        now: i64,
    ) -> Result<MarketForBuyFullSet, anyhow::Error>;

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

    async fn resolve_for_cancel_batch(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        order_ids: &[u64],
        owner_pn_address: &str,
        now: i64,
    ) -> Result<Option<CancelBatchResolution>, anyhow::Error> {
        (**self)
            .resolve_for_cancel_batch(market_address, symbol, order_ids, owner_pn_address, now)
            .await
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

    async fn resolve_for_buy_full_set(
        &self,
        market_address: &MarketAddress,
        now: i64,
    ) -> Result<MarketForBuyFullSet, anyhow::Error> {
        (**self).resolve_for_buy_full_set(market_address, now).await
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
            // Render the full `with_context` chain on one line so ops can
            // distinguish gateway flap from ABI parse failure — Debug
            // formatting (`?e`) folds the chain into a multi-line block
            // that's awkward to grep in fmt layers. Preserve a typed
            // `DomainError` from the reader (in particular
            // `AccountNotDeployed`) so the API surfaces 404 rather than
            // collapsing every read-side failure to 503.
            warn!(
                error = %format_args!("{e:#}"),
                pn = %input.pn_address,
                "get_details failed",
            );
            if let Some(domain) = e.downcast_ref::<dodex_domain::DomainError>() {
                return anyhow::anyhow!(*domain);
            }
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        // Build the per-token_type aggregate from the union of `_balance`
        // and `_lockedInOrders` keys. A token_type that appears only on the
        // locked side — the textbook case is a LIMIT SELL that consumed the
        // caller's entire free balance, leaving `_balance[X]` absent (or
        // pruned to 0) while `_lockedInOrders[X] > 0` — must still surface
        // in the response with `free = "0"`. Iterating `_balance` alone
        // would silently drop it.
        //
        // Each map is keyed by token_type and must be unique; the chain
        // emits `map(uint32 → uint128)` so a duplicate key is read-model
        // corruption. `HashMap::insert` returning `Some` fails closed
        // rather than silently letting the last write win.
        let mut free_by_tt: std::collections::HashMap<u32, String> =
            std::collections::HashMap::with_capacity(details.balance.len());
        for (tt, raw_free) in &details.balance {
            if free_by_tt.insert(*tt, raw_free.clone()).is_some() {
                warn!(token_type = *tt, "duplicate token_type in PN _balance");
                return Err(anyhow::Error::from(dodex_domain::DomainError::MarketInconsistent)
                    .context(format!("duplicate token_type {tt} in PN _balance")));
            }
        }
        let mut locked_by_tt: std::collections::HashMap<u32, String> =
            std::collections::HashMap::with_capacity(details.locked_in_orders.len());
        for (tt, raw_locked) in &details.locked_in_orders {
            if locked_by_tt.insert(*tt, raw_locked.clone()).is_some() {
                warn!(token_type = *tt, "duplicate token_type in PN _lockedInOrders");
                return Err(anyhow::Error::from(dodex_domain::DomainError::MarketInconsistent)
                    .context(format!("duplicate token_type {tt} in PN _lockedInOrders")));
            }
        }
        let mut by_tt: std::collections::HashMap<u32, (String, String)> =
            std::collections::HashMap::with_capacity(free_by_tt.len() + locked_by_tt.len());
        for (tt, raw_free) in free_by_tt {
            by_tt.entry(tt).or_insert_with(|| ("0".to_string(), "0".to_string())).0 = raw_free;
        }
        for (tt, raw_locked) in locked_by_tt {
            by_tt.entry(tt).or_insert_with(|| ("0".to_string(), "0".to_string())).1 = raw_locked;
        }

        let mut rows: Vec<dodex_domain::AssetBalance> = Vec::with_capacity(by_tt.len());
        for (tt, (raw_free, raw_locked)) in &by_tt {
            let token = self.refs.lookup_ref_token(*tt).await?.ok_or_else(|| {
                warn!(token_type = tt, "PN state carries unknown token_type");
                anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
            })?;
            rows.push(dodex_domain::AssetBalance {
                asset: token.token_code.clone(),
                free: scale_decimal(raw_free, token.decimals)?,
                locked: scale_decimal(raw_locked, token.decimals)?,
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
/// All inputs — including `"0"` and the empty string — are padded to
/// exactly `decimals` fractional digits (e.g. `"10000000000"` with
/// `decimals=9` → `"10.000000000"`, `"1"` → `"0.000000001"`,
/// `"0"` → `"0.000000000"`). The empty string is normalised to `"0"`
/// before scaling. `decimals == 0` returns `raw` unchanged (or `"0"`
/// for an empty input).
///
/// `raw` is validated as a non-negative integer literal before any
/// byte-level slicing — non-digit or multibyte input would otherwise
/// either produce garbage output or panic at the UTF-8 split when
/// `raw.len() > decimals` and the split falls inside a multibyte char.
/// Invalid input surfaces as `DomainError::MarketInconsistent` (503).
/// Upper bound on `decimals` accepted by `scale_decimal`. Mirrors
/// `crates/infrastructure/src/postgres_repo.rs::MAX_DECIMAL_PRECISION`:
/// the SQL NUMERIC(38, …) cap, far beyond any real asset's precision.
/// `scale_decimal` allocates `O(decimals)` bytes via `"0".repeat(...)`,
/// so an unbounded value on a corrupt `ref_tokens` row would OOM the
/// API process on the first scaled balance.
const MAX_DECIMALS: u8 = 38;

fn scale_decimal(raw: &str, decimals: u8) -> Result<String, DomainError> {
    use std::str::FromStr;
    if decimals > MAX_DECIMALS {
        tracing::warn!(
            decimals,
            max = MAX_DECIMALS,
            "scale_decimal: decimals exceed MAX_DECIMALS — refusing to allocate",
        );
        return Err(DomainError::MarketInconsistent);
    }
    let raw = if raw.is_empty() { "0" } else { raw };
    // Parse and re-emit so the slicing path below operates on a
    // canonical decimal string. `BigUint::from_str` accepts leading
    // zeros ("00012345"), so without canonicalisation a padded input
    // would survive into the `>` branch and slice to "00012.345"
    // instead of "12.345". Triggers: a future tvm_abi version that
    // emits zero-padded uint128 literals, or a corrupt repo row.
    let canonical = BigUint::from_str(raw)
        .map_err(|err| {
            tracing::warn!(raw, error = %err, "scale_decimal: input is not a non-negative integer");
            DomainError::MarketInconsistent
        })?
        .to_string();
    let raw = canonical.as_str();
    let d = decimals as usize;
    if d == 0 {
        // Keep the response format invariant: every scaled value has a decimal
        // point. A strict client parser (`^[0-9]+\.[0-9]+$`) would otherwise
        // reject the bare integer. `decimals=0` is reachable today because the
        // schema does not CHECK `> 0` on `ref_tokens.decimals` /
        // `market_outcomes.quantity_precision`.
        return Ok(format!("{raw}.0"));
    }
    if raw.len() <= d {
        let padded = "0".repeat(d - raw.len()) + raw;
        Ok(format!("0.{padded}"))
    } else {
        let split = raw.len() - d;
        Ok(format!("{}.{}", &raw[..split], &raw[split..]))
    }
}

/// Input for `GetMarketBalancesUseCase`. The HTTP layer assembles it
/// from the validated query plus the resolved auth context.
#[derive(Debug, Clone)]
pub struct GetMarketBalancesInput {
    pub pn_address: String,
    pub market_address: MarketAddress,
    /// Unix milliseconds. Echoed as `updateTime` in the response.
    pub now_ms: i64,
}

/// Signature for the off-chain hash function — the use case holds it
/// as a function pointer so unit tests can plug a stub hasher without
/// pulling in the real `tvm_abi` machinery. Returns
/// `Err(DomainError::MarketInconsistent)` on parse or hash failure so
/// read-model corruption surfaces as a 503 instead of silently
/// producing all-zero outcome balances.
///
/// `token_type` is `u32` because the repo boundary already validates
/// that the DB value is non-negative; callers never need to cast.
pub type StakeHasher =
    fn(event_id: &str, oracle_list_hash: &str, token_type: u32) -> Result<String, DomainError>;

pub struct GetMarketBalancesUseCase<P, R> {
    pn: P,
    repo: R,
    hasher: StakeHasher,
}

impl<P, R> GetMarketBalancesUseCase<P, R> {
    pub fn new(pn: P, repo: R, hasher: StakeHasher) -> Self {
        Self { pn, repo, hasher }
    }
}

impl<P, R> GetMarketBalancesUseCase<P, R>
where
    P: PnStateReader,
    R: MarketReadRepository,
{
    pub async fn execute(
        &self,
        input: GetMarketBalancesInput,
    ) -> Result<dodex_domain::MarketBalances, anyhow::Error> {
        // Resolve the market. The repo lifts unknown / unreconciled
        // pairs to InvalidMarketOrSymbol; pass that error through verbatim.
        let res = self.repo.resolve_market_for_balances(&input.market_address).await?;

        // Compute the stake hash off chain. The hasher returns Err on
        // parse / hash failure (read-model corruption) — propagate as
        // MarketInconsistent so the caller receives a 503.
        // `res.token_type` is already `u32` (validated at the repo boundary).
        let stake_hash = (self.hasher)(&res.event_id, &res.oracle_list_hash, res.token_type)
            .map_err(|e| anyhow::anyhow!(e))?;

        // Fan out: chain-side stake lookup + DB-side sell aggregation.
        // The two are independent, so we issue them in parallel.
        let pn_address = input.pn_address.clone();
        let stake_fut = self.pn.get_stake(&pn_address, &stake_hash);
        let sum_fut = self.repo.sum_open_sell_remaining(&res.orderbook_address, &input.pn_address);
        let (stake_opt, sums) = tokio::try_join!(stake_fut, sum_fut).map_err(|e| {
            // Preserve a typed `DomainError` from either branch (e.g. the
            // repo lifts negative `outcome_id` to MarketInconsistent and
            // the reader can produce `AccountNotDeployed`). Without this
            // downcast, the outer wrap would replace the inner
            // classification and the handler would see only the
            // freshly-minted MarketInconsistent. Log either way so a
            // second simultaneous failure isn't silently dropped by
            // try_join!'s "first error wins" behaviour.
            if let Some(domain) = e.downcast_ref::<dodex_domain::DomainError>() {
                tracing::warn!(
                    ?domain,
                    market_address = %input.market_address.0,
                    pn = %input.pn_address,
                    error = %format_args!("{e:#}"),
                    "balances fan-out failed (typed domain error)",
                );
                return anyhow::anyhow!(*domain);
            }
            tracing::warn!(error = %format_args!("{e:#}"), "balances fan-out failed");
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })?;

        let n = res.num_outcomes as usize;

        // Shape validation: arrays in PnStake must be either empty
        // (absent key) OR exactly num_outcomes long.
        if let Some(ref s) = stake_opt {
            let any_empty =
                s.amount.is_empty() || s.debt_amount.is_empty() || s.coupons_amount.is_empty();
            let all_empty =
                s.amount.is_empty() && s.debt_amount.is_empty() && s.coupons_amount.is_empty();
            if any_empty && !all_empty {
                tracing::warn!(
                    amount_len = s.amount.len(),
                    debt_len = s.debt_amount.len(),
                    coupons_len = s.coupons_amount.len(),
                    "stake arrays mismatch: some empty, some populated"
                );
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
            for (label, arr) in [
                ("amount", &s.amount),
                ("debtAmount", &s.debt_amount),
                ("couponsAmount", &s.coupons_amount),
            ] {
                if !arr.is_empty() && arr.len() != n {
                    tracing::warn!(
                        field = label,
                        got = arr.len(),
                        expected = n,
                        "stake array length mismatch"
                    );
                    return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
                }
            }
        }

        // Sanity: every aggregated outcome_id must fall within [0, n).
        for k in sums.keys() {
            if (*k as usize) >= n {
                tracing::warn!(outcome_id = k, "aggregation returned out-of-range outcome_id");
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
        }
        for outcome in &res.outcomes {
            if (outcome.outcome_id as usize) >= n {
                tracing::warn!(
                    outcome_id = outcome.outcome_id,
                    num_outcomes = res.num_outcomes,
                    "outcome_id out of range for num_outcomes"
                );
                return Err(anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent));
            }
        }

        // Compose response, sorted by outcome_id ASC (resolution is
        // already in ascending order by construction in the repo).
        let mut rows: Vec<dodex_domain::OutcomeBalance> = Vec::with_capacity(n);
        for outcome in &res.outcomes {
            let i = outcome.outcome_id as usize;
            let raw_free: String = match stake_opt {
                Some(ref s) if !s.amount.is_empty() => {
                    add_decimal_strs(&s.amount[i], &s.debt_amount[i], &s.coupons_amount[i])?
                }
                _ => "0".to_string(),
            };
            let raw_locked =
                sums.get(&outcome.outcome_id).cloned().unwrap_or_else(|| "0".to_string());
            rows.push(dodex_domain::OutcomeBalance {
                outcome_id: outcome.outcome_id,
                symbol: outcome.symbol.clone(),
                free: scale_decimal(&raw_free, outcome.quantity_precision)?,
                locked_in_orders: scale_decimal(&raw_locked, outcome.quantity_precision)?,
            });
        }

        Ok(dodex_domain::MarketBalances {
            market_address: input.market_address,
            update_time_ms: input.now_ms,
            balances: rows,
        })
    }
}

/// Sum three non-negative integer decimal strings. Uses
/// `num_bigint::BigUint` so 128-bit-ish values cannot overflow.
fn add_decimal_strs(a: &str, b: &str, c: &str) -> Result<String, anyhow::Error> {
    use std::str::FromStr;
    let parse = |s: &str| -> Result<BigUint, anyhow::Error> {
        if s.is_empty() {
            return Ok(BigUint::from(0u32));
        }
        BigUint::from_str(s).map_err(|e| {
            tracing::warn!(value = s, error = ?e, "decimal parse failed");
            anyhow::anyhow!(dodex_domain::DomainError::MarketInconsistent)
        })
    };
    let total = parse(a)? + parse(b)? + parse(c)?;
    Ok(total.to_string())
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
    /// `DexChainSender` re-encodes it as hex for `KeyPair.public`.
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

/// Input shape for `CancelBatchOrdersUseCase`. Mirrors the body of
/// `DELETE /api/v1/batchOrders` — one trading PN, one `(marketAddress,
/// symbol)`, plus the per-item id list. The chain ABI takes one
/// `(eventId, oracleListHash, tokenType)` per batch, so all items share
/// the same market resolution.
#[derive(Debug, Clone)]
pub struct CancelBatchOrdersInput {
    pub trading_pn: TradingPn,
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub order_ids: Vec<u64>,
    /// Unix seconds. Drives status derivation in the bulk SELECT.
    pub now_seconds: i64,
    /// Unix milliseconds. Returned to the client as `transactTime` for
    /// every item in the batch — symmetric with peer Input types so the
    /// handler cannot drift by re-sampling per item.
    pub now_ms: i64,
}

/// Per-item chain payload field for `cancel_batch_order`. Peer of
/// [`BatchOrderPayloadItem`] on the place-batch path: the chain ABI
/// carries `(eventId, oracleListHash, tokenType)` at batch level and
/// only `(order_id, client_order_id)` per row. `client_order_id` is
/// the resolved `live_orders.client_order_id` for this entry — `None`
/// for an order placed without a caller-supplied coid. Not part of
/// the chain ABI; used only for the audit log so ops can grep a
/// cancel-batch incident by `clientOrderId` without joining
/// `live_orders` back.
#[derive(Debug, Clone)]
pub struct CancelBatchPayloadItem {
    pub order_id: u64,
    pub client_order_id: Option<String>,
}

/// Chain-shaped payload handed to `ChainOrderSender::cancel_batch_order`.
/// Mirrors `NewBatchOrderPayload` but the ABI is narrower —
/// `PrivateNote.cancelBatch` takes only event/oracle/token coordinates
/// plus the chain-assigned `orderId[]`. No price, amount, or flags.
#[derive(Debug, Clone)]
pub struct CancelBatchOrderPayload {
    pub pn_address: String,
    pub pn_pubkey: String,
    pub pn_seckey: SensitiveBytes,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub items: Vec<CancelBatchPayloadItem>,
}

/// One matched live order from
/// `MarketReadRepository::resolve_for_cancel_batch`. Carries only
/// per-row data — chain identity and market status are pulled up to
/// [`CancelBatchResolution`] because they are constant across all
/// rows of one resolution (the SELECT is filtered to a single
/// `(pmp_address, symbol)` pair, so every row joins the same `markets`
/// snapshot). The chain `order_id` lives in the wrapping map's key,
/// not on the row.
#[derive(Debug, Clone)]
pub struct OrderForCancelBatch {
    /// `live_orders.client_order_id`. NULL surfaces as `None`; the
    /// handler renders that as the empty string per
    /// api-spec §Cancel Batch Orders.
    pub client_order_id: Option<String>,
}

/// Result of `MarketReadRepository::resolve_for_cancel_batch`. Wraps
/// the matched per-row data with the market-level identity and status
/// that drive the chain payload, expressed once because they are
/// constant by SELECT construction. `None` (at the trait level) means
/// zero rows matched — the use case maps that to `UnknownOrder`.
#[derive(Debug, Clone)]
pub struct CancelBatchResolution {
    /// Chain identity from the `markets` row the JOIN matched — feeds
    /// `CancelBatchOrderPayload` directly, closing the race window
    /// against `resolve_for_new_order`'s earlier MVCC view that could
    /// have hashed a different market generation.
    pub event_id: String,
    /// `markets.oracle_list_hash`. NULL/blank surfaces as the empty
    /// string with a warn at the infra layer (mirrors
    /// `resolve_for_cancel`); the use case maps empty → MarketInconsistent.
    pub oracle_list_hash: String,
    /// `markets.token_type`. The use case applies `u32::try_from` so
    /// an out-of-range read-model value collapses to MarketInconsistent
    /// rather than panic.
    pub token_type: i32,
    /// `MarketStatus` derived from the JOINed `markets` row at the
    /// use-case-provided `now`. The use case re-checks this
    /// post-SELECT to close the race window against the earlier
    /// `resolve_for_new_order` snapshot.
    pub market_status: MarketStatus,
    /// Matched live orders keyed by chain `order_id`. Trait contract
    /// requires every key to be a member of the caller's
    /// `order_ids[]` slice (Postgres enforces with
    /// `lo.order_id = ANY($3::text[]::numeric[])`), and the natural
    /// HashMap uniqueness on `(orderbook_address, order_id)` PK
    /// guarantees no key collisions can surface here. The use case
    /// rejects `orders.len() < order_ids.len()` as `UnknownOrder`
    /// (shortfall) and then looks each input id up directly via
    /// `orders.remove(&id)`. By trait contract `orders.len() <=
    /// order_ids.len()` after the SELECT, so the shortfall gate plus
    /// the per-id lookup together cover the full keyset-mismatch
    /// space.
    pub orders: HashMap<u64, OrderForCancelBatch>,
}

/// Output of `CancelBatchOrdersUseCase`. One entry per request item, in
/// request order — the HTTP layer maps these into the `PENDING_CANCEL`
/// array response per [api-spec §Cancel Batch Orders](../../docs/api-spec.md#cancel-batch-orders).
#[derive(Debug, Clone)]
pub struct CancelledBatchOrder {
    pub order_id: u64,
    pub client_order_id: Option<String>,
}

/// Input shape for `BuyFullSetUseCase`. The HTTP layer parses
/// `POST /api/v1/buyFullSet` body + `AuthContext` + clock into this
/// struct. `collateral` is kept as a string until precision validation
/// lifts it to `u128` against the quote asset's on-chain `decimals`.
/// Unlike the order-placement inputs, no `now_ms` is carried — the
/// handler stamps `transactTime` from its own clock read, and this
/// use case never returns a body-derived timestamp.
#[derive(Debug, Clone)]
pub struct BuyFullSetInput {
    pub trading_pn: TradingPn,
    pub market_address: MarketAddress,
    pub collateral: String,
    /// Unix seconds. Drives status derivation on the market row.
    pub now_seconds: i64,
}

/// Chain-shaped payload handed to `ChainOrderSender::split_full_set`.
/// Maps directly to `ackinacki-kit::PrivateNote::split_full_set`
/// (`ParamsOfSplitFullSet`); the only call-site responsibility is
/// re-encoding pubkey/seckey for the `KeyPair` boundary.
///
/// `collateral_raw` is a decimal string (smallest-unit `uint128`)
/// already lifted by the quote asset's `decimals` and range-checked at
/// the use-case boundary — same shape `NewOrderPayload.amount_raw`
/// uses. The chain sender parses to `u128` at the ABI boundary; the
/// u64-ceiling rationale lives in
/// [write-api.md §clientOrderId generation].
#[derive(Debug, Clone)]
pub struct SplitFullSetPayload {
    pub pn_address: String,
    /// Decimal-encoded `uint256` public half of the trading-PN keypair.
    /// `DexChainSender` re-encodes it as hex for `KeyPair.public`.
    pub pn_pubkey: String,
    pub pn_seckey: SensitiveBytes,
    pub event_id: String,
    pub oracle_list_hash: String,
    pub token_type: u32,
    pub collateral_raw: String,
}

/// Dispatch a `PrivateNote.placeOrder` external message to chain.
/// Returns once the chain submission path has observed execution of
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
    /// Returns once the chain submission path has observed PN's execution of
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

    /// Dispatch a `PrivateNote.cancelBatch` external message to chain.
    /// `cancelBatch` accepts the orderId list atomically at the PN
    /// (one external message, one `_busy` window) and forwards a single
    /// `OrderBook.executeBatch` for the per-order cancels. Chain-side
    /// rejects mapped here: `121 ERR_NOTE_BUSY` → `OrderPnBusy`, and the
    /// range guards `161 ERR_BATCH_TOO_LARGE` / `162 ERR_EMPTY_BATCH`
    /// → `MarketInconsistent` (defence-in-depth — the use case
    /// pre-checks both, so reaching either code means read-model drift
    /// from the on-chain ceiling).
    /// Per-order OrderBook outcomes (silent no-op on owner-mismatch or
    /// already-closed, queue overflow) remain asynchronous and surface
    /// through the indexer.
    async fn cancel_batch_order(&self, payload: CancelBatchOrderPayload)
        -> Result<(), DomainError>;

    /// Dispatch a `PrivateNote.splitFullSet` external message to chain.
    /// Returns once the chain submission path has observed PN's
    /// execution — so PrivateNote-side `require(...)` failures
    /// (`ERR_NOTE_BUSY`, `ERR_LOW_VALUE`, `ERR_DEBT_NON_ZERO`) come
    /// back as typed `DomainError`s here. The chain-side
    /// `onSplitAccepted` (credit) / `onBounce` (refund on a PMP-side
    /// revert) callback runs as a separate internal message; its
    /// outcome is visible only through
    /// the on-chain `PrivateNote._stakes` getter, which the API
    /// surfaces via `GET /api/v1/account/balances` (see
    /// [api-spec §Buy Full Set](../../docs/api-spec.md#buy-full-set)
    /// — "the response confirms acceptance only").
    async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError>;
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

    async fn cancel_batch_order(
        &self,
        payload: CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        (**self).cancel_batch_order(payload).await
    }

    async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError> {
        (**self).split_full_set(payload).await
    }
}

/// One row of `ref_tokens` exposed to the application layer.
///
/// `token_type` is `u32` because the chain ABI emits a `uint32`. The DB
/// column is `integer` (i32 range), so a value above `i32::MAX` cannot
/// exist in the table and the repo lifts it to
/// `DomainError::MarketInconsistent` at the boundary. Carrying it as
/// `u32` above the repo lets the rest of the path skip defensive sign
/// checks.
#[derive(Debug, Clone)]
pub struct RefToken {
    pub token_type: u32,
    pub token_code: String,
    pub decimals: u8,
}

/// Source of `ref_tokens` lookups. Kept as a separate port from
/// `MarketReadRepository` because callers (use cases) need only
/// `lookup_ref_token` and dragging in the heavy MarketRead surface
/// would unnecessarily couple this trait's consumers to the wider
/// market-read API.
#[async_trait]
pub trait ReferenceRepository: Send + Sync {
    /// Returns `None` for an unknown `token_type` (no matching row in
    /// `ref_tokens`). The repo additionally lifts structurally
    /// impossible values — a `u32` above the DB column's `i32` range,
    /// or a row whose `decimals` does not fit in `u8` — to
    /// `DomainError::MarketInconsistent`. Use cases turn the genuine
    /// `None` into `MarketInconsistent` too (the indexer ships with the
    /// canonical set; an unknown type is read-model corruption).
    async fn lookup_ref_token(&self, token_type: u32) -> Result<Option<RefToken>, anyhow::Error>;
}

#[async_trait]
impl<T: ?Sized + ReferenceRepository> ReferenceRepository for Arc<T> {
    async fn lookup_ref_token(&self, token_type: u32) -> Result<Option<RefToken>, anyhow::Error> {
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
    pub balance: Vec<(u32, String)>,
    pub locked_in_orders: Vec<(u32, String)>,
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

        // Defence-in-depth: `resolve_for_new_order` already lifts a
        // NULL/blank `oracle_list_hash` to `MarketInconsistent` at the
        // repo boundary, so the projection should never reach here with
        // an empty value. Keep the check as a second-line guard in case
        // a future repo implementation regresses; mirrors the
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
    // but the upstream `ackinacki-kit` → `serde_json::json!`
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

        // Defence-in-depth: `resolve_for_cancel` already lifts a
        // NULL/blank `oracle_list_hash` to `MarketInconsistent` at the
        // repo boundary. Keep this guard as a second-line check so a
        // future repo regression cannot push a zero-hash submission
        // to chain.
        if oracle_list_hash.is_empty() {
            return Err(DomainError::MarketInconsistent);
        }

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

/// Orchestrates `DELETE /api/v1/batchOrders`: resolves market+outcome
/// once, validates the request shape (non-empty,
/// `len ≤ outcome.max_batch_size`, no intra-batch duplicates), resolves
/// every order via one bulk SELECT, then dispatches a single
/// `PrivateNote.cancelBatch` call. Any shortfall in the resolved set
/// collapses to `UnknownOrder` for the whole batch — same opacity as
/// single-cancel.
pub struct CancelBatchOrdersUseCase<R, S> {
    repo: R,
    sender: S,
}

impl<R, S> CancelBatchOrdersUseCase<R, S> {
    pub fn new(repo: R, sender: S) -> Self {
        Self { repo, sender }
    }
}

impl<R, S> CancelBatchOrdersUseCase<R, S>
where
    R: MarketReadRepository,
    S: ChainOrderSender,
{
    pub async fn execute(
        &self,
        input: CancelBatchOrdersInput,
    ) -> Result<Vec<CancelledBatchOrder>, DomainError> {
        // `phase = "shape"` lets ops query the substring
        // `cancelBatch rejected` and disambiguate empty/duplicate/oversize
        // — symmetric with `CreateBatchOrdersUseCase`.
        if input.order_ids.is_empty() {
            warn!(phase = "shape", order_ids_len = 0, "cancelBatch rejected");
            return Err(DomainError::InvalidParameter);
        }

        // `resolve_for_new_order` here is the early-exit gate: only its
        // `outcome.max_batch_size` and `status` feed the pre-bulk-SELECT
        // checks. Chain identity (`event_id`, `oracle_list_hash`,
        // `token_type`) is read later from `CancelBatchResolution` of
        // the bulk SELECT — same MVCC snapshot as the order rows — so a
        // reconciler commit between the two queries cannot leak a
        // stale market generation into the chain payload.
        let MarketForPlacement { status, outcome, .. } = self
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
        // Cap check runs BEFORE the dedup HashSet allocation and BEFORE
        // the bulk SELECT so an oversize input is rejected without paying
        // O(N) memory or a second DB round trip.
        if input.order_ids.len() > outcome.max_batch_size as usize {
            warn!(
                phase = "shape",
                order_ids_len = input.order_ids.len(),
                max_batch_size = outcome.max_batch_size,
                "cancelBatch rejected",
            );
            return Err(DomainError::InvalidParameter);
        }

        // Intra-batch duplicates would produce two PENDING_CANCEL
        // receipts for the same id and waste one slot in the chain's
        // MAX_BATCH_SIZE window. Bounded by max_batch_size above.
        let mut seen: HashSet<u64> = HashSet::with_capacity(input.order_ids.len());
        for &id in &input.order_ids {
            if !seen.insert(id) {
                warn!(
                    phase = "shape",
                    order_ids_len = input.order_ids.len(),
                    duplicate_id = id,
                    "cancelBatch rejected",
                );
                return Err(DomainError::InvalidParameter);
            }
        }

        let resolution = self
            .repo
            .resolve_for_cancel_batch(
                &input.market_address,
                &input.symbol,
                &input.order_ids,
                &input.trading_pn.pn_address,
                input.now_seconds,
            )
            .await
            .map_err(|err| {
                if let Some(domain) = err.downcast_ref::<DomainError>() {
                    return *domain;
                }
                error!(
                    ?err,
                    market_address = %input.market_address.0,
                    "resolve_for_cancel_batch failed (non-domain)",
                );
                DomainError::Unexpected
            })?;

        // Zero rows matched — atomic validation collapses to the
        // single ambiguous code per single-cancel's contract. Log the
        // discriminator so ops can tell "all 50 unknown" from "1 of
        // 50 misowned" and detect probe-style abuse without leaking
        // the distinction to the caller.
        let Some(resolution) = resolution else {
            warn!(
                phase = "resolution-empty",
                pn = %input.trading_pn.pn_address,
                market_address = %input.market_address.0,
                symbol = %input.symbol.0,
                input_count = input.order_ids.len(),
                "cancel_batch resolution returned no rows",
            );
            return Err(DomainError::UnknownOrder);
        };

        let CancelBatchResolution {
            event_id,
            oracle_list_hash,
            token_type,
            market_status,
            mut orders,
        } = resolution;

        // Shortfall collapses to the single ambiguous code, peer of
        // single-cancel. Trait contract guarantees every key in
        // `orders` is a member of `input.order_ids`, so by pigeonhole
        // `len == input.len()` implies the keys are exactly the input
        // set (no overage possible — HashMap dedups on PK). Peer log
        // to `resolution-empty` so ops can distinguish a partial
        // shortfall (e.g. mixed-ownership probe) from a wholesale
        // miss.
        if orders.len() < input.order_ids.len() {
            warn!(
                phase = "resolution-shortfall",
                pn = %input.trading_pn.pn_address,
                market_address = %input.market_address.0,
                symbol = %input.symbol.0,
                resolved_count = orders.len(),
                input_count = input.order_ids.len(),
                "cancel_batch resolution returned fewer rows than requested",
            );
            return Err(DomainError::UnknownOrder);
        }

        // Close the race between `resolve_for_new_order` and the bulk
        // SELECT: a reconciler commit between the two MVCC snapshots
        // could flip the market out of `Trading`.
        if market_status != MarketStatus::Trading {
            return Err(DomainError::OrderValidationFailed);
        }
        if oracle_list_hash.is_empty() {
            // Peer of the other MarketInconsistent branches in this
            // use case: log at use-case level so the symbol/order_ids
            // context isn't lost in the infra-layer warn.
            warn!(
                market_address = %input.market_address.0,
                symbol = %input.symbol.0,
                input_count = input.order_ids.len(),
                "cancel_batch resolution carries empty oracle_list_hash",
            );
            return Err(DomainError::MarketInconsistent);
        }
        let token_type = u32::try_from(token_type).map_err(|_| {
            error!(
                market_address = %input.market_address.0,
                symbol = %input.symbol.0,
                token_type,
                "resolve_for_cancel_batch: token_type does not fit u32",
            );
            DomainError::MarketInconsistent
        })?;

        // Look each input id up against the resolution map. Trait
        // contract + shortfall gate above mean every id MUST be
        // present; a `None` here is a contract violation
        // (MarketInconsistent rather than panic). Lookup preserves
        // `input.order_ids` ordering for the response — no positional
        // pairing on a separate index lives anywhere on the wire.
        //
        // Capture the resolution size BEFORE the lookup loop so the
        // contract-violation warn reports the size the impl actually
        // returned, not the count remaining after earlier `.remove`
        // calls have drained the map.
        let original_rows_len = orders.len();
        let items: Vec<CancelBatchPayloadItem> = input
            .order_ids
            .iter()
            .map(|&order_id| {
                orders
                    .remove(&order_id)
                    .map(|order| CancelBatchPayloadItem {
                        order_id,
                        client_order_id: order.client_order_id,
                    })
                    .ok_or_else(|| {
                        warn!(
                            market_address = %input.market_address.0,
                            symbol = %input.symbol.0,
                            rows_len = original_rows_len,
                            input_count = input.order_ids.len(),
                            missing_order_id = order_id,
                            "resolve_for_cancel_batch missing input order_id (trait contract violated)",
                        );
                        DomainError::MarketInconsistent
                    })
            })
            .collect::<Result<_, _>>()?;
        let response: Vec<CancelledBatchOrder> = items
            .iter()
            .map(|item| CancelledBatchOrder {
                order_id: item.order_id,
                client_order_id: item.client_order_id.clone(),
            })
            .collect();

        let payload = CancelBatchOrderPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            items,
        };
        self.sender.cancel_batch_order(payload).await?;

        Ok(response)
    }
}

/// Orchestrates `POST /api/v1/buyFullSet`: resolves the market (gate
/// `AWAITING_FREEZE | TRADING` per [api-spec §Buy Full Set]), looks up
/// the quote asset's on-chain `decimals` so the public `collateral`
/// decimal string can be lifted into the ABI's `uint128`, validates the
/// value as strictly positive and within `u64::MAX` (the same
/// `serde_json::json!` ceiling that bounds `placeOrder.amount` —
/// [write-api.md §clientOrderId generation](../../docs/tech-specs/write-api.md#clientorderid-generation)),
/// then dispatches a single `PrivateNote.splitFullSet` external message.
/// On `AWAITING_FREEZE` the first successful call also activates the
/// OrderBook for everyone else (chain-side effect of `splitFullSet`);
/// from the caller's standpoint the request and response are identical
/// to any later call.
pub struct BuyFullSetUseCase<R, F, S> {
    repo: R,
    refs: F,
    sender: S,
}

impl<R, F, S> BuyFullSetUseCase<R, F, S> {
    pub fn new(repo: R, refs: F, sender: S) -> Self {
        Self { repo, refs, sender }
    }
}

impl<R, F, S> BuyFullSetUseCase<R, F, S>
where
    R: MarketReadRepository,
    F: ReferenceRepository,
    S: ChainOrderSender,
{
    pub async fn execute(&self, input: BuyFullSetInput) -> Result<(), DomainError> {
        let MarketForBuyFullSet { event_id, oracle_list_hash, token_type, status } = self
            .repo
            .resolve_for_buy_full_set(&input.market_address, input.now_seconds)
            .await
            .map_err(|err| {
                if let Some(domain) = err.downcast_ref::<DomainError>() {
                    return *domain;
                }
                error!(?err, market_address = %input.market_address.0, "resolve_for_buy_full_set failed (non-domain)");
                DomainError::Unexpected
            })?;

        // api-spec §Buy Full Set: available while the market is in
        // `AWAITING_FREEZE` or `TRADING`. Every other `MarketStatus`
        // variant (Pending / Upcoming / Staking / Resolving / Resolved
        // / Cancelled / Expired) collapses to -2010. Log so an ops
        // incident triaged by `marketAddress` shows the actual phase
        // rather than just the wire code.
        if !matches!(status, MarketStatus::AwaitingFreeze | MarketStatus::Trading) {
            warn!(
                market_address = %input.market_address.0,
                ?status,
                "buyFullSet rejected: market not in AWAITING_FREEZE/TRADING",
            );
            return Err(DomainError::OrderValidationFailed);
        }

        // Defence-in-depth: the Postgres impl lifts NULL/blank
        // `oracle_list_hash` on a reconciled row to MarketInconsistent
        // already; this second-line guard keeps a future repo regression
        // from pushing a zero-hash submission to chain. Log if hit —
        // it means the repo's own guard let one through.
        if oracle_list_hash.is_empty() {
            warn!(
                market_address = %input.market_address.0,
                event_id = %event_id,
                token_type,
                "buyFullSet: blank oracle_list_hash slipped past the repo guard",
            );
            return Err(DomainError::MarketInconsistent);
        }

        // Quote asset's on-chain `decimals` come from `ref_tokens`,
        // keyed by the same `token_type` used in the ABI call. An
        // unknown token_type means read-model corruption (the initial
        // migration seeds the canonical set); 503 is the right surface.
        let token = self
            .refs
            .lookup_ref_token(token_type)
            .await
            .map_err(|err| {
                if let Some(domain) = err.downcast_ref::<DomainError>() {
                    return *domain;
                }
                error!(
                    ?err,
                    market_address = %input.market_address.0,
                    token_type,
                    "lookup_ref_token failed (non-domain)",
                );
                DomainError::Unexpected
            })?
            .ok_or_else(|| {
                warn!(
                    market_address = %input.market_address.0,
                    token_type,
                    "market token_type missing from ref_tokens",
                );
                DomainError::MarketInconsistent
            })?;

        // Three gates below all surface as -1130 per
        // `docs/tech-specs/write-api.md §POST /api/v1/buyFullSet
        // §Input validation`. The spec doc is the single source of
        // truth for why; comments here only name the gate they enforce.
        //
        // 1. quote-asset precision → remap PrecisionExceeded to
        //    InvalidParameter (-1130, not -1111: it is not part of
        //    api-spec Validation Rules);
        // 2. strictly positive (zero parses cleanly through
        //    `lift_decimal`);
        // 3. fits in u64 (upstream `serde_json::json!` ceiling — see
        //    `write-api.md §clientOrderId generation`).
        let collateral_lifted =
            lift_decimal(&input.collateral, token.decimals).map_err(|e| match e {
                DomainError::PrecisionExceeded => DomainError::InvalidParameter,
                other => other,
            })?;
        if collateral_lifted == BigUint::from(0u32) {
            return Err(DomainError::InvalidParameter);
        }
        if collateral_lifted > BigUint::from(u64::MAX) {
            return Err(DomainError::InvalidParameter);
        }
        let collateral_raw = collateral_lifted.to_str_radix(10);

        let payload = SplitFullSetPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            collateral_raw,
        };
        self.sender.split_full_set(payload).await
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
        live_orders: Vec<FakeCancelableOrder>,
        /// Lets a test simulate a race between `resolve_for_new_order`
        /// and `resolve_for_cancel_batch`: `self.market.status` answers
        /// the first call (placement-shape snapshot), this override
        /// answers the second (order-resolution snapshot). `None`
        /// means "same status as the market" — the no-race default.
        cancel_batch_status_override: Option<MarketStatus>,
        /// When `Some(rogue_id)`, `resolve_for_cancel_batch` replaces
        /// the first matched key with `rogue_id`. Models the
        /// trait-contract violation where an impl returns a key not
        /// present in `input.order_ids` despite the documented
        /// `WHERE lo.order_id = ANY($3)` guarantee. The use case must
        /// raise MarketInconsistent before any chain dispatch.
        cancel_batch_rogue_key: Option<u64>,
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
                live_orders: Vec::new(),
                cancel_batch_status_override: None,
                cancel_batch_rogue_key: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn empty() -> Self {
            Self {
                market: None,
                cancelable_order: None,
                live_orders: Vec::new(),
                cancel_batch_status_override: None,
                cancel_batch_rogue_key: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn with_cancelable_order(market: Market, order: FakeCancelableOrder) -> Self {
            Self {
                market: Some(market),
                cancelable_order: Some(order),
                live_orders: Vec::new(),
                cancel_batch_status_override: None,
                cancel_batch_rogue_key: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        fn with_live_orders(market: Market, orders: Vec<FakeCancelableOrder>) -> Self {
            Self {
                market: Some(market),
                cancelable_order: None,
                live_orders: orders,
                cancel_batch_status_override: None,
                cancel_batch_rogue_key: None,
                orders_response: empty_orders_page(),
                recorded_orders_queries: Mutex::new(Vec::new()),
            }
        }

        /// Builder that ONLY affects the bulk-cancel SELECT's status,
        /// leaving `resolve_for_new_order` to answer with the market's
        /// declared status. Models the inter-SELECT race directly.
        fn with_cancel_batch_status(mut self, status: MarketStatus) -> Self {
            self.cancel_batch_status_override = Some(status);
            self
        }

        fn with_cancel_batch_rogue_key(mut self, rogue_id: u64) -> Self {
            self.cancel_batch_rogue_key = Some(rogue_id);
            self
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
            let token_type = u32::try_from(market.token_type)
                .map_err(|_| anyhow::anyhow!(DomainError::MarketInconsistent))?;
            Ok(MarketForPlacement {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type,
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
            let token_type = u32::try_from(market.token_type)
                .map_err(|_| anyhow::anyhow!(DomainError::MarketInconsistent))?;
            Ok(OrderForCancel {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type,
                market_status: market.status,
                client_order_id: order.client_order_id,
            })
        }

        async fn resolve_for_cancel_batch(
            &self,
            _: &MarketAddress,
            symbol: &Symbol,
            order_ids: &[u64],
            owner_pn_address: &str,
            _: i64,
        ) -> Result<Option<CancelBatchResolution>, anyhow::Error> {
            // Mirror Postgres impl: any predicate miss simply yields
            // fewer rows than asked. The use case promotes a shortfall
            // to `UnknownOrder` for the whole batch; zero matches
            // (no market, no symbol, no rows) surface as `None`.
            let Some(market) = self.market.clone() else {
                return Ok(None);
            };
            if !market.outcomes.iter().any(|o| o.symbol == *symbol) {
                return Ok(None);
            }
            let mut orders: HashMap<u64, OrderForCancelBatch> = order_ids
                .iter()
                .filter_map(|&id| {
                    self.live_orders
                        .iter()
                        .find(|o| o.order_id == id && o.owner_pn_address == owner_pn_address)
                        .map(|o| {
                            (
                                id,
                                OrderForCancelBatch {
                                    // Mirror Postgres: NULL or whitespace-only
                                    // `client_order_id` demotes to `None` so the
                                    // fake doesn't hide trim drift from unit tests.
                                    client_order_id: o.client_order_id.as_ref().and_then(|raw| {
                                        let trimmed = raw.trim();
                                        (!trimmed.is_empty()).then(|| trimmed.to_string())
                                    }),
                                },
                            )
                        })
                })
                .collect();
            if orders.is_empty() {
                return Ok(None);
            }
            if let Some(rogue_id) = self.cancel_batch_rogue_key {
                // Two preconditions the test fixture must hold for
                // the rogue swap to actually exercise the use-case's
                // contract-violation gate (instead of silently
                // collapsing into a shortfall):
                //   1. `order_ids[0]` matched a live_orders row, so
                //      there is something to swap out.
                //   2. `rogue_id` is NOT itself in `input.order_ids`
                //      — otherwise `insert(rogue_id, row)` would
                //      overwrite a legitimate match, shrinking
                //      orders.len() and tripping the shortfall gate
                //      first.
                // Both are easy to get wrong by parameterising over
                // ids; panic here so a misconfigured test surfaces
                // loudly instead of passing for the wrong reason.
                let first_id = *order_ids
                    .first()
                    .expect("with_cancel_batch_rogue_key requires non-empty order_ids");
                let row = orders.remove(&first_id).expect(
                    "with_cancel_batch_rogue_key requires order_ids[0] to match a live_order",
                );
                assert!(
                    !order_ids.contains(&rogue_id),
                    "with_cancel_batch_rogue_key requires rogue_id ({rogue_id}) to be \
                     distinct from every entry in order_ids — otherwise the swap collides \
                     with a real match and the test exercises the shortfall gate instead \
                     of the contract-violation gate",
                );
                orders.insert(rogue_id, row);
            }
            Ok(Some(CancelBatchResolution {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type: market.token_type,
                market_status: self.cancel_batch_status_override.unwrap_or(market.status),
                orders,
            }))
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

        async fn resolve_for_buy_full_set(
            &self,
            _: &MarketAddress,
            _: i64,
        ) -> Result<MarketForBuyFullSet, anyhow::Error> {
            // Projects the seeded `Market` down to the slim shape the
            // buyFullSet use case consumes. Mirrors
            // `resolve_for_new_order`'s miss behaviour so tests can reuse
            // the same `with_market` / `without_market` seeding helpers.
            let Some(market) = self.market.clone() else {
                return Err(anyhow::anyhow!(DomainError::InvalidMarketOrSymbol));
            };
            let token_type = u32::try_from(market.token_type)
                .map_err(|_| anyhow::anyhow!(DomainError::MarketInconsistent))?;
            Ok(MarketForBuyFullSet {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type,
                status: market.status,
            })
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
        recorded_cancel_batches: Mutex<Vec<CancelBatchOrderPayload>>,
        recorded_full_sets: Mutex<Vec<SplitFullSetPayload>>,
        fail_with: Option<DomainError>,
    }

    impl FakeSender {
        fn ok() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                recorded_batches: Mutex::new(Vec::new()),
                recorded_cancel_batches: Mutex::new(Vec::new()),
                recorded_full_sets: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(err: DomainError) -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                recorded_batches: Mutex::new(Vec::new()),
                recorded_cancel_batches: Mutex::new(Vec::new()),
                recorded_full_sets: Mutex::new(Vec::new()),
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

        fn cancel_batch_calls(&self) -> Vec<CancelBatchOrderPayload> {
            self.recorded_cancel_batches.lock().unwrap().clone()
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

        async fn cancel_batch_order(
            &self,
            payload: CancelBatchOrderPayload,
        ) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded_cancel_batches.lock().unwrap().push(payload);
            Ok(())
        }

        async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded_full_sets.lock().unwrap().push(payload);
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
            maker_commission: dodex_domain::MAKER_COMMISSION.to_string(),
            taker_commission: dodex_domain::TAKER_COMMISSION.to_string(),
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
        // (`ackinacki-kit` → `serde_json::json!` without
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
        // The generator MUST stay inside u64: kit / `serde_json`
        // panic on serialize for values above u64::MAX, so a
        // `Uuid::new_v4().as_u128()` regression would crash the worker
        // ~50 % of the time. 256 samples is more than enough to
        // surface that regression.
        for _ in 0..256 {
            let coid = generate_client_order_id();
            assert!(
                coid.parse::<u64>().is_ok(),
                "generated coid {coid:?} does not fit in u64 — would panic in dodex_chain::Dex::place_order",
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

    // ---- CancelBatchOrdersUseCase ----

    fn base_cancel_batch_input(symbol: &str, order_ids: Vec<u64>) -> CancelBatchOrdersInput {
        CancelBatchOrdersInput {
            trading_pn: TradingPn {
                pn_address: "0:pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            market_address: MarketAddress("0:market".into()),
            symbol: Symbol(symbol.into()),
            order_ids,
            now_seconds: 1_000,
            now_ms: 1_000_000,
        }
    }

    fn live_order(order_id: u64, coid: Option<&str>) -> FakeCancelableOrder {
        FakeCancelableOrder {
            order_id,
            owner_pn_address: "0:pn".into(),
            client_order_id: coid.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn cancel_batch_orders_happy_path_two_items() {
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let repo = FakeRepo::with_live_orders(
            market,
            vec![live_order(123, Some("42")), live_order(456, None)],
        );
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let out = uc
            .execute(base_cancel_batch_input("PM-YES", vec![123, 456]))
            .await
            .expect("happy path");

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].order_id, 123);
        assert_eq!(out[0].client_order_id.as_deref(), Some("42"));
        assert_eq!(out[1].order_id, 456);
        assert_eq!(out[1].client_order_id, None);

        let calls = sender.cancel_batch_calls();
        assert_eq!(calls.len(), 1);
        let p = &calls[0];
        assert_eq!(p.pn_address, "0:pn");
        assert_eq!(p.event_id, "0xevent");
        assert_eq!(p.oracle_list_hash, "0xdead");
        assert_eq!(p.token_type, 1);
        let order_ids: Vec<u64> = p.items.iter().map(|i| i.order_id).collect();
        assert_eq!(order_ids, vec![123, 456]);
        // Audit-only field: mirrors the response coids in input order so
        // ops can grep the cancel-batch incident by clientOrderId. Pins
        // both the alignment (position i ↔ items[i].order_id) and the
        // None-on-NULL contract for orders placed without a coid.
        let coids: Vec<Option<String>> =
            p.items.iter().map(|i| i.client_order_id.clone()).collect();
        assert_eq!(coids, vec![Some("42".to_string()), None]);
    }

    #[tokio::test]
    async fn cancel_batch_orders_preserves_input_order_in_response() {
        // SQL has no ordering guarantee — the use case must reorder rows
        // by the input id sequence so callers can correlate positionally.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let repo = FakeRepo::with_live_orders(
            market,
            vec![live_order(11, Some("a")), live_order(22, Some("b")), live_order(33, Some("c"))],
        );
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        // Input: 33, 11, 22 (deliberately scrambled).
        let out = uc.execute(base_cancel_batch_input("PM-YES", vec![33, 11, 22])).await.unwrap();

        assert_eq!(out.iter().map(|o| o.order_id).collect::<Vec<_>>(), vec![33, 11, 22]);
        assert_eq!(
            out.iter().map(|o| o.client_order_id.clone()).collect::<Vec<_>>(),
            vec![Some("c".into()), Some("a".into()), Some("b".into())],
        );
        // Chain payload must carry the input order verbatim.
        let p_ids: Vec<u64> =
            sender.cancel_batch_calls()[0].items.iter().map(|i| i.order_id).collect();
        assert_eq!(p_ids, vec![33, 11, 22]);
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_empty_batch() {
        // Pre-flight: an empty `orderIds[]` would reach the chain as
        // ERR_EMPTY_BATCH (162); failing here saves the round-trip.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CancelBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![])).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_accepts_exactly_max_batch_size() {
        // Boundary pin: `outcome.max_batch_size` ids must succeed.
        // Catches a future off-by-one (e.g. `>=` instead of `>`) at the
        // cap check that would reject the boundary value.
        let market = trading_market("PM-YES");
        let max = test_outcome("PM-YES").max_batch_size as usize;
        let sender = Arc::new(FakeSender::ok());
        let live: Vec<FakeCancelableOrder> =
            (0..max).map(|i| live_order(1000 + i as u64, None)).collect();
        let repo = FakeRepo::with_live_orders(market, live);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let ids: Vec<u64> = (0..max).map(|i| 1000 + i as u64).collect();
        let out = uc
            .execute(base_cancel_batch_input("PM-YES", ids.clone()))
            .await
            .expect("max size accepted");
        assert_eq!(out.len(), max);
        let dispatched: Vec<u64> =
            sender.cancel_batch_calls()[0].items.iter().map(|i| i.order_id).collect();
        assert_eq!(dispatched, ids);
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_above_max_batch_size() {
        // Boundary pin from the other side: `max + 1` ids must fail
        // locally with -1130 instead of paying a chain
        // ERR_BATCH_TOO_LARGE round-trip. Pairs with
        // `cancel_batch_orders_accepts_exactly_max_batch_size`.
        let market = trading_market("PM-YES");
        let max = test_outcome("PM-YES").max_batch_size as usize;
        let sender = Arc::new(FakeSender::ok());
        let uc = CancelBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let ids: Vec<u64> = (0..=max).map(|i| 1 + i as u64).collect();
        let err = uc.execute(base_cancel_batch_input("PM-YES", ids)).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_intra_batch_duplicates() {
        // Two PENDING_CANCEL receipts for the same id would be useless
        // and would also waste a MAX_BATCH_SIZE slot — reject before
        // chain submission.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let uc = CancelBatchOrdersUseCase::new(FakeRepo::with(market), sender.clone());

        let err =
            uc.execute(base_cancel_batch_input("PM-YES", vec![10, 20, 10])).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_when_market_not_trading() {
        let mut market = trading_market("PM-YES");
        market.status = MarketStatus::Resolving;
        let sender = Arc::new(FakeSender::ok());
        let repo =
            FakeRepo::with_live_orders(market, vec![live_order(1, None), live_order(2, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1, 2])).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_when_market_flips_between_selects() {
        // `resolve_for_new_order` sees `Trading` (the placement-shape
        // snapshot); `resolve_for_cancel_batch` then returns rows
        // tagged `Resolving` (the order-resolution snapshot). Without
        // the post-SELECT status re-check the request would dispatch
        // `cancelBatch` against a market that single-cancel would
        // reject — single-cancel does both reads in one atomic JOIN
        // and naturally sees the later state. Pins the race-window
        // closure.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let repo =
            FakeRepo::with_live_orders(market, vec![live_order(1, None), live_order(2, None)])
                .with_cancel_batch_status(MarketStatus::Resolving);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1, 2])).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_when_market_missing_oracle_list_hash() {
        let mut market = trading_market("PM-YES");
        market.oracle_list_hash = String::new();
        let sender = Arc::new(FakeSender::ok());
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_negative_token_type_as_market_inconsistent() {
        // `markets.token_type` is `i32` in the schema but the chain ABI
        // takes `uint32`. A negative read-model value is corruption and
        // must collapse to `MarketInconsistent` (503 / -1500) via the
        // `u32::try_from` arm — never panic, never reach chain dispatch.
        let mut market = trading_market("PM-YES");
        market.token_type = -1;
        let sender = Arc::new(FakeSender::ok());
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_unknown_when_any_id_missing() {
        // Atomic validation: one shortfall rejects the whole batch.
        // No chain message is sent.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1, 2])).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_unknown_when_owner_mismatch() {
        // Wrong-owner case MUST NOT differ from "no such order" — the
        // existence of another account's order would otherwise leak
        // through the error code.
        let market = trading_market("PM-YES");
        let foreign = FakeCancelableOrder {
            order_id: 1,
            owner_pn_address: "0:someone-else".into(),
            client_order_id: None,
        };
        let sender = Arc::new(FakeSender::ok());
        let uc = CancelBatchOrdersUseCase::new(
            FakeRepo::with_live_orders(market, vec![foreign]),
            sender.clone(),
        );

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_rejects_rogue_key_as_market_inconsistent() {
        // Trait contract: every key in the returned map MUST be in
        // `input.order_ids`. Postgres enforces this with the
        // `WHERE lo.order_id = ANY($3)` predicate, but an impl that
        // bypasses it would silently feed an unrelated order_id into
        // the chain payload. The use case's per-id lookup raises
        // MarketInconsistent before any chain dispatch.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::ok());
        let repo =
            FakeRepo::with_live_orders(market, vec![live_order(1, Some("a")), live_order(2, None)])
                .with_cancel_batch_rogue_key(999);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1, 2])).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
        assert!(sender.cancel_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_orders_propagates_sender_pn_busy() {
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::failing(DomainError::OrderPnBusy));
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::OrderPnBusy);
    }

    #[tokio::test]
    async fn cancel_batch_orders_propagates_sender_request_timeout() {
        // Mirrors the elapsed branch of `classify_chain_outcome`: the
        // chain leg outran `cancel_batch_timeout_ms`. The use case must
        // pass it through unchanged so the HTTP layer can render the
        // documented 504 / -1007.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::failing(DomainError::RequestTimeout));
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::RequestTimeout);
    }

    #[tokio::test]
    async fn cancel_batch_orders_propagates_sender_unexpected() {
        // Unmapped chain `tvm_exit` codes and gateway transport failures
        // surface as `Unexpected` from `classify_chain_outcome`; same
        // pass-through contract as the other two sender errors.
        let market = trading_market("PM-YES");
        let sender = Arc::new(FakeSender::failing(DomainError::Unexpected));
        let repo = FakeRepo::with_live_orders(market, vec![live_order(1, None)]);
        let uc = CancelBatchOrdersUseCase::new(repo, sender.clone());

        let err = uc.execute(base_cancel_batch_input("PM-YES", vec![1])).await.unwrap_err();
        assert_eq!(err, DomainError::Unexpected);
    }
}

#[cfg(test)]
mod get_account_use_case_tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    struct StubPn {
        details: Result<PnDetails, String>,
    }

    #[async_trait]
    impl PnStateReader for StubPn {
        async fn get_details(&self, _pn_address: &str) -> anyhow::Result<PnDetails> {
            self.details.clone().map_err(|e| anyhow::anyhow!(e))
        }

        async fn get_stake(&self, _pn: &str, _hash: &str) -> anyhow::Result<Option<PnStake>> {
            unreachable!("get_account never calls get_stake")
        }
    }

    struct StubRefs {
        rows: Mutex<std::collections::HashMap<u32, RefToken>>,
    }

    #[async_trait]
    impl ReferenceRepository for StubRefs {
        async fn lookup_ref_token(&self, t: u32) -> anyhow::Result<Option<RefToken>> {
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
                    (3, "3750000000".to_string()), // 3_750 USDC locked
                    (1, "1500000000".to_string()), // 1.5 NACKL locked
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
        assert_eq!(out.balances[0].locked, "0.000000000");
    }

    #[tokio::test]
    async fn free_defaults_to_zero_when_key_appears_only_in_locked_map() {
        // A LIMIT SELL has consumed the entire free balance: the chain map
        // `_balance` no longer carries the tokenType (or carries it as 0
        // but pruned by the contract), yet `_lockedInOrders[tokenType] > 0`.
        // The response must still include the asset with free="0" so the
        // user sees what they still own — iterating `_balance` alone would
        // silently drop it.
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![],
                locked_in_orders: vec![(1, "2500000000".to_string())], // 2.5 NACKL
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
        assert_eq!(out.balances.len(), 1);
        assert_eq!(out.balances[0].asset, "NACKL");
        assert_eq!(out.balances[0].free, "0.000000000");
        assert_eq!(out.balances[0].locked, "2.500000000");
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
    async fn duplicate_token_type_in_balance_yields_market_inconsistent() {
        // Chain emits `_balance` as map(uint32 → uint128), so a duplicate key
        // cannot occur in a healthy reply. If a parser bug produces one, the
        // use case must fail closed rather than silently letting the last
        // write win and corrupting `free`.
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![(1, "10000000000".to_string()), (1, "999999999".to_string())],
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
    async fn duplicate_token_type_in_locked_yields_market_inconsistent() {
        let pn = StubPn {
            details: Ok(PnDetails {
                balance: vec![(1, "10000000000".to_string())],
                locked_in_orders: vec![(1, "1500000000".to_string()), (1, "999999999".to_string())],
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

    #[tokio::test]
    async fn pn_reader_account_not_deployed_preserves_typed_variant() {
        // The reader signals "Account::is_none" with a typed
        // DomainError::AccountNotDeployed; the use case must preserve
        // the variant through its map_err chain so the API surface
        // serves a 404 instead of collapsing to 503 / MarketInconsistent.
        struct TypedStubPn;
        #[async_trait]
        impl PnStateReader for TypedStubPn {
            async fn get_details(&self, _pn_address: &str) -> anyhow::Result<PnDetails> {
                Err(anyhow::Error::from(DomainError::AccountNotDeployed))
            }

            async fn get_stake(&self, _pn: &str, _hash: &str) -> anyhow::Result<Option<PnStake>> {
                unreachable!()
            }
        }
        let uc = GetAccountUseCase::new(TypedStubPn, make_refs());
        let err = uc
            .execute(GetAccountInput {
                account_id: uuid::Uuid::nil(),
                pn_address: "0:pn".into(),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::AccountNotDeployed));
    }

    #[test]
    fn scale_decimal_pads_to_full_precision() {
        assert_eq!(scale_decimal("10000000000", 9).unwrap(), "10.000000000");
        assert_eq!(scale_decimal("1500000000", 9).unwrap(), "1.500000000");
        assert_eq!(scale_decimal("1", 9).unwrap(), "0.000000001");
        assert_eq!(scale_decimal("0", 9).unwrap(), "0.000000000");
        assert_eq!(scale_decimal("", 9).unwrap(), "0.000000000");
        assert_eq!(scale_decimal("25000000000", 6).unwrap(), "25000.000000");
        // decimals=0 still emits a decimal point so the wire format stays
        // `^[0-9]+\.[0-9]+$` regardless of the token's precision.
        assert_eq!(scale_decimal("42", 0).unwrap(), "42.0");
        assert_eq!(scale_decimal("0", 0).unwrap(), "0.0");
        assert_eq!(scale_decimal("", 0).unwrap(), "0.0");
    }

    #[test]
    fn scale_decimal_handles_branch_boundary() {
        // raw.len() == decimals is the seam between the two branches:
        // the `<=` branch produces "0.<raw>" with zero leading-zero padding.
        assert_eq!(scale_decimal("123456789", 9).unwrap(), "0.123456789");
    }

    #[test]
    fn scale_decimal_rejects_decimals_above_max() {
        // Mirrors infra's MAX_DECIMAL_PRECISION cap: a corrupt
        // `ref_tokens` row with decimals beyond NUMERIC(38, …) would
        // make `scale_decimal` allocate hundreds of bytes per balance.
        // Cap kicks in before the allocation; values exactly at the
        // limit still pass.
        assert_eq!(scale_decimal("1", 38).unwrap(), format!("0.{}1", "0".repeat(37)));
        let err = scale_decimal("1", 39).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
        let err = scale_decimal("1", 200).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
    }

    #[test]
    fn scale_decimal_rejects_non_digit_input() {
        // Without entry validation, byte-level slicing in the `>` branch
        // would either return garbage or panic on a UTF-8 split inside a
        // multibyte char (e.g. raw="1é", decimals=2 splits "é" in half).
        let err = scale_decimal("abc", 9).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
        let err = scale_decimal("1é", 2).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
        let err = scale_decimal("-1", 9).unwrap_err();
        assert!(matches!(err, DomainError::MarketInconsistent));
    }

    #[test]
    fn scale_decimal_strips_leading_zeros() {
        // BigUint::from_str accepts zero-padded literals, so a future
        // tvm_abi version that emits fixed-width uint128 amounts would
        // reach the slicing branch with leading zeros and emit a
        // non-canonical decimal. Canonicalisation must strip them on
        // every branch.
        assert_eq!(scale_decimal("00012345", 3).unwrap(), "12.345");
        assert_eq!(scale_decimal("000", 9).unwrap(), "0.000000000");
        assert_eq!(scale_decimal("0001500000000", 9).unwrap(), "1.500000000");
        assert_eq!(scale_decimal("00", 0).unwrap(), "0.0");
    }
}

#[cfg(test)]
mod balances_port_tests {
    use super::*;

    // Compile-time guard: passes iff the trait is dyn-compatible and the
    // value-object fields have the expected names and types. Behavioural
    // coverage lives in get_market_balances_use_case_tests.
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

#[cfg(test)]
mod get_market_balances_use_case_tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    struct StubPn {
        stake: Mutex<Option<PnStake>>,
        last_hash: Mutex<Option<String>>,
        fail: bool,
    }

    #[async_trait]
    impl PnStateReader for StubPn {
        async fn get_details(&self, _: &str) -> anyhow::Result<PnDetails> {
            unreachable!("balances use case never calls get_details")
        }

        async fn get_stake(&self, _pn: &str, hash: &str) -> anyhow::Result<Option<PnStake>> {
            if self.fail {
                anyhow::bail!("gateway down")
            }
            *self.last_hash.lock().unwrap() = Some(hash.to_string());
            Ok(self.stake.lock().unwrap().clone())
        }
    }

    struct StubRepo {
        resolution: Mutex<Result<MarketBalancesResolution, dodex_domain::DomainError>>,
        sums: Mutex<std::collections::HashMap<u32, String>>,
    }

    #[async_trait]
    impl MarketReadRepository for StubRepo {
        async fn list_markets(&self, _: &MarketsRequest) -> anyhow::Result<MarketsPage> {
            unreachable!()
        }

        async fn get_depth(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: u16,
        ) -> anyhow::Result<DepthSnapshot> {
            unreachable!()
        }

        async fn resolve_for_new_order(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: i64,
        ) -> anyhow::Result<MarketForPlacement> {
            unreachable!()
        }

        async fn resolve_for_cancel(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: u64,
            _: &str,
            _: i64,
        ) -> anyhow::Result<OrderForCancel> {
            unreachable!()
        }

        async fn resolve_for_cancel_batch(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: &[u64],
            _: &str,
            _: i64,
        ) -> anyhow::Result<Option<CancelBatchResolution>> {
            unreachable!()
        }

        async fn list_orders(&self, _: &OrdersQuery) -> anyhow::Result<OrdersPage> {
            unreachable!()
        }

        async fn resolve_market_for_balances(
            &self,
            _: &MarketAddress,
        ) -> anyhow::Result<MarketBalancesResolution> {
            self.resolution.lock().unwrap().clone().map_err(anyhow::Error::from)
        }

        async fn resolve_for_buy_full_set(
            &self,
            _: &MarketAddress,
            _: i64,
        ) -> anyhow::Result<MarketForBuyFullSet> {
            unreachable!("balances use case does not exercise resolve_for_buy_full_set")
        }

        async fn sum_open_sell_remaining(
            &self,
            _: &str,
            _: &str,
        ) -> anyhow::Result<std::collections::HashMap<u32, String>> {
            Ok(self.sums.lock().unwrap().clone())
        }
    }

    fn make_resolution(num_outcomes: u32) -> MarketBalancesResolution {
        let outcomes: Vec<_> = (0..num_outcomes)
            .map(|i| BalanceOutcome {
                outcome_id: i,
                symbol: Symbol(format!("X-{i}")),
                quantity_precision: 2,
            })
            .collect();
        MarketBalancesResolution {
            event_id: "1".into(),
            oracle_list_hash: "2".into(),
            token_type: 1,
            orderbook_address: "0:ob".into(),
            num_outcomes,
            outcomes,
        }
    }

    fn make_pn(stake: Option<PnStake>) -> StubPn {
        StubPn { stake: Mutex::new(stake), last_hash: Mutex::new(None), fail: false }
    }

    // Concrete stake_hash impl plugged in via the use case constructor —
    // the use case doesn't depend on the real hash, only that the same
    // input produces the same string.
    fn stub_hasher(_e: &str, _o: &str, _t: u32) -> Result<String, DomainError> {
        Ok("deadbeef".to_string())
    }

    fn failing_hasher(_e: &str, _o: &str, _t: u32) -> Result<String, DomainError> {
        Err(DomainError::MarketInconsistent)
    }

    #[tokio::test]
    async fn happy_path_sums_three_pools_per_outcome() {
        let stake = PnStake {
            amount: vec!["10".into(), "5".into()],     // outcome 0=10, 1=5
            debt_amount: vec!["0".into(), "1".into()], //         0=0, 1=1
            coupons_amount: vec!["2".into(), "0".into()], //         0=2, 1=0
        };
        let pn = make_pn(Some(stake));
        let mut sums = std::collections::HashMap::new();
        sums.insert(1u32, "100".into()); // outcome 1 has 100 locked
        let repo =
            StubRepo { resolution: Mutex::new(Ok(make_resolution(2))), sums: Mutex::new(sums) };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let out = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .expect("ok");
        assert_eq!(out.balances.len(), 2);
        // outcome 0: free = 10+0+2 = 12, scale=2 → "0.12"; locked=0 → "0.00"
        assert_eq!(out.balances[0].outcome_id, 0);
        assert_eq!(out.balances[0].free, "0.12");
        assert_eq!(out.balances[0].locked_in_orders, "0.00");
        // outcome 1: free = 5+1+0 = 6, scale=2 → "0.06"; locked=100 → "1.00"
        assert_eq!(out.balances[1].outcome_id, 1);
        assert_eq!(out.balances[1].free, "0.06");
        assert_eq!(out.balances[1].locked_in_orders, "1.00");
    }

    #[tokio::test]
    async fn missing_stake_key_yields_zero_free() {
        let pn = make_pn(None); // simulates absent key
        let mut sums = std::collections::HashMap::new();
        sums.insert(0u32, "500".into());
        let repo =
            StubRepo { resolution: Mutex::new(Ok(make_resolution(2))), sums: Mutex::new(sums) };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let out = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .expect("ok");
        assert_eq!(out.balances.len(), 2);
        assert_eq!(out.balances[0].free, "0.00");
        assert_eq!(out.balances[0].locked_in_orders, "5.00");
        assert_eq!(out.balances[1].free, "0.00");
        assert_eq!(out.balances[1].locked_in_orders, "0.00");
    }

    #[tokio::test]
    async fn stake_array_shorter_than_num_outcomes_is_market_inconsistent() {
        let stake = PnStake {
            amount: vec!["1".into()], // only one entry for two outcomes
            debt_amount: vec!["0".into(), "0".into()],
            coupons_amount: vec!["0".into(), "0".into()],
        };
        let pn = make_pn(Some(stake));
        let repo = StubRepo {
            resolution: Mutex::new(Ok(make_resolution(2))),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn stake_array_longer_than_num_outcomes_is_market_inconsistent() {
        // stake arrays have length 3, but num_outcomes = 2 — the
        // invariant check must catch this and return MarketInconsistent.
        let stake = PnStake {
            amount: vec!["1".into(), "2".into(), "3".into()],
            debt_amount: vec!["0".into(), "0".into(), "0".into()],
            coupons_amount: vec!["0".into(), "0".into(), "0".into()],
        };
        let pn = make_pn(Some(stake));
        let repo = StubRepo {
            resolution: Mutex::new(Ok(make_resolution(2))),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn unknown_market_yields_invalid_market_or_symbol() {
        let pn = make_pn(None);
        let repo = StubRepo {
            resolution: Mutex::new(Err(DomainError::InvalidMarketOrSymbol)),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:unknown".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::InvalidMarketOrSymbol));
    }

    #[tokio::test]
    async fn pn_failure_yields_market_inconsistent() {
        let pn = StubPn { stake: Mutex::new(None), last_hash: Mutex::new(None), fail: true };
        let repo = StubRepo {
            resolution: Mutex::new(Ok(make_resolution(2))),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn mixed_empty_and_populated_stake_arrays_is_market_inconsistent() {
        let stake = PnStake {
            amount: vec!["10".into(), "5".into()],
            debt_amount: vec![],
            coupons_amount: vec![],
        };
        let pn = make_pn(Some(stake));
        let repo = StubRepo {
            resolution: Mutex::new(Ok(make_resolution(2))),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn hasher_failure_yields_market_inconsistent() {
        let pn = make_pn(None);
        let repo = StubRepo {
            resolution: Mutex::new(Ok(make_resolution(2))),
            sums: Mutex::new(std::collections::HashMap::new()),
        };
        let uc = GetMarketBalancesUseCase::new(pn, repo, failing_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }

    #[tokio::test]
    async fn out_of_range_outcome_id_yields_market_inconsistent() {
        // sum_open_sell_remaining returns outcome_id=99 which is >= num_outcomes=2.
        // The bounds-check loop must catch this and return MarketInconsistent.
        let pn = make_pn(None);
        let mut sums = std::collections::HashMap::new();
        sums.insert(99u32, "5".into()); // outcome 99 is out of range for a 2-outcome market
        let repo =
            StubRepo { resolution: Mutex::new(Ok(make_resolution(2))), sums: Mutex::new(sums) };
        let uc = GetMarketBalancesUseCase::new(pn, repo, stub_hasher);
        let err = uc
            .execute(GetMarketBalancesInput {
                pn_address: "0:pn".into(),
                market_address: MarketAddress("0:m".into()),
                now_ms: 0,
            })
            .await
            .unwrap_err();
        let dom = err.downcast_ref::<DomainError>().expect("DomainError");
        assert!(matches!(dom, DomainError::MarketInconsistent));
    }
}

#[cfg(test)]
mod buy_full_set_use_case_tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    // Slim `MarketReadRepository` for the buyFullSet path: only the
    // resolver the use case actually calls is implemented. Other methods
    // panic so an accidental coupling regression (the use case taking a
    // second dependency on the repo) surfaces loudly instead of passing
    // for the wrong reason.
    struct BuyFullSetRepo {
        resolution: Result<MarketForBuyFullSet, DomainError>,
    }

    #[async_trait]
    impl MarketReadRepository for BuyFullSetRepo {
        async fn list_markets(&self, _: &MarketsRequest) -> anyhow::Result<MarketsPage> {
            unreachable!()
        }

        async fn get_depth(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: u16,
        ) -> anyhow::Result<DepthSnapshot> {
            unreachable!()
        }

        async fn resolve_for_new_order(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: i64,
        ) -> anyhow::Result<MarketForPlacement> {
            unreachable!()
        }

        async fn resolve_for_cancel(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: u64,
            _: &str,
            _: i64,
        ) -> anyhow::Result<OrderForCancel> {
            unreachable!()
        }

        async fn resolve_for_cancel_batch(
            &self,
            _: &MarketAddress,
            _: &Symbol,
            _: &[u64],
            _: &str,
            _: i64,
        ) -> anyhow::Result<Option<CancelBatchResolution>> {
            unreachable!()
        }

        async fn list_orders(&self, _: &OrdersQuery) -> anyhow::Result<OrdersPage> {
            unreachable!()
        }

        async fn resolve_market_for_balances(
            &self,
            _: &MarketAddress,
        ) -> anyhow::Result<MarketBalancesResolution> {
            unreachable!()
        }

        async fn resolve_for_buy_full_set(
            &self,
            _: &MarketAddress,
            _: i64,
        ) -> anyhow::Result<MarketForBuyFullSet> {
            self.resolution.clone().map_err(anyhow::Error::from)
        }

        async fn sum_open_sell_remaining(
            &self,
            _: &str,
            _: &str,
        ) -> anyhow::Result<std::collections::HashMap<u32, String>> {
            unreachable!()
        }
    }

    struct BuyFullSetRefs {
        token: Option<RefToken>,
        fail: bool,
    }

    #[async_trait]
    impl ReferenceRepository for BuyFullSetRefs {
        async fn lookup_ref_token(&self, _: u32) -> anyhow::Result<Option<RefToken>> {
            if self.fail {
                anyhow::bail!("gateway down")
            }
            Ok(self.token.clone())
        }
    }

    struct BuyFullSetSender {
        recorded: Mutex<Vec<SplitFullSetPayload>>,
        fail_with: Option<DomainError>,
    }

    impl BuyFullSetSender {
        fn ok() -> Self {
            Self { recorded: Mutex::new(Vec::new()), fail_with: None }
        }

        fn failing(err: DomainError) -> Self {
            Self { recorded: Mutex::new(Vec::new()), fail_with: Some(err) }
        }

        fn calls(&self) -> Vec<SplitFullSetPayload> {
            self.recorded.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ChainOrderSender for BuyFullSetSender {
        async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
            unreachable!()
        }

        async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
            unreachable!()
        }

        async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
            unreachable!()
        }

        async fn cancel_batch_order(&self, _: CancelBatchOrderPayload) -> Result<(), DomainError> {
            unreachable!()
        }

        async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError> {
            if let Some(err) = self.fail_with {
                return Err(err);
            }
            self.recorded.lock().unwrap().push(payload);
            Ok(())
        }
    }

    fn ok_resolution(status: MarketStatus) -> MarketForBuyFullSet {
        MarketForBuyFullSet {
            event_id: "0xevent".into(),
            oracle_list_hash: "0xdead".into(),
            // token_type=3 matches the USDC entry in `usdc_refs` below
            // (decimals=6), so the lifted collateral can be asserted to
            // exact-decimal precision in the happy-path tests.
            token_type: 3,
            status,
        }
    }

    fn usdc_refs() -> BuyFullSetRefs {
        BuyFullSetRefs {
            token: Some(RefToken { token_type: 3, token_code: "USDC".into(), decimals: 6 }),
            fail: false,
        }
    }

    fn input(collateral: &str) -> BuyFullSetInput {
        BuyFullSetInput {
            trading_pn: TradingPn {
                pn_address: "0:pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            market_address: MarketAddress("0:market".into()),
            collateral: collateral.into(),
            now_seconds: 1_000,
        }
    }

    #[tokio::test]
    async fn happy_path_on_trading_lifts_collateral_by_quote_decimals() {
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());

        uc.execute(input("1.5")).await.expect("happy path");

        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        let p = &calls[0];
        assert_eq!(p.pn_address, "0:pn");
        assert_eq!(p.pn_pubkey, "1");
        assert_eq!(p.event_id, "0xevent");
        assert_eq!(p.oracle_list_hash, "0xdead");
        assert_eq!(p.token_type, 3);
        // 1.5 lifted by USDC decimals=6 → 1_500_000 raw.
        assert_eq!(p.collateral_raw, "1500000");
    }

    #[tokio::test]
    async fn happy_path_on_awaiting_freeze_dispatches_too() {
        // api-spec §Buy Full Set: AWAITING_FREEZE is explicitly allowed —
        // first successful call activates the OrderBook for the market.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::AwaitingFreeze)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());

        uc.execute(input("10")).await.expect("happy path AWAITING_FREEZE");
        assert_eq!(sender.calls().len(), 1);
    }

    #[tokio::test]
    async fn rejects_non_trading_non_awaiting_freeze_statuses() {
        // Every other lifecycle phase from `MarketStatus` must collapse to
        // OrderValidationFailed (-2010). Spell each out — a future status
        // additive change should force this list to update before silently
        // admitting collateral on a market that should reject.
        for status in [
            MarketStatus::Pending,
            MarketStatus::Upcoming,
            MarketStatus::Staking,
            MarketStatus::Resolving,
            MarketStatus::Resolved,
            MarketStatus::Cancelled,
            MarketStatus::Expired,
        ] {
            let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(status)) };
            let sender = std::sync::Arc::new(BuyFullSetSender::ok());
            let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
            let err = uc.execute(input("10")).await.expect_err("non-trading rejected");
            assert_eq!(err, DomainError::OrderValidationFailed, "status={status:?}");
            assert!(sender.calls().is_empty(), "no dispatch on bad status={status:?}");
        }
    }

    #[tokio::test]
    async fn rejects_missing_market() {
        let repo = BuyFullSetRepo { resolution: Err(DomainError::InvalidMarketOrSymbol) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidMarketOrSymbol);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn rejects_zero_collateral_as_invalid_parameter() {
        // Strictly-positive invariant — `lift_decimal("0", 6)` lifts to
        // zero without erroring; the explicit `> 0` check is what catches
        // it. Without it the chain would accept and refund, burning a
        // PN-busy window for nothing.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        let err = uc.execute(input("0")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn rejects_over_precision_as_invalid_parameter() {
        // api-spec table maps "exceeds quote-asset precision" to -1130
        // (InvalidParameter), not -1111 (PrecisionExceeded). Pin the
        // remap so a future refactor cannot drift back to -1111.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        // USDC decimals=6; 7 fractional digits exceeds precision.
        let err = uc.execute(input("0.0000001")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[tokio::test]
    async fn rejects_non_numeric_collateral_as_invalid_parameter() {
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        let err = uc.execute(input("not-a-number")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[tokio::test]
    async fn rejects_collateral_above_u64_max_as_invalid_parameter() {
        // ackinacki-kit's `serde_json::json!(params)` panics on any
        // `collateral: u128 > u64::MAX` (no arbitrary_precision feature
        // upstream). Surface as -1130 before we hand off to the chain
        // sender; reaching the sender would mean an at-rest user-facing
        // panic instead of a typed error.
        //
        // Pinned at exactly `u64::MAX + 1` so a future regression that
        // flipped `>` to `>=` would be caught by the companion test
        // below (which passes the same gate at the boundary).
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        // u64::MAX + 1 = 18_446_744_073_709_551_616 raw → at 6 decimals
        // that's "18446744073709.551616" in human form. Smallest value
        // the gate must reject.
        let err = uc.execute(input("18446744073709.551616")).await.unwrap_err();
        assert_eq!(err, DomainError::InvalidParameter);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn accepts_collateral_at_u64_max() {
        // Boundary companion: a collateral whose lifted value is
        // exactly `u64::MAX` must still pass the gate. Catches a
        // future off-by-one (`>=` instead of `>`) on the comparison
        // that would reject the boundary value the SDK can actually
        // serialise.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        // u64::MAX = 18_446_744_073_709_551_615 raw → "18446744073709.551615"
        // at 6 decimals.
        uc.execute(input("18446744073709.551615")).await.expect("boundary must pass");
        assert_eq!(sender.calls().len(), 1);
        assert_eq!(sender.calls()[0].collateral_raw, u64::MAX.to_string());
    }

    #[tokio::test]
    async fn unknown_quote_token_type_collapses_to_market_inconsistent() {
        // `lookup_ref_token` returning None means the migration-seeded
        // canonical set does not cover this token_type — read-model
        // corruption, 503.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let refs = BuyFullSetRefs { token: None, fail: false };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, refs, sender.clone());
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
        assert!(sender.calls().is_empty());
    }

    #[tokio::test]
    async fn ref_repo_failure_collapses_to_unexpected() {
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let refs = BuyFullSetRefs { token: None, fail: true };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, refs, sender.clone());
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::Unexpected);
    }

    #[tokio::test]
    async fn empty_oracle_list_hash_collapses_to_market_inconsistent() {
        let mut res = ok_resolution(MarketStatus::Trading);
        res.oracle_list_hash = String::new();
        let repo = BuyFullSetRepo { resolution: Ok(res) };
        let sender = std::sync::Arc::new(BuyFullSetSender::ok());
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender.clone());
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[tokio::test]
    async fn chain_err_low_value_propagates_as_validation_failed() {
        // PN-side `ERR_LOW_VALUE` (102) on splitFullSet (insufficient
        // free quote-asset balance for the requested collateral) is
        // mapped by the infra `map_tvm_exit_code` to
        // `OrderValidationFailed`; verify the use case forwards the
        // sender error verbatim without remapping.
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = BuyFullSetSender::failing(DomainError::OrderValidationFailed);
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender);
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn chain_err_note_busy_propagates_as_pn_busy() {
        let repo = BuyFullSetRepo { resolution: Ok(ok_resolution(MarketStatus::Trading)) };
        let sender = BuyFullSetSender::failing(DomainError::OrderPnBusy);
        let uc = BuyFullSetUseCase::new(repo, usdc_refs(), sender);
        let err = uc.execute(input("10")).await.unwrap_err();
        assert_eq!(err, DomainError::OrderPnBusy);
    }
}
