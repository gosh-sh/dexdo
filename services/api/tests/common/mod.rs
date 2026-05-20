// Shared helpers for the api HTTP integration tests. Each test file
// declares `mod common;` and uses the setup/sign primitives below.
//
// The whole suite shares a single test DB (the docker-compose.test.yml
// Postgres). The seeded credentials baked into `seed::seed_accounts`
// are read-only as far as tests are concerned, so parallel reads are
// safe. Tests that mutate rows (e.g. the USER_DATA-only permission
// case) carry their own cleanup.

#![allow(dead_code)]

pub mod deploy_market;
pub mod e2e_setup;
pub mod test_pns;

use std::env;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedRepo;
use dodex_application::CancelOrderPayload;
use dodex_application::ChainOrderSender;
use dodex_application::NewBatchOrderPayload;
use dodex_application::NewOrderPayload;
use dodex_domain::DomainError;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::seed;
use hmac::Hmac;
use hmac::Mac;
use salvo::Service;
use sha2::Sha256;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub type HmacSha256 = Hmac<Sha256>;

/// First seeded api_key/secret pair from `seed.rs`. Used as the
/// "happy path" credential across the auth_http tests.
pub const SEED_API_KEY: &str = "dk_live_test_001";
pub const SEED_API_SECRET: &str =
    "1de6fc5cf8899e7f1dacf449fe46c3c88854478b7fcd9dd26c664535ee589966";

/// Fixed KEK for tests so seeded ciphertexts decrypt across runs.
pub fn test_kek() -> Arc<Kek> {
    Arc::new(Kek::from_hex(&"ab".repeat(32)).expect("test kek"))
}

/// Bring up the test environment: connect to TEST_DATABASE_URL, run
/// migrations, seed credentials, build a Salvo service around the same
/// router production uses. Returns `None` (with a skip notice) when
/// the env var is not set so `cargo test` without docker still passes.
pub async fn setup() -> Option<(Service, PgPool, Arc<Kek>)> {
    // Pick up a `.env` if one sits at the workspace root. CI and the
    // throwaway docker-compose Postgres still work without it; locally
    // it spares developers an explicit `export TEST_DATABASE_URL=...`.
    let _ = dotenvy::dotenv();
    let url = env::var("TEST_DATABASE_URL").ok().filter(|s| !s.is_empty())?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    database::run_migrations(&pool).await.expect("run migrations");

    let kek = test_kek();
    seed::seed_accounts(&pool, &kek).await.expect("seed credentials");

    let repo: SharedRepo = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let auth_config = AuthSection {
        kek_hex: "ab".repeat(32),
        default_recv_window_ms: 5_000,
        max_recv_window_ms: 60_000,
        seed_accounts: false,
    };
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek.clone(), &auth_config));
    // The seeded test DB carries no markets, so the order handler stops
    // at `InvalidMarketOrSymbol` before reaching the chain sender. A
    // no-op fake is enough to satisfy `AppState::new`'s type bound;
    // tests that exercise the full submission path inject their own.
    let chain_sender: SharedChainSender = Arc::new(NoopChainSender);
    let state = AppState::new(repo, authenticator, chain_sender);
    let service = Service::new(build_router(state));
    Some((service, pool, kek))
}

/// `ChainOrderSender` fake that succeeds on every call. The auth-layer
/// tests under this module never reach the sender (no markets seeded,
/// so the use case 404s first), but `AppState` needs *some* concrete
/// implementation at construction time.
pub struct NoopChainSender;

#[async_trait]
impl ChainOrderSender for NoopChainSender {
    async fn submit_order(&self, _: NewOrderPayload) -> Result<(), DomainError> {
        Ok(())
    }

    async fn cancel_order(&self, _: CancelOrderPayload) -> Result<(), DomainError> {
        Ok(())
    }

    async fn submit_batch_order(&self, _: NewBatchOrderPayload) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Unix milliseconds, the unit `timestamp` in signed requests uses.
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Compute the canonical query string the spec mandates: sort
/// `key=value` pairs lexicographically by key, drop `signature`,
/// rejoin with `&` without re-encoding. Matches the server-side
/// `canonical_query_string` in `infrastructure::auth`.
pub fn canonical_query(pairs: &[(&str, &str)]) -> String {
    let mut filtered: Vec<(&str, &str)> =
        pairs.iter().copied().filter(|(k, _)| *k != "signature").collect();
    filtered.sort_by(|a, b| a.0.cmp(b.0));
    filtered.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// HMAC-SHA256(canonical_query + body, hex-decoded secret) as
/// lowercase hex. The hash function is identical to
/// `verify_hmac` in the auth module — symmetric by construction.
pub fn sign(api_secret_hex: &str, canonical: &str, body: &[u8]) -> String {
    let secret = hex::decode(api_secret_hex).expect("api_secret_hex must be valid hex");
    let mut mac = HmacSha256::new_from_slice(&secret).expect("hmac init");
    mac.update(canonical.as_bytes());
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}
