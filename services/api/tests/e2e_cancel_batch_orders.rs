// End-to-end smoke test for `DELETE /api/v1/batchOrders` against a real
// shellnet OrderBook. Deploys a fresh PMP + OrderBook, provisions an
// HMAC api_key, posts a two-item batch of BUY LIMIT GTC orders, polls
// the chain until both surface in `getOrdersByOwner`, drives the
// **HTTP cancel-batch path** through the production router with both
// chain-assigned `orderId`s, and verifies that both orders disappear —
// proving the `DELETE /batchOrders` handler → use case →
// `BeeDexChainSender::cancel_batch_order` → chain →
// `OrderBook.OrderCancelled` × N round-trip end to end.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixture `tests/fixtures/test_pns.json` (PN with
//     enough NACKL — see `tests/fixtures/README.md` for fixture setup
//     and topping up via `mint_pn_pool`).
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_cancel_batch_orders -- --ignored --nocapture
//
// === SECURITY NOTE ===
// `tests/fixtures/test_pns.json` ships plaintext `owner_secret_key_hex`
// values for shellnet-only throwaway trading PNs. See the same note
// at the top of `e2e_batch_orders.rs` and the `[SHELLNET-TESTKEYS]`
// section in `tests/fixtures/README.md`.

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
async fn cancel_batch_orders_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,bee_dex=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_cancel_batch_orders] TEST_DATABASE_URL not set, skipping");
        return;
    };

    // Slot 3 per the slot-ownership table in
    // `tests/fixtures/README.md#pn-slot-ownership` — every e2e test
    // claims a unique PN so a parallel `cargo test -- --ignored` run
    // never contends on the same PN's chain-side `_busy` lock.
    let pn_pool = TestPnPool::load();
    let trader = pn_pool.slot(3).clone();
    let market = deploy_ephemeral_market(
        vec![SHELLNET_ENDPOINT.to_string()],
        &trader,
        DeployOptions::default(),
    )
    .await
    .expect("deploy ephemeral market");

    let outcome_for_symbol = market.outcome_name.replace(' ', "-");
    let pmp_short = &market.pmp_address[..16.min(market.pmp_address.len())];
    let market_name = format!("PM-E2E-CXLBATCH-{pmp_short}");
    let symbol = format!("{market_name}-{outcome_for_symbol}");
    upsert_market(&pool, &market, &market_name, &symbol).await;

    let chain_sender: SharedChainSender = Arc::new(
        BeeDexChainSender::new(
            vec![SHELLNET_ENDPOINT.to_string()],
            Duration::from_secs(30),
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

    // ---- Phase 1: place two BUY LIMIT GTC orders so DELETE has real
    // ---- chain-assigned orderIds to cancel. Non-crossing prices keep
    // ---- both orders OPEN against a fresh OrderBook with no opposite
    // ---- liquidity. Notionals 12.25 / 12.50 NACKL — comfortably above
    // ---- MIN_ORDER_NOTIONAL_NACKL = 10.
    let coid_a = fresh_coid(1).to_string();
    let coid_b = fresh_coid(2).to_string();
    let place_body = serde_json::to_vec(&json!({
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
    let sig = sign(&secret_hex, &canonical, &place_body);

    let mut place_resp = TestClient::post("http://test/api/v1/batchOrders")
        .add_header("X-DODEX-APIKEY", api_key.clone(), true)
        .add_header("content-type", "application/json", true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .body(place_body)
        .send(&service)
        .await;
    let place_status = place_resp.status_code;
    let place_resp_body = place_resp.take_string().await.expect("place response body");
    assert_eq!(
        place_status,
        Some(StatusCode::OK),
        "POST /api/v1/batchOrders; body: {place_resp_body}",
    );

    let coid_a_u128: u128 = coid_a.parse().expect("coid_a u128");
    let coid_b_u128: u128 = coid_b.parse().expect("coid_b u128");
    use bee_dex::Dex as RawDex;
    let raw_dex = RawDex::new(vec![SHELLNET_ENDPOINT.to_string()]).expect("RawDex::new");

    // From this point on we have live orders on the chain. Failures
    // accumulate; cleanup runs unconditionally; a single combined panic
    // is raised at the end if anything went wrong.
    let mut failures: Vec<String> = Vec::new();

    // Poll until both orderIds are chain-assigned, then capture them.
    use common::cleanup::PollOutcome;
    let order_ids = match common::cleanup::poll_orders_find(
        &raw_dex,
        &market,
        &trader,
        "e2e_cancel_batch_orders",
        |orders| {
            let id_a = orders.iter().find(|o| o.client_order_id == coid_a_u128).map(|o| o.order_id);
            let id_b = orders.iter().find(|o| o.client_order_id == coid_b_u128).map(|o| o.order_id);
            match (id_a, id_b) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            }
        },
    )
    .await
    {
        PollOutcome::Found(ids) => Some(ids),
        PollOutcome::NotFound => {
            failures.push(format!(
                "batch placements (coid_a={coid_a}, coid_b={coid_b}) did not both surface in \
                 getOrdersByOwner within 60s — can't drive DELETE /batchOrders without \
                 chain-assigned orderIds",
            ));
            None
        }
        PollOutcome::ChainSilent => {
            failures.push(
                "surface-poll never got a successful `get_orders_by_owner` response — cannot \
                 drive the cancel-batch path"
                    .into(),
            );
            None
        }
    };

    if let Some((order_id_a, order_id_b)) = order_ids {
        // The DELETE handler's `resolve_for_cancel_batch` reads
        // `live_orders` — normally populated by the indexer projecting
        // `OrderBook.OrderPlaced`. The indexer is out of scope here
        // (HTTP→chain only), so hand-seed both rows the same way
        // `e2e_cancel_order` does for its single-row case.
        for (order_id, coid) in [(order_id_a, &coid_a), (order_id_b, &coid_b)] {
            sqlx::query(
                r#"insert into live_orders
                       (orderbook_address, order_id, outcome_id, is_buy,
                        price, amount_remaining, amount_initial, client_order_id,
                        status, last_chain_order, owner_pn_address,
                        chain_created_at, chain_updated_at, placed_chain_order)
                   values ($1, $2::numeric, $3, true,
                           5000::numeric, 25000000000::numeric, 25000000000::numeric, $4,
                           'OPEN', '0001', $5,
                           now(), now(), '0001')
                   on conflict (orderbook_address, order_id) do nothing"#,
            )
            .bind(&market.order_book_address)
            .bind(order_id.to_string())
            .bind(market.outcome_id as i32)
            .bind(coid)
            .bind(&trader.address)
            .execute(&pool)
            .await
            .expect("seed live_orders row");
        }

        // ---- Phase 2: drive DELETE /api/v1/batchOrders through the
        // ---- handler. Body carries both orderIds as strings; the auth
        // ---- hoop signs over the body bytes plus the canonical query.
        let cancel_ts = now_ms();
        let cancel_body = serde_json::to_vec(&json!({
            "marketAddress": market.pmp_address,
            "symbol": symbol,
            "orderIds": [order_id_a.to_string(), order_id_b.to_string()],
        }))
        .unwrap();
        let cancel_canonical =
            canonical_query(&[("recvWindow", "5000"), ("timestamp", &cancel_ts.to_string())]);
        let cancel_sig = sign(&secret_hex, &cancel_canonical, &cancel_body);

        let mut cancel_resp = TestClient::delete("http://test/api/v1/batchOrders")
            .add_header("X-DODEX-APIKEY", api_key.clone(), true)
            .add_header("content-type", "application/json", true)
            .query("recvWindow", "5000")
            .query("timestamp", cancel_ts.to_string())
            .query("signature", cancel_sig)
            .body(cancel_body)
            .send(&service)
            .await;
        let cancel_status = cancel_resp.status_code;
        let cancel_resp_body = cancel_resp.take_string().await.expect("cancel response body");

        if cancel_status != Some(StatusCode::OK) {
            failures.push(format!(
                "DELETE /api/v1/batchOrders status={cancel_status:?}; body: {cancel_resp_body}",
            ));
        } else {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct CancelItem {
                order_id: String,
                client_order_id: String,
                transact_time: i64,
                status: String,
            }
            match serde_json::from_str::<Vec<CancelItem>>(&cancel_resp_body) {
                Ok(items) => {
                    if items.len() != 2 {
                        failures.push(format!("expected 2 cancel items, got {}", items.len()));
                    } else {
                        for item in &items {
                            if item.status != "PENDING_CANCEL" {
                                failures.push(format!(
                                    "item status={}, want PENDING_CANCEL",
                                    item.status,
                                ));
                            }
                            if item.transact_time <= 0 {
                                failures.push(format!(
                                    "item transactTime={}, want > 0",
                                    item.transact_time,
                                ));
                            }
                        }
                        // api-spec promise: transactTime identical across items
                        // — one chain submission, one moment of acceptance.
                        if items[0].transact_time != items[1].transact_time {
                            failures.push(format!(
                                "transactTime mismatch: {} vs {}",
                                items[0].transact_time, items[1].transact_time,
                            ));
                        }
                        // Response carries items in request order; orderId
                        // echoed from the body; clientOrderId from the
                        // seeded `live_orders` row.
                        if items[0].order_id != order_id_a.to_string() {
                            failures.push(format!(
                                "items[0].orderId={}, want {order_id_a}",
                                items[0].order_id,
                            ));
                        }
                        if items[0].client_order_id != coid_a {
                            failures.push(format!(
                                "items[0].clientOrderId={}, want {coid_a}",
                                items[0].client_order_id,
                            ));
                        }
                        if items[1].order_id != order_id_b.to_string() {
                            failures.push(format!(
                                "items[1].orderId={}, want {order_id_b}",
                                items[1].order_id,
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
                Err(err) => failures.push(format!("cancel body parse: {err}")),
            }

            // ---- Phase 3: poll until the chain removes BOTH orders.
            // ---- `cancelBatch` queues a single `OrderBook.executeBatch`
            // ---- which then drains one-by-one. 60s budget is generous;
            // ---- typical shellnet round-trip ~10-20s.
            match common::cleanup::poll_orders(
                &raw_dex,
                &market,
                &trader,
                "e2e_cancel_batch_orders",
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
                    "cancellation of coid_a={coid_a} / coid_b={coid_b} did not remove the orders \
                     from getOrdersByOwner within 60s after DELETE /api/v1/batchOrders — chain \
                     may have queued cancel but not yet executed it",
                )),
                PollOutcome::ChainSilent => failures.push(
                    "absence-poll never got a successful `get_orders_by_owner` response — \
                     cannot verify cancellation; trading PN may still have collateral locked"
                        .into(),
                ),
            }
        }
    }

    // Defence-in-depth cleanup: if anything above tripped after the
    // POST landed, `cancel_order_by_client` is a no-op on already-gone
    // orders and frees collateral on any leftover.
    common::cleanup::cancel_coids_best_effort(
        &raw_dex,
        &trader,
        &market,
        &[coid_a_u128, coid_b_u128],
        "e2e_cancel_batch_orders",
    )
    .await;

    if !failures.is_empty() {
        panic!("e2e_cancel_batch_orders failures:\n  - {}", failures.join("\n  - "));
    }
}
