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
    pub status: MarketStatus,
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
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub limit: u16,
}

pub const ORDERS_DEFAULT_LIMIT: u16 = 100;
pub const ORDERS_MAX_LIMIT: u16 = 500;

/// Caller-supplied filter on order status. `is_all()` means "no filter,
/// every row passes"; otherwise the inner set is the canonical subset
/// of [`OrderStatus`] tokens the caller listed in the request `status`
/// CSV. `PendingNew` and `PendingCancel` are rejected at parse time —
/// both are write-side synthetic statuses and never appear on a
/// `live_orders` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatusSet(std::collections::BTreeSet<OrderStatus>);

impl OrderStatusSet {
    /// Parse the request `status` parameter. `None` or all-whitespace
    /// means "all statuses"; anything else is split on `,`, trimmed,
    /// de-duplicated, and matched against the allow-list. An unknown
    /// token (or `PENDING_NEW` / `PENDING_CANCEL`, which are write-side
    /// only) returns [`DomainError::InvalidParameter`].
    ///
    /// Whitespace-only input is treated as "all statuses" by design.
    /// This is asymmetric with the `cursor` parameter — a
    /// whitespace-only `cursor` is rejected as `MissingParameter` —
    /// because the two parameters express different intents: `status`
    /// is an optional narrowing filter whose absence (any falsy form)
    /// trivially means "no filter applied", while `cursor` is an
    /// opaque server-issued token whose syntactic emptiness is always
    /// a client-side bug. See api-spec.md §Orders behaviour bullets
    /// for the public contract. Do not collapse the two parsers into
    /// a shared "blank-is-empty" helper.
    pub fn from_csv(raw: Option<&str>) -> Result<Self, DomainError> {
        let Some(value) = raw else {
            return Ok(Self::all());
        };
        let mut set = std::collections::BTreeSet::new();
        let mut had_token = false;
        for token in value.split(',') {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            had_token = true;
            let status = match trimmed {
                "NEW" => OrderStatus::New,
                "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
                "FILLED" => OrderStatus::Filled,
                "CANCELED" => OrderStatus::Canceled,
                "REJECTED" => OrderStatus::Rejected,
                _ => return Err(DomainError::InvalidParameter),
            };
            set.insert(status);
        }
        if !had_token {
            return Ok(Self::all());
        }
        Ok(Self(set))
    }

    pub fn all() -> Self {
        Self(std::collections::BTreeSet::new())
    }

    pub fn is_all(&self) -> bool {
        self.0.is_empty()
    }

    pub fn canonical_vec(&self) -> Vec<OrderStatus> {
        // BTreeSet iteration order is the enum's `Ord` — see the
        // load-bearing-declaration-order note on
        // [`OrderStatus`](dodex_domain::OrderStatus) for the
        // authoritative variant list. The write-side synthetic states
        // (`PendingNew`, `PendingCancel`) are rejected at parse time
        // by `from_csv` and never enter the set, so the result is
        // stable and pending-state-free.
        self.0.iter().copied().collect()
    }
}

/// Opaque pagination cursor for `/api/v1/orders`. The inner string is
/// the `placed_chain_order` of the last row returned by a previous
/// page; the server reads it as a lexicographic token via the strict
/// `<` predicate in [`PostgresReadModelRepository::list_orders`].
///
/// The pub-field shape matches sibling newtypes (`MarketAddress`,
/// `Symbol`) so existing call-site idioms still work, but
/// construction goes through [`OrdersCursor::new`] which enforces the
/// non-blank invariant. Callers that already hold a known-good value
/// (e.g. the projector loading a row's `placed_chain_order`) may
/// construct the tuple form directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrdersCursor(pub String);

