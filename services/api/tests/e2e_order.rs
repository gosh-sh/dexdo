// End-to-end smoke test for `POST /api/v1/order` against a real
// shellnet OrderBook. Deploys a fresh PMP + OrderBook per run, drives
// the production router (real `DexChainSender`, real
// `PostgresAuthenticator`, real `PostgresReadModelRepository`), places
// a BUY LIMIT GTC order, then polls `OrderBook.getOrdersByOwner`
// until the chain assigns an `orderId` to the `clientOrderId` we
// sent. Two-pass cleanup at the end cancels the placed order via
// `dodex_chain::Dex::cancel_order_by_client` so collateral does not
// remain locked on the trading PN.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixture `tests/fixtures/seed_notes.json` (PN with
//     ≥ ~300 NACKL collateral to cover stakes + split — refresh via
//     `mint_pn_pool` when balances drop).
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_order -- --ignored --nocapture
//
// === SECURITY NOTE ===
// `tests/fixtures/seed_notes.json` ships plaintext `pn_seckey_hex`
// values for shellnet-only throwaway trading PNs. This is intentional
// and safe ONLY because shellnet is a public devnet, the PNs hold test
// NACKL only, and the keys are not reused outside e2e. Do NOT
// repurpose this format for any non-devnet network.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::canonical_query;
use common::deploy_market::deploy_ephemeral_market;
use common::deploy_market::DeployOptions;
use common::e2e_setup::db_pool;
use common::e2e_setup::fresh_coid;
use common::e2e_setup::network_endpoint;
use common::e2e_setup::provision_account;
use common::e2e_setup::upsert_market;
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
async fn buy_limit_gtc_against_shellnet() {
    // Surface the `DexChainSender` chain error stream into the test
    // output so a transport failure does not collapse into an opaque
    // `-1000` body. `with_test_writer` routes lines through `print!`,
    // which `--nocapture` then exposes.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_order] TEST_DATABASE_URL not set, skipping");
        return;
    };

    // Deploy a fresh PMP + OrderBook on shellnet. The deployer-PN is
    // the same PN we then trade against — `splitFullSet` primed its
    // outcome-token balance during deploy.
    // All e2e tests share one note. The suite runs single-threaded
    // (`--test-threads 1`) regardless — every test routes through the same
    // shellnet root singletons (`RootOracle` / `RootPn`), which a distinct
    // PN per slot does not deconflict (see tests/fixtures/README.md). With
    // no parallelism the PN `_busy` lock never contends, so one funded note
    // covers the whole suite.
    let pn_pool = TestPnPool::load();
    let trader = pn_pool.first().clone();
    let market =
        deploy_ephemeral_market(vec![network_endpoint()], &trader, DeployOptions::default())
            .await
            .expect("deploy ephemeral market");

    let outcome_for_symbol = market.outcome_name.replace(' ', "-");
    let pmp_short = &market.pmp_address[..16.min(market.pmp_address.len())];
    let market_name = format!("PM-E2E-{pmp_short}");
    let symbol = format!("{market_name}-{outcome_for_symbol}");
    upsert_market(&pool, &market, &market_name, &symbol).await;

    let chain_sender: SharedChainSender = Arc::new(
        DexChainSender::new(
            vec![network_endpoint()],
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

    let coid = fresh_coid(1).to_string();
    // 30 NACKL of outcome at price 0.5 (probability; encodes to 5000 bps).
    // Notional = 30 * 0.5 = 15 NACKL, above MIN_ORDER_NOTIONAL_NACKL = 10.
    let body = serde_json::to_vec(&json!({
        "marketAddress": market.pmp_address,
        "symbol": symbol,
        "newOrderClientId": coid,
        "side": "BUY",
        "quantity": "30",
        "price": "0.5",
        "type": "LIMIT",
        "timeInForce": "GTC",
    }))
    .unwrap();

    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&secret_hex, &canonical, &body);

    let mut resp = TestClient::post("http://test/api/v1/order")
        .add_header("X-DODEX-APIKEY", api_key, true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(body)
        .send(&service)
        .await;

    // Read body up front so the assertion message and the JSON parse
    // share the same buffer; `take_string` is `&mut self`.
    let status = resp.status_code;
    let body = resp.take_string().await.expect("response body");
    let post_ok = status == Some(StatusCode::OK);

    // Record-then-cancel-then-panic: from here on we may have a live
    // order on the chain. Accumulate failures, run cleanup
    // unconditionally, then raise a single combined panic. A naked
    // `assert!` between this point and the explicit cancel below
    // would leak collateral on the trading PN.
    let mut failures: Vec<String> = Vec::new();
    if !post_ok {
        failures.push(format!("POST /api/v1/order status={status:?}; body: {body}"));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OkBody {
        client_order_id: String,
        transact_time: i64,
        status: String,
    }

    let coid_u128: u128 = coid.parse().expect("coid u128");
    use dodex_chain::Dex as RawDex;
    let raw_dex = RawDex::from_endpoints(vec![network_endpoint()]).expect("RawDex::from_endpoints");

    if post_ok {
        match serde_json::from_str::<OkBody>(&body) {
            Ok(parsed) => {
                // Minimal-response contract: three fields, status
                // PENDING_NEW until `OrderBook.OrderPlaced` projects
                // through the indexer.
                if parsed.status != "PENDING_NEW" {
                    failures.push(format!("status={}, want PENDING_NEW", parsed.status));
                }
                if parsed.client_order_id != coid {
                    failures
                        .push(format!("clientOrderId={}, want {coid}", parsed.client_order_id,));
                }
                if parsed.transact_time <= 0 {
                    failures.push(format!("transactTime={}, want > 0", parsed.transact_time));
                }
            }
            Err(err) => failures.push(format!("happy path body parse: {err}")),
        }

        // Poll OrderBook until the chain reflects placement (60s
        // budget, 2s ticks — same as kit integration tests).
        use common::cleanup::PollOutcome;
        match common::cleanup::poll_orders(&raw_dex, &market, &trader, "e2e_order", |orders| {
            orders.iter().any(|o| o.client_order_id == coid_u128)
        })
        .await
        {
            PollOutcome::Found(()) => {}
            PollOutcome::NotFound => failures.push(format!(
                "order with client_order_id={coid} did not surface in getOrdersByOwner \
                 within 60s",
            )),
            PollOutcome::ChainSilent => failures.push(
                "surface-poll never got a successful `get_orders_by_owner` response — \
                 cannot verify placement"
                    .into(),
            ),
        }

        // Cleanup runs unconditionally when the POST returned OK so a
        // missed `surfaced` poll above does not leak collateral.
        common::cleanup::cancel_coids_best_effort(
            &raw_dex,
            &trader,
            &market,
            &[coid_u128],
            "e2e_order",
        )
        .await;

        // Absence-poll: turn a leaked order (silent on captured stderr
        // from `cancel_coids_best_effort`) into a recorded failure.
        match common::cleanup::poll_orders(&raw_dex, &market, &trader, "e2e_order", |orders| {
            !orders.iter().any(|o| o.client_order_id == coid_u128)
        })
        .await
        {
            PollOutcome::Found(()) => {}
            PollOutcome::NotFound => failures.push(format!(
                "cancellation of client_order_id={coid} did not remove the order from \
                 getOrdersByOwner within 60s — trading PN may still have collateral locked",
            )),
            PollOutcome::ChainSilent => failures.push(
                "absence-poll never got a successful `get_orders_by_owner` response — \
                 cannot verify cancellation; trading PN may still have collateral locked"
                    .into(),
            ),
        }
    }

    // Recover the deploy's split collateral + stakes back to the shared
    // deployer note so they are not stranded in this throwaway market.
    common::cleanup::reclaim_full_set_best_effort(&raw_dex, &trader, &market, "e2e_order").await;

    if !failures.is_empty() {
        panic!("e2e_order failures:\n  - {}", failures.join("\n  - "));
    }
}
