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
use dodex_application::CancelBatchResolution;
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::MarketBalancesResolution;
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
/// set of `(owner_pn_address, order_id, OrderForCancelBatch)` rows for
/// the batch resolution path. Both arms can be set independently, so
/// tests can isolate market-side failures from order-side failures.
/// The fake's `resolve_for_cancel_batch` returns a `HashMap` whose
/// iteration order is unspecified — that's deliberate, so a test
/// exercising the handler's input-position reorder cannot accidentally
/// pass by virtue of the fake handing rows back in input order.
type StoredRow = (String, u64, OrderForCancelBatch);

struct FakeRepo {
    market: Mutex<Option<Market>>,
    /// Triple of (owner_pn_address, order_id, row prototype). Default
    /// owner for rows added via `with_market_and_rows` is `PN_ADDRESS`;
    /// cross-account tests use `with_market_and_owned_rows` to seed
    /// rows owned by another account so the production
    /// `lo.owner_pn_address = $4` predicate stays modelled at the HTTP
    /// boundary.
    rows: Mutex<Vec<StoredRow>>,
    /// Raw anyhow (no `DomainError` cause) to return from the named
    /// resolver method — exercises the non-domain downcast fallback in
    /// `CancelBatchOrdersUseCase::execute` that maps the failure to
    /// `Unexpected` → 500 / -1000.
    resolve_for_new_order_raw_anyhow: Option<String>,
    resolve_for_cancel_batch_raw_anyhow: Option<String>,
    /// Overrides `market_status` on the `CancelBatchResolution`
    /// wrapper returned by `resolve_for_cancel_batch` while leaving
    /// `resolve_for_new_order` untouched — simulates a reconciler
    /// commit between the two independent MVCC snapshots.
    cancel_batch_status_override: Option<MarketStatus>,
}

