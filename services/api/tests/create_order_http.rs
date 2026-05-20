// HTTP-level integration tests for `POST /api/v1/order` that exercise
// the handler + use case end to end **without** a database or a real
// chain. A fake `Authenticator` short-circuits HMAC verification, a
// fake `MarketReadRepository` returns a configurable `Market`, and a
// recording `ChainOrderSender` lets each test inspect (or fail) the
// payload that would have been dispatched.
//
// The HMAC pipeline itself is covered by `auth_http.rs`. The matching
// per-row coverage for the error table in `docs/tech-specs/write-api.md
// §Error mapping` lives here.

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
use dodex_application::ChainOrderSender;
use dodex_application::MarketForPlacement;
use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_application::NewOrderPayload;
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

/// `Authenticator` that always succeeds and injects a `TradingPn`
/// with the configured permissions. Bypasses HMAC verification — the
/// real pipeline is covered by `auth_http.rs`. These tests focus on
/// handler + use case behaviour given an already-authenticated caller.
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
                pn_address: "0:fake-pn".into(),
                pn_pubkey: "1".into(),
                pn_dih: "2".into(),
                pn_seckey: SensitiveBytes::new(vec![0u8; 32]),
            },
            permissions: self.permissions.clone(),
        })
    }
}

/// `MarketReadRepository` that either returns a single configurable
/// market or short-circuits `resolve_for_new_order` with a fixed
/// typed error. The fail-with mode lets tests exercise resolver-side
/// failure paths (e.g. blank `orderbook_address` → `MarketInconsistent`)
/// that the `with(market)` path can't synthesise because it builds
/// `MarketForPlacement` straight from the `Market` struct.
/// `get_depth` panics — tests that exercise depth must not use this.
struct FakeRepo {
    market: Mutex<Option<Market>>,
    resolver_error: Option<DomainError>,
    /// Raw anyhow that doesn't wrap a DomainError — exercises the
    /// non-domain fallback branch in `CreateOrderUseCase::execute`.
    resolver_raw_error: Option<String>,
}

impl FakeRepo {
    fn with(market: Market) -> Self {
        Self { market: Mutex::new(Some(market)), resolver_error: None, resolver_raw_error: None }
    }

    fn empty() -> Self {
        Self { market: Mutex::new(None), resolver_error: None, resolver_raw_error: None }
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
        Ok(MarketsPage {
            markets: self.market.lock().unwrap().clone().into_iter().collect(),
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
        unimplemented!("get_depth is not exercised by create_order_http tests")
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
    ) -> Result<dodex_application::OrderForCancel, anyhow::Error> {
        unimplemented!("resolve_for_cancel is not exercised by create_order_http tests")
    }

    async fn list_orders(
        &self,
        _: &dodex_application::OrdersQuery,
    ) -> Result<dodex_application::OrdersPage, anyhow::Error> {
        unimplemented!("list_orders is not exercised by create_order_http tests")
    }
}

/// `ChainOrderSender` that records every payload it sees, or fails
/// with a configured `DomainError`. The recorded payloads are what the
/// handler would dispatch to `bee_dex::Dex::place_order` in production.
struct RecordingSender {
    recorded: Mutex<Vec<NewOrderPayload>>,
    fail_with: Option<DomainError>,
}

impl RecordingSender {
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
impl ChainOrderSender for RecordingSender {
    async fn submit_order(&self, payload: NewOrderPayload) -> Result<(), DomainError> {
        if let Some(err) = self.fail_with {
            return Err(err);
        }
        self.recorded.lock().unwrap().push(payload);
        Ok(())
    }

    async fn cancel_order(
        &self,
        _: dodex_application::CancelOrderPayload,
    ) -> Result<(), DomainError> {
        // `create_order_http.rs` covers POST only; the cancel path has
        // its own test file with its own recorder. Reaching this arm
        // means the suite mixed concerns.
        unreachable!("RecordingSender::cancel_order called from POST test")
    }

    async fn submit_batch_order(
        &self,
        _: dodex_application::NewBatchOrderPayload,
    ) -> Result<(), DomainError> {
        // POST /order tests never reach the batch path.
        unreachable!("RecordingSender::submit_batch_order called from POST /order test")
    }
}

// ---- Fixtures ------------------------------------------------------------

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
            // 0.5 so a base scenario of price=0.615, qty=1.5 (notional
            // 0.9225) clears comfortably; tests that target the notional
            // rule construct their own market.
            min_notional: "0.5".into(),
            max_batch_size: 5,
        }],
    }
}

