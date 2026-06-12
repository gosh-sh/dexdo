// End-to-end smoke test for `POST /api/v1/accounts` against real
// shellnet PrivateNote state. Unlike the other e2e tests, this one wires
// the production `GraphqlPnStateReader` so the registration handler's
// on-chain existence probe (`PnStateReader::get_details`) actually hits
// shellnet — the path the unit/HTTP tests fake with `FakePnStateReader`.
//
// It drives the production router (real `PostgresAccountRegistry`, real
// `PostgresAuthenticator`, real `GraphqlPnStateReader`) and verifies:
//   - registering a deployed-but-unregistered PN returns 200 + a usable
//     credential, and that credential authenticates a real `GET
//     /api/v1/account` read of the same PN;
//   - registering the same PN again returns -2015 / 409 (insert-only);
//   - registering a well-formed but undeployed address is rejected by the
//     real chain probe (404 / -2013 if no BOC, or 503 / -1500 if the
//     gateway returns an uninitialized account whose getDetails traps) and
//     writes no account row.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - `tests/fixtures/seed_notes.json` with at least one PN — the test
//     uses the shared deployer note (`TestPnPool::first()`). Registration
//     is chain-read-only (it never takes the PN `_busy` lock — only
//     `get_details` and the `_ephemeralPubkey` owner read) and clears its
//     own `accounts` row before and after, so it can reuse that shared
//     note even though the trading e2e tests also use it. Run e2e
//     sequentially (`--test-threads=1`) so its account-row delete cannot
//     race a trading test's `provision_account` on the same note.
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_register_account -- --ignored --nocapture
//
// === SECURITY NOTE ===
// Same shellnet-only constraint as `e2e_order.rs`'s SECURITY NOTE —
// `tests/fixtures/seed_notes.json` carries plaintext throwaway devnet
// keys. Do NOT repurpose for any non-devnet network.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::canonical_query;
use common::e2e_setup::db_pool;
use common::e2e_setup::network_endpoint;
use common::now_ms;
use common::sign;
use common::test_pns::TestPnPool;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedPnReader;
use dodex_api::testkit::SharedRefRepo;
use dodex_api::testkit::SharedRegistry;
use dodex_api::testkit::SharedRepo;
use dodex_infrastructure::account_registry::PostgresAccountRegistry;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::pn_state_reader::GraphqlPnStateReader;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::postgres_repo::PostgresReferenceRepository;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;

#[derive(Debug, Deserialize)]
struct RegisterBody {
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(rename = "apiSecret")]
    api_secret: String,
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: i32,
    #[allow(dead_code)]
    msg: String,
}

/// Build the production router with the *real* `GraphqlPnStateReader`
/// pointed at shellnet, so the registration chain probe and the
/// `GET /api/v1/account` read both hit live chain state. The chain sender
/// is a no-op because neither path submits an external message.
fn build_service(pool: &PgPool, kek: Arc<dodex_infrastructure::crypto::Kek>) -> Service {
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
    let chain_sender: SharedChainSender = Arc::new(common::NoopChainSender);
    let graphql = Arc::new(
        GraphqlClient::new(format!("{}/graphql", network_endpoint()), Duration::from_secs(30))
            .expect("GraphqlClient::new"),
    );
    let pn_reader: SharedPnReader =
        Arc::new(GraphqlPnStateReader::new(graphql).expect("GraphqlPnStateReader::new"));
    let ref_repo: SharedRefRepo = Arc::new(PostgresReferenceRepository::new(pool.clone()));
    let registry: SharedRegistry = Arc::new(PostgresAccountRegistry::new(pool.clone(), kek));
    let state = AppState::new(repo, authenticator, chain_sender, pn_reader, ref_repo)
        .with_account_registry(registry);
    Service::new(build_router(state))
}

async fn delete_account(pool: &PgPool, pn_address: &str) {
    sqlx::query("delete from accounts where pn_address = $1")
        .bind(pn_address)
        .execute(pool)
        .await
        .expect("delete account");
}

