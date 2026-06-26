// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// HTTP-level integration tests for `POST /api/v1/prediction/buyFullSet` that
// exercise the handler + use case end to end **without** a database or
// a real chain. Mirrors the triad in `cancel_order_http.rs`: a fake
// `Authenticator` short-circuits HMAC, a fake `MarketReadRepository`
// returns a configurable `MarketForBuyFullSet`, and a recording
// `ChainOrderSender` lets each test inspect the `splitFullSet` payload
// the handler would dispatch in production.
//
// Per-row coverage for the api-spec §Buy Full Set error table lives
// here.

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedRepo;
use dodex_application::AuthContext;
use dodex_application::AuthenticateRequest;
use dodex_application::Authenticator;
use dodex_application::CancelBatchOrderPayload;
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::MarketBalancesResolution;
use dodex_application::MarketForBuyFullSet;
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_application::OrderForCancel;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
use dodex_application::SplitFullSetPayload;
use dodex_application::TradesLimit;
use dodex_application::TradingPn;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use dodex_domain::Symbol;
use dodex_domain::Trade;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

// ---- Fakes ---------------------------------------------------------------

struct FakeAuthenticator {
    permissions: Vec<Permission>,
}

#[async_trait]
impl Authenticator for FakeAuthenticator {
    async fn authenticate(&self, _: AuthenticateRequest) -> Result<AuthContext, DomainError> {
        Ok(AuthContext {
            account_id: Uuid::new_v4(),
            api_key_id: 1,
            trading_pn: TradingPn {
                pn_address: PN_ADDRESS.into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            permissions: self.permissions.clone(),
        })
    }
}

/// `MarketReadRepository` that returns one configurable
/// `MarketForBuyFullSet`, or short-circuits the resolver with a typed
/// `DomainError`. Other trait methods panic — buyFullSet does not
/// exercise them and an accidental coupling regression should surface
/// loudly.
struct FakeRepo {
    market: Mutex<Option<MarketForBuyFullSet>>,
    resolver_error: Option<DomainError>,
    /// Plain-string error for the "anyhow with no `DomainError` cause"
    /// branch — exercises the use-case fallback that maps non-domain
    /// errors to `Unexpected`. Mirrors `FakeRepo::failing_resolver_raw`
    /// in `create_order_http.rs`.
    resolver_raw_error: Option<String>,
}

impl FakeRepo {
    fn with(market: MarketForBuyFullSet) -> Self {
        Self { market: Mutex::new(Some(market)), resolver_error: None, resolver_raw_error: None }
    }

    fn failing_resolver(err: DomainError) -> Self {
        Self { market: Mutex::new(None), resolver_error: Some(err), resolver_raw_error: None }
    }

    fn failing_resolver_raw(msg: &str) -> Self {
        Self {
            market: Mutex::new(None),
            resolver_error: None,
            resolver_raw_error: Some(msg.to_string()),
        }
    }
}

#[async_trait]
impl MarketReadRepository for FakeRepo {
    async fn list_markets(&self, _: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        unimplemented!("list_markets is not exercised by buy_full_set_http tests")
    }

    async fn get_depth(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        unimplemented!("get_depth is not exercised by buy_full_set_http tests")
    }

    async fn get_trades(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: TradesLimit,
    ) -> Result<Vec<Trade>, anyhow::Error> {
        unimplemented!("get_trades is not exercised by buy_full_set_http tests")
    }

    async fn resolve_for_new_order(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
        unimplemented!("resolve_for_new_order is not exercised by buy_full_set_http tests")
    }

    async fn resolve_for_cancel(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u64,
        _: &str,
        _: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        unimplemented!("resolve_for_cancel is not exercised by buy_full_set_http tests")
    }

    async fn resolve_for_cancel_batch(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: &[u64],
        _: &str,
        _: i64,
    ) -> Result<Option<dodex_application::CancelBatchResolution>, anyhow::Error> {
        unimplemented!("resolve_for_cancel_batch is not exercised by buy_full_set_http tests")
    }

    async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by buy_full_set_http tests")
    }

