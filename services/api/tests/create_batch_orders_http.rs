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

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use common::now_ms;
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
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_application::OrderForCancel;
use dodex_application::OrdersPage;
use dodex_application::OrdersQuery;
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
    /// Raw anyhow that doesn't wrap a DomainError — exercises the
    /// non-domain fallback branch in `CreateBatchOrdersUseCase::execute`.
    resolver_raw_error: Option<String>,
}

impl FakeRepo {
    fn with(market: Market) -> Self {
        Self { market: Mutex::new(Some(market)), resolver_error: None, resolver_raw_error: None }
    }

    fn empty() -> Self {
        Self { market: Mutex::new(None), resolver_error: None, resolver_raw_error: None }
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
        if let Some(msg) = &self.resolver_raw_error {
            return Err(anyhow::anyhow!("{msg}"));
        }
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
        _: &Symbol,
        _: u64,
        _: &str,
        _: i64,
    ) -> Result<OrderForCancel, anyhow::Error> {
        unimplemented!("resolve_for_cancel is not exercised by create_batch_orders_http tests")
    }

    async fn resolve_for_cancel_batch(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: &[u64],
        _: &str,
        _: i64,
    ) -> Result<Option<dodex_application::CancelBatchResolution>, anyhow::Error> {
        unimplemented!(
            "resolve_for_cancel_batch is not exercised by create_batch_orders_http tests"
        )
    }

    async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by create_batch_orders_http tests")
    }

    async fn resolve_market_for_balances(
        &self,
        _: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error> {
        unimplemented!(
            "resolve_market_for_balances is not exercised by create_batch_orders_http tests"
        )
    }

    async fn sum_open_sell_remaining(
        &self,
        _: &str,
        _: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        unimplemented!("sum_open_sell_remaining is not exercised by create_batch_orders_http tests")
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

    async fn cancel_batch_order(
        &self,
        _: dodex_application::CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("RecordingBatchSender::cancel_batch_order called from POST /batchOrders test")
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
    Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )))
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
    let before_ms = now_ms();
    let mut resp = post_batch(&service, body).send(&service).await;
    let after_ms = now_ms();
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<BatchItemOk>>().await.expect("batch ok body");
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.status, "PENDING_NEW");
        assert!(
            (before_ms..=after_ms).contains(&item.transact_time),
            "transactTime {} outside window [{}, {}]",
            item.transact_time,
            before_ms,
            after_ms,
        );
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

#[tokio::test]
async fn exactly_max_batch_size_returns_pending_new_array() {
    // `max_batch_size = 5` per the trading_market fixture. A batch of
    // exactly 5 items must reach the chain and round-trip a 5-item
    // PENDING_NEW array — pins the boundary against an off-by-one in
    // the `orders.len() > max_batch_size` gate.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let orders: Vec<_> = (0..5).map(|i| valid_item(&i.to_string())).collect();
    let mut resp = post_batch(&service, valid_body_with(orders)).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let items = resp.take_json::<Vec<BatchItemOk>>().await.expect("batch ok body");
    assert_eq!(items.len(), 5);
    for item in &items {
        assert_eq!(item.status, "PENDING_NEW");
    }
    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].orders.len(), 5);
}

#[tokio::test]
async fn per_item_missing_quantity_returns_400_minus_1102() {
    // Per-item required-field guard analog to the missing-`side` test:
    // a missing `quantity` must short-circuit at the shape gate with
    // -1102, before market resolution or chain dispatch.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "side": "BUY",
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
async fn per_item_quantity_excess_precision_returns_400_minus_1111() {
    // Symmetric to `per_item_excess_precision_returns_400_minus_1111`
    // but on the quantity side: stepSize = 0.000001 → 6 dp; 7 dp must
    // reject the whole batch with -1111 before the chain is touched.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "side": "BUY",
            "quantity": "1.5000005",
            "price": "0.615",
            "type": "LIMIT",
            "timeInForce": "GTC",
        }),
    ]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1111);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn per_item_below_min_notional_returns_400_minus_2010() {
    // minNotional = 0.5; a LIMIT BUY at price=0.001 × quantity=0.000001
    // = 1e-9 notional must reject the whole batch with -2010
    // (OrderValidationFailed) before the chain is touched.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = valid_body_with(vec![
        valid_item("11"),
        json!({
            "newOrderClientId": "22",
            "side": "BUY",
            "quantity": "0.000001",
            "price": "0.001",
            "type": "LIMIT",
            "timeInForce": "GTC",
        }),
    ]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
    assert!(sender.calls().is_empty());
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

