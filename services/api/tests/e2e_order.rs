// End-to-end smoke test for `POST /api/v1/order` against a real
// shellnet OrderBook. Drives the production router (with the real
// `BeeDexChainSender`, real `PostgresAuthenticator`, real
// `PostgresReadModelRepository`), placing a BUY LIMIT GTC order and
// then polling `OrderBook.getOrdersByOwner` until the chain assigns
// an `orderId` to the `clientOrderId` we sent.
//
// Marked `#[ignore]` because it needs:
//   - TEST_DATABASE_URL (test Postgres up — see README.md#test-postgres)
//   - reachable shellnet endpoint
//   - the bundled fixtures `tests/fixtures/ob_pool.json` and
//     `tests/fixtures/test_pns.json`
//
// Run explicitly:
//
//   cargo test -p dodex-api --test e2e_order -- --ignored --nocapture
//
// === SECURITY NOTE ===
// `tests/fixtures/test_pns.json` ships plaintext `owner_secret_key_hex`
// values for FOUR shellnet-only throwaway trading PNs. This is
// intentional and safe ONLY because:
//   - shellnet is a public devnet — the seckeys hold test NACKL only;
//   - the PNs are not used anywhere except this e2e test;
//   - anyone with shellnet access can already replicate them.
// Do NOT repurpose this fixture format for any non-devnet network.
// New environments (stage, prod) must keep seckeys in the secret store
// per `auth.md`, NOT in a checked-in JSON file.
//
// Fixture lifetime: `ob_pool.json` is minted with a bounded trading
// window (~10 hours of `freeze_unix → result_start_unix`). The test
// sanity-checks `freeze_unix < now < result_start_unix` at start and
// fails fast with an explicit "fixture expired" message if the
// window has elapsed. Expected rotation cadence: at most once per
// trading window, more typically whenever an unrelated PR touches
// the fixtures or the chain side. Refreshed `ob_pool.json` /
// `test_pns.json` are committed by the project maintainer in
// lockstep — test runners do NOT regenerate fixtures themselves.

mod common;

use std::env;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::canonical_query;
use common::now_ms;
use common::sign;
use common::test_kek;
use common::HmacSha256;
use dodex_api::testkit::build_router;
use dodex_api::testkit::AppState;
use dodex_api::testkit::SharedAuth;
use dodex_api::testkit::SharedChainSender;
use dodex_api::testkit::SharedRepo;
use dodex_infrastructure::auth::PostgresAuthenticator;
use dodex_infrastructure::chain_sender::BeeDexChainSender;
use dodex_infrastructure::config::AuthSection;
use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::crypto::{self};
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use hmac::Mac;
use num_bigint::BigUint;
use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const SHELLNET_ENDPOINT: &str = "shellnet.ackinacki.org";
// On-chain constants for NACKL (token_type=1), from
// `contracts/modifiers/modifiers.sol`. The `market_outcomes` row
// inserted by this test mirrors them so our local validation does
// not reject (or under-scale) values the chain would actually
// accept:
//   - TICK_SIZE = 10 bps; price must be a uint multiple of 10.
//   - LOT_SIZE_NACKL = 10_000_000 raw = 0.01 NACKL (9 decimals).
//   - MIN_ORDER_NOTIONAL_NACKL = 10 NACKL.
const TEST_TICK_SIZE: &str = "10";
const TEST_STEP_SIZE: &str = "0.01";
const TEST_MIN_NOTIONAL: &str = "10";
const TEST_PRICE_PRECISION: i32 = 0;
const TEST_QUANTITY_PRECISION: i32 = 9;
/// Per-process `clientOrderId` salt: the suite's start timestamp
/// shifted left by 32 bits (occupying bits 32 through ~62 for any
/// realistic unix time; the high bit stays clear until ≈2106).
/// Matches `bee_dex::tests::integration::order_book::salted_coid` —
/// the shift width matters because `ParamsOfPlaceOrder` serializes
/// `client_order_id: u128` through `serde_json`, which rejects
/// values that do not fit in `u64` ("number out of range"). A `<< 32`
/// salt leaves room for a 32-bit `base` in the low bits and keeps the
/// whole value inside u64.
fn salt() -> u128 {
    static S: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        (secs as u128) << 32
    })
}

