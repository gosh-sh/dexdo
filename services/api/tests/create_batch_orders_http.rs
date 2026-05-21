// HTTP-level integration tests for `POST /api/v1/batchOrders` that
// exercise the handler + use case end to end **without** a database or
// a real chain. Three fakes plug into the boundaries the production
// router takes by trait: a fake `Authenticator` short-circuits HMAC,
// a fake `MarketReadRepository` returns a configurable `Market`, and
// a recording `ChainOrderSender` captures the batch payload the
// handler would dispatch.
//
// The chain-side `AppError → DomainError` mapping is exercised
// against real `bee_dex::AppError` values in `chain_sender::tests`.
// The tests here fake the sender at the `DomainError` boundary and
// pin the HTTP shape contract: response envelopes, status codes,
// error code numbers, and which inputs short-circuit before the
// chain is touched.

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
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_application::OpenOrdersPage;
use dodex_application::OpenOrdersQuery;
use dodex_application::OrderForCancel;
use dodex_application::TradingPn;
use dodex_domain::DepthSnapshot;
use dodex_domain::DomainError;
use dodex_domain::Market;
use dodex_domain::MarketAddress;
use dodex_domain::MarketEvent;
use dodex_domain::MarketName;
use dodex_domain::MarketStatus;
use dodex_domain::MarketsPage;
use dodex_domain::Outcome;
use dodex_domain::Permission;
use dodex_domain::SensitiveBytes;
use dodex_domain::Symbol;
use salvo::http::StatusCode;
use salvo::test::RequestBuilder;
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

struct FakeRepo {
    market: Mutex<Option<Market>>,
    resolver_error: Option<DomainError>,
}

impl FakeRepo {
    fn with(market: Market) -> Self {
        Self { market: Mutex::new(Some(market)), resolver_error: None }
    }

    fn empty() -> Self {
        Self { market: Mutex::new(None), resolver_error: None }
    }
}

#[async_trait]
impl MarketReadRepository for FakeRepo {
    async fn list_markets(&self, _: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        unimplemented!("list_markets is not exercised by create_batch_orders_http tests")
    }

    async fn get_depth(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        unimplemented!("get_depth is not exercised by create_batch_orders_http tests")
    }

    async fn resolve_for_new_order(
        &self,
        _: &MarketAddress,
        symbol: &Symbol,
        _: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
        if let Some(err) = self.resolver_error {
            return Err(anyhow::anyhow!(err));
        }
        let Some(market) = self.market.lock().unwrap().clone() else {
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
        _: &Symbol,
        _: u64,
        _: &str,
        _: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        unimplemented!("resolve_for_cancel is not exercised by create_batch_orders_http tests")
    }

    async fn list_open_orders(&self, _: &OpenOrdersQuery) -> Result<OpenOrdersPage, anyhow::Error> {
        unimplemented!("list_open_orders is not exercised by create_batch_orders_http tests")
    }
}

/// `ChainOrderSender` that records every batch payload it sees, or
/// fails with a configured `DomainError`. The `submit_order` /
/// `cancel_order` arms panic on call: this fake is scoped to the
/// POST /batchOrders path and reaching them would mean the router
/// fanned out the request to the wrong handler.
struct RecordingBatchSender {
    recorded: Mutex<Vec<NewBatchOrderPayload>>,
    fail_with: Option<DomainError>,
}

impl RecordingBatchSender {
    fn ok() -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: None }
    }

    fn failing(err: DomainError) -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: Some(err) }
    }

    fn calls(&self) -> Vec<NewBatchOrderPayload> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChainOrderSender for RecordingBatchSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingBatchSender::submit_order called from POST /batchOrders test")
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!("RecordingBatchSender::cancel_order called from POST /batchOrders test")
    }

    async fn submit_batch_order(&self, payload: NewBatchOrderPayload) -> Result<(), DomainError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        self.recorded.lock().unwrap().push(payload);
        Ok(())
    }
}

// ---- Fixtures ------------------------------------------------------------

const PN_ADDRESS: &str = "0:fake-pn";
const SYMBOL: &str = "PM-FAKE-YES";
const SYMBOL_NO: &str = "PM-FAKE-NO";
const MARKET_ADDRESS: &str = "0:market-fake";

fn trading_market() -> Market {
    Market {
        market_address: MarketAddress(MARKET_ADDRESS.into()),
        order_book_address: "0:ob-fake".into(),
        oracle_list_hash: "0xfeedface".into(),
        market_name: MarketName("PM-FAKE".into()),
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
        outcomes: vec![Outcome {
            outcome_id: 1,
            outcome_name: "YES".into(),
            symbol: Symbol(SYMBOL.into()),
            price_precision: 3,
            quantity_precision: 6,
            tick_size: "0.001".into(),
            step_size: "0.000001".into(),
            min_notional: "0.5".into(),
            max_batch_size: 5,
        }],
    }
}