impl OrdersCursor {
    /// Validating constructor: trims whitespace, rejects blank input
    /// as [`DomainError::MissingParameter`] (mirrors the public
    /// `/api/v1/orders` contract — a whitespace-only `?cursor=` is
    /// always a client-side bug, not "no cursor").
    pub fn new(raw: String) -> Result<Self, DomainError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainError::MissingParameter);
        }
        Ok(Self(trimmed.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct OrdersQuery {
    pub owner_pn_address: String,
    pub market: Option<OrdersMarketFilter>,
    pub status: OrderStatusSet,
    pub limit: u16,
    pub cursor: Option<OrdersCursor>,
}

#[derive(Debug, Clone)]
pub struct OrdersMarketFilter {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
}

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
}

#[async_trait]
impl<T: ?Sized + ChainOrderSender> ChainOrderSender for Arc<T> {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        (**self).submit_order(payload).await
    }

    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
        (**self).cancel_order(payload).await
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

        // Flag encoding rejects (MARKET, GTC/FOK/POST_ONLY); LIMIT path
        // falls through with defaulted GTC when TIF is absent.
        let flags = encode_order_flags(input.order_type, input.time_in_force)?;

        // `price` is required for LIMIT and rejected for MARKET per
        // api-spec §New Order. Resolve the field-presence + order-type
        // matrix once, into an `Option<&str>` the rest of the function
        // can reference without re-checking — no `.expect("checked
        // above")` further down.
        let price_input: Option<&str> = match (input.order_type, input.price.as_deref()) {
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

        precision_within(&input.quantity, outcome.quantity_precision)?;
        if !is_multiple_of(&input.quantity, &outcome.step_size)? {
            return Err(DomainError::PrecisionExceeded);
        }
        let amount_lifted = lift_decimal(&input.quantity, outcome.quantity_precision)?;
        // Strictly-positive invariant. `quantity == "0"` survives
        // `precision_within` (no fractional digits) and `is_multiple_of`
        // (zero is a multiple of every non-zero step), and the
        // MARKET-SELL branch below skips the notional check that
        // implicitly catches it for LIMIT and MARKET-BUY (where
        // `0 * price < min_notional`). Without this gate the chain
        // would reject with `ERR_LOW_VALUE` (102) — correct shape but
        // a wasted round-trip and an avoidable contention with the
        // per-PN `_busy` lock for the legitimate next submission.
        if amount_lifted == BigUint::from(0u32) {
            return Err(DomainError::OrderValidationFailed);
        }
        // SDK serialization ceiling. `PrivateNote.placeOrder.amount`
        // is `uint128` at the chain ABI, but the upstream
        // `bee_dex` → `ackinacki-kit` → `serde_json::json!` path
        // panics on `u128 > u64::MAX` for the same reason
        // `clientOrderId` is capped — see
        // `docs/tech-specs/write-api.md §clientOrderId generation`.
        // Until the SDK gains `serde_json/arbitrary_precision` the
        // amount surface is also u64. Catch over-ceiling values here
        // so they surface as 400 / -2010 ("order cannot succeed")
        // instead of a 500 from the worker panic.
        if amount_lifted > BigUint::from(u64::MAX) {
            return Err(DomainError::OrderValidationFailed);
        }
        let amount_raw = amount_lifted.to_str_radix(10);

        // Notional check splits per (type, side) per spec validation
        // table. `price_input` carries the validated LIMIT price (or
        // `None` for MARKET); the MARKET-SELL branch has no spec rule.
        match (input.order_type, input.side, price_input) {
            (OrderType::Limit, _, Some(p)) => {
                if !notional_meets_minimum(p, &input.quantity, &outcome.min_notional)? {
                    return Err(DomainError::OrderValidationFailed);
                }
            }
            (OrderType::Market, OrderSide::Buy, _) => {
                // MARKET BUY: `quantity` is the quote-asset spend amount,
                // compared directly against `minNotional`.
                if !notional_meets_minimum("1", &input.quantity, &outcome.min_notional)? {
                    return Err(DomainError::OrderValidationFailed);
                }
            }
            (OrderType::Market, OrderSide::Sell, _) => {
                // api-spec doesn't list a notional rule for MARKET SELL;
                // the chain enforces its own MIN_ORDER_NOTIONAL. Skip
                // here rather than guess.
            }
            // The (Limit, None) and (Market, Some) cases above already
            // returned, so this arm is structurally unreachable. We
            // collapse it to `Unexpected` (500) rather than `panic!`
            // so a future refactor that broke the invariant could not
            // turn into an opaque crash in the request handler.
            (OrderType::Limit, _, None) => return Err(DomainError::Unexpected),
        }

        // `markets.token_type` is `integer` in Postgres (signed), but the
        // on-chain `PrivateNote.placeOrder` ABI is `uint32`. The
        // reconciler only ever writes values pulled from
        // `PMP.getDetails()`, so a negative here would mean the DB row
        // was corrupted post-reconcile — fail closed with 503 instead
        // of pushing a sign-folded value to chain.
        let token_type = u32::try_from(token_type).map_err(|_| DomainError::MarketInconsistent)?;

        // Caller-supplied `newOrderClientId` is bounded at `u64::MAX`
        // by the upstream serialization constraint documented in
        // `docs/tech-specs/write-api.md §clientOrderId generation`.
        // Reject larger or non-numeric values as 400 / -1130 here
        // rather than letting them panic deep in `ackinacki-kit`.
        let client_order_id = match input.client_order_id.as_deref() {
            Some(raw) => {
                raw.parse::<u64>().map_err(|_| DomainError::InvalidParameter)?;
                raw.to_string()
            }
            None => generate_client_order_id(),
        };

        let payload = NewOrderPayload {
            pn_address: input.trading_pn.pn_address,
            pn_pubkey: input.trading_pn.pn_pubkey,
            pn_seckey: input.trading_pn.pn_seckey,
            event_id,
            oracle_list_hash,
            token_type,
            outcome_id: outcome.outcome_id,
            is_buy: input.side.is_buy(),
            price_raw,
            amount_raw,
            flags,
            client_order_id: client_order_id.clone(),
        };
        self.sender.submit_order(payload).await?;

        Ok(SubmittedOrder { client_order_id })
    }
}