    async fn resolve_market_for_balances(
        &self,
        _: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error> {
        unimplemented!("resolve_market_for_balances is not exercised by buy_full_set_http tests")
    }

    async fn resolve_for_buy_full_set(
        &self,
        _: &MarketAddress,
        _: i64,
    ) -> Result<MarketForBuyFullSet, anyhow::Error> {
        if let Some(msg) = &self.resolver_raw_error {
            return Err(anyhow::anyhow!("{msg}"));
        }
        if let Some(err) = self.resolver_error {
            return Err(anyhow::anyhow!(err));
        }
        let Some(market) = self.market.lock().unwrap().clone() else {
            return Err(anyhow::anyhow!(DomainError::InvalidMarketOrSymbol));
        };
        Ok(market)
    }

    async fn sum_open_sell_remaining(
        &self,
        _: &str,
        _: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        unimplemented!("sum_open_sell_remaining is not exercised by buy_full_set_http tests")
    }
}

/// `ChainOrderSender` that records every splitFullSet payload it sees,
/// or fails with a configured `DomainError`. Other entry points panic —
/// the buyFullSet path must never reach them.
struct RecordingSplitFullSetSender {
    recorded: Mutex<Vec<SplitFullSetPayload>>,
    fail_with: Option<DomainError>,
}

impl RecordingSplitFullSetSender {
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
impl ChainOrderSender for RecordingSplitFullSetSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingSplitFullSetSender::submit_order called from buyFullSet test")
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingSplitFullSetSender::cancel_order called from buyFullSet test")
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingSplitFullSetSender::submit_batch_order called from buyFullSet test")
    }

    async fn cancel_batch_order(&self, _: CancelBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingSplitFullSetSender::cancel_batch_order called from buyFullSet test")
    }

    async fn split_full_set(&self, payload: SplitFullSetPayload) -> Result<(), DomainError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        self.recorded.lock().unwrap().push(payload);
        Ok(())
    }
}

// ---- Fixtures ------------------------------------------------------------

const PN_ADDRESS: &str = "0:fake-pn";
const MARKET_ADDRESS: &str = "0:market-fake";

// token_type=3 matches the USDC entry seeded in `FakeReferenceRepo`
// (decimals=6), so lifted collateral can be asserted at exact precision.
const USDC_TOKEN_TYPE: u32 = 3;

fn market(status: MarketStatus) -> MarketForBuyFullSet {
    MarketForBuyFullSet {
        event_id: "0xevent".into(),
        oracle_list_hash: "0xfeedface".into(),
        token_type: USDC_TOKEN_TYPE,
        status,
    }
}

fn setup_with(repo: SharedRepo, sender: SharedChainSender) -> Service {
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )))
}

fn auth_envelope() -> Vec<(&'static str, String)> {
    vec![("recvWindow", "5000".into()), ("timestamp", "0".into()), ("signature", "00".into())]
}

async fn send_post(service: &Service, body: serde_json::Value) -> salvo::Response {
    let mut req = TestClient::post("http://test/api/v1/prediction/buyFullSet").add_header(
        "X-DODEX-APIKEY",
        "fake",
        true,
    );
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    req.json(&body).send(service).await
}