fn setup_with(repo: SharedRepo, sender: SharedChainSender) -> Service {
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    Service::new(build_router(AppState::new(repo, authenticator, sender)))
}

fn valid_body() -> serde_json::Value {
    json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "side": "BUY",
        "quantity": "1.5",
        "price": "0.615",
        "type": "LIMIT",
        "timeInForce": "GTC",
    })
}

/// MARKET-side body counterpart to `valid_body()`. No `price` field,
/// `timeInForce` omitted (the api-spec says it's ignored on MARKET).
fn market_body(side: &str, quantity: &str) -> serde_json::Value {
    json!({
        "marketAddress": MARKET_ADDRESS,
        "symbol": SYMBOL,
        "side": side,
        "quantity": quantity,
        "type": "MARKET",
    })
}

/// Dummy auth envelope query params + header. `FakeAuthenticator`
/// ignores their content; we still have to send them so the auth-hoop
/// envelope parser does not bail before delegating.
fn auth_envelope() -> Vec<(&'static str, String)> {
    vec![("recvWindow", "5000".into()), ("timestamp", "0".into()), ("signature", "00".into())]
}

fn post_order(_service: &Service, body: serde_json::Value) -> RequestBuilder {
    let body_bytes = serde_json::to_vec(&body).expect("serialize body");
    let mut req = TestClient::post("http://test/api/v1/order")
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

/// Minimal POST /order response shape. Matches `CreateOrderResponse`
/// in `services/api/src/lib.rs` — three fields, no echo of request
/// data, status is always `PENDING_NEW` on success. See
/// `docs/tech-specs/write-api.md §Response` for the rationale.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderOk {
    client_order_id: String,
    transact_time: i64,
    status: String,
}

// ---- Happy path ----------------------------------------------------------

#[tokio::test]
async fn happy_path_buy_limit_gtc_returns_pending_new() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut resp = post_order(&service, valid_body()).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let ok = resp.take_json::<OrderOk>().await.expect("happy path body");
    // Three-field response: status pins the optimistic placement
    // contract, transactTime anchors the moment we accepted, and
    // clientOrderId is the only correlation handle the client has
    // until the indexer projects `OrderPlaced` into `live_orders`.
    assert_eq!(ok.status, "PENDING_NEW");
    assert!(ok.transact_time > 0);
    assert!(!ok.client_order_id.is_empty()); // generated, since none provided

    // The chain dispatch carries the lifted-precision raw values per
    // spec §Chain submission. Most of the previous response-shape
    // coverage moved here — the response no longer echoes these.
    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].pn_address, "0:fake-pn");
    assert_eq!(calls[0].event_id, "0xevent");
    assert_eq!(calls[0].oracle_list_hash, "0xfeedface");
    assert_eq!(calls[0].token_type, 1);
    assert_eq!(calls[0].outcome_id, 1);
    assert!(calls[0].is_buy);
    assert_eq!(calls[0].price_raw, "615");
    assert_eq!(calls[0].amount_raw, "1500000");
    assert_eq!(calls[0].flags, 0); // LIMIT × GTC
}

#[tokio::test]
async fn happy_path_echoes_explicit_client_order_id() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut body = valid_body();
    body["newOrderClientId"] = json!("777");
    let mut resp = post_order(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let ok = resp.take_json::<OrderOk>().await.expect("body");
    assert_eq!(ok.client_order_id, "777");
    assert_eq!(ok.status, "PENDING_NEW");
    assert_eq!(sender.calls()[0].client_order_id, "777");
}

// ---- LIMIT × TIF happy paths --------------------------------------------
//
// The flag-encoding table is unit-tested row-by-row in
// `dodex_domain::tests::encode_flags_limit_table`. These three pins
// exercise the same matrix from the HTTP boundary: a request-parsing
// regression that silently defaulted `timeInForce: "IOC"` to `Gtc`
// before reaching `encode_order_flags` would still pass the unit
// test (which calls the encoder directly with the right enum) but
// would flip the `flags` byte these assertions read off the chain
// payload.

