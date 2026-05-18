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
use dodex_domain::OrderSide;
use dodex_domain::OrderType;
use dodex_domain::Outcome;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use dodex_domain::Symbol;
use dodex_domain::TimeInForce;
use num_bigint::BigUint;
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
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub limit: u16,
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
}

#[async_trait]
impl<T: ?Sized + ChainOrderSender> ChainOrderSender for Arc<T> {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        (**self).submit_order(payload).await
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
                // an unexpected internal error.
                err.downcast_ref::<DomainError>().copied().unwrap_or(DomainError::Unexpected)
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

    struct FakeRepo {
        market: Option<Market>,
    }

    impl FakeRepo {
        fn with(market: Market) -> Self {
            Self { market: Some(market) }
        }

        fn empty() -> Self {
            Self { market: None }
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
    }

    struct FakeSender {
        recorded: Mutex<Vec<NewOrderPayload>>,
        fail_with: Option<DomainError>,
    }

    impl FakeSender {
        fn ok() -> Self {
            Self { recorded: Mutex::new(Vec::new()), fail_with: None }
        }

        fn failing(err: DomainError) -> Self {
            Self { recorded: Mutex::new(Vec::new()), fail_with: Some(err) }
        }

        fn calls(&self) -> Vec<NewOrderPayload> {
            self.recorded.lock().unwrap().clone()
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
}