/// Same shape as `trading_market` but with a second outcome (`NO`,
/// outcome_id=2) on the same market. Used to pin that the symbol-resolved
/// outcome propagates to the chain payload — a future refactor that
/// mis-binds to `outcomes[0]` would silently pass a one-outcome fixture.
fn trading_market_with_no_outcome() -> Market {
    let mut market = trading_market();
    market.outcomes.push(Outcome {
        outcome_id: 2,
        outcome_name: "NO".into(),
        symbol: Symbol(SYMBOL_NO.into()),
        price_precision: 3,
        quantity_precision: 6,
        tick_size: "0.001".into(),
        step_size: "0.000001".into(),
        min_notional: "0.5".into(),
        max_batch_size: 5,
    });
    market
}

fn setup_with(repo: SharedRepo, sender: SharedChainSender) -> Service {
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    Service::new(build_router(AppState::new(repo, authenticator, sender)))
}

fn valid_item(client_order_id: &str) -> serde_json::Value {
    json!({
        "newOrderClientId": client_order_id,
        "side": "BUY",
        "quantity": "1.5",
        "price": "0.615",
        "type": "LIMIT",
        "timeInForce": "GTC",
    })
}

fn valid_body_with(orders: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "orders": orders,
    })
}

fn auth_envelope() -> Vec<(&'static str, String)> {
    vec![("recvWindow", "5000".into()), ("timestamp", "0".into()), ("signature", "00".into())]
}

fn post_batch(_service: &Service, body: serde_json::Value) -> RequestBuilder {
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let mut req = TestClient::post("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", "fake", true)
        .add_header("content-type", "application/json", true);
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    req.body(body_bytes)
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchItemOk {
    client_order_id: String,
    transact_time: i64,
    status: String,
}

// ---- Happy path ----------------------------------------------------------

#[tokio::test]
async fn happy_path_two_items_returns_pending_new_array() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("22")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<BatchItemOk>>().await.expect("batch ok body");
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.status, "PENDING_NEW");
        assert!(item.transact_time > 0);
    }
    // api-spec: one `transactTime` per batch — every item carries the
    // handler's single `now_ms`, not a per-item re-clock.
    assert_eq!(items[0].transact_time, items[1].transact_time);
    assert_eq!(items[0].client_order_id, "11");
    assert_eq!(items[1].client_order_id, "22");

    // The chain dispatch carries the lifted-precision raw values per
    // spec §Chain submission. One call, two items, request order
    // preserved.
    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    let payload = &calls[0];
    assert_eq!(payload.pn_address, PN_ADDRESS);
    assert_eq!(payload.event_id, "0xevent");
    assert_eq!(payload.oracle_list_hash, "0xfeedface");
    assert_eq!(payload.token_type, 1);
    assert_eq!(payload.orders.len(), 2);
    assert_eq!(payload.orders[0].client_order_id, "11");
    assert_eq!(payload.orders[1].client_order_id, "22");
    assert_eq!(payload.orders[0].outcome_id, 1);
    assert!(payload.orders[0].is_buy);
    assert_eq!(payload.orders[0].price_raw, "615");
    assert_eq!(payload.orders[0].amount_raw, "1500000");
    assert_eq!(payload.orders[0].flags, 0); // LIMIT × GTC
}

#[tokio::test]
async fn multi_outcome_market_routes_to_symbol_outcome() {
    // Symbol resolves to `outcomes[1]` (outcome_id=2). Pins that the
    // payload carries the symbol-resolved outcome, not `outcomes[0]` —
    // catches a future refactor that hard-codes the first outcome on a
    // market that now has more than one.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market_with_no_outcome()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL_NO,
        "orders": [valid_item("11"), valid_item("22")],
    });
    let resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].orders.len(), 2);
    assert!(calls[0].orders.iter().all(|o| o.outcome_id == 2));
}

