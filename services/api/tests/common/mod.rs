// Shared helpers for the api HTTP integration tests. Each test file
// declares `mod common;` and uses the setup/sign primitives below.
//
// The whole suite shares a single test DB (the docker-compose.test.yml
// Postgres). The credentials seeded from `SEED_NOTES_FIXTURE` via
// `seed::seed_accounts_from_notes`
// are read-only as far as tests are concerned, so parallel reads are
// safe. Tests that mutate rows (e.g. the USER_DATA-only permission
// case) carry their own cleanup.

#![allow(dead_code)]

pub mod airegistry;
pub mod cleanup;
pub mod deploy_market;
pub mod e2e_setup;
pub mod read_model;
pub mod test_pns;

use std::collections::HashMap as StdHashMap;
use std::collections::HashSet as StdHashSet;
use std::env;
use std::sync::Arc;
use std::sync::Mutex;
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
use dodex_application::PnDetails;
use dodex_application::PnStake;
use dodex_application::PnStateReader;
use dodex_application::RefToken;
use dodex_application::ReferenceRepository;
use dodex_domain::DomainError;
use dodex_infrastructure::account_registry::PostgresAccountRegistry;
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

/// Credentials the seeder mints for the first two notes of the test
/// fixture (`SEED_NOTES_FIXTURE`). The api_key is `dk_live_test_{n:03}`;
/// the secret is `crypto::derive_api_secret(test_kek, index)`, pinned
/// here so the signing tests need no DB round-trip and guarded against
/// drift by `seed_secret_consts_match_kek_derivation` in auth_http.rs.
pub const SEED_API_KEY: &str = "dk_live_test_001";
pub const SEED_API_SECRET: &str =
    "86c223a600ce630f9abf62ea2244ca638a2e02bb16d73d74128bf31f5d3e1910";

/// Second seeded credential, used for cross-tenant isolation tests.
pub const SEED_API_KEY_2: &str = "dk_live_test_002";
pub const SEED_API_SECRET_2: &str =
    "55ed737c64cf4069c33275745ba96a06a8e9bd7d14b65bf97eadf701a38b2a55";

/// Notes fixture the api test suite seeds from. Dummy custody keys — the
/// suite mocks the chain, so `pn_seckey` is never used to sign on-chain;
/// real notes are provided on CI out of band.
pub const SEED_NOTES_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/seed_notes_dummy.json");

/// Fixed KEK for tests so seeded ciphertexts decrypt across runs.
pub fn test_kek() -> Arc<Kek> {
    Arc::new(Kek::from_hex(&"ab".repeat(32)).expect("test kek"))
}

/// Bring up the test environment: connect to TEST_DATABASE_URL, run
/// migrations, seed credentials, build a Salvo service around the same
/// router production uses. Returns `None` (with a skip notice) when
/// the env var is not set so `cargo test` without docker still passes.
pub async fn setup() -> Option<(Service, PgPool, Arc<Kek>, Arc<FakePnStateReader>)> {
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
    seed::seed_accounts_from_notes(&pool, &kek, std::path::Path::new(SEED_NOTES_FIXTURE))
        .await
        .expect("seed credentials");

    let read_model = Arc::new(PostgresReadModelRepository::new(pool.clone()));
    let repo: SharedRepo = read_model.clone();
    let inference_repo: dodex_api::testkit::SharedInferenceRepo = read_model;
    let auth_config = AuthSection {
        kek_hex: "ab".repeat(32),
        default_recv_window_ms: 5_000,
        max_recv_window_ms: 60_000,
        seed_accounts: false,
        seed_accounts_path: None,
    };
    let authenticator: SharedAuth =
        Arc::new(PostgresAuthenticator::new(pool.clone(), kek.clone(), &auth_config));
    // The seeded test DB carries no markets, so the order handler stops
    // at `InvalidMarketOrSymbol` before reaching the chain sender. A
    // no-op fake is enough to satisfy `AppState::new`'s type bound;
    // tests that exercise the full submission path inject their own.
    let chain_sender: SharedChainSender = Arc::new(NoopChainSender);
    let pn_reader_inner = Arc::new(FakePnStateReader::default());
    let pn_reader: dodex_api::testkit::SharedPnReader = pn_reader_inner.clone();
    let ref_repo: dodex_api::testkit::SharedRefRepo = Arc::new(FakeReferenceRepo::with_seeded());
    let registry: dodex_api::testkit::SharedRegistry =
        Arc::new(PostgresAccountRegistry::new(pool.clone(), kek.clone()));
    let state = AppState::new(repo, authenticator, chain_sender, pn_reader, ref_repo)
        .with_account_registry(registry)
        .with_inference_repo(inference_repo);
    let service = Service::new(build_router(state));
    Some((service, pool, kek, pn_reader_inner))
}