#[tokio::test]
async fn limit_ioc_dispatches_with_flag_ioc() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut body = valid_body();
    body["timeInForce"] = json!("IOC");
    let mut resp = post_order(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let _: OrderOk = resp.take_json().await.expect("body");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].flags, dodex_domain::FLAG_IOC);
}

#[tokio::test]
async fn limit_fok_dispatches_with_flag_fok() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut body = valid_body();
    body["timeInForce"] = json!("FOK");
    let mut resp = post_order(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let _: OrderOk = resp.take_json().await.expect("body");

    assert_eq!(sender.calls()[0].flags, dodex_domain::FLAG_FOK);
}

#[tokio::test]
async fn limit_post_only_dispatches_with_flag_post_only() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut body = valid_body();
    body["timeInForce"] = json!("POST_ONLY");
    let mut resp = post_order(&service, body).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let _: OrderOk = resp.take_json().await.expect("body");

    assert_eq!(sender.calls()[0].flags, dodex_domain::FLAG_POST_ONLY);
}

// ---- Error mapping rows --------------------------------------------------

async fn expect_error(service: &Service, body: serde_json::Value, http: StatusCode, code: i32) {
    let mut resp = post_order(service, body).send(service).await;
    assert_eq!(resp.status_code, Some(http), "wrong http status");
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, code, "wrong error code");
}

#[tokio::test]
async fn missing_market_address_returns_1102() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("marketAddress");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1102).await;
}

#[tokio::test]
async fn missing_symbol_returns_1102() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("symbol");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1102).await;
}

#[tokio::test]
async fn missing_side_returns_1102() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("side");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1102).await;
}

#[tokio::test]
async fn missing_quantity_returns_1102() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("quantity");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1102).await;
}

#[tokio::test]
async fn invalid_side_enum_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["side"] = json!("HOLD");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn invalid_type_enum_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["type"] = json!("STOP_LIMIT");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn invalid_time_in_force_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["timeInForce"] = json!("DAY");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn market_market_with_explicit_price_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["type"] = json!("MARKET");
    body.as_object_mut().unwrap().remove("timeInForce");
    // price is still in the body — invalid for MARKET per spec.
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn limit_without_price_returns_1102() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body.as_object_mut().unwrap().remove("price");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1102).await;
}

#[tokio::test]
async fn unknown_market_returns_1121() {
    // Covers two indistinguishable-at-HTTP cases the Postgres
    // resolver bundles into the same `InvalidMarketOrSymbol`
    // response: (a) no `markets` row exists for `pmp_address`, and
    // (b) the row exists but `last_reconciled_at IS NULL` (the
    // `WHERE m.last_reconciled_at is not null` filter in
    // `resolve_for_new_order` filters those out). The application
    // contract is identical — 404 / -1121 — and the resolver does
    // not leak which one happened.
    let service = setup_with(Arc::new(FakeRepo::empty()), Arc::new(RecordingSender::ok()));
    expect_error(&service, valid_body(), StatusCode::NOT_FOUND, -1121).await;
}

#[tokio::test]
async fn resolver_raw_anyhow_returns_1000() {
    // Pin the non-domain fallback in `CreateOrderUseCase::execute`:
    // an anyhow without a `DomainError` cause must surface as
    // 500/-1000, not be swallowed as a no-op. The `error!` log on
    // that branch is not asserted here — it's observable in CI logs
    // and the branch is structurally exercised by this test.
    let service = setup_with(
        Arc::new(FakeRepo::failing_resolver_raw("simulated sqlx pool drop")),
        Arc::new(RecordingSender::ok()),
    );
    expect_error(&service, valid_body(), StatusCode::INTERNAL_SERVER_ERROR, -1000).await;
}