fn full_body(collateral: &str) -> serde_json::Value {
    json!({ "predictionMarketAddress": MARKET_ADDRESS, "collateral": collateral })
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuyFullSetBody {
    #[serde(rename = "predictionMarketAddress")]
    market_address: String,
    transact_time: i64,
}

// ---- Tests ---------------------------------------------------------------

#[tokio::test]
async fn happy_path_on_trading_returns_200_and_dispatches_payload() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender = Arc::new(RecordingSplitFullSetSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = send_post(&service, full_body("1.5")).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let body = resp.take_json::<BuyFullSetBody>().await.expect("buyFullSet body");
    assert_eq!(body.market_address, MARKET_ADDRESS);
    assert!(body.transact_time > 0, "transactTime should be a real ms timestamp");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    let p = &calls[0];
    assert_eq!(p.pn_address, PN_ADDRESS);
    assert_eq!(p.event_id, "0xevent");
    assert_eq!(p.oracle_list_hash, "0xfeedface");
    assert_eq!(p.token_type, USDC_TOKEN_TYPE);
    // 1.5 lifted by USDC decimals=6 → 1_500_000 raw.
    assert_eq!(p.collateral_raw, "1500000");
}

#[tokio::test]
async fn happy_path_on_awaiting_freeze_dispatches_too() {
    // api-spec §Buy Full Set: AWAITING_FREEZE is explicitly allowed —
    // the first successful call activates the OrderBook for the market.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::AwaitingFreeze)));
    let sender = Arc::new(RecordingSplitFullSetSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    assert_eq!(sender.calls().len(), 1);
}

#[tokio::test]
async fn missing_market_address_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({ "collateral": "10" });
    let mut resp = send_post(&service, body).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_collateral_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({ "predictionMarketAddress": MARKET_ADDRESS });
    let mut resp = send_post(&service, body).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn blank_market_address_returns_400_minus_1102() {
    // `non_empty` trims at the boundary, so a whitespace-only
    // `predictionMarketAddress` collapses to `None` — pinned here so the trim
    // can't silently drift to a permissive accept on this destructive
    // write surface (mirrors the same guard on cancelBatchOrders).
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({ "predictionMarketAddress": "   ", "collateral": "10" });
    let mut resp = send_post(&service, body).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn blank_collateral_returns_400_minus_1102() {
    // Symmetric to blank_market_address: `non_empty` runs on
    // `collateral` too, so whitespace-only must surface as
    // MissingParameter (-1102), not as a numeric-parse failure (-1130).
    // Pins the ordering between `non_empty` and `parse_positive_decimal`.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("\t\n")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn unknown_field_in_body_returns_400_minus_1130() {
    // `#[serde(deny_unknown_fields)]` surfaces caller typos (e.g.
    // `marketAddres` vs `predictionMarketAddress`) as a structural reject
    // (-1130 InvalidParameter via the body-parse path), not as a
    // misleading `MissingParameter` from the now-silently-`None`
    // real field. Pins the strict-input contract on this
    // destructive write surface.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "predictionMarketAddress": MARKET_ADDRESS,
        "collateral": "10",
        "marketAddres": "typo",
    });
    let mut resp = send_post(&service, body).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn zero_collateral_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("0")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn over_precision_collateral_returns_400_minus_1130() {
    // USDC decimals=6 in the seeded ref repo; 7 fractional digits exceeds.
    // api-spec §Buy Full Set maps "exceeds quote-asset precision" to
    // -1130, not -1111 — pin the remap.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("0.0000001")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn non_numeric_collateral_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("not-a-number")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn unknown_market_returns_404_minus_1121() {
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolver(DomainError::InvalidMarketOrSymbol));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
}

#[tokio::test]
async fn non_open_market_returns_400_minus_2010() {
    // Every `MarketStatus` variant outside {AwaitingFreeze, Trading}
    // must collapse to -2010. Spelled out so a future additive change
    // to the enum forces an explicit decision here rather than
    // silently admitting a new phase.
    for status in [
        MarketStatus::Pending,
        MarketStatus::Upcoming,
        MarketStatus::Staking,
        MarketStatus::Resolving,
        MarketStatus::Resolved,
        MarketStatus::Cancelled,
        MarketStatus::Expired,
    ] {
        let repo: SharedRepo = Arc::new(FakeRepo::with(market(status)));
        let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
        let service = setup_with(repo, sender);

        let mut resp = send_post(&service, full_body("10")).await;
        assert_eq!(
            resp.status_code,
            Some(StatusCode::BAD_REQUEST),
            "status={status:?} should reject with 400",
        );
        let err = resp.take_json::<ErrorBody>().await.expect("error body");
        assert_eq!(err.code, -2010, "status={status:?} should map to -2010");
    }
}

#[tokio::test]
async fn blank_oracle_list_hash_returns_503_minus_1500() {
    let mut m = market(MarketStatus::Trading);
    m.oracle_list_hash = String::new();
    let repo: SharedRepo = Arc::new(FakeRepo::with(m));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

#[tokio::test]
async fn unknown_quote_token_type_returns_503_minus_1500() {
    // `lookup_ref_token` returning None means the migration-seeded
    // canonical set does not cover this token_type — read-model
    // corruption, 503. Use a token_type the seeded repo has no row
    // for; `FakeReferenceRepo::with_seeded` covers 1, 2, 3.
    let mut m = market(MarketStatus::Trading);
    m.token_type = 99;
    let repo: SharedRepo = Arc::new(FakeRepo::with(m));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

#[tokio::test]
async fn pn_busy_returns_429_minus_2014() {
    // Sender raising `OrderPnBusy` simulates a real `ERR_NOTE_BUSY`
    // (121) coming back from `dodex_chain::Dex::split_full_set` while
    // another op from the same PN is still in flight.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender =
        Arc::new(RecordingSplitFullSetSender::failing(DomainError::OrderPnBusy));
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::TOO_MANY_REQUESTS));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2014);
}