/// Variant of [`setup`] that lets a test plug in its own `PnStateReader`
/// (for example [`RawJsonPnStateReader`] to drive payloads through real
/// boundary validation). All other wiring matches `setup`.
pub async fn setup_with_pn_reader(
    pn_reader: dodex_api::testkit::SharedPnReader,
) -> Option<(Service, PgPool, Arc<Kek>)> {
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
    seed::seed_accounts_from_notes(&pool, &kek, std::path::Path::new(SEED_NOTES_FIXTURE))
        .await
        .expect("seed credentials");

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
    let chain_sender: SharedChainSender = Arc::new(NoopChainSender);
    let ref_repo: dodex_api::testkit::SharedRefRepo = Arc::new(FakeReferenceRepo::with_seeded());
    let registry: dodex_api::testkit::SharedRegistry =
        Arc::new(PostgresAccountRegistry::new(pool.clone(), kek.clone()));
    let state = AppState::new(repo, authenticator, chain_sender, pn_reader, ref_repo)
        .with_account_registry(registry);
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

    async fn cancel_batch_order(
        &self,
        _: dodex_application::CancelBatchOrderPayload,
    ) -> Result<(), DomainError> {
        Ok(())
    }

    async fn split_full_set(
        &self,
        _: dodex_application::SplitFullSetPayload,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

/// In-memory PnStateReader for HTTP tests. Each handler test that
/// needs PN reads creates a `FakePnStateReader::default()` and sets
/// the desired state via `set_details` / `set_stake`. The fake is
/// `Send + Sync` because `AppState` requires it.
///
/// Three modes for `get_stake`, checked in this order:
/// 1. **Hash-keyed:** `set_stake_for_hash(hash, s)` registers a stake by the
///    exact `stake_hash` the production hasher emits. When this map is
///    non-empty, any hash absent from it returns `None`. Tests that need to
///    verify the production hasher's output is actually used as the lookup
///    key use this mode — a drift between `balances_stake_hash` and the
///    `_stakes` access path surfaces as the registered stake disappearing.
/// 2. **Per-address:** `set_stake_for(pn, s)` stores per-address answers
///    independent of the hash. Used by cross-tenant isolation tests.
/// 3. **Global default:** `set_stake(s)` sets a single answer returned for
///    every `(pn_address, stake_hash)` pair.
///
/// `get_details` uses per-address-then-global precedence (no hash dimension).
#[derive(Default)]
pub struct FakePnStateReader {
    // Global defaults.
    details: Mutex<Option<Result<PnDetails, String>>>,
    stake: Mutex<Option<Option<PnStake>>>,
    stake_err: Mutex<Option<String>>,
    // Untyped fault for `owner_pubkey` — the single chain read registration
    // uses as its existence probe. Maps to a retryable 503 (-1500), distinct
    // from `not_deployed`'s typed -2013.
    owner_err: Mutex<Option<String>>,
    // Per-address overrides.
    details_by_pn: Mutex<StdHashMap<String, Result<PnDetails, String>>>,
    // Addresses that `get_details` answers with a typed
    // `DomainError::AccountNotDeployed` (-2013) — distinct from
    // `fail_details`, which maps to a generic 503. Lets registration tests
    // drive the "PN not on-chain" path.
    not_deployed: Mutex<StdHashSet<String>>,
    stake_by_pn: Mutex<StdHashMap<String, Option<PnStake>>>,
    // Hash-keyed overrides. When non-empty, any stake_hash absent from the
    // map returns `None` — this is how tests verify the production hasher
    // is actually wired through to the lookup.
    stake_by_hash: Mutex<StdHashMap<String, Option<PnStake>>>,
}

impl FakePnStateReader {
    pub fn set_details(&self, d: PnDetails) {
        *self.details.lock().unwrap() = Some(Ok(d));
    }

    pub fn fail_details(&self, msg: &str) {
        *self.details.lock().unwrap() = Some(Err(msg.into()));
    }

    /// Make `owner_pubkey(pn_address)` fail with an untyped (gateway/ABI)
    /// error so callers exercise the retryable 503 path on registration's
    /// chain read.
    pub fn fail_owner_pubkey(&self, msg: &str) {
        *self.owner_err.lock().unwrap() = Some(msg.into());
    }

    pub fn set_stake(&self, s: Option<PnStake>) {
        *self.stake.lock().unwrap() = Some(s);
    }

    pub fn fail_stake(&self, msg: &str) {
        *self.stake_err.lock().unwrap() = Some(msg.into());
    }

    pub fn set_details_for(&self, pn_address: &str, d: PnDetails) {
        self.details_by_pn.lock().unwrap().insert(pn_address.to_string(), Ok(d));
    }

    /// Make `get_details(pn_address)` return a typed
    /// `DomainError::AccountNotDeployed` so callers exercise the -2013
    /// path (e.g. registering a note that is not deployed on-chain).
    pub fn set_not_deployed(&self, pn_address: &str) {
        self.not_deployed.lock().unwrap().insert(pn_address.to_string());
    }

    pub fn set_stake_for(&self, pn_address: &str, s: Option<PnStake>) {
        self.stake_by_pn.lock().unwrap().insert(pn_address.to_string(), s);
    }

    pub fn set_stake_for_hash(&self, stake_hash: &str, s: Option<PnStake>) {
        self.stake_by_hash.lock().unwrap().insert(stake_hash.to_string(), s);
    }
}

#[async_trait]
impl PnStateReader for FakePnStateReader {
    async fn get_details(&self, pn_address: &str) -> anyhow::Result<PnDetails> {
        if self.not_deployed.lock().unwrap().contains(pn_address) {
            return Err(anyhow::Error::from(DomainError::AccountNotDeployed));
        }
        if let Some(result) = self.details_by_pn.lock().unwrap().get(pn_address).cloned() {
            return match result {
                Ok(d) => Ok(d),
                Err(msg) => Err(anyhow::anyhow!(msg)),
            };
        }
        match self.details.lock().unwrap().clone() {
            Some(Ok(d)) => Ok(d),
            Some(Err(msg)) => Err(anyhow::anyhow!(msg)),
            None => Err(anyhow::anyhow!("FakePnStateReader: details not set")),
        }
    }

    async fn get_stake(
        &self,
        pn_address: &str,
        stake_hash: &str,
    ) -> anyhow::Result<Option<PnStake>> {
        if let Some(msg) = self.stake_err.lock().unwrap().clone() {
            return Err(anyhow::anyhow!(msg));
        }
        {
            let map = self.stake_by_hash.lock().unwrap();
            if !map.is_empty() {
                return Ok(map.get(stake_hash).cloned().unwrap_or(None));
            }
        }
        if let Some(s) = self.stake_by_pn.lock().unwrap().get(pn_address).cloned() {
            return Ok(s);
        }
        Ok(self.stake.lock().unwrap().clone().unwrap_or(None))
    }

    async fn owner_pubkey(&self, pn_address: &str) -> anyhow::Result<String> {
        if let Some(msg) = self.owner_err.lock().unwrap().clone() {
            return Err(anyhow::anyhow!(msg));
        }
        if self.not_deployed.lock().unwrap().contains(pn_address) {
            return Err(anyhow::Error::from(DomainError::AccountNotDeployed));
        }
        // The fixture note is owned by the canonical test seckey ("00"*32);
        // a registration submitting a different key fails the binding.
        Ok(dodex_application::derive_ed25519_pubkey_hex(&"00".repeat(32)).unwrap())
    }
}

/// `PnStateReader` that routes the test-supplied payload through the
/// real `details_from_value` / `stake_from_value` parsers used by
/// `GraphqlPnStateReader`. Lets HTTP-level tests exercise the
/// boundary validation that production applies after `run_getter`
/// detokenises the chain reply — i.e. relaxing rules like
/// "balance amount must be a non-empty ASCII digit string" cannot
/// pass through the HTTP suite green just because [`FakePnStateReader`]
/// skips the GraphQL layer.
///
/// Payload shape mirrors what `run_getter` produces for the matching
/// getter:
/// * `set_details_raw(pn, value)` accepts the `getDetails()` reply:
///   `{ "balance": {"<token_type>": "<uint128>"},
///      "lockedInOrders": {...}, ... }`.
/// * `set_stakes_raw(pn, value)` accepts the `_stakes` reply:
///   `{ "_stakes": { "<hash>": { "amount": [...], "debtAmount": [...],
///                                "couponsAmount": [...] } } }`.
#[derive(Default)]
pub struct RawJsonPnStateReader {
    details_by_pn: Mutex<StdHashMap<String, serde_json::Value>>,
    stakes_by_pn: Mutex<StdHashMap<String, serde_json::Value>>,
}

impl RawJsonPnStateReader {
    pub fn set_details_raw(&self, pn_address: &str, v: serde_json::Value) {
        self.details_by_pn.lock().unwrap().insert(pn_address.to_string(), v);
    }

    pub fn set_stakes_raw(&self, pn_address: &str, v: serde_json::Value) {
        self.stakes_by_pn.lock().unwrap().insert(pn_address.to_string(), v);
    }
}

#[async_trait]
impl PnStateReader for RawJsonPnStateReader {
    async fn get_details(&self, pn_address: &str) -> anyhow::Result<PnDetails> {
        let v =
            self.details_by_pn.lock().unwrap().get(pn_address).cloned().ok_or_else(|| {
                anyhow::anyhow!("RawJsonPnStateReader: no details for {pn_address}")
            })?;
        dodex_infrastructure::pn_state_reader::details_from_value(&v)
    }

    async fn get_stake(
        &self,
        pn_address: &str,
        stake_hash: &str,
    ) -> anyhow::Result<Option<PnStake>> {
        let v =
            self.stakes_by_pn.lock().unwrap().get(pn_address).cloned().ok_or_else(|| {
                anyhow::anyhow!("RawJsonPnStateReader: no stakes for {pn_address}")
            })?;
        dodex_infrastructure::pn_state_reader::stake_from_value(&v, stake_hash)
    }

    async fn owner_pubkey(&self, _pn_address: &str) -> anyhow::Result<String> {
        anyhow::bail!("RawJsonPnStateReader: register flow uses FakePnStateReader")
    }
}

/// Reference-token repo backed by an in-memory map. Pre-populated
/// with the three seeded rows so tests don't have to repeat the
/// fixture.
#[derive(Default)]
pub struct FakeReferenceRepo {
    rows: Mutex<StdHashMap<u32, RefToken>>,
}

impl FakeReferenceRepo {
    pub fn with_seeded() -> Self {
        let mut m = StdHashMap::new();
        m.insert(1, RefToken { token_type: 1, token_code: "NACKL".into(), decimals: 9 });
        m.insert(2, RefToken { token_type: 2, token_code: "SHELL".into(), decimals: 9 });
        m.insert(3, RefToken { token_type: 3, token_code: "USDC".into(), decimals: 6 });
        Self { rows: Mutex::new(m) }
    }

    pub fn add(&self, token_type: u32, code: &str, decimals: u8) {
        self.rows
            .lock()
            .unwrap()
            .insert(token_type, RefToken { token_type, token_code: code.into(), decimals });
    }
}

#[async_trait]
impl ReferenceRepository for FakeReferenceRepo {
    async fn lookup_ref_token(&self, token_type: u32) -> anyhow::Result<Option<RefToken>> {
        Ok(self.rows.lock().unwrap().get(&token_type).cloned())
    }
}

/// Resolve any API key to its account's `pn_address`.
///
/// Joins `api_keys → accounts` rather than relying on insertion order because
/// `created_at` defaults to `now()` and seed inserts share the same instant.
pub async fn seeded_pn_address_for_key(pool: &PgPool, api_key: &str) -> String {
    sqlx::query_scalar(
        "select a.pn_address \
         from accounts a \
         join api_keys k on k.account_id = a.id \
         where k.api_key = $1 limit 1",
    )
    .bind(api_key)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Insert a TRADE-only API key for `test-mm-001`. Used by permission tests
/// that need a key without USER_DATA access.
pub async fn insert_trade_only_key(pool: &PgPool, kek: &Kek, api_key: &str, secret_hex: &str) {
    use dodex_infrastructure::crypto;

    let account_id: uuid::Uuid =
        sqlx::query_scalar("select id from accounts where label = 'test-mm-001'")
            .fetch_one(pool)
            .await
            .expect("seeded account exists");

    let secret = hex::decode(secret_hex).unwrap();
    let secret_enc = crypto::seal(kek, &secret).expect("seal trade-only secret");

    sqlx::query(
        r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
           values ($1, $2, $3, array['TRADE'::auth_permission])"#,
    )
    .bind(account_id)
    .bind(api_key)
    .bind(&secret_enc)
    .execute(pool)
    .await
    .expect("insert trade-only api_key");
}

/// Remove a trade-only API key inserted by `insert_trade_only_key`.
pub async fn cleanup_trade_only_key(pool: &PgPool, api_key: &str) {
    sqlx::query("delete from api_keys where api_key = $1")
        .bind(api_key)
        .execute(pool)
        .await
        .expect("failed to clean up trade-only api_key");
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
