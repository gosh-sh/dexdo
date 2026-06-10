// End-to-end smoke test for `POST /api/v1/batchOrders` against a real
// shellnet OrderBook. Deploys a fresh PMP + OrderBook, provisions an
// HMAC api_key, drives the production router with `DexChainSender`
// / `PostgresAuthenticator` / `PostgresReadModelRepository`, then
// submits a two-item batch of BUY LIMIT GTC orders, polls
// `OrderBook.getOrdersByOwner` until **both** `clientOrderId`s
// surface, and cleans up each by `dodex_chain::Dex::cancel_order_by_client`
// so collateral does not remain locked.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixture `tests/fixtures/seed_notes.json` (PN with
//     enough NACKL — see `tests/fixtures/README.md` for fixture setup
//     and topping up via `mint_pn_pool`).
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_batch_orders -- --ignored --nocapture
//
// === SECURITY NOTE ===
// `tests/fixtures/seed_notes.json` ships plaintext `pn_seckey_hex`
// values for shellnet-only throwaway trading PNs. Safe ONLY because
// shellnet is a public devnet and the PNs hold test NACKL. The keys
// are loaded into memory by every test that calls `TestPnPool::load()`;
// never copy this fixture format into a stage or prod config. The
// `[SHELLNET-TESTKEYS]` tag in `tests/fixtures/README.md` is the
// canonical entry point for this constraint set.

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
use dodex_infrastructure::chain_sender::DexChainSender;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet + tests/fixtures/seed_notes.json"]
async fn batch_orders_buy_limit_gtc_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_batch_orders] TEST_DATABASE_URL not set, skipping");
        return;
    };

    // All e2e tests share one note. The suite runs single-threaded
    // (`--test-threads 1`) regardless — every test routes through the same
    // shellnet root singletons (`RootOracle` / `RootPn`), which a distinct
    // PN per slot does not deconflict (see tests/fixtures/README.md). With
    // no parallelism the PN `_busy` lock never contends, so one funded note
    // covers the whole suite.
    let pn_pool = TestPnPool::load();
    let trader = pn_pool.first().clone();
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
        DexChainSender::new(
            vec![SHELLNET_ENDPOINT.to_string()],
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .expect("DexChainSender::new"),
    );

    let (api_key, secret_hex) = provision_account(&pool, &kek, &trader).await;

    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let auth_config = AuthSection {
        kek_hex: "ab".repeat(32),
        default_recv_window_ms: 5_000,
        max_recv_window_ms: 60_000,
        seed_accounts: false,
        seed_accounts_path: None,
    };
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek.clone(), &auth_config));
    let service = Service::new(build_router(AppState::new(
        repo,
        authenticator,
        chain_sender,
        Arc::new(common::FakePnStateReader::default()),
        Arc::new(common::FakeReferenceRepo::with_seeded()),
    )));

    // Two BUY LIMIT GTC orders at non-crossing probability prices. Each
    // notional (= quantity * price NACKL) stays comfortably above
    // MIN_ORDER_NOTIONAL_NACKL = 10 and well inside the ~100 NACKL
    // split-collateral budget on the deployer-PN.
    //   25 * 0.49 = 12.25 NACKL
    //   25 * 0.50 = 12.50 NACKL
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
                "price": "0.49",
                "type": "LIMIT",
                "timeInForce": "GTC",
            },
            {
                "newOrderClientId": coid_b,
                "side": "BUY",
                "quantity": "25",
                "price": "0.5",
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
    let post_ok = status == Some(StatusCode::OK);

    // From this point on we may have live orders on the chain. The
    // assertion shape is `record-then-cancel-then-panic`: accumulate
    // failures in `failures`, run cleanup unconditionally, then raise
    // a single combined panic at the very end. A naked `assert!` here
    // would leak collateral on the trading PN if any of the polls or
    // chain-side checks below tripped.
    let mut failures: Vec<String> = Vec::new();
    if !post_ok {
        failures.push(format!("POST /api/v1/batchOrders status={status:?}; body: {resp_body}"));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BatchItem {
        client_order_id: String,
        transact_time: i64,
        status: String,
    }

    let coid_a_u128: u128 = coid_a.parse().expect("coid_a u128");
    let coid_b_u128: u128 = coid_b.parse().expect("coid_b u128");
    use dodex_chain::Dex as RawDex;
    let raw_dex = RawDex::from_endpoints(vec![SHELLNET_ENDPOINT.to_string()])
        .expect("RawDex::from_endpoints");

    if post_ok {
        match serde_json::from_str::<Vec<BatchItem>>(&resp_body) {
            Ok(items) => {
                if items.len() != 2 {
                    failures.push(format!("expected 2 batch items, got {}", items.len()));
                } else {
                    for item in &items {
                        if item.status != "PENDING_NEW" {
                            failures.push(format!("item status={}, want PENDING_NEW", item.status));
                        }
                        if item.transact_time <= 0 {
                            failures.push(format!(
                                "item transactTime={}, want > 0",
                                item.transact_time
                            ));
                        }
                    }
                    // api-spec: one `transactTime` per batch — every item carries
                    // the handler's single `now_ms`, not a per-item re-clock.
                    if items[0].transact_time != items[1].transact_time {
                        failures.push(format!(
                            "transactTime mismatch: {} vs {}",
                            items[0].transact_time, items[1].transact_time,
                        ));
                    }
                    if items[0].client_order_id != coid_a {
                        failures.push(format!(
                            "items[0].clientOrderId={}, want {coid_a}",
                            items[0].client_order_id,
                        ));
                    }
                    if items[1].client_order_id != coid_b {
                        failures.push(format!(
                            "items[1].clientOrderId={}, want {coid_b}",
                            items[1].client_order_id,
                        ));
                    }
                }
            }
            Err(err) => failures.push(format!("batch body parse: {err}")),
        }

        // Poll OrderBook until the chain reflects BOTH placements (60s
        // budget, 2s ticks). `placeBatch` is atomic on the chain —
        // once one coid surfaces the other should follow within the
        // same tick.
        use common::cleanup::PollOutcome;
        match common::cleanup::poll_orders(
            &raw_dex,
            &market,
            &trader,
            "e2e_batch_orders",
            |orders| {
                let has_a = orders.iter().any(|o| o.client_order_id == coid_a_u128);
                let has_b = orders.iter().any(|o| o.client_order_id == coid_b_u128);
                has_a && has_b
            },
        )
        .await
        {
            PollOutcome::Found(()) => {}
            PollOutcome::NotFound => failures.push(format!(
                "batch items (coid_a={coid_a}, coid_b={coid_b}) did not both surface in \
                 getOrdersByOwner within 60s",
            )),
            PollOutcome::ChainSilent => failures.push(
                "surface-poll never got a successful `get_orders_by_owner` response — \
                 cannot verify placement"
                    .into(),
            ),
        }

        // ---- Cleanup runs UNCONDITIONALLY when the POST returned OK,
        // ---- even if surfacing failed. `cancel_order_by_client` is a
        // ---- no-op against an already-rejected order on the chain side
        // ---- (the PN's `_clientOrderIds` map drives the lookup) so
        // ---- this is safe to run on any state.
        common::cleanup::cancel_coids_best_effort(
            &raw_dex,
            &trader,
            &market,
            &[coid_a_u128, coid_b_u128],
            "e2e_batch_orders",
        )
        .await;

        // Absence-poll: turn leaked orders (silent on captured stderr
        // from `cancel_coids_best_effort`) into a recorded failure.
        match common::cleanup::poll_orders(
            &raw_dex,
            &market,
            &trader,
            "e2e_batch_orders",
            |orders| {
                let still_a = orders.iter().any(|o| o.client_order_id == coid_a_u128);
                let still_b = orders.iter().any(|o| o.client_order_id == coid_b_u128);
                !still_a && !still_b
            },
        )
        .await
        {
            PollOutcome::Found(()) => {}
            PollOutcome::NotFound => failures.push(format!(
                "cancellation of coid_a={coid_a} / coid_b={coid_b} did not remove the \
                 orders from getOrdersByOwner within 60s — trading PN may still have \
                 collateral locked",
            )),
            PollOutcome::ChainSilent => failures.push(
                "absence-poll never got a successful `get_orders_by_owner` response — \
                 cannot verify cancellation; trading PN may still have collateral locked"
                    .into(),
            ),
        }
    }

    if !failures.is_empty() {
        panic!("e2e_batch_orders failures:\n  - {}", failures.join("\n  - "));
    }
}