/// Generate a fresh `clientOrderId`. Decimal string of a `uint64`
/// random value (low 64 bits of `Uuid::new_v4()`), bounded by the
/// upstream serialization constraint documented in
/// `docs/tech-specs/write-api.md §clientOrderId generation`.
fn generate_client_order_id() -> String {
    (Uuid::new_v4().as_u128() as u64).to_string()
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
        let OrderForCancel { event_id, oracle_list_hash, token_type, status, client_order_id } =
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

        if status != MarketStatus::Trading {
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

/// Inputs to `GetOrdersUseCase::execute`, mirroring the shape of
/// [`NewOrderInput`] for symmetry across read/write use cases. The
/// HTTP handler is the only intended constructor: it owns the
/// `AuthContext` and passes a clone of `ctx.trading_pn.pn_address`
/// here. The CSV `status` / `cursor` strings are raw request values;
/// validation happens inside `execute`.
pub struct GetOrdersInput {
    pub owner_pn_address: String,
    pub market_address: Option<MarketAddress>,
    pub symbol: Option<Symbol>,
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
        let market = match (input.market_address, input.symbol) {
            (None, None) => None,
            (Some(market_address), Some(symbol)) => {
                Some(OrdersMarketFilter { market_address, symbol })
            }
            _ => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let status = OrderStatusSet::from_csv(input.status.as_deref())
            .map_err(|err| anyhow::anyhow!(err))?;

        let limit = match input.limit {
            None => ORDERS_DEFAULT_LIMIT,
            Some(v) if (1..=i64::from(ORDERS_MAX_LIMIT)).contains(&v) => v as u16,
            Some(_) => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let cursor = match input.cursor {
            None => None,
            Some(raw) => Some(OrdersCursor::new(raw).map_err(|err| anyhow::anyhow!(err))?),
        };

        self.repo
            .list_orders(&OrdersQuery {
                owner_pn_address: input.owner_pn_address,
                market,
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

    #[derive(Clone)]
    struct FakeOpenOrder {
        order_id: u64,
        owner_pn_address: String,
        client_order_id: Option<String>,
    }

    struct FakeRepo {
        market: Option<Market>,
        open_order: Option<FakeOpenOrder>,
    }

    impl FakeRepo {
        fn with(market: Market) -> Self {
            Self { market: Some(market), open_order: None }
        }

        fn empty() -> Self {
            Self { market: None, open_order: None }
        }

        fn with_open_order(market: Market, order: FakeOpenOrder) -> Self {
            Self { market: Some(market), open_order: Some(order) }
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
            let order = self.open_order.clone().ok_or_else(unknown)?;
            if order.order_id != order_id || order.owner_pn_address != owner_pn_address {
                return Err(unknown());
            }
            Ok(OrderForCancel {
                event_id: market.event.event_id,
                oracle_list_hash: market.oracle_list_hash,
                token_type: market.token_type,
                status: market.status,
                client_order_id: order.client_order_id,
            })
        }

        async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
            unimplemented!("list_orders is not exercised by CreateOrderUseCase tests")
        }
    }

    struct FakeSender {
        recorded: Mutex<Vec<NewOrderPayload>>,
        recorded_cancels: Mutex<Vec<CancelOrderPayload>>,
        fail_with: Option<DomainError>,
    }

    impl FakeSender {
        fn ok() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        fn failing(err: DomainError) -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
                recorded_cancels: Mutex::new(Vec::new()),
                fail_with: Some(err),
            }
        }

        fn calls(&self) -> Vec<NewOrderPayload> {
            self.recorded.lock().unwrap().clone()
        }

        fn cancel_calls(&self) -> Vec<CancelOrderPayload> {
            self.recorded_cancels.lock().unwrap().clone()
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
    fn status_set_parses_csv_and_dedups() {
        let set = OrderStatusSet::from_csv(Some("NEW, FILLED ,NEW, CANCELED")).expect("valid CSV");
        let canonical = set.canonical_vec();
        assert_eq!(canonical, vec![OrderStatus::New, OrderStatus::Filled, OrderStatus::Canceled]);
    }

    #[test]
    fn status_set_treats_absent_and_empty_as_all() {
        assert!(OrderStatusSet::from_csv(None).expect("absent").is_all());
        assert!(OrderStatusSet::from_csv(Some("   ")).expect("blank").is_all());
    }

    #[test]
    fn status_set_rejects_unknown_token() {
        let err = OrderStatusSet::from_csv(Some("NEW,SUPER_FILLED")).expect_err("unknown token");
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[test]
    fn status_set_rejects_pending_states() {
        // PendingNew and PendingCancel are write-side synthetic statuses
        // and must not be accepted as a /orders filter — neither appears
        // on a live_orders row.
        let err = OrderStatusSet::from_csv(Some("PENDING_NEW")).expect_err("pending_new rejected");
        assert_eq!(err, DomainError::InvalidParameter);
        let err =
            OrderStatusSet::from_csv(Some("PENDING_CANCEL")).expect_err("pending_cancel rejected");
        assert_eq!(err, DomainError::InvalidParameter);
    }

    #[test]
    fn generated_client_order_id_fits_in_u64() {
        // Regression guard for the bug round 4 caught: an earlier
        // implementation used the full `Uuid::new_v4().as_u128()`,
        // which produces values exceeding `u64::MAX` ~50% of the
        // time. Those panic deep inside `bee_dex` / `serde_json`
        // when the worker tries to serialize them. The generator
        // MUST stay inside u64 until the SDK supports
        // arbitrary-precision serialization. 256 samples is more
        // than enough to surface a regression to the full u128.
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
        let order = FakeOpenOrder {
            order_id: 123,
            owner_pn_address: "0:pn".into(),
            client_order_id: Some("42".into()),
        };
        let sender = Arc::new(FakeSender::ok());
        let uc = CancelOrderUseCase::new(FakeRepo::with_open_order(market, order), sender.clone());

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
        let order =
            FakeOpenOrder { order_id: 123, owner_pn_address: "0:pn".into(), client_order_id: None };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_open_order(market, order),
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
        let order =
            FakeOpenOrder { order_id: 123, owner_pn_address: "0:pn".into(), client_order_id: None };
        let uc =
            CancelOrderUseCase::new(FakeRepo::with_open_order(market, order), FakeSender::ok());
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
        let order = FakeOpenOrder {
            order_id: 123,
            owner_pn_address: "0:someone-else".into(),
            client_order_id: None,
        };
        let uc =
            CancelOrderUseCase::new(FakeRepo::with_open_order(market, order), FakeSender::ok());
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::UnknownOrder);
    }

    #[tokio::test]
    async fn cancel_order_rejects_non_trading_status() {
        let mut market = trading_market("PM-YES");
        market.status = MarketStatus::Resolving;
        let order =
            FakeOpenOrder { order_id: 123, owner_pn_address: "0:pn".into(), client_order_id: None };
        let uc =
            CancelOrderUseCase::new(FakeRepo::with_open_order(market, order), FakeSender::ok());
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::OrderValidationFailed);
    }

    #[tokio::test]
    async fn cancel_order_rejects_blank_oracle_list_hash() {
        let mut market = trading_market("PM-YES");
        market.oracle_list_hash = String::new();
        let order =
            FakeOpenOrder { order_id: 123, owner_pn_address: "0:pn".into(), client_order_id: None };
        let uc =
            CancelOrderUseCase::new(FakeRepo::with_open_order(market, order), FakeSender::ok());
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::MarketInconsistent);
    }

    #[tokio::test]
    async fn cancel_order_propagates_sender_pn_busy() {
        let market = trading_market("PM-YES");
        let order =
            FakeOpenOrder { order_id: 123, owner_pn_address: "0:pn".into(), client_order_id: None };
        let uc = CancelOrderUseCase::new(
            FakeRepo::with_open_order(market, order),
            FakeSender::failing(DomainError::OrderPnBusy),
        );
        let err = uc.execute(base_cancel_input("PM-YES", 123)).await.unwrap_err();
        assert_eq!(err, DomainError::OrderPnBusy);
    }
}
