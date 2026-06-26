// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// HTTP-level integration tests for `DELETE /api/v1/prediction/order` that
// exercise the handler + use case end to end **without** a database or
// a real chain. Mirrors the triad in `create_order_http.rs`: a fake
// `Authenticator` short-circuits HMAC, a fake `MarketReadRepository`
// returns a configurable `OrderForCancel`, and a recording
// `ChainOrderSender` lets each test inspect the cancel payload the
// handler would dispatch in production.
//
// The matching per-row coverage for the cancel error-mapping table in
// `docs/tech-specs/write-api.md §DELETE /api/v1/prediction/order` lives here.

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
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::MarketBalancesResolution;
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_application::NewOrderPayload;
use dodex_application::OrderForCancel;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
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
/// `OrderForCancel`, or short-circuits the resolver with a typed
/// `DomainError`. Tests pick whichever shape they need to exercise.
struct FakeRepo {
    order: Mutex<Option<OrderForCancel>>,
    resolver_error: Option<DomainError>,
}

impl FakeRepo {
    fn with(order: OrderForCancel) -> Self {
        Self { order: Mutex::new(Some(order)), resolver_error: None }
    }

    fn failing_resolver(err: DomainError) -> Self {
        Self { order: Mutex::new(None), resolver_error: Some(err) }
    }
}

#[async_trait]
impl MarketReadRepository for FakeRepo {
    async fn list_markets(&self, _: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        unimplemented!("list_markets is not exercised by cancel_order_http tests")
    }

    async fn get_depth(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        unimplemented!("get_depth is not exercised by cancel_order_http tests")
    }

    async fn get_trades(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: TradesLimit,
    ) -> Result<Vec<Trade>, anyhow::Error> {
        unimplemented!("get_trades is not exercised by cancel_order_http tests")
    }

    async fn resolve_for_new_order(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
        unimplemented!("resolve_for_new_order is not exercised by cancel_order_http tests")
    }

    async fn resolve_for_cancel(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u64,
        _: &str,
        _: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        if let Some(err) = self.resolver_error {
            return Err(anyhow::anyhow!(err));
        }
        let Some(order) = self.order.lock().unwrap().clone() else {
            return Err(anyhow::anyhow!(DomainError::UnknownOrder));
        };
        Ok(order)
    }

    async fn resolve_for_cancel_batch(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: &[u64],
        _: &str,
        _: i64,
    ) -> Result<Option<dodex_application::CancelBatchResolution>, anyhow::Error> {
        unimplemented!("resolve_for_cancel_batch is not exercised by cancel_order_http tests")
    }

    async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by cancel_order_http tests")
    }

    async fn resolve_market_for_balances(
        &self,
        _: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error> {
        unimplemented!("resolve_market_for_balances is not exercised by cancel_order_http tests")
    }

    async fn resolve_for_buy_full_set(
        &self,
        _: &MarketAddress,
        _: i64,
    ) -> Result<dodex_application::MarketForBuyFullSet, anyhow::Error> {
        unimplemented!("resolve_for_buy_full_set is not exercised by cancel_order_http tests")
    }

    async fn sum_open_sell_remaining(
        &self,
        _: &str,
        _: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        unimplemented!("sum_open_sell_remaining is not exercised by cancel_order_http tests")
    }
}

/// `ChainOrderSender` that records every cancel payload it sees, or
/// fails with a configured `DomainError`. Mirrors `RecordingSender` in
/// `create_order_http.rs` but inverts which arm is real.
struct RecordingCancelSender {
    recorded: Mutex<Vec<CancelOrderPayload>>,
    fail_with: Option<DomainError>,
}

impl RecordingCancelSender {
    fn ok() -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: None }
    }

    fn failing(err: DomainError) -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: Some(err) }
    }

    fn calls(&self) -> Vec<CancelOrderPayload> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChainOrderSender for RecordingCancelSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingCancelSender::submit_order called from DELETE test")
    }

    async fn cancel_order(&self, payload: CancelOrderPayload) -> Result<(), DomainError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        self.recorded.lock().unwrap().push(payload);
        Ok(())
    }

    async fn submit_batch_order(
        &self,
        _: dodex_application::NewBatchOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("RecordingCancelSender::submit_batch_order called from DELETE /order test")
    }

    async fn cancel_batch_order(
        &self,
        _: dodex_application::CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("RecordingCancelSender::cancel_batch_order called from DELETE /order test")
    }

    async fn split_full_set(
        &self,
        _: dodex_application::SplitFullSetPayload,
    ) -> Result<(), DomainError> {
        unreachable!("RecordingCancelSender::split_full_set called from order/batch test")
    }
}

// ---- Fixtures ------------------------------------------------------------

const PN_ADDRESS: &str = "0:fake-pn";
const SYMBOL: &str = "PM-FAKE-YES";
const MARKET_ADDRESS: &str = "0:market-fake";
const ORDER_ID: u64 = 123_456_789;

