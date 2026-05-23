// HTTP-level integration tests for `DELETE /api/v1/batchOrders` that
// exercise the handler + use case end to end **without** a database or
// a real chain. Mirrors the triad in `cancel_order_http.rs` and
// `create_batch_orders_http.rs`: a fake `Authenticator` short-circuits
// HMAC, a fake `MarketReadRepository` returns a configurable `Market`
// plus per-id rows, and a recording `ChainOrderSender` captures the
// `cancelBatch` payload the handler would dispatch in production.
//
// Matching per-row coverage for the error-mapping table in
// `docs/tech-specs/write-api.md §DELETE /api/v1/batchOrders` lives here.

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
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_application::OrderForCancel;
use dodex_application::OrderForCancelBatch;
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

/// `MarketReadRepository` that resolves a configurable `Market` and a
/// set of `(order_id, client_order_id)` rows for the batch resolution
/// path. Both arms can be set independently, so tests can isolate
/// market-side failures from order-side failures.
///
/// `scrambled_rows` flips the row order returned by
/// `resolve_for_cancel_batch` so the use case's reordering is genuinely
/// exercised — Postgres has no `ORDER BY` on the underlying SELECT and
/// production rows can come back in any order.
struct FakeRepo {
    market: Mutex<Option<Market>>,
    /// Pair of (owner_pn_address, row). Default owner for rows added
    /// via `with_market_and_rows` is `PN_ADDRESS`; cross-account tests
    /// use `with_market_and_owned_rows` to seed rows owned by another
    /// account so the production `lo.owner_pn_address = $4` predicate
    /// stays modelled at the HTTP boundary.
    rows: Mutex<Vec<(String, OrderForCancelBatch)>>,
    scrambled_rows: bool,
}

impl FakeRepo {
    fn with_market_and_rows(market: Market, rows: Vec<OrderForCancelBatch>) -> Self {
        let owned = rows.into_iter().map(|r| (PN_ADDRESS.to_string(), r)).collect();
        Self { market: Mutex::new(Some(market)), rows: Mutex::new(owned), scrambled_rows: false }
    }

    fn with_market_and_owned_rows(
        market: Market,
        rows: Vec<(String, OrderForCancelBatch)>,
    ) -> Self {
        Self { market: Mutex::new(Some(market)), rows: Mutex::new(rows), scrambled_rows: false }
    }

    fn with_market(market: Market) -> Self {
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(Vec::new()),
            scrambled_rows: false,
        }
    }

    fn scrambled(mut self) -> Self {
        self.scrambled_rows = true;
        self
    }

    fn empty() -> Self {
        Self { market: Mutex::new(None), rows: Mutex::new(Vec::new()), scrambled_rows: false }
    }
}

#[async_trait]
impl MarketReadRepository for FakeRepo {
    async fn list_markets(&self, _: &MarketsRequest) -> Result<MarketsPage, anyhow::Error> {
        unimplemented!("list_markets is not exercised by cancel_batch_orders_http tests")
    }

    async fn get_depth(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        _: u16,
    ) -> Result<DepthSnapshot, anyhow::Error> {
        unimplemented!("get_depth is not exercised by cancel_batch_orders_http tests")
    }

    async fn resolve_for_new_order(
        &self,
        _: &MarketAddress,
        symbol: &Symbol,
        _: i64,
    ) -> Result<MarketForPlacement, anyhow::Error> {
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
        unimplemented!("resolve_for_cancel is not exercised by cancel_batch_orders_http tests")
    }

    async fn resolve_for_cancel_batch(
        &self,
        _: &MarketAddress,
        _: &Symbol,
        order_ids: &[u64],
        owner_pn_address: &str,
        _: i64,
    ) -> Result<Vec<OrderForCancelBatch>, anyhow::Error> {
        // Production Postgres returns matching rows in arbitrary order;
        // when `scrambled_rows` is set we reverse to verify the use
        // case reorders into the request sequence. Owner and id filters
        // mirror the production predicate set so a regression that
        // drops `lo.owner_pn_address = $4` would actually fail an HTTP
        // test, not slip through silently.
        let stored = self.rows.lock().unwrap().clone();
        let mut matched: Vec<OrderForCancelBatch> = stored
            .into_iter()
            .filter(|(owner, row)| {
                owner == owner_pn_address && order_ids.iter().any(|&id| id == row.order_id)
            })
            .map(|(_, row)| row)
            .collect();
        if self.scrambled_rows {
            matched.reverse();
        }
        Ok(matched)
    }

    async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by cancel_batch_orders_http tests")
    }
}

/// `ChainOrderSender` that records every cancelBatch payload it sees,
/// or fails with a configured `DomainError`. The other arms panic on
/// call — this test file covers DELETE /batchOrders only and reaching
/// them would mean the suite mixed concerns.
struct RecordingCancelBatchSender {
    recorded: Mutex<Vec<CancelBatchOrderPayload>>,
    fail_with: Option<DomainError>,
}

