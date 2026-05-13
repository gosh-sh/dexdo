// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::OpenOrder;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use dodex_domain::Symbol;
use serde::Deserialize;
use serde::Serialize;
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

#[async_trait]
pub trait MarketReadRepository: Send + Sync {
    async fn list_markets(&self, request: &MarketsRequest) -> Result<MarketsPage, anyhow::Error>;

    async fn get_depth(
        &self,
        market_address: &MarketAddress,
        symbol: &Symbol,
        limit: u16,
    ) -> Result<DepthSnapshot, anyhow::Error>;

    async fn list_open_orders(
        &self,
        query: &OpenOrdersQuery,
    ) -> Result<OpenOrdersPage, anyhow::Error>;
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

    async fn list_open_orders(
        &self,
        query: &OpenOrdersQuery,
    ) -> Result<OpenOrdersPage, anyhow::Error> {
        (**self).list_open_orders(query).await
    }
}

#[derive(Debug, Clone)]
pub struct GetDepthQuery {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
    pub limit: u16,
}

pub const OPEN_ORDERS_DEFAULT_LIMIT: u16 = 100;
pub const OPEN_ORDERS_MAX_LIMIT: u16 = 500;

// Upper bound on `OpenOrdersCursor.chain_created_at_us`. `chain_created_at`
// is a `timestamptz` (microsecond resolution); 8e18 µs ≈ year 255000 AD,
// far above any real chain epoch and well under Postgres' `to_timestamp`
// limit (year 294276 AD). Anything outside this range is treated as a
// malformed cursor so the SQL never raises a timestamp-out-of-range error.
pub const OPEN_ORDERS_MAX_CURSOR_TS_US: i64 = 8_000_000_000_000_000_000;

#[derive(Debug, Clone)]
pub struct OpenOrdersQuery {
    pub owner_pn_address: String,
    pub market: Option<OpenOrdersMarketFilter>,
    pub limit: u16,
    pub cursor: Option<OpenOrdersCursor>,
}

#[derive(Debug, Clone)]
pub struct OpenOrdersMarketFilter {
    pub market_address: MarketAddress,
    pub symbol: Symbol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenOrdersCursor {
    // Unix microseconds. The column `live_orders.chain_created_at` is a
    // `timestamptz` (microsecond resolution); storing milliseconds here
    // would round-trip past the row's full timestamp and let the
    // strict-`>` next-page predicate return the boundary row again.
    #[serde(rename = "t")]
    pub chain_created_at_us: i64,
    #[serde(rename = "o")]
    pub order_id: String,
    // Orderbook address is the unique tie-breaker for the all-markets
    // variant: `order_id` is unique only within one orderbook (PK
    // `(orderbook_address, order_id)`), so two open orders on different
    // books that share `(chain_created_at, order_id)` would otherwise
    // both be filtered out by the next-page `>` predicate.
    #[serde(rename = "b")]
    pub orderbook_address: String,
}

impl OpenOrdersCursor {
    pub fn encode(&self) -> String {
        // Infallible: serde_json on this fixed struct cannot fail.
        let json = serde_json::to_vec(self).expect("encode OpenOrdersCursor");
        B64.encode(json)
    }

    pub fn decode(s: &str) -> Result<Self, DomainError> {
        let bytes = B64.decode(s).map_err(|_| DomainError::MissingParameter)?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| DomainError::MissingParameter)?;
        // `order_id` is a uint256 decimal string on the wire, bound into
        // `numeric(78, 0)` by the repo. Anything outside that domain — a
        // non-decimal character, an empty string, or a value with more than
        // 78 digits — would surface as a sqlx cast / overflow error mapped
        // to -1000/500 instead of the documented -1102/400 for malformed
        // cursors. Reject up front in the codec.
        if cursor.order_id.is_empty()
            || cursor.order_id.len() > 78
            || !cursor.order_id.chars().all(|c| c.is_ascii_digit())
        {
            return Err(DomainError::MissingParameter);
        }
        if cursor.orderbook_address.is_empty() {
            return Err(DomainError::MissingParameter);
        }
        // The repo binds `chain_created_at_us` into
        // `to_timestamp($ / 1_000_000.0)`. An extreme `t` value (e.g., near
        // `i64::MAX`) would push Postgres past its timestamp range and raise
        // `-1000/500`. Bound the value at parse time so malformed cursors
        // keep returning the documented `-1102/400`.
        if !(0..=OPEN_ORDERS_MAX_CURSOR_TS_US).contains(&cursor.chain_created_at_us) {
            return Err(DomainError::MissingParameter);
        }
        Ok(cursor)
    }
}

#[derive(Debug, Clone)]
pub struct OpenOrdersPage {
    pub orders: Vec<OpenOrder>,
    pub next_cursor: Option<OpenOrdersCursor>,
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

pub struct GetOpenOrdersUseCase<R> {
    repo: R,
}

impl<R> GetOpenOrdersUseCase<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }
}