fn fresh_coid(base: u32) -> u128 {
    salt() | (base as u128)
}

// ---- Fixture types ------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ObPool {
    markets: Vec<ObMarket>,
    #[serde(default)]
    #[allow(dead_code)]
    token_type: u32,
}

#[derive(Debug, Deserialize)]
struct ObMarket {
    pmp_address: String,
    order_book_address: String,
    event_id: String,
    oracle_list_hash: String,
    token_type: u32,
    outcome_names: serde_json::Map<String, serde_json::Value>,
    stake_start_unix: i64,
    stake_end_unix: i64,
    result_start_unix: i64,
    result_end_unix: i64,
    freeze_unix: i64,
}

#[derive(Debug, Deserialize)]
struct TestPnPool {
    notes: Vec<TestPn>,
}

#[derive(Debug, Deserialize, Clone)]
struct TestPn {
    address: String,
    deposit_identifier_hash: String,
    owner_public_key_hex: String,
    owner_secret_key_hex: String,
    #[serde(default)]
    #[allow(dead_code)]
    shell_funded: bool,
    #[serde(default)]
    #[allow(dead_code)]
    native_funded: bool,
}

fn load_fixtures() -> (ObPool, TestPnPool) {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let ob_path = format!("{manifest}/../../tests/fixtures/ob_pool.json");
    let pn_path = format!("{manifest}/../../tests/fixtures/test_pns.json");
    let ob: ObPool = serde_json::from_str(
        &std::fs::read_to_string(&ob_path).unwrap_or_else(|err| panic!("read {ob_path}: {err}")),
    )
    .unwrap_or_else(|err| panic!("parse ob_pool.json: {err}"));
    let pns: TestPnPool = serde_json::from_str(
        &std::fs::read_to_string(&pn_path).unwrap_or_else(|err| panic!("read {pn_path}: {err}")),
    )
    .unwrap_or_else(|err| panic!("parse test_pns.json: {err}"));
    (ob, pns)
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Verify the bundled `ob_pool.json` market is currently inside its
/// trading window. Fails the test with a clear "fixture expired"
/// message otherwise — refreshing fixtures is the project
/// maintainer's job, not the test runner's; this assertion just
/// surfaces the staleness so it does not show up as an opaque
/// chain-side error later in the run.
fn assert_market_in_trading_window(market: &ObMarket) {
    let now = now_seconds();
    assert!(
        now >= market.freeze_unix,
        "ob_pool.json fixture is not yet TRADING: freeze_unix={} but now={} ({}s early). \
         Wait, or contact the project maintainer for a refreshed fixture pair.",
        market.freeze_unix,
        now,
        market.freeze_unix - now,
    );
    assert!(
        now < market.result_start_unix,
        "ob_pool.json fixture has exited its trading window: result_start_unix={} but \
         now={} ({}s past). Contact the project maintainer for a refreshed fixture pair.",
        market.result_start_unix,
        now,
        now - market.result_start_unix,
    );
}

// ---- Test setup ---------------------------------------------------------

async fn db_pool() -> Option<(PgPool, Arc<Kek>)> {
    let _ = dotenvy::dotenv();
    let url = env::var("TEST_DATABASE_URL").ok().filter(|s| !s.is_empty())?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    database::run_migrations(&pool).await.expect("run migrations");
    Some((pool, test_kek()))
}

/// Decimal-encode the hex pubkey so it round-trips through the
/// `numeric(78,0)` column. Hex-bytes for the seckey stay as-is and
/// get sealed under the KEK by `apply_seed`.
fn pubkey_hex_to_decimal(hex: &str) -> String {
    BigUint::parse_bytes(hex.as_bytes(), 16)
        .unwrap_or_else(|| panic!("invalid pubkey hex: {hex}"))
        .to_str_radix(10)
}

/// Insert (or refresh) the ob_pool market into the read-model and
/// stamp it as fully reconciled. The handler resolves
/// `(marketAddress, symbol)` against this row; without it the request
/// would 404 with -1121 before reaching the chain.
async fn upsert_market(pool: &PgPool, market: &ObMarket, market_name: &str, symbol: &str) {
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code, event_id,
                oracle_list_hash, orderbook_address, approved, stake_start, stake_end,
                result_start, result_end, frozen_at, num_outcomes, last_reconciled_at)
           values ($1, $2, $2, $3, 'NACKL', $4::numeric, $5::numeric, $6, true,
                   $7, $8, $9, $10, $11, 1, now())
           on conflict (pmp_address) do update set
               orderbook_address = excluded.orderbook_address,
               event_id = excluded.event_id,
               oracle_list_hash = excluded.oracle_list_hash,
               frozen_at = excluded.frozen_at,
               last_reconciled_at = excluded.last_reconciled_at"#,
    )
    .bind(&market.pmp_address)
    .bind(market_name)
    .bind(market.token_type as i32)
    .bind(hex_to_decimal(&market.event_id))
    .bind(hex_to_decimal(&market.oracle_list_hash))
    .bind(&market.order_book_address)
    .bind(market.stake_start_unix)
    .bind(market.stake_end_unix)
    .bind(market.result_start_unix)
    .bind(market.result_end_unix)
    .bind(market.freeze_unix)
    .execute(pool)
    .await
    .expect("upsert market");

    // Use exactly one outcome (the one we'll trade). A real market
    // carries every outcome but the test only needs the symbol it
    // POSTs against; refreshing both each run keeps cleanup linear.
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                max_batch_size)
           select id, $1, 1, 'Team A', $2, $3, $4, $5, $6, $7, 5
             from markets where pmp_address = $1
           on conflict (pmp_address, outcome_id) do update set
               symbol = excluded.symbol,
               price_precision = excluded.price_precision,
               quantity_precision = excluded.quantity_precision,
               tick_size = excluded.tick_size,
               step_size = excluded.step_size,
               min_notional = excluded.min_notional"#,
    )
    .bind(&market.pmp_address)
    .bind(symbol)
    .bind(TEST_PRICE_PRECISION)
    .bind(TEST_QUANTITY_PRECISION)
    .bind(TEST_TICK_SIZE)
    .bind(TEST_STEP_SIZE)
    .bind(TEST_MIN_NOTIONAL)
    .execute(pool)
    .await
    .expect("upsert market_outcomes");
}