async fn post_register(service: &Service, body: &serde_json::Value) -> salvo::Response {
    TestClient::post("http://test/api/v1/accounts").json(body).send(service).await
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet + tests/fixtures/seed_notes.json"]
async fn register_deployed_pn_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,dodex_infrastructure=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_register_account] TEST_DATABASE_URL not set, skipping");
        return;
    };
    let service = build_service(&pool, kek);

    // Any deployed PN works — registration only reads it. The shared
    // `first()` note is fine even though trading tests use it too: clearing
    // the account row first makes the insert-only registration start clean,
    // and the read-only path never touches the PN `_busy` lock.
    let pn = TestPnPool::load().first().clone();
    delete_account(&pool, &pn.address).await;

    let body = json!({
        "pnAddress": pn.address,
        "pnPubkeyHex": pn.owner_public_key_hex,
        "pnSeckeyHex": pn.owner_secret_key_hex,
        // The pool normalises dih to decimal; the endpoint wants hex. Both
        // forms are valid input, so feed the original hex back.
        "pnDihHex": format!("{:x}", pn.deposit_identifier_hash.parse::<num_bigint::BigUint>().unwrap()),
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let reg: RegisterBody = resp.take_json().await.expect("register body");
    assert_eq!(status, Some(StatusCode::OK), "deployed PN must register");
    assert!(reg.api_key.starts_with("dk_live_"));
    assert_eq!(reg.api_secret.len(), 64);
    assert!(reg.permissions.contains(&"TRADE".to_string()));
    assert!(reg.permissions.contains(&"USER_DATA".to_string()));

    // Re-registering the same note is insert-only: -2015 / 409.
    let mut dup = post_register(&service, &body).await;
    let dup_status = dup.status_code;
    let dup_err = dup.take_json::<ErrorBody>().await.expect("dup error body");
    assert_eq!(dup_status, Some(StatusCode::CONFLICT));
    assert_eq!(dup_err.code, -2015);

    // The minted credential must authenticate a real chain read of the PN.
    let ts = now_ms();
    let canonical = canonical_query(&[("recvWindow", "5000"), ("timestamp", &ts.to_string())]);
    let sig = sign(&reg.api_secret, &canonical, b"");
    let auth_resp = TestClient::get("http://test/api/v1/account")
        .add_header("X-DODEX-APIKEY", reg.api_key.as_str(), true)
        .query("recvWindow", "5000")
        .query("timestamp", ts.to_string())
        .query("signature", sig)
        .send(&service)
        .await;
    assert_eq!(
        auth_resp.status_code,
        Some(StatusCode::OK),
        "freshly minted credential must authenticate and read the PN over shellnet"
    );

    delete_account(&pool, &pn.address).await;
}

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet"]
async fn register_undeployed_pn_returns_2013_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,dodex_infrastructure=debug")
        .try_init();

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_register_account] TEST_DATABASE_URL not set, skipping");
        return;
    };
    let service = build_service(&pool, kek);

    // A well-formed address with no PrivateNote behind it. The owner-key read
    // — the single chain round-trip, which doubles as the deployment probe —
    // can reject it two ways, both of which must block registration:
    //   - the gateway returns no BOC at all -> AccountNotDeployed (404 / -2013);
    //   - the gateway returns an uninitialized account whose field decode
    //     traps -> MarketInconsistent (503 / -1500), the same reader-failure
    //     posture as `GET /api/v1/account`.
    // Which one fires depends on live chain state for the address, so the
    // assertion accepts either. The deterministic -2013 mapping is pinned by
    // `register_account_http.rs::register_undeployed_note_returns_2013`. What
    // matters here is the real-network invariant: a note with nothing behind
    // it is rejected and writes no account row.
    //
    // The submitted pair must be self-consistent (`pnPubkeyHex` = the key
    // `pnSeckeyHex` derives), or the local pair check short-circuits to -1130
    // before the chain read ever runs — the order checks run is local first.
    let bogus_addr = format!("0:{}", "ab".repeat(32));
    let body = json!({
        "pnAddress": bogus_addr,
        "pnPubkeyHex": dodex_application::derive_ed25519_pubkey_hex(&"00".repeat(32)).unwrap(),
        "pnSeckeyHex": "00".repeat(32),
        "pnDihHex": "cd".repeat(32),
    });

    let mut resp = post_register(&service, &body).await;
    let status = resp.status_code;
    let err = resp.take_json::<ErrorBody>().await.expect("error body");
    assert!(
        matches!(status, Some(StatusCode::NOT_FOUND) | Some(StatusCode::SERVICE_UNAVAILABLE)),
        "undeployed note must be rejected (404 or 503), got {status:?}"
    );
    assert!(
        err.code == -2013 || err.code == -1500,
        "expected -2013 (no BOC) or -1500 (uninit getter trap), got {}",
        err.code
    );

    // No account row should have been created for the bogus address.
    let count: i64 = sqlx::query_scalar("select count(*) from accounts where pn_address = $1")
        .bind(&bogus_addr)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 0, "a failed chain probe must not write an account row");
}