#[tokio::test]
async fn chain_validation_failure_returns_400_minus_2010() {
    // Sender raising `OrderValidationFailed` simulates `ERR_LOW_VALUE`
    // (102) — caller's free quote-asset balance is below `collateral`.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender =
        Arc::new(RecordingSplitFullSetSender::failing(DomainError::OrderValidationFailed));
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
}

#[tokio::test]
async fn resolver_raw_anyhow_returns_500_minus_1000() {
    // Pin the non-domain fallback in `BuyFullSetUseCase::execute`: an
    // anyhow without a `DomainError` cause (e.g. raw sqlx pool drop)
    // must surface as 500/-1000, not be swallowed as a no-op. The
    // `error!` log on that branch carries `market_address` already
    // (see the use-case site); not asserted here.
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolver_raw("simulated sqlx pool drop"));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1000);
}

#[tokio::test]
async fn handler_exceeding_request_timeout_returns_504_minus_1007() {
    // Same race the timeout hoop guards on every chain entry point: a
    // hung `split_full_set` past `chain.split_full_set_timeout_ms +
    // slack` must surface as 504 with -1007 rather than stalling the
    // worker. `SlowSplitFullSetSender` simulates the hung chain; a
    // 50 ms request budget keeps the test fast.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(SlowSplitFullSetSender);
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    let state = AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )
    .with_request_timeout(std::time::Duration::from_millis(50));
    let service = Service::new(build_router(state));

    let started = std::time::Instant::now();
    let mut resp = send_post(&service, full_body("10")).await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
    // Tight bound — much less than the 5 s sender sleep — confirms the
    // timeout hoop actually cancelled the handler rather than letting
    // it run to completion.
    assert!(elapsed < std::time::Duration::from_secs(1), "elapsed {elapsed:?}");
}

/// `ChainOrderSender` that hangs in `split_full_set` longer than any
/// reasonable test request-timeout budget. Used to drive the handler
/// past the budget without an actual chain call.
struct SlowSplitFullSetSender;

#[async_trait]
impl ChainOrderSender for SlowSplitFullSetSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowSplitFullSetSender::submit_order called from buyFullSet test")
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowSplitFullSetSender::cancel_order called from buyFullSet test")
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowSplitFullSetSender::submit_batch_order called from buyFullSet test")
    }

    async fn cancel_batch_order(&self, _: CancelBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowSplitFullSetSender::cancel_batch_order called from buyFullSet test")
    }

    async fn split_full_set(&self, _: SplitFullSetPayload) -> Result<(), DomainError> {
        // 5 s — far longer than any reasonable test request-timeout
        // budget. The regression test caps the budget at 50 ms.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(())
    }
}

#[tokio::test]
async fn caller_without_trade_permission_returns_401() {
    // require_auth(Permission::Trade) catches this before the use case
    // sees the request — mirrors the same enforcement on every other
    // TRADE endpoint.
    let repo: SharedRepo = Arc::new(FakeRepo::with(market(MarketStatus::Trading)));
    let sender: SharedChainSender = Arc::new(RecordingSplitFullSetSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

    let mut resp = send_post(&service, full_body("10")).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1002);
}