fn hex_to_decimal(hex: &str) -> String {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    BigUint::parse_bytes(stripped.as_bytes(), 16)
        .unwrap_or_else(|| panic!("invalid hex: {hex}"))
        .to_str_radix(10)
}

/// Provision an account + api_key for `pn` so the e2e test can sign
/// HMAC requests against it. The api_key is UUID-suffixed per run so
/// repeat invocations never collide on the unique constraint.
async fn provision_account(pool: &PgPool, kek: &Kek, pn: &TestPn) -> (String, String) {
    let pubkey_dec = pubkey_hex_to_decimal(&pn.owner_public_key_hex);
    let seckey_bytes =
        hex::decode(&pn.owner_secret_key_hex).expect("test_pns.json: seckey must be hex");
    let pn_seckey_enc = crypto::seal(kek, &seckey_bytes).expect("seal pn_seckey");

    sqlx::query(
        r#"insert into accounts (label, pn_address, pn_pubkey, pn_seckey_enc, pn_dih)
           values ($1, $2, $3::numeric, $4, $5::numeric)
           on conflict (pn_address) do update set
               pn_pubkey = excluded.pn_pubkey,
               pn_seckey_enc = excluded.pn_seckey_enc,
               pn_dih = excluded.pn_dih"#,
    )
    .bind("e2e-test-pn")
    .bind(&pn.address)
    .bind(&pubkey_dec)
    .bind(&pn_seckey_enc)
    .bind(&pn.deposit_identifier_hash)
    .execute(pool)
    .await
    .expect("upsert account");

    let scope = uuid::Uuid::new_v4().simple().to_string();
    let api_key = format!("dk_e2e_{scope}");
    // Random 32-byte secret, exposed both as hex (for HMAC computation)
    // and sealed (for storage). Generated locally so we never need to
    // surface a "show once" workflow in tests.
    let secret_bytes = {
        let mut h = HmacSha256::new_from_slice(&[0u8; 32]).unwrap();
        h.update(scope.as_bytes());
        h.finalize().into_bytes().to_vec()
    };
    let secret_hex = hex::encode(&secret_bytes);
    let secret_enc = crypto::seal(kek, &secret_bytes).expect("seal api_secret");

    sqlx::query(
        r#"insert into api_keys (account_id, api_key, api_secret_enc, permissions)
           select id, $1, $2, '{USER_DATA,TRADE}'::auth_permission[]
             from accounts where pn_address = $3"#,
    )
    .bind(&api_key)
    .bind(&secret_enc)
    .bind(&pn.address)
    .execute(pool)
    .await
    .expect("insert api_key");

    (api_key, secret_hex)
}