#[tokio::test]
async fn resolver_inconsistent_returns_1500() {
    // Covers the resolver-side `MarketInconsistent` paths the
    // application layer can't synthesise from a populated `Market`
    // — most notably a reconciled market with a NULL/blank
    // `orderbook_address` (CHECK constraint says
    // `last_reconciled_at IS NOT NULL ⇒ orderbook_address IS NOT NULL`,
    // but a whitespace-only string slips past). The companion
    // `empty_oracle_list_hash_returns_1500` test below covers the
    // post-resolve `oracle_list_hash` check; this one pins that the
    // resolver layer's own `MarketInconsistent` also surfaces as
    // 503 / -1500 at HTTP.
    let service = setup_with(
        Arc::new(FakeRepo::failing_resolver(DomainError::MarketInconsistent)),
        Arc::new(RecordingSender::ok()),
    );
    expect_error(&service, valid_body(), StatusCode::SERVICE_UNAVAILABLE, -1500).await;
}

#[tokio::test]
async fn symbol_not_in_market_returns_1121() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["symbol"] = json!("PM-FAKE-NO");
    expect_error(&service, body, StatusCode::NOT_FOUND, -1121).await;
}

#[tokio::test]
async fn non_trading_status_returns_2010() {
    let mut market = trading_market();
    market.status = MarketStatus::Resolving;
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    expect_error(&service, valid_body(), StatusCode::BAD_REQUEST, -2010).await;
}

#[tokio::test]
async fn price_precision_exceeded_returns_1111() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["price"] = json!("0.6155"); // 4 dp > pricePrecision=3
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1111).await;
}

#[tokio::test]
async fn price_not_tick_multiple_returns_1111() {
    let mut market = trading_market();
    market.outcomes[0].tick_size = "0.003".into();
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["price"] = json!("0.001"); // not a multiple of 0.003
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1111).await;
}

#[tokio::test]
async fn notional_below_min_returns_2010() {
    let mut market = trading_market();
    market.outcomes[0].min_notional = "100".into();
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    // 0.615 * 1.5 = 0.9225 << 100
    expect_error(&service, valid_body(), StatusCode::BAD_REQUEST, -2010).await;
}

#[tokio::test]
async fn sender_transport_failure_returns_1000() {
    let service = setup_with(
        Arc::new(FakeRepo::with(trading_market())),
        Arc::new(RecordingSender::failing(DomainError::Unexpected)),
    );
    expect_error(&service, valid_body(), StatusCode::INTERNAL_SERVER_ERROR, -1000).await;
}

#[tokio::test]
async fn pn_busy_chain_reject_returns_2014_429() {
    // The chain serialises `placeOrder` per PN via `_busy`; a second
    // in-flight call from the same PN raises `ERR_NOTE_BUSY` (chain
    // exit 121) which `BeeDexChainSender` maps to `OrderPnBusy`.
    // Surfaces synchronously as 429 with -2014 so MM clients can
    // back off and retry instead of polling `/orders` for absence.
    let service = setup_with(
        Arc::new(FakeRepo::with(trading_market())),
        Arc::new(RecordingSender::failing(DomainError::OrderPnBusy)),
    );
    expect_error(&service, valid_body(), StatusCode::TOO_MANY_REQUESTS, -2014).await;
}

#[tokio::test]
async fn chain_low_value_reject_returns_2010() {
    // `ERR_LOW_VALUE` (chain exit 102) — insufficient balance for BUY
    // or insufficient stake for SELL. We did not pre-check balance
    // (chain is authoritative), so the only way the client learns is
    // through this synchronous reject mapped to -2010.
    let service = setup_with(
        Arc::new(FakeRepo::with(trading_market())),
        Arc::new(RecordingSender::failing(DomainError::OrderValidationFailed)),
    );
    expect_error(&service, valid_body(), StatusCode::BAD_REQUEST, -2010).await;
}

#[tokio::test]
async fn happy_path_market_buy_returns_pending_new_with_flag_market() {
    // The MARKET BUY contract is asserted at the chain-payload layer
    // now that the response no longer echoes type/side/price/tif —
    // `flags = FLAG_MARKET (0x04)`, `price_raw = "0"`, `is_buy = true`.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut resp = post_order(&service, market_body("BUY", "1.5")).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let ok = resp.take_json::<OrderOk>().await.expect("body");
    assert_eq!(ok.status, "PENDING_NEW");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].flags, 0x04); // FLAG_MARKET
    assert_eq!(calls[0].price_raw, "0"); // ignored on chain for MARKET
    assert!(calls[0].is_buy);
}