impl RecordingCancelBatchSender {
    fn ok() -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: None }
    }

    fn failing(err: DomainError) -> Self {
        Self { recorded: Mutex::new(Vec::new()), fail_with: Some(err) }
    }

    fn calls(&self) -> Vec<CancelBatchOrderPayload> {
        self.recorded.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChainOrderSender for RecordingCancelBatchSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!(
            "RecordingCancelBatchSender::submit_order called from DELETE /batchOrders test"
        )
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!(
            "RecordingCancelBatchSender::cancel_order called from DELETE /batchOrders test"
        )
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!(
            "RecordingCancelBatchSender::submit_batch_order called from DELETE /batchOrders test"
        )
    }

    async fn cancel_batch_order(
        &self,
        payload: CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
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
const MARKET_ADDRESS: &str = "0:market-fake";

fn trading_market() -> Market {
    Market {
        market_address: MarketAddress(MARKET_ADDRESS.into()),
        order_book_address: "0:ob-fake".into(),
        oracle_list_hash: "0xfeedface".into(),
        market_name: MarketName("PM-FAKE".into()),
        status: MarketStatus::Trading,
        quote_asset: "NACKL".into(),
        token_type: 7,
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

fn row(order_id: u64, coid: Option<&str>) -> OrderForCancelBatch {
    OrderForCancelBatch {
        order_id,
        client_order_id: coid.map(|s| s.to_string()),
        market_status: MarketStatus::Trading,
    }
}

fn setup_with(repo: SharedRepo, sender: SharedChainSender) -> Service {
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    Service::new(build_router(AppState::new(repo, authenticator, sender)))
}

fn auth_envelope() -> Vec<(&'static str, String)> {
    vec![("recvWindow", "5000".into()), ("timestamp", "0".into()), ("signature", "00".into())]
}

fn delete_batch(_service: &Service, body: serde_json::Value) -> RequestBuilder {
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let mut req = TestClient::delete("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", "fake", true)
        .add_header("content-type", "application/json", true);
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    req.body(body_bytes)
}

fn valid_body(order_ids: Vec<&str>) -> serde_json::Value {
    json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "orderIds": order_ids,
    })
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelBatchItem {
    order_id: String,
    client_order_id: String,
    transact_time: i64,
    status: String,
}

// ---- Happy path ----------------------------------------------------------

#[tokio::test]
async fn happy_path_two_ids_returns_pending_cancel_array() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_rows(
        trading_market(),
        vec![row(123, Some("client-42")), row(456, None)],
    ));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["123", "456"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<CancelBatchItem>>().await.expect("cancel batch body");
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(item.status, "PENDING_CANCEL");
        assert!(item.transact_time > 0);
    }
    // api-spec §Cancel Batch Orders: "Identical across every item —
    // one chain submission, one moment of acceptance." A regression
    // that calls `now_millis()` per item would break this.
    assert_eq!(items[0].transact_time, items[1].transact_time);
    assert_eq!(items[0].order_id, "123");
    assert_eq!(items[0].client_order_id, "client-42");
    assert_eq!(items[1].order_id, "456");
    // api-spec §Cancel Batch Orders: empty string when the order was
    // placed without a `newOrderClientId`.
    assert_eq!(items[1].client_order_id, "");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    let payload = &calls[0];
    assert_eq!(payload.pn_address, PN_ADDRESS);
    assert_eq!(payload.event_id, "0xevent");
    assert_eq!(payload.oracle_list_hash, "0xfeedface");
    assert_eq!(payload.token_type, 7);
    assert_eq!(payload.order_ids, vec![123u64, 456u64]);
}