// ---- E2E test -----------------------------------------------------------

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL + shellnet + tests/fixtures/{ob_pool,test_pns}.json"]
async fn buy_limit_gtc_against_shellnet() {
    // Surface the `BeeDexChainSender` / `bee_dex` error stream into
    // the test output so a transport failure does not collapse into
    // an opaque `-1000` body. `with_test_writer` routes lines through
    // `print!`, which `--nocapture` then exposes.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,bee_dex=debug,dodex_infrastructure::chain_sender=debug")
        .try_init();

    let (ob_pool, pn_pool) = load_fixtures();
    let ob_market = ob_pool.markets.first().expect("ob_pool.json: at least one market");
    assert_market_in_trading_window(ob_market);
    assert_eq!(ob_market.token_type, 1, "test assumes NACKL collateral (token_type=1)");
    let outcome_name = ob_market
        .outcome_names
        .get("1")
        .and_then(|v| v.as_str())
        .expect("ob_pool.json: outcome 1 name")
        .replace(' ', "-");

    let Some((pool, kek)) = db_pool().await else {
        eprintln!("[e2e_order] TEST_DATABASE_URL not set, skipping");
        return;
    };

    let market_name = format!("PM-E2E-{}", &ob_market.event_id[2..10]);
    let symbol = format!("{market_name}-{outcome_name}");
    upsert_market(&pool, ob_market, &market_name, &symbol).await;

    // The handler talks to chain through this sender; we construct
    // it once here and let `AppState` own the trait object. 30 s
    // matches the production default and is well above shellnet's
    // typical 1–3 s round-trip.
    let chain_sender: SharedChainSender = Arc::new(
        BeeDexChainSender::new(vec![SHELLNET_ENDPOINT.to_string()], Duration::from_secs(30))
            .expect("BeeDexChainSender::new"),
    );

    let trader = pn_pool.notes.first().cloned().expect("test_pns.json: at least one PN");
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
    // 30 NACKL of outcome at 5000 bps (= 0.5 probability) — matches
    // bee_dex integration tests' `ORDER_AMOUNT` / `ORDER_PRICE_BPS`.
    // Notional = 30 * 5000 / 10000 = 15 NACKL, comfortably above
    // MIN_ORDER_NOTIONAL_NACKL = 10 NACKL.
    let body = serde_json::to_vec(&json!({
        "marketAddress": ob_market.pmp_address,
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

    // Read the body once up-front so the assertion message and the
    // JSON parse share the same buffer; the alternative — reading
    // `status_code` and `take_string` in one `assert_eq!` — fails the
    // borrow checker because `take_string` is `&mut self`.
    let status = resp.status_code;
    let body = resp.take_string().await.expect("response body");
    assert_eq!(status, Some(StatusCode::OK), "POST /api/v1/order; body: {body}");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Ok {
        client_order_id: String,
        transact_time: i64,
        status: String,
    }
    let ok: Ok = serde_json::from_str(&body).expect("happy path body");
    // Minimal-response contract: three fields, status PENDING_NEW
    // until `OrderBook.OrderPlaced` projects through the indexer
    // (which surfaces the row as NEW via /api/v1/openOrders).
    assert_eq!(ok.status, "PENDING_NEW");
    assert_eq!(ok.client_order_id, coid);
    assert!(ok.transact_time > 0);

    // Poll the OrderBook until the chain reflects our placement.
    // The bee_dex integration tests use a 60s budget with 2s ticks
    // against the same shellnet — reuse those numbers.
    use bee_dex::Dex as RawDex;
    let raw_dex = RawDex::new(vec![SHELLNET_ENDPOINT.to_string()]).expect("RawDex::new");
    let coid_u128: u128 = coid.parse().expect("coid u128");
    let mut surfaced = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let owned = match raw_dex
            .get_orders_by_owner(
                &ob_market.order_book_address,
                trader.deposit_identifier_hash.clone(),
            )
            .await
        {
            Ok(o) => o,
            Err(err) => {
                eprintln!("[e2e_order] get_orders_by_owner errored (will retry): {err:?}");
                continue;
            }
        };
        if owned.orders.iter().any(|o| o.client_order_id == coid_u128) {
            surfaced = true;
            break;
        }
    }
    assert!(
        surfaced,
        "order with client_order_id={coid} did not surface in getOrdersByOwner within 60s — \
         either the chain rejected (check shellnet logs) or the trading PN lacks NACKL.",
    );

    // ---- Cleanup: cancel through bee_dex so the test does not leave
    // ---- collateral locked on the trading PN between runs. We use
    // ---- `cancelOrderByClient` so the cleanup keys off the same
    // ---- `clientOrderId` we just placed; no need to remember the
    // ---- chain-assigned order_id.
    use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
    use ackinacki_kit::tvm_client::abi::Signer;
    use ackinacki_kit::tvm_client::crypto::KeyPair;
    let signer = Signer::Keys {
        keys: KeyPair {
            public: trader.owner_public_key_hex.clone(),
            secret: trader.owner_secret_key_hex.clone(),
        },
    };
    let cancel_params = ParamsOfCancelOrderByClient {
        event_id: ob_market.event_id.clone(),
        oracle_list_hash: ob_market.oracle_list_hash.clone(),
        token_type: ob_market.token_type,
        client_order_id: coid_u128,
    };
    // Retry the cancel against shellnet flake — the placement step
    // already does the same shape (`surfaced` boolean below). Without
    // the retry, a transient gateway error would leave collateral
    // locked on the trading PN, which is exactly the failure mode
    // the cleanup is supposed to prevent.
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
                eprintln!("[e2e_order] cancel attempt {attempt} failed, retrying: {err:?}");
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    assert!(
        cancel_sent,
        "cancel_order_by_client failed after 5 attempts (last error: {last_err:?}) — \
         order may remain on book with collateral locked",
    );

    // Poll until the order is gone — same 60s budget as placement
    // confirmation. Without this, a fast-following test (or a
    // re-run) could race the cancel callback.
    let mut cancelled = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let owned = match raw_dex
            .get_orders_by_owner(
                &ob_market.order_book_address,
                trader.deposit_identifier_hash.clone(),
            )
            .await
        {
            Ok(o) => o,
            Err(err) => {
                eprintln!("[e2e_order] cleanup poll errored (will retry): {err:?}");
                continue;
            }
        };
        if !owned.orders.iter().any(|o| o.client_order_id == coid_u128) {
            cancelled = true;
            break;
        }
    }
    assert!(
        cancelled,
        "cancellation of client_order_id={coid} did not remove the order from \
         getOrdersByOwner within 60s — the trading PN may still have collateral locked.",
    );
}