impl FakeRepo {
    fn with_market_and_rows(market: Market, rows: Vec<(u64, OrderForCancelBatch)>) -> Self {
        let owned =
            rows.into_iter().map(|(id, proto)| (PN_ADDRESS.to_string(), id, proto)).collect();
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(owned),
            resolve_for_new_order_raw_anyhow: None,
            resolve_for_cancel_batch_raw_anyhow: None,
            cancel_batch_status_override: None,
        }
    }

    fn with_market_and_owned_rows(market: Market, rows: Vec<StoredRow>) -> Self {
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(rows),
            resolve_for_new_order_raw_anyhow: None,
            resolve_for_cancel_batch_raw_anyhow: None,
            cancel_batch_status_override: None,
        }
    }

    fn with_market(market: Market) -> Self {
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(Vec::new()),
            resolve_for_new_order_raw_anyhow: None,
            resolve_for_cancel_batch_raw_anyhow: None,
            cancel_batch_status_override: None,
        }
    }

    fn with_cancel_batch_status(mut self, status: MarketStatus) -> Self {
        self.cancel_batch_status_override = Some(status);
        self
    }

    fn empty() -> Self {
        Self {
            market: Mutex::new(None),
            rows: Mutex::new(Vec::new()),
            resolve_for_new_order_raw_anyhow: None,
            resolve_for_cancel_batch_raw_anyhow: None,
            cancel_batch_status_override: None,
        }
    }

    fn failing_resolve_for_new_order_raw(market: Market, msg: &str) -> Self {
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(Vec::new()),
            resolve_for_new_order_raw_anyhow: Some(msg.to_string()),
            resolve_for_cancel_batch_raw_anyhow: None,
            cancel_batch_status_override: None,
        }
    }

    fn failing_resolve_for_cancel_batch_raw(market: Market, msg: &str) -> Self {
        Self {
            market: Mutex::new(Some(market)),
            rows: Mutex::new(Vec::new()),
            resolve_for_new_order_raw_anyhow: None,
            resolve_for_cancel_batch_raw_anyhow: Some(msg.to_string()),
            cancel_batch_status_override: None,
        }
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
        if let Some(msg) = &self.resolve_for_new_order_raw_anyhow {
            return Err(anyhow::anyhow!("{msg}"));
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
            // Test token: chain decimals == fixture display precision (6).
            decimals: 6,
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
        symbol: &Symbol,
        order_ids: &[u64],
        owner_pn_address: &str,
        _: i64,
    ) -> Result<Option<CancelBatchResolution>, anyhow::Error> {
        if let Some(msg) = &self.resolve_for_cancel_batch_raw_anyhow {
            return Err(anyhow::anyhow!("{msg}"));
        }
        // Owner, symbol, and id filters mirror the production
        // predicate set so a regression that drops `lo.owner_pn_address
        // = $4` or `mo.symbol = $2` would actually fail an HTTP test,
        // not slip through silently. The HashMap return type keys by
        // chain order_id (natural identity); HashMap iteration order
        // is unspecified so the use case can't accidentally depend on
        // a fake-imposed order.
        let Some(market) = self.market.lock().unwrap().clone() else {
            return Ok(None);
        };
        // Production SQL joins on `mo.symbol = $2`; if the symbol
        // doesn't exist on this market, no rows can join. Without
        // this gate the fake would silently match rows by id alone
        // and an early-resolve-for-new-order refactor would let
        // `unknown_symbol_on_known_market_returns_404_minus_1121`
        // pass via the wrong code path.
        if !market.outcomes.iter().any(|o| o.symbol == *symbol) {
            return Ok(None);
        }
        let stored = self.rows.lock().unwrap().clone();
        let orders: std::collections::HashMap<u64, OrderForCancelBatch> = order_ids
            .iter()
            .filter_map(|&id| {
                stored
                    .iter()
                    .find(|(owner, stored_id, _)| owner == owner_pn_address && *stored_id == id)
                    .map(|(_, _, proto)| (id, proto.clone()))
            })
            .collect();
        if orders.is_empty() {
            return Ok(None);
        }
        Ok(Some(CancelBatchResolution {
            event_id: market.event.event_id,
            oracle_list_hash: market.oracle_list_hash,
            token_type: market.token_type,
            market_status: self.cancel_batch_status_override.unwrap_or(market.status),
            orders,
        }))
    }

    async fn list_orders(&self, _: &OrdersQuery) -> Result<OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by cancel_batch_orders_http tests")
    }

    async fn resolve_market_for_balances(
        &self,
        _: &MarketAddress,
    ) -> Result<MarketBalancesResolution, anyhow::Error> {
        unimplemented!(
            "resolve_market_for_balances is not exercised by cancel_batch_orders_http tests"
        )
    }

    async fn resolve_for_buy_full_set(
        &self,
        _: &MarketAddress,
        _: i64,
    ) -> Result<dodex_application::MarketForBuyFullSet, anyhow::Error> {
        unimplemented!(
            "resolve_for_buy_full_set is not exercised by cancel_batch_orders_http tests"
        )
    }

    async fn sum_open_sell_remaining(
        &self,
        _: &str,
        _: &str,
    ) -> Result<std::collections::HashMap<u32, String>, anyhow::Error> {
        unimplemented!("sum_open_sell_remaining is not exercised by cancel_batch_orders_http tests")
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

    async fn split_full_set(
        &self,
        _: dodex_application::SplitFullSetPayload,
    ) -> Result<(), DomainError> {
        unreachable!("RecordingCancelBatchSender::split_full_set called from order/batch test")
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

/// Build a seedable (id, prototype) pair. Market identity flows from
/// the fake's `Market` fixture, not from individual rows, so this
/// helper only needs the per-row fields. The chain `order_id` is the
/// natural key the fake's `resolve_for_cancel_batch` uses when it
/// assembles the resolution HashMap.
fn row(order_id: u64, coid: Option<&str>) -> (u64, OrderForCancelBatch) {
    (order_id, OrderForCancelBatch { client_order_id: coid.map(|s| s.to_string()) })
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
    let payload_order_ids: Vec<u64> = payload.items.iter().map(|i| i.order_id).collect();
    assert_eq!(payload_order_ids, vec![123u64, 456u64]);
    // Audit trail: per-item `client_order_id` is not part of the chain
    // ABI but is the only path by which ops can grep an incident by
    // coid without joining `live_orders` back. A refactor that dropped
    // it from the payload would only show up at the unit layer
    // otherwise — pin the wire-level promise here, parallel to
    // `order_id` above.
    let payload_coids: Vec<Option<String>> =
        payload.items.iter().map(|i| i.client_order_id.clone()).collect();
    assert_eq!(payload_coids, vec![Some("client-42".to_string()), None]);
}

#[tokio::test]
async fn response_preserves_input_order_when_repo_returns_unordered_rows() {
    // Production Postgres has no ORDER BY on the bulk SELECT — the
    // handler MUST reorder by request sequence so callers can
    // correlate positionally. The fake's `HashMap` return type
    // inherits Rust's `RandomState` hasher, so iteration order is
    // randomised per-process AND independent of insertion order. The
    // use case must therefore walk `input.order_ids` (not iterate
    // the HashMap) for the response to come back in input order —
    // this test pins that walk.
    //
    // Caveat: a regression that did `orders.into_iter().collect()`
    // would yield response in HashMap iteration order. With N input
    // ids, the probability of HashMap iter happening to coincide
    // with input order on any one process is 1/N!. With N = 5 below
    // that's 1/120 per run — better than 1/6 with 3 ids but still
    // probabilistic. Code-review enforcement that the use case walks
    // `input.order_ids` is the structural backstop; this test is the
    // bug-catcher with the highest catch rate we can express without
    // controlling the HashMap seed.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_rows(
        trading_market(),
        vec![
            row(11, Some("a")),
            row(22, Some("b")),
            row(33, Some("c")),
            row(44, Some("d")),
            row(55, Some("e")),
        ],
    ));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    // Deliberately scrambled request order so the response cannot
    // come back sorted by id (matches input order) or by insertion
    // order in the fake (would be [11, 22, 33, 44, 55]).
    let mut resp =
        delete_batch(&service, valid_body(vec!["44", "11", "55", "22", "33"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let items = resp.take_json::<Vec<CancelBatchItem>>().await.expect("cancel batch body");
    let ids: Vec<&str> = items.iter().map(|i| i.order_id.as_str()).collect();
    assert_eq!(ids, vec!["44", "11", "55", "22", "33"]);
    let coids: Vec<&str> = items.iter().map(|i| i.client_order_id.as_str()).collect();
    assert_eq!(coids, vec!["d", "a", "e", "b", "c"]);

    // Chain payload must also carry the input order verbatim — the
    // chain's _doCancel processes ids in batch order.
    let dispatched_ids: Vec<u64> = sender.calls()[0].items.iter().map(|i| i.order_id).collect();
    assert_eq!(dispatched_ids, vec![44u64, 11u64, 55u64, 22u64, 33u64]);
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
    // Some HTTP proxies / SDKs strip bodies from DELETE requests. An
    // empty body short-circuits in `parse_strict_body` *before* it
    // reaches `serde_json::from_slice`, emitting `reason = empty`
    // (distinct from the `reason = malformed` tag the sibling
    // `malformed_json_body_returns_400_minus_1130` test exercises
    // through the same helper). Both route to `InvalidParameter` →
    // -1130, peer of `POST /order` and `POST /batchOrders`.
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
async fn blank_market_address_returns_400_minus_1102() {
    // `non_empty` trims at the boundary, so a whitespace-only
    // `marketAddress` collapses to `None` — pinned here so the trim
    // can't silently drift to a permissive accept.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": "   ",
        "symbol": SYMBOL,
        "orderIds": ["1"],
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
}

#[tokio::test]
async fn unknown_field_in_body_returns_400_minus_1130() {
    // `#[serde(deny_unknown_fields)]` surfaces caller typos (e.g.
    // `orderIDs` vs `orderIds`) as a structural reject (-1130
    // InvalidParameter via the body-parse `reason = "shape_mismatch"`
    // path), not as a misleading `MissingParameter` from the now-silently
    // `None` real field. Pins the strict-input contract on this
    // destructive write surface.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "orderIds": ["1"],
        "orderIDs": ["typo"],
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1130);
}

#[tokio::test]
async fn blank_symbol_returns_400_minus_1102() {
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": "\t\n",
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
async fn null_order_ids_returns_400_minus_1102() {
    // `orderIds: null` and a missing `orderIds` field must collapse to
    // the same MissingParameter surface. `CancelBatchOrdersRequest`
    // uses `Option<Vec<String>>` so serde maps both to `None` today,
    // but a deserializer-config drift (custom `Deserialize`,
    // `#[serde(default)]` removed, etc.) could leak null through as
    // an empty Vec or a different error code — pin the public
    // contract here.
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "orderIds": serde_json::Value::Null,
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1102);
    assert!(sender.calls().is_empty());
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
async fn exactly_max_batch_size_returns_pending_cancel_array() {
    // trading_market().outcomes[0].max_batch_size == 5. Exactly five
    // ids must reach the chain and round-trip a 5-item PENDING_CANCEL
    // array — pins the `>` boundary against an off-by-one that would
    // either reject the at-cap call (`>=`) or let an oversized one
    // through. Peer of create-batch's
    // `exactly_max_batch_size_returns_pending_new_array`.
    let ids: Vec<u64> = (1..=5).collect();
    let rows: Vec<(u64, OrderForCancelBatch)> =
        ids.iter().map(|id| row(*id, Some(&format!("c-{id}")))).collect();
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_rows(trading_market(), rows));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let id_strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    let body_ids: Vec<&str> = id_strs.iter().map(String::as_str).collect();
    let mut resp = delete_batch(&service, valid_body(body_ids)).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let items = resp.take_json::<Vec<CancelBatchItem>>().await.expect("cancel batch body");
    assert_eq!(items.len(), 5);
    for item in &items {
        assert_eq!(item.status, "PENDING_CANCEL");
    }
    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    let dispatched: Vec<u64> = calls[0].items.iter().map(|i| i.order_id).collect();
    assert_eq!(dispatched, ids);
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
async fn unknown_symbol_on_known_market_returns_404_minus_1121() {
    // `marketAddress` exists but `symbol` is not one of its outcomes
    // — the placement-shape lookup (`resolve_for_new_order`) rejects
    // before the bulk SELECT runs, surfacing the same -1121 as a
    // wholly-unknown market. Pins which of the two read calls
    // distinguishes "wrong market" from "wrong symbol for this
    // market".
    let repo: SharedRepo = Arc::new(FakeRepo::with_market(trading_market()));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let body = json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": "PM-NOT-AN-OUTCOME",
        "orderIds": vec!["1"],
    });
    let mut resp = delete_batch(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::NOT_FOUND));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1121);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn non_trading_market_returns_400_minus_2010() {
    // Seed a matching row so the test pins the non-trading branch as
    // the gate that fires, distinct from "market unknown" (-1121) or
    // "order unknown" (-2011). Without the seeded row, an empty
    // resolution would shortcut to -2011 if the early
    // `resolve_for_new_order` status check were ever bypassed, and
    // the assertion would fail for the wrong reason.
    let mut market = trading_market();
    market.status = MarketStatus::Resolving;
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(market, vec![row(1, Some("c-1"))]));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn market_flip_between_selects_returns_400_minus_2010() {
    // HTTP-peer to the unit test
    // `cancel_batch_orders_rejects_when_market_flips_between_selects`:
    // `resolve_for_new_order` returns `Trading`, but the bulk SELECT's
    // own snapshot rows are tagged non-Trading. The use case's
    // post-SELECT `rows[0].market_status == Trading` gate must reject
    // before chain dispatch, otherwise a regression that drops it slips
    // past the HTTP matrix.
    let repo: SharedRepo = Arc::new(
        FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)])
            .with_cancel_batch_status(MarketStatus::Resolving),
    );
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::BAD_REQUEST));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -2010);
    assert!(sender.calls().is_empty());
}