fn trading_order(client_order_id: Option<&str>) -> OrderForCancel {
    OrderForCancel {
        event_id: "0xevent".into(),
        oracle_list_hash: "0xfeedface".into(),
        token_type: 3,
        market_status: MarketStatus::Trading,
        client_order_id: client_order_id.map(|s| s.to_string()),
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

/// Build a DELETE request with the standard auth envelope plus the
/// caller-supplied (key, value) query pairs. `params` lets each test
/// omit or override individual fields without copy-pasting the auth
/// envelope wiring.
async fn send_delete(service: &Service, params: Vec<(&'static str, String)>) -> salvo::Response {
    let mut req = TestClient::delete("http://test/api/v1/prediction/order").add_header(
        "X-DODEX-APIKEY",
        "fake",
        true,
    );
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    for (k, v) in params {
        req = req.query(k, v);
    }
    req.send(service).await
}

fn full_params(order_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("predictionMarketAddress", MARKET_ADDRESS.into()),
        ("symbol", SYMBOL.into()),
        ("orderId", order_id.into()),
    ]
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelBody {
    order_id: String,
    client_order_id: String,
    transact_time: i64,
    status: String,
}

// ---- Tests ---------------------------------------------------------------

#[tokio::test]
async fn happy_path_returns_pending_cancel_and_dispatches_payload() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(Some("client-42"))));
    let sender = Arc::new(RecordingCancelSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let body = resp.take_json::<CancelBody>().await.expect("cancel body");
    assert_eq!(body.order_id, ORDER_ID.to_string());
    assert_eq!(body.client_order_id, "client-42");
    assert_eq!(body.status, "PENDING_CANCEL");
    assert!(body.transact_time > 0, "transactTime should be a real ms timestamp");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    let payload = &calls[0];
    assert_eq!(payload.pn_address, PN_ADDRESS);
    assert_eq!(payload.event_id, "0xevent");
    assert_eq!(payload.oracle_list_hash, "0xfeedface");
    assert_eq!(payload.token_type, 3);
    assert_eq!(payload.order_id, ORDER_ID);
}

#[tokio::test]
async fn echoes_empty_string_when_client_order_id_absent() {
    // api-spec §Cancel Order: response.clientOrderId is "empty string
    // if the order was placed without a newOrderClientId."
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let body = resp.take_json::<CancelBody>().await.expect("cancel body");
    assert_eq!(body.client_order_id, "");
}

#[tokio::test]
async fn missing_market_address_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let params = vec![("symbol", SYMBOL.into()), ("orderId", ORDER_ID.to_string())];
    let mut resp = send_delete(&service, params).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_symbol_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let params =
        vec![("predictionMarketAddress", MARKET_ADDRESS.into()), ("orderId", ORDER_ID.to_string())];
    let mut resp = send_delete(&service, params).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_order_id_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let params =
        vec![("predictionMarketAddress", MARKET_ADDRESS.into()), ("symbol", SYMBOL.into())];
    let mut resp = send_delete(&service, params).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn non_numeric_order_id_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params("not-a-number")).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn over_u64_order_id_returns_400_minus_1130() {
    // ABI on-chain is uint128 but the SDK-serialization ceiling at the
    // public boundary is u64. `u64::MAX + 1` overflows the parser →
    // `InvalidParameter` (-1130), same shape as malformed input.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let overflow = "18446744073709551616"; // u64::MAX + 1
    let mut resp = send_delete(&service, full_params(overflow)).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn unknown_order_returns_404_minus_2011() {
    // Repo collapses every miss (no row, wrong owner, wrong market,
    // closed order) to `UnknownOrder` — verifies the handler's HTTP
    // mapping.
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolver(DomainError::UnknownOrder));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2011);
}

#[tokio::test]
async fn non_trading_market_returns_400_minus_2010() {
    let mut order = trading_order(None);
    order.market_status = MarketStatus::Resolving;
    let repo: SharedRepo = Arc::new(FakeRepo::with(order));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
}

#[tokio::test]
async fn blank_oracle_list_hash_returns_503_minus_1500() {
    let mut order = trading_order(None);
    order.oracle_list_hash = String::new();
    let repo: SharedRepo = Arc::new(FakeRepo::with(order));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

#[tokio::test]
async fn pn_busy_returns_429_minus_2014() {
    // Sender raising `OrderPnBusy` simulates a real `ERR_NOTE_BUSY`
    // (121) coming back from `dodex_chain::Dex::cancel_order` while another
    // op from the same PN is still in flight.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender =
        Arc::new(RecordingCancelSender::failing(DomainError::OrderPnBusy));
    let service = setup_with(repo, sender);

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::TOO_MANY_REQUESTS));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2014);
}

#[tokio::test]
async fn caller_without_trade_permission_returns_401() {
    // require_auth(Permission::Trade) catches this before the use
    // case sees the request — mirrors the same enforcement on POST.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_order(None)));
    let sender: SharedChainSender = Arc::new(RecordingCancelSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

    let mut resp = send_delete(&service, full_params(&ORDER_ID.to_string())).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    // -1002 per api-spec: "verification was attempted and the credential
    // was rejected (... or key lacks the required permission)."
    assert_eq!(err.code, -1002);
}