#[tokio::test]
async fn generated_client_order_ids_when_absent() {
    // Per spec §clientOrderId generation: items without
    // `newOrderClientId` get a fresh u64-bounded decimal id, distinct
    // intra-batch (otherwise chain ERR_INVALID_PARAMS on coid
    // collision would always fire).
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let item = json!({
        "side": "BUY",
        "quantity": "1.5",
        "price": "0.615",
        "type": "LIMIT",
        "timeInForce": "GTC",
    });
    let body = valid_body_with(vec![item.clone(), item]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<BatchItemOk>>().await.expect("batch ok body");
    assert_eq!(items.len(), 2);
    for item in &items {
        assert!(!item.client_order_id.is_empty());
        assert!(item.client_order_id.parse::<u64>().is_ok());
    }
    assert_ne!(items[0].client_order_id, items[1].client_order_id);
}

// ---- Body-shape failures -------------------------------------------------

#[tokio::test]
async fn malformed_json_body_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = b"{not valid json".to_vec();
    let mut req = TestClient::post("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", "fake", true)
        .add_header("content-type", "application/json", true);
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    let mut resp = req.body(body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn missing_market_address_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "symbol": SYMBOL,
        "orders": [valid_item("11")],
    });
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn empty_market_address_returns_400_minus_1102() {
    // Present-but-blank `marketAddress` collapses through `non_empty`
    // to the same -1102 the missing-field path returns. Locks the
    // NonEmpty boundary so a future refactor that swaps the helper for
    // a raw `Option::is_some` check doesn't quietly accept `""`.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": "",
        "symbol": SYMBOL,
        "orders": [valid_item("11")],
    });
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_symbol_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "orders": [valid_item("11")],
    });
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_orders_field_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
    });
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn empty_orders_returns_400_minus_1130() {
    // Empty `orders[]` is a present-but-shape-invalid body — surface
    // -1130 to distinguish from a missing field.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    // Pre-flight gate must fire before market resolution and chain.
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn over_max_batch_size_returns_400_minus_1130() {
    // trading_market().outcomes[0].max_batch_size == 5. Six items
    // must fail locally with -1130 instead of paying a chain
    // ERR_BATCH_TOO_LARGE round-trip.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let orders: Vec<_> = (0..6).map(|i| valid_item(&i.to_string())).collect();
    let body = valid_body_with(orders);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn per_item_missing_side_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "quantity": "1.5",
            "price": "0.615",
            "type": "LIMIT",
            "timeInForce": "GTC",
        }),
    ]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn per_item_invalid_side_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "side": "HOLD",
            "quantity": "1.5",
            "price": "0.615",
        }),
    ]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn per_item_excess_precision_returns_400_minus_1111() {
    // First item OK, second has 4 dp against pricePrecision=3. The
    // entire batch must reject with the per-item failure code; the
    // chain must NOT see the call.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "side": "BUY",
            "quantity": "1.5",
            "price": "0.6155",
            "type": "LIMIT",
            "timeInForce": "GTC",
        }),
    ]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1111);
    assert!(sender.calls().is_empty(), "chain hit despite per-item reject");
}

// ---- Market/status failures ----------------------------------------------

#[tokio::test]
async fn unknown_market_returns_404_minus_1121() {
    let repo: SharedRepo = Arc::new(FakeRepo::empty());
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
}

#[tokio::test]
async fn non_trading_market_returns_400_minus_2010() {
    let mut market = trading_market();
    market.status = MarketStatus::Resolving;
    let repo: SharedRepo = Arc::new(FakeRepo::with(market));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
}

#[tokio::test]
async fn blank_oracle_list_hash_returns_503_minus_1500() {
    let mut market = trading_market();
    market.oracle_list_hash = String::new();
    let repo: SharedRepo = Arc::new(FakeRepo::with(market));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

// ---- Chain sender outcomes -----------------------------------------------

#[tokio::test]
async fn intra_batch_client_order_id_collision_returns_400_minus_1130() {
    // Spec: duplicate `newOrderClientId` within a single batch is not
    // pre-validated locally — the chain raises ERR_INVALID_PARAMS (129)
    // on its own coid check, which maps to `DomainError::InvalidParameter`
    // and surfaces as `-1130 / 400`. Simulate the chain leg with a
    // failing sender; this pins the HTTP shape contract for the path.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender =
        Arc::new(RecordingBatchSender::failing(DomainError::InvalidParameter));
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn intra_batch_duplicate_coids_reach_chain_unfiltered() {
    // Companion to the chain-fails-129 pin above: pins the local
    // contract that intra-batch duplicate `newOrderClientId` values
    // are NOT filtered before the chain. If a future refactor adds
    // a local HashSet pre-check, both coids would no longer reach
    // the payload and `submit_batch_order` would not be called.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("11")]);
    let resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].orders.len(), 2);
    assert_eq!(calls[0].orders[0].client_order_id, "11");
    assert_eq!(calls[0].orders[1].client_order_id, "11");
}

#[tokio::test]
async fn chain_timeout_returns_504_minus_1007() {
    // Sender raising `RequestTimeout` simulates an elapsed
    // `place_batch_timeout_ms` budget inside `classify_chain_outcome`.
    // Pins that the use-case error propagates through the handler's
    // `?` — a dropped propagation would silently land the request as
    // 200 with an empty body.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender =
        Arc::new(RecordingBatchSender::failing(DomainError::RequestTimeout));
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("22")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
}

#[tokio::test]
async fn pn_busy_returns_429_minus_2014() {
    // Sender raising `OrderPnBusy` simulates a real `ERR_NOTE_BUSY`
    // (121) coming back from `bee_dex::Dex::place_batch` while another
    // op from the same PN is still in flight.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender =
        Arc::new(RecordingBatchSender::failing(DomainError::OrderPnBusy));
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("22")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::TOO_MANY_REQUESTS));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2014);
}

// ---- Authorization -------------------------------------------------------

#[tokio::test]
async fn caller_without_trade_permission_returns_401() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(repo, authenticator, sender)));

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1002);
}