#[tokio::test]
async fn response_preserves_input_order_when_repo_returns_scrambled_rows() {
    // Production Postgres has no ORDER BY on the bulk SELECT — the
    // handler must reorder by request sequence so callers can correlate
    // positionally. Inject scrambled rows to verify.
    let repo: SharedRepo = Arc::new(
        FakeRepo::with_market_and_rows(
            trading_market(),
            vec![row(11, Some("a")), row(22, Some("b")), row(33, Some("c"))],
        )
        .scrambled(),
    );
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    // Input: 33, 11, 22 (deliberately scrambled). The repo above also
    // reverses what it returns, so the only way the response can come
    // back in [33, 11, 22] order is the handler's reorder step.
    let mut resp = delete_batch(&service, valid_body(vec!["33", "11", "22"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<CancelBatchItem>>().await.expect("cancel batch body");
    let ids: Vec<&str> = items.iter().map(|i| i.order_id.as_str()).collect();
    assert_eq!(ids, vec!["33", "11", "22"]);
    let coids: Vec<&str> = items.iter().map(|i| i.client_order_id.as_str()).collect();
    assert_eq!(coids, vec!["c", "a", "b"]);

    // Chain payload must also carry the input order verbatim — the
    // chain's _doCancel processes ids in batch order.
    assert_eq!(sender.calls()[0].order_ids, vec![33u64, 11u64, 22u64]);
}

// ---- Body-shape failures -------------------------------------------------

#[tokio::test]
async fn malformed_json_body_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = b"{not valid json".to_vec();
    let mut req = TestClient::delete("http://test/api/v1/batchOrders")
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
async fn empty_body_returns_400_minus_1130() {
    // Some HTTP proxies / SDKs strip bodies from DELETE requests; an
    // empty body fails JSON parsing the same way a malformed one does
    // and routes through the same `parse_json` → InvalidParameter
    // channel as `POST /order` and `POST /batchOrders`.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut req = TestClient::delete("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", "fake", true)
        .add_header("content-type", "application/json", true);
    for (k, v) in auth_envelope() {
        req = req.query(k, v);
    }
    let mut resp = req.body(Vec::<u8>::new()).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn missing_market_address_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "symbol": SYMBOL,
        "orderIds": ["1"],
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_symbol_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "orderIds": ["1"],
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn missing_order_ids_field_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn empty_order_ids_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec![])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    // Pre-flight gate must fire before chain.
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn over_max_batch_size_returns_400_minus_1130() {
    // trading_market().outcomes[0].max_batch_size == 5; six ids must
    // fail locally with -1130 instead of paying a chain
    // ERR_BATCH_TOO_LARGE round-trip.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let ids = vec!["1", "2", "3", "4", "5", "6"];
    let mut resp = delete_batch(&service, valid_body(ids)).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn intra_batch_duplicate_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["10", "20", "10"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn non_numeric_order_id_returns_400_minus_1130() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp =
        delete_batch(&service, valid_body(vec!["1", "not-a-number"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn over_u64_order_id_returns_400_minus_1130() {
    // ABI is uint128[] but the SDK-serialization ceiling at the public
    // boundary is u64; `u64::MAX + 1` overflows the parser → -1130.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let overflow = "18446744073709551616"; // u64::MAX + 1
    let mut resp = delete_batch(&service, valid_body(vec!["1", overflow])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn blank_order_id_string_returns_400_minus_1102() {
    // An empty element is the absence of a value for that slot rather
    // than a malformed value — surface -1102 to distinguish.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1", ""])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn whitespace_only_order_id_string_returns_400_minus_1102() {
    // A whitespace-only element is the same "absent slot" case as the
    // empty-string one above — `non_empty` trims before checking, so
    // both must route to -1102. Pinning this catches a regression that
    // drops the trim step and lets `"  ".parse::<u64>()` collapse the
    // case into -1130 InvalidParameter.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1", "  "])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

// ---- Market-side failures ------------------------------------------------

#[tokio::test]
async fn unknown_market_returns_404_minus_1121() {
    let repo: SharedRepo = Arc::new(FakeRepo::empty());
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
}

#[tokio::test]
async fn non_trading_market_returns_400_minus_2010() {
    let mut market = trading_market();
    market.status = MarketStatus::Resolving;
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(market));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
}

#[tokio::test]
async fn blank_oracle_list_hash_returns_503_minus_1500() {
    let mut market = trading_market();
    market.oracle_list_hash = String::new();
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(market));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

// ---- Order-side failures -------------------------------------------------

#[tokio::test]
async fn unknown_order_returns_404_minus_2011() {
    // Repo resolves the market but returns zero rows for the requested
    // ids — atomic validation rejects the whole batch with the same
    // opaque error code single-cancel uses.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1", "2"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2011);
    // Atomic validation: no chain message dispatched.
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn partial_shortfall_rejects_whole_batch_with_minus_2011() {
    // Two ids requested, only one resolves — atomic surface returns
    // UnknownOrder for the whole request. No chain message sent.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, Some("a"))]));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1", "2"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2011);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn wrong_owner_returns_404_minus_2011() {
    // Pins the `lo.owner_pn_address = $4` predicate at the HTTP
    // boundary: a row owned by another account must be invisible to
    // this caller, collapsing the batch into a shortfall → -2011 with
    // the same deliberate opacity as single-cancel. Without the fake's
    // owner filter, a regression that drops the predicate from
    // postgres_repo.rs would only be caught by the pg integration
    // tests.
    let foreign_row = row(1, Some("attackers-coid"));
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_owned_rows(
        trading_market(),
        vec![("0:someone-else".to_string(), foreign_row)],
    ));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2011);
    assert!(sender.calls().is_empty());
}

// ---- Chain-side failures -------------------------------------------------

#[tokio::test]
async fn chain_batch_size_drift_returns_503_minus_1500() {
    // Defence-in-depth: the use case pre-rejects oversize/empty batches
    // locally, but the chain still raises ERR_BATCH_TOO_LARGE (161) /
    // ERR_EMPTY_BATCH (162) if the local guard is bypassed. Reaching
    // either code means the read-model's `max_batch_size` drifted from
    // the on-chain ceiling — a server-state inconsistency, not a client
    // bug — so `chain_sender::map_tvm_exit_code` surfaces it as
    // `DomainError::MarketInconsistent` → 503 / -1500. Simulate the
    // chain leg with a failing sender; this pins the HTTP shape contract
    // for that path.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)]));
    let sender: SharedChainSender =
        Arc::new(RecordingCancelBatchSender::failing(DomainError::MarketInconsistent));
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::SERVICE_UNAVAILABLE));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1500);
}

#[tokio::test]
async fn pn_busy_returns_429_minus_2014() {
    // Sender raising `OrderPnBusy` simulates a real `ERR_NOTE_BUSY`
    // (121) coming back from `bee_dex::Dex::cancel_batch` while another
    // op from the same PN is still in flight.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)]));
    let sender: SharedChainSender =
        Arc::new(RecordingCancelBatchSender::failing(DomainError::OrderPnBusy));
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::TOO_MANY_REQUESTS));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2014);
}

#[tokio::test]
async fn chain_request_timeout_returns_504_minus_1007() {
    // `classify_chain_outcome` maps the elapsed branch to
    // `DomainError::RequestTimeout` when `bee_dex::Dex::cancel_batch`
    // doesn't return within `chain.cancel_batch_timeout_ms`. The
    // `SlowCancelBatchSender` test below exercises the wall-clock hoop
    // around the whole handler; this one pins the HTTP shape for the
    // chain-leg timeout specifically, symmetric with the create-batch
    // sibling `chain_timeout_returns_504_minus_1007`.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)]));
    let sender: SharedChainSender =
        Arc::new(RecordingCancelBatchSender::failing(DomainError::RequestTimeout));
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
}

// ---- Authorization -------------------------------------------------------

#[tokio::test]
async fn caller_without_trade_permission_returns_401() {
    // require_auth(Permission::Trade) catches this before the use case
    // sees the request — mirrors the same enforcement on the create
    // and single-cancel paths.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(repo, authenticator, sender)));

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1002);
}

// ---- Resolver / timeout boundary -----------------------------------------

/// `ChainOrderSender` whose `cancel_batch_order` hangs longer than any
/// reasonable test budget. The `entered` counter is bumped on first
/// dispatch so the timeout test can prove the handler actually reached
/// the chain sender (rather than failing earlier — at auth, market
/// resolve, or use-case validation).
struct SlowCancelBatchSender {
    entered: std::sync::atomic::AtomicU32,
}

impl SlowCancelBatchSender {
    fn new() -> Self {
        Self { entered: std::sync::atomic::AtomicU32::new(0) }
    }

    fn entered_count(&self) -> u32 {
        self.entered.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl ChainOrderSender for SlowCancelBatchSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowCancelBatchSender::submit_order called from DELETE /batchOrders test")
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        unreachable!("SlowCancelBatchSender::cancel_order called from DELETE /batchOrders test")
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        unreachable!(
            "SlowCancelBatchSender::submit_batch_order called from DELETE /batchOrders test"
        )
    }

    async fn cancel_batch_order(&self, _: CancelBatchOrderPayload) -> Result<(), DomainError> {
        self.entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 5 s is indefinitely longer than any reasonable budget; the
        // test caps it at 50 ms so wall-clock stays in the tens of ms.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(())
    }
}

#[tokio::test]
async fn handler_exceeding_request_timeout_returns_504_minus_1007() {
    // A chain cancel_batch hanging past `request_timeout` must surface as
    // 504/-1007 via the timeout hoop, not stall the worker. Symmetric
    // with the create-batch sibling test.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)]));
    let sender = Arc::new(SlowCancelBatchSender::new());
    let sender_dyn: SharedChainSender = sender.clone();
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    let state = AppState::new(repo, authenticator, sender_dyn)
        .with_request_timeout(std::time::Duration::from_millis(50));
    let service = Service::new(build_router(state));

    let started = std::time::Instant::now();
    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
    // Tight bound — well below SlowCancelBatchSender's 5 s sleep —
    // confirms the hoop actually cancelled the handler.
    assert!(elapsed < std::time::Duration::from_secs(1), "elapsed {elapsed:?}");
    // Pin that the cancellation happened MID-`cancel_batch_order`, not
    // before the handler reached it — otherwise this test would also
    // pass against an unrelated upstream short-circuit (auth/resolve)
    // that happens to take >50 ms.
    assert_eq!(sender.entered_count(), 1, "cancel_batch_order was not entered");
}