#[tokio::test]
async fn happy_path_market_sell_returns_pending_new() {
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let service = setup_with(repo, sender.clone());

    let mut resp = post_order(&service, market_body("SELL", "1.5")).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));

    let ok = resp.take_json::<OrderOk>().await.expect("body");
    assert_eq!(ok.status, "PENDING_NEW");

    let calls = sender.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].flags, 0x04);
    assert!(!calls[0].is_buy);
}

#[tokio::test]
async fn market_buy_notional_below_min_returns_2010() {
    // MARKET BUY: `quantity` is the quote-asset spend amount and is
    // checked directly against `min_notional`. A body with
    // quantity < min_notional must surface as -2010 at the HTTP
    // layer (the application-level unit test covered the use case;
    // this locks the wire-level mapping).
    let mut market = trading_market();
    market.outcomes[0].min_notional = "100".into();
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    // 0.001 << 100
    expect_error(&service, market_body("BUY", "0.001"), StatusCode::BAD_REQUEST, -2010).await;
}

#[tokio::test]
async fn market_with_gtc_tif_returns_1130() {
    // MARKET + non-IOC TIF is semantically nonsensical (MARKET never
    // rests). The flag encoder in `dodex_domain::encode_order_flags`
    // rejects it explicitly. The handler must surface that as -1130;
    // covering all three rejected TIFs here so a future change to
    // the encoder mapping cannot silently widen the accepted set.
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = market_body("BUY", "1.5");
    body["timeInForce"] = json!("GTC");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn market_with_fok_tif_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = market_body("BUY", "1.5");
    body["timeInForce"] = json!("FOK");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn market_with_post_only_tif_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = market_body("BUY", "1.5");
    body["timeInForce"] = json!("POST_ONLY");
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn quantity_precision_exceeded_returns_1111() {
    // Mirror of `price_precision_exceeded_returns_1111` for the
    // quantity side — `quantityPrecision = 6` on `trading_market()`,
    // so 7 dp must fail.
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["quantity"] = json!("1.5000000"); // 7 dp > quantityPrecision=6
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1111).await;
}

#[tokio::test]
async fn quantity_not_step_multiple_returns_1111() {
    // Mirror of `price_not_tick_multiple_returns_1111` for quantity.
    // Override step_size to something coarser than the value so the
    // check has bite (default step is 0.000001, which 1.5 trivially
    // matches).
    let mut market = trading_market();
    market.outcomes[0].step_size = "0.5".into();
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["quantity"] = json!("0.7"); // 0.7 is not a multiple of 0.5
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1111).await;
}

#[tokio::test]
async fn client_order_id_overflowing_u64_returns_1130() {
    // Chain ABI is `uint128`, but the serialization path through
    // `bee_dex` → `ackinacki-kit` → `serde_json::json!` panics on
    // `u128 > u64::MAX` (no `arbitrary_precision` feature upstream).
    // Until the SDK supports arbitrary precision, callers MUST stay
    // inside u64. A value past `u64::MAX` would otherwise crash
    // deep in the sender as a worker panic and surface to the
    // client as -1000 / 500 — the use case validates the boundary
    // so the wire mapping is -1130 / 400.
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let mut body = valid_body();
    body["newOrderClientId"] = json!("18446744073709551616"); // u64::MAX + 1
    expect_error(&service, body, StatusCode::BAD_REQUEST, -1130).await;
}

#[tokio::test]
async fn empty_oracle_list_hash_returns_1500() {
    // Pin the wire mapping: a reconciled market with blank
    // `oracle_list_hash` must fail at the application boundary, not
    // round-trip the chain.
    let mut market = trading_market();
    market.oracle_list_hash = String::new();
    let service = setup_with(Arc::new(FakeRepo::with(market)), Arc::new(RecordingSender::ok()));
    expect_error(&service, valid_body(), StatusCode::SERVICE_UNAVAILABLE, -1500).await;
}

#[tokio::test]
async fn chain_outcome_mismatch_reject_returns_1500() {
    // `ERR_INVALID_OUTCOME_ID` (chain exit 130) — read-model claims
    // an outcome the on-chain PMP doesn't have. Inconsistency is on
    // our side, not the client's; 503 lets ops triage.
    let service = setup_with(
        Arc::new(FakeRepo::with(trading_market())),
        Arc::new(RecordingSender::failing(DomainError::MarketInconsistent)),
    );
    expect_error(&service, valid_body(), StatusCode::SERVICE_UNAVAILABLE, -1500).await;
}

