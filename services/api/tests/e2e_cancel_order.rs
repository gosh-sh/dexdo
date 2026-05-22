// End-to-end smoke test for `DELETE /api/v1/order` against a real
// shellnet OrderBook. Mirrors `e2e_order.rs` for setup (deploy fresh
// PMP + OrderBook, provision api_key, post a LIMIT GTC order, poll
// `getOrdersByOwner` until the chain accepts it), then drives the
// **HTTP cancel path** through the production router and verifies
// that the order disappears from `getOrdersByOwner` — proving the
// `DELETE /order` handler → use case → `BeeDexChainSender::cancel_order`
// → chain → `OrderBook.OrderCancelled` round-trip end to end.
//
// Marked `#[ignore]` because it needs the same setup as `e2e_order`:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixture `tests/fixtures/test_pns.json` (PN with
//     enough NACKL — see `e2e_order.rs` and `mint_pn_pool`).
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_cancel_order -- --ignored --nocapture
//
// Wall-clock per run ≈ 2–4 min (deploy ~2 min + bidding wait ~60s +
// place/cancel poll ~30s on average).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::canonical_query;
use common::deploy_market::deploy_ephemeral_market;
use common::deploy_market::DeployOptions;
use common::e2e_setup::db_pool;
use common::e2e_setup::fresh_coid;
use common::e2e_setup::percent_encode_query_value;
use common::e2e_setup::provision_account;
use common::e2e_setup::upsert_market;
use common::e2e_setup::SHELLNET_ENDPOINT;
use common::now_ms;
use common::sign;
use common::test_pns::TestPnPool;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedRepo;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::chain_sender::BeeDexChainSender;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet + tests/fixtures/test_pns.json"]
async fn cancel_order_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,bee_dex=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_cancel_order] TEST_DATABASE_URL not set, skipping");
        return;
    };

    // ---- Phase 1: deploy a fresh market and place an order so we
    // ---- have something concrete to cancel. The placement step is
    // ---- intentionally minimal here — exhaustive POST coverage lives
    // ---- in the dedicated POST e2e test; this one owns DELETE.
    // Slot 1 per the slot-ownership table in
    // `tests/fixtures/README.md#pn-slot-ownership` — every e2e test
    // claims a unique PN so a parallel `cargo test -- --ignored` run
    // never contends on the same PN's chain-side `_busy` lock.
    let pn_pool = TestPnPool::load();
    let trader = pn_pool.slot(1).clone();
    let market = deploy_ephemeral_market(
        vec![SHELLNET_ENDPOINT.to_string()],
        &trader,
        DeployOptions::default(),
    )
    .await
    .expect("deploy ephemeral market");

    let outcome_for_symbol = market.outcome_name.replace(' ', "-");
    let pmp_short = &market.pmp_address[..16.min(market.pmp_address.len())];
    let market_name = format!("PM-E2E-CXL-{pmp_short}");
    let symbol = format!("{market_name}-{outcome_for_symbol}");
    upsert_market(&pool, &market, &market_name, &symbol).await;

    let chain_sender: SharedChainSender = Arc::new(
        BeeDexChainSender::new(
            vec![SHELLNET_ENDPOINT.to_string()],
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .expect("BeeDexChainSender::new"),
    );

    let (api_key, secret_hex) = provision_account(&pool, &kek, &trader).await;

    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let auth_config = AuthSection {
        kek_hex: "ab".repeat(32),
        default_recv_window_ms: 5_000,
        max_recv_window_ms: 60_000,
        seed_accounts: false,
    };
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek.clone(), &auth_config));
    let service = Service::new(build_router(AppState::new(repo, authenticator, chain_sender)));

    let coid = fresh_coid(1).to_string();
    let place_body = serde_json::to_vec(&json!({
        "marketAddress": market.pmp_address,
        "symbol": symbol,
        "newOrderClientId": coid,
        "side": "BUY",
        "quantity": "30",
        "price": "5000",
        "type": "LIMIT",
        "timeInForce": "GTC",
    }))
    .unwrap();

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&secret_hex, &canonical, &place_body);

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", api_key.clone(), true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(place_body)
        .send(&service)
        .await;
    let status = resp.status_code;
    let resp_body = resp.take_string().await.expect("place response body");
    assert_eq!(status, Some(StatusCode::OK), "POST /api/v1/order; body: {resp_body}");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PostOk {
        status: String,
        client_order_id: String,
    }
    let post_ok: PostOk = serde_json::from_str(&resp_body).expect("place body");
    assert_eq!(post_ok.status, "PENDING_NEW");
    assert_eq!(post_ok.client_order_id, coid);

    // Wait until the chain assigns an orderId. The handler does not
    // return it — we look it up by clientOrderId in getOrdersByOwner.
    use bee_dex::Dex as RawDex;
    let raw_dex = RawDex::new(vec![SHELLNET_ENDPOINT.to_string()]).expect("RawDex::new");
    let coid_u128: u128 = coid.parse().expect("coid u128");
    use common::cleanup::PollOutcome;
    let order_id = match common::cleanup::poll_orders_find(
        &raw_dex,
        &market,
        &trader,
        "e2e_cancel_order",
        |orders| orders.iter().find(|o| o.client_order_id == coid_u128).map(|o| o.order_id),
    )
    .await
    {
        PollOutcome::Found(id) => id,
        PollOutcome::NotFound => panic!(
            "order with the supplied clientOrderId did not surface in getOrdersByOwner within \
             60s — can't drive the cancel path without a chain-assigned orderId",
        ),
        PollOutcome::ChainSilent => panic!(
            "surface-poll never got a successful `get_orders_by_owner` response — can't drive \
             the cancel path without a chain-assigned orderId",
        ),
    };

    // The DELETE handler's `resolve_for_cancel` does an ownership
    // lookup against `live_orders` — which is normally populated by
    // the indexer projecting `OrderBook.OrderPlaced`. We don't run
    // the indexer in this test (the e2e scope is HTTP→chain only),
    // so we hand-seed the row here to satisfy the handler's
    // precondition. The row mirrors what `apply_order_placed`
    // would have written for this `(orderbook_address, order_id)`.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy,
                price, amount_remaining, amount_initial, client_order_id,
                status, last_chain_order, owner_pn_address,
                chain_created_at, chain_updated_at, placed_chain_order)
           values ($1, $2::numeric, $3, true,
                   5000::numeric, 30000000000::numeric, 30000000000::numeric, $4,
                   'OPEN', '0001', $5,
                   now(), now(), '0001')
           on conflict (orderbook_address, order_id) do nothing"#,
    )
    .bind(&market.order_book_address)
    .bind(order_id.to_string())
    .bind(market.outcome_id as i32)
    .bind(&coid)
    .bind(&trader.address)
    .execute(&pool)
    .await
    .expect("seed live_orders row");

    // ---- Phase 2: drive DELETE /api/v1/order through the handler.
    // ---- The auth hoop signs `raw_query_string` byte-for-byte, so we
    // ---- pre-encode every value the way `TestClient` (and any real
    // ---- HTTP client) would put it on the wire, then build the URL
    // ---- with the canonicalised query string baked in. Going through
    // ---- `.query()` would risk a divergence between the byte sequence
    // ---- we sign and the bytes the server actually receives.
    let cancel_ts = now_ms();
    let order_id_str = order_id.to_string();
    let ts_str = cancel_ts.to_string();
    let pmp_enc = percent_encode_query_value(&market.pmp_address);
    let symbol_enc = percent_encode_query_value(&symbol);
    let cancel_canonical = canonical_query(&[
        ("marketAddress", pmp_enc.as_str()),
        ("orderId", order_id_str.as_str()),
        ("recvWindow", "5000"),
        ("symbol", symbol_enc.as_str()),
        ("timestamp", ts_str.as_str()),
    ]);
    let cancel_sig = sign(&secret_hex, &cancel_canonical, &[]);

    let url = format!("http://test/api/v1/order?{cancel_canonical}&signature={cancel_sig}");
    let mut cancel_resp =
        TestClient::delete(url).add_header("X-DODEX-APIKEY", api_key, true).send(&service).await;
    let cancel_status = cancel_resp.status_code;
    let cancel_body = cancel_resp.take_string().await.expect("cancel response body");
    assert_eq!(cancel_status, Some(StatusCode::OK), "DELETE /api/v1/order; body: {cancel_body}");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CancelOk {
        order_id: String,
        client_order_id: String,
        transact_time: i64,
        status: String,
    }
    let cancel_ok: CancelOk = serde_json::from_str(&cancel_body).expect("cancel body");
    // Minimal-response contract: four fields, status PENDING_CANCEL.
    // `clientOrderId` is echoed from the `live_orders` row we just
    // seeded above — same value the original POST sent.
    assert_eq!(cancel_ok.status, "PENDING_CANCEL");
    assert_eq!(cancel_ok.order_id, order_id.to_string());
    assert_eq!(cancel_ok.client_order_id, coid);
    assert!(cancel_ok.transact_time > 0);

    // ---- Phase 3: poll until the chain removes the order. 60s
    // ---- budget — `cancelOrder` is queued on the chain side
    // ---- (`OrderBook.executeBatch`), the same shape as placement.
    match common::cleanup::poll_orders(&raw_dex, &market, &trader, "e2e_cancel_order", |orders| {
        !orders.iter().any(|o| o.client_order_id == coid_u128)
    })
    .await
    {
        PollOutcome::Found(()) => {}
        PollOutcome::NotFound => panic!(
            "order with client_order_id={coid} did not disappear from getOrdersByOwner within \
             60s after DELETE /api/v1/order — chain may have queued cancel but not yet \
             executed it",
        ),
        PollOutcome::ChainSilent => panic!(
            "absence-poll never got a successful `get_orders_by_owner` response — cannot \
             verify cancellation of client_order_id={coid}",
        ),
    }
}