#[tokio::test]
async fn chain_market_inconsistent_returns_503_minus_1500() {
    // Sender raising `MarketInconsistent` simulates `ERR_BATCH_TOO_LARGE`
    // (161) or `ERR_EMPTY_BATCH` (162) coming back from the chain — i.e.
    // the local guard and the chain's defence-in-depth disagree on the
    // batch-size envelope. The use case classifies that as a service-state
    // inconsistency, not a client error, and the wire shape must surface
    // as 503/-1500 so ops sees it instead of the client retrying a -1130.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender =
        Arc::new(RecordingBatchSender::failing(DomainError::MarketInconsistent));
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11"), valid_item("22")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

// ---- Authorization -------------------------------------------------------

#[tokio::test]
async fn caller_without_trade_permission_returns_401() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1002);
}

// ---- Resolver / timeout boundary -----------------------------------------

#[tokio::test]
async fn resolver_raw_anyhow_returns_1000() {
    // Pin the non-domain fallback in `CreateBatchOrdersUseCase::execute`:
    // a resolver anyhow without a `DomainError` cause must surface as
    // 500/-1000, not be swallowed. The `error!` log on that branch is
    // observable in CI logs and the branch is structurally exercised
    // by this test.
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolver_raw("simulated sqlx pool drop"));
    let sender: SharedChainSender = Arc::new(RecordingBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = valid_body_with(vec![valid_item("11")]);
    let mut resp = post_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1000);
}

/// `ChainOrderSender` whose `submit_batch_order` hangs longer than any
/// reasonable test budget. The `entered` counter is bumped on first
/// dispatch so the timeout test can prove the handler actually reached
/// the chain sender (rather than failing earlier — at auth, market
/// resolve, or use-case validation).
struct SlowBatchSender {
    entered: std::sync::atomic::AtomicU32,
}

impl SlowBatchSender {
    fn new() -> Self {
        Self { entered: std::sync::atomic::AtomicU32::new(0) }
    }

    fn entered_count(&self) -> u32 {
        self.entered.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl ChainOrderSender for SlowBatchSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowBatchSender::submit_order called from POST /batchOrders test")
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowBatchSender::cancel_order called from POST /batchOrders test")
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        self.entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 5 s is indefinitely longer than any reasonable budget; the
        // test caps it at 50 ms so wall-clock stays in the tens of ms.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(())
    }

    async fn cancel_batch_order(
        &self,
        _: dodex_application::CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("SlowBatchSender::cancel_batch_order called from POST /batchOrders test")
    }
}

#[tokio::test]
async fn handler_exceeding_request_timeout_returns_504_minus_1007() {
    // A chain call hanging past `request_timeout` must surface as
    // 504/-1007 via the timeout hoop, not stall the worker.
    // `SlowBatchSender` plays the stuck-chain role; 50 ms budget keeps
    // the test fast.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(SlowBatchSender::new());
    let sender_dyn: SharedChainSender = sender.clone();
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    let state = AppState::new(
        repo,
        authenticator,
        sender_dyn,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )
    .with_request_timeout(std::time::Duration::from_millis(50));
    let service = Service::new(build_router(state));

    let started = std::time::Instant::now();
    let mut resp = post_batch(&service, valid_body_with(vec![valid_item("11"), valid_item("22")]))
        .send(&service)
        .await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
    // Tight bound — well below SlowBatchSender's 5 s sleep — confirms
    // the hoop actually cancelled the handler.
    assert!(elapsed < std::time::Duration::from_secs(1), "elapsed {elapsed:?}");
    // Pin that the cancellation happened MID-`submit_batch_order`, not
    // before the handler reached it — otherwise this test would also
    // pass against an unrelated upstream short-circuit (auth/resolve)
    // that happens to take >50 ms.
    assert_eq!(sender.entered_count(), 1, "submit_batch_order was not entered");
}