#[tokio::test]
async fn malformed_json_body_returns_1130() {
    let service =
        setup_with(Arc::new(FakeRepo::with(trading_market())), Arc::new(RecordingSender::ok()));
    let body = b"{not valid json".to_vec();
    let mut req = TestClient::post("http://test/api/v1/order")
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

/// `ChainOrderSender` that always hangs longer than the test's
/// `request_timeout` budget. Used by the timeout regression test to
/// drive the handler past the budget without an actual chain call.
struct SlowSender;

#[async_trait]
impl ChainOrderSender for SlowSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        // 5 s is "indefinitely longer than any reasonable test
        // budget"; the regression test caps the budget at 50 ms, so
        // wall-clock impact stays in the tens of milliseconds.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        Ok(())
    }

    async fn cancel_order(
        &self,
        _: dodex_application::CancelOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("SlowSender::cancel_order called from POST test")
    }

    async fn submit_batch_order(
        &self,
        _: dodex_application::NewBatchOrderPayload,
    ) -> Result<(), DomainError> {
        unreachable!("SlowSender::submit_batch_order called from POST /order test")
    }
}

#[tokio::test]
async fn handler_exceeding_request_timeout_returns_504_minus_1007() {
    // The auth-hoop comment + api.local.yaml describe the race the
    // timeout hoop guards against: a chain call that hangs past
    // place_order_timeout + slack must surface as 504 instead of
    // stalling the worker. SlowSender plays the stuck-chain role; a
    // 50 ms budget keeps the test fast.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender: SharedChainSender = Arc::new(SlowSender);
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    let state = AppState::new(repo, authenticator, sender)
        .with_request_timeout(std::time::Duration::from_millis(50));
    let service = Service::new(build_router(state));

    let started = std::time::Instant::now();
    let mut resp = post_order(&service, valid_body()).send(&service).await;
    let elapsed = started.elapsed();
    assert_eq!(resp.status_code, Some(StatusCode::GATEWAY_TIMEOUT));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1007);
    // Tight bound — much less than the 5 s sender sleep — confirms the
    // timeout hoop actually cancelled the handler rather than letting
    // it run to completion.
    assert!(elapsed < std::time::Duration::from_secs(1), "elapsed {elapsed:?}");
}

#[tokio::test]
async fn handler_within_request_timeout_succeeds() {
    // Counterpart to the 504 test: a fast handler under a non-zero
    // budget must not be cancelled. Pins that the hoop is gated on
    // actual elapse, not just budget-being-set.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    let sender = Arc::new(RecordingSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::Trade] });
    let state = AppState::new(repo, authenticator, sender.clone() as SharedChainSender)
        .with_request_timeout(std::time::Duration::from_secs(5));
    let service = Service::new(build_router(state));

    let mut resp = post_order(&service, valid_body()).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::OK));
    let ok = resp.take_json::<OrderOk>().await.expect("body");
    assert_eq!(ok.status, "PENDING_NEW");
    assert_eq!(sender.calls().len(), 1);
}

#[tokio::test]
async fn user_data_only_key_returns_1002() {
    // Fake authenticator returns USER_DATA only. `require_auth(Trade)`
    // in the handler must reject before parsing body or hitting the
    // repo — the order of checks pinned by spec §Authorization.
    let repo: SharedRepo = Arc::new(FakeRepo::with(trading_market()));
    // Keep the concrete `Arc<RecordingSender>` for `.calls()` while
    // passing a clone (coerced to `Arc<dyn ChainOrderSender>`) into
    // `AppState`.
    let sender = Arc::new(RecordingSender::ok());
    let authenticator: SharedAuth =
        Arc::new(FakeAuthenticator { permissions: vec![Permission::UserData] });
    let service = Service::new(build_router(AppState::new(repo, authenticator, sender.clone())));

    let mut resp = post_order(&service, valid_body()).send(&service).await;
    assert_eq!(resp.status_code, Some(StatusCode::UNAUTHORIZED));
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert_eq!(err.code, -1002);
    // And the sender must NOT have been called.
    assert!(sender.calls().is_empty());
}