impl<R> GetOpenOrdersUseCase<R>
where
    R: MarketReadRepository,
{
    pub async fn execute(
        &self,
        ctx: &AuthContext,
        market_address: Option<MarketAddress>,
        symbol: Option<Symbol>,
        limit: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<OpenOrdersPage, anyhow::Error> {
        let market = match (market_address, symbol) {
            (None, None) => None,
            (Some(market_address), Some(symbol)) => {
                Some(OpenOrdersMarketFilter { market_address, symbol })
            }
            _ => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let limit = match limit {
            None => OPEN_ORDERS_DEFAULT_LIMIT,
            Some(v) if (1..=i64::from(OPEN_ORDERS_MAX_LIMIT)).contains(&v) => v as u16,
            Some(_) => return Err(anyhow::anyhow!(DomainError::MissingParameter)),
        };

        let cursor = match cursor {
            None => None,
            Some(raw) => Some(OpenOrdersCursor::decode(raw).map_err(|err| anyhow::anyhow!(err))?),
        };

        self.repo
            .list_open_orders(&OpenOrdersQuery {
                owner_pn_address: ctx.trading_pn.pn_address.clone(),
                market,
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

    // OpenOrdersCursor codec — pure-logic guards that the openOrders
    // endpoint relies on. The integration tests in `dodex-api` also
    // exercise these paths through the real router, but those are gated
    // on `TEST_DATABASE_URL` and silently skip in unenrolled CI. The
    // tests below run unconditionally.

    fn sample_cursor() -> OpenOrdersCursor {
        OpenOrdersCursor {
            chain_created_at_us: 1_700_000_000_500_500,
            order_id: "42".into(),
            orderbook_address: "0:book".into(),
        }
    }

    #[test]
    fn cursor_round_trips_through_encode_decode() {
        let cursor = sample_cursor();
        let encoded = cursor.encode();
        let decoded = OpenOrdersCursor::decode(&encoded).expect("round-trip");
        assert_eq!(decoded, cursor);
    }

    #[test]
    fn cursor_decode_rejects_invalid_base64() {
        let err = OpenOrdersCursor::decode("not~valid~base64").unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_empty_string() {
        // Empty base64 decodes to an empty byte string; serde_json then
        // refuses with "unexpected end of input". Either way: -1102.
        let err = OpenOrdersCursor::decode("").unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_unparseable_json() {
        // Valid base64, invalid JSON.
        let encoded = B64.encode(b"not json");
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_missing_required_field() {
        // Drop the `b` field entirely.
        let encoded = B64.encode(br#"{"t":0,"o":"1"}"#);
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_wrong_field_type() {
        // `t` should be a number; pass a string.
        let encoded = B64.encode(br#"{"t":"0","o":"1","b":"0:book"}"#);
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_empty_order_id() {
        let encoded = OpenOrdersCursor { order_id: "".into(), ..sample_cursor() }.encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_nondecimal_order_id() {
        let encoded = OpenOrdersCursor { order_id: "abc".into(), ..sample_cursor() }.encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_order_id_over_78_digits() {
        let encoded = OpenOrdersCursor { order_id: "1".repeat(79), ..sample_cursor() }.encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_accepts_order_id_at_78_digits() {
        // 78 digits is the documented maximum that fits `numeric(78,0)`.
        let encoded = OpenOrdersCursor { order_id: "9".repeat(78), ..sample_cursor() }.encode();
        OpenOrdersCursor::decode(&encoded).expect("78 digits is valid");
    }

    #[test]
    fn cursor_decode_rejects_empty_orderbook_address() {
        let encoded = OpenOrdersCursor { orderbook_address: "".into(), ..sample_cursor() }.encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_negative_timestamp() {
        let encoded = OpenOrdersCursor { chain_created_at_us: -1, ..sample_cursor() }.encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_rejects_timestamp_above_bound() {
        let encoded = OpenOrdersCursor {
            chain_created_at_us: OPEN_ORDERS_MAX_CURSOR_TS_US + 1,
            ..sample_cursor()
        }
        .encode();
        let err = OpenOrdersCursor::decode(&encoded).unwrap_err();
        assert_eq!(err, DomainError::MissingParameter);
    }

    #[test]
    fn cursor_decode_accepts_timestamp_at_bound() {
        let encoded = OpenOrdersCursor {
            chain_created_at_us: OPEN_ORDERS_MAX_CURSOR_TS_US,
            ..sample_cursor()
        }
        .encode();
        OpenOrdersCursor::decode(&encoded).expect("bound is inclusive");
    }

    #[test]
    fn cursor_decode_accepts_timestamp_zero() {
        let encoded = OpenOrdersCursor { chain_created_at_us: 0, ..sample_cursor() }.encode();
        OpenOrdersCursor::decode(&encoded).expect("zero is valid");
    }
}
