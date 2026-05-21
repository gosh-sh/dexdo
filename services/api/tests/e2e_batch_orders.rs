// End-to-end smoke test for `POST /api/v1/batchOrders` against a real
// shellnet OrderBook. Mirrors `e2e_order.rs` for setup (deploy fresh
// PMP + OrderBook, provision api_key, drive the production router
// with `BeeDexChainSender` / `PostgresAuthenticator` /
// `PostgresReadModelRepository`), then submits a two-item batch of
// BUY LIMIT GTC orders, polls `OrderBook.getOrdersByOwner` until
// **both** `clientOrderId`s surface, and cleans up each by
// `bee_dex::Dex::cancel_order_by_client` so collateral does not
// remain locked.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixture `tests/fixtures/test_pns.json` (PN with
//     enough NACKL — see `e2e_order.rs` and `mint_pn_pool`).
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_batch_orders -- --ignored --nocapture
//
// === SECURITY NOTE ===
// `tests/fixtures/test_pns.json` ships plaintext `owner_secret_key_hex`
// values for shellnet-only throwaway trading PNs. Safe ONLY because
// shellnet is a public devnet and the PNs hold test NACKL. Do NOT
// repurpose this format outside devnet — see `e2e_order.rs` for the
// canonical version of these constraints.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::canonical_query;
use common::deploy_market::deploy_ephemeral_market;
use common::deploy_market::DeployOptions;
use common::e2e_setup::db_pool;
use common::e2e_setup::fresh_coid;
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
async fn batch_orders_buy_limit_gtc_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,bee_dex=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_batch_orders] TEST_DATABASE_URL not set, skipping");
        return;
    };

    // Slot 2 belongs to this test. `e2e_order.rs` owns slot 0 and
    // `e2e_cancel_order.rs` owns slot 1; a parallel `cargo test --
    // --ignored` run must not contend on the same PN's chain-side
    // `_busy` lock.
    let pn_pool = TestPnPool::load();
    let trader = pn_pool.slot(2).clone();
    let market = deploy_ephemeral_market(
        vec![SHELLNET_ENDPOINT.to_string()],
        &trader,
        DeployOptions::default(),
    )
    .await
    .expect("deploy ephemeral market");

    let outcome_for_symbol = market.outcome_name.replace(' ', "-");
    let pmp_short = &market.pmp_address[..16.min(market.pmp_address.len())];
    let market_name = format!("PM-E2E-BATCH-{pmp_short}");
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

    // Two BUY LIMIT GTC orders at non-crossing prices. Each notional
    // (= quantity * price / 10000 NACKL) stays comfortably above
    // MIN_ORDER_NOTIONAL_NACKL = 10 and well inside the ~100 NACKL
    // split-collateral budget on the deployer-PN.
    //   25 * 4900 / 10000 = 12.25 NACKL
    //   25 * 5000 / 10000 = 12.50 NACKL
    let coid_a = fresh_coid(1).to_string();
    let coid_b = fresh_coid(2).to_string();
    let body = serde_json::to_vec(&json!({
        "marketAddress": market.pmp_address,
        "symbol": symbol,
        "orders": [
            {
                "newOrderClientId": coid_a,
                "side": "BUY",
                "quantity": "25",
                "price": "4900",
                "type": "LIMIT",
                "timeInForce": "GTC",
            },
            {
                "newOrderClientId": coid_b,
                "side": "BUY",
                "quantity": "25",
                "price": "5000",
                "type": "LIMIT",
                "timeInForce": "GTC",
            },
        ],
    }))
    .unwrap();

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&secret_hex, &canonical, &body);

    let mut resp = TestClient::post("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", api_key, true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body)
        .send(&service)
        .await;

    let status = resp.status_code;
    let resp_body = resp.take_string().await.expect("response body");
    assert_eq!(status, Some(StatusCode::OK), "POST /api/v1/batchOrders; body: {resp_body}");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BatchItem {
        client_order_id: String,
        transact_time: i64,
        status: String,
    }
    let items: Vec<BatchItem> = serde_json::from_str(&resp_body).expect("batch body");
    assert_eq!(items.len(), 2, "expected 2 batch items, got {}", items.len());
    for item in &items {
        assert_eq!(item.status, "PENDING_NEW");
        assert!(item.transact_time > 0);
    }
    // api-spec: one `transactTime` per batch — every item carries the
    // handler's single `now_ms`, not a per-item re-clock.
    assert_eq!(items[0].transact_time, items[1].transact_time);
    assert_eq!(items[0].client_order_id, coid_a);
    assert_eq!(items[1].client_order_id, coid_b);

    // Poll OrderBook until the chain reflects BOTH placements (60s
    // budget, 2s ticks). `placeBatch` is atomic on the chain — once
    // one coid surfaces the other should follow within the same tick,
    // but we wait on both before declaring success so a partial
    // observation does not silently pass.
    use bee_dex::Dex as RawDex;
    let raw_dex = RawDex::new(vec![SHELLNET_ENDPOINT.to_string()]).expect("RawDex::new");
    let coid_a_u128: u128 = coid_a.parse().expect("coid_a u128");
    let coid_b_u128: u128 = coid_b.parse().expect("coid_b u128");
    let mut both_surfaced = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let owned = match raw_dex
            .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
            .await
        {
            Ok(o) => o,
            Err(err) => {
                eprintln!("[e2e_batch_orders] get_orders_by_owner errored (retry): {err:?}");
                continue;
            }
        };
        let has_a = owned.orders.iter().any(|o| o.client_order_id == coid_a_u128);
        let has_b = owned.orders.iter().any(|o| o.client_order_id == coid_b_u128);
        if has_a && has_b {
            both_surfaced = true;
            break;
        }
    }
    assert!(
        both_surfaced,
        "batch items (coid_a={coid_a}, coid_b={coid_b}) did not both surface in \
         getOrdersByOwner within 60s",
    );

    // ---- Cleanup: cancel each order by clientOrderId so the test
    // ---- does not leave collateral locked on the trading PN. We use
    // ---- `cancelOrderByClient` per item rather than `cancelBatch`
    // ---- because the latter takes chain-assigned `orderId`s which
    // ---- we would have to look up first — `cancelOrderByClient`
    // ---- keys off the same `clientOrderId`s we already hold.
    use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
    use ackinacki_kit::tvm_client::abi::Signer;
    use ackinacki_kit::tvm_client::crypto::KeyPair;
    let signer = Signer::Keys {
        keys: KeyPair {
            public: trader.owner_public_key_hex.clone(),
            secret: trader.owner_secret_key_hex.clone(),
        },
    };
    for coid_u128 in [coid_a_u128, coid_b_u128] {
        let cancel_params = ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: coid_u128,
        };
        let mut cancel_sent = false;
        let mut last_err: Option<bee_dex::errors::AppError> = None;
        for attempt in 1..=5 {
            match raw_dex
                .cancel_order_by_client(&trader.address, cancel_params.clone(), signer.clone())
                .await
            {
                Ok(_) => {
                    cancel_sent = true;
                    break;
                }
                Err(err) => {
                    eprintln!(
                        "[e2e_batch_orders] cancel coid={coid_u128} attempt {attempt} failed, \
                         retrying: {err:?}"
                    );
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        assert!(
            cancel_sent,
            "cancel_order_by_client(coid={coid_u128}) failed after 5 attempts \
             (last error: {last_err:?})",
        );
    }

    // Poll until both orders are gone — 60s budget, same as placement.
    let mut both_cancelled = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let owned = match raw_dex
            .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
            .await
        {
            Ok(o) => o,
            Err(err) => {
                eprintln!("[e2e_batch_orders] cleanup poll errored (retry): {err:?}");
                continue;
            }
        };
        let still_a = owned.orders.iter().any(|o| o.client_order_id == coid_a_u128);
        let still_b = owned.orders.iter().any(|o| o.client_order_id == coid_b_u128);
        if !still_a && !still_b {
            both_cancelled = true;
            break;
        }
    }
    assert!(
        both_cancelled,
        "cancellation of coid_a={coid_a} / coid_b={coid_b} did not remove the orders from \
         getOrdersByOwner within 60s — trading PN may still have collateral locked",
    );
}