#[tokio::test]
async fn blank_oracle_list_hash_returns_503_minus_1500() {
    // `oracle_list_hash` flows from the bulk-SELECT resolution, not
    // the earlier `resolve_for_new_order` snapshot — pinned by seeding
    // an otherwise-Trading market with a blank hash and a matchable
    // row, then asserting the use case rejects after the bulk fetch.
    let mut market = trading_market();
    market.oracle_list_hash = String::new();
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(market, vec![row(1, Some("a"))]));
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
    let (foreign_id, foreign_proto) = row(1, Some("attackers-coid"));
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_owned_rows(
        trading_market(),
        vec![("0:someone-else".to_string(), foreign_id, foreign_proto)],
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

#[tokio::test]
async fn mixed_ownership_batch_returns_404_minus_2011() {
    // Probe-style abuse vector: caller submits one of their own ids
    // plus a foreigner's. The owner predicate must strip the foreign
    // row at resolution time, collapsing to a shortfall the use case
    // promotes to -2011 with the same opacity as a fully-unknown
    // batch. Pins that NO row from the batch reaches the chain — a
    // regression where partial ownership dispatched the caller's
    // subset would expose the foreign id's existence via timing or
    // chain failure surface.
    let (mine_id, mine_proto) = row(1, Some("mine-coid"));
    let (foreign_id, foreign_proto) = row(2, Some("attackers-coid"));
    let repo: SharedRepo = Arc::new(FakeRepo::with_market_and_owned_rows(
        trading_market(),
        vec![
            (PN_ADDRESS.to_string(), mine_id, mine_proto),
            ("0:someone-else".to_string(), foreign_id, foreign_proto),
        ],
    ));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1", "2"])).send(&service).await;
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
    // (121) coming back from `dodex_chain::Dex::cancel_batch` while another
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
    // `DomainError::RequestTimeout` when `dodex_chain::Dex::cancel_batch`
    // doesn't return within `chain.cancel_batch_timeout_ms`.
    // `handler_exceeding_request_timeout_returns_504_minus_1007`
    // (driven by `SlowCancelBatchSender`) exercises the wall-clock
    // hoop around the whole handler; this one pins the HTTP shape for
    // the chain-leg timeout specifically, symmetric with the
    // create-batch sibling `chain_timeout_returns_504_minus_1007`.
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

#[tokio::test]
async fn chain_unexpected_returns_500_minus_1000() {
    // HTTP-peer to the unit test
    // `cancel_batch_orders_propagates_sender_unexpected`: unmapped
    // `tvm_exit` codes and gateway transport failures surface as
    // `DomainError::Unexpected` from `classify_chain_outcome`. The HTTP
    // layer must render that as 500 / -1000, symmetric with the other
    // chain-leg shape tests above.
    let repo: SharedRepo =
        Arc::new(FakeRepo::with_market_and_rows(trading_market(), vec![row(1, None)]));
    let sender: SharedChainSender =
        Arc::new(RecordingCancelBatchSender::failing(DomainError::Unexpected));
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1000);
}

// ---- Non-domain resolver failures ----------------------------------------

#[tokio::test]
async fn resolve_for_new_order_raw_anyhow_returns_500_minus_1000() {
    // Pin the non-domain fallback in CancelBatchOrdersUseCase::execute:
    // a `resolve_for_new_order` anyhow without a DomainError cause (e.g.
    // sqlx pool drop) must surface as 500/-1000, not be swallowed.
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolve_for_new_order_raw(
        trading_market(),
        "simulated sqlx pool drop",
    ));
    let sender: SharedChainSender = Arc::new(RecordingCancelBatchSender::ok());
    let service = setup_with(repo, sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1000);
}

#[tokio::test]
async fn resolve_for_cancel_batch_raw_anyhow_returns_500_minus_1000() {
    // Symmetric pin for the second fallback in
    // CancelBatchOrdersUseCase::execute — the bulk order resolution leg
    // has its own downcast branch and its own `error!` log; a
    // regression there would silently lose the -1000 mapping.
    let repo: SharedRepo = Arc::new(FakeRepo::failing_resolve_for_cancel_batch_raw(
        trading_market(),
        "simulated bulk SELECT failure",
    ));
    let sender = Arc::new(RecordingCancelBatchSender::ok());
    let chain_sender: SharedChainSender = sender.clone();
    let service = setup_with(repo, chain_sender);

    let mut resp = delete_batch(&service, valid_body(vec!["1"])).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::INTERNAL_SERVER_ERROR));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1000);
    // Bulk resolution failure must short-circuit before chain dispatch.
    assert!(sender.calls().is_empty());
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
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

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

    async fn split_full_set(
        &self,
        _: dodex_application::SplitFullSetPayload,
    ) -> Result<(), DomainError> {
        unreachable!("SlowCancelBatchSender::split_full_set called from order/batch test")
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
