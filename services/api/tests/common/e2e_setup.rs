// Shared helpers for the shellnet-backed e2e tests. POST (`e2e_order`)
// and DELETE (`e2e_cancel_order`) both: deploy a fresh OrderBook,
// upsert it into the read-model, provision an HMAC api_key for the
// deployer-PN, then drive the production router. The split out of
// `mod.rs` keeps the unit-test helpers (`canonical_query`, `sign`,
// etc.) free of the heavy `ackinacki_kit` chain dependencies — every
// test compiles only what it actually needs.

#![allow(dead_code)]

use std::env;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::crypto::{self};
use dodex_infrastructure::database;
use hmac::Mac;
use num_bigint::BigUint;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use super::deploy_market::EphemeralMarket;
use super::test_kek;
use super::test_pns::TestPn;
use super::HmacSha256;

/// Reachable shellnet endpoint. Hard-coded — the e2e flow is anchored
/// to this network's RootOracle / RootPN addresses and a different
/// endpoint would need its own fixture pool.
pub const SHELLNET_ENDPOINT: &str = "shellnet.ackinacki.org";

// On-chain constants for NACKL (token_type=1), from
// `contracts/modifiers/modifiers.sol`. The `market_outcomes` row we
// insert mirrors them so our local validation does not reject (or
// under-scale) values the chain would actually accept:
//   - TICK_SIZE = 10 bps; price must be a uint multiple of 10.
//   - LOT_SIZE_NACKL = 10_000_000 raw = 0.01 NACKL (9 decimals).
//   - MIN_ORDER_NOTIONAL_NACKL = 10 NACKL.
pub const TEST_TICK_SIZE: &str = "0.001";
pub const TEST_STEP_SIZE: &str = "0.01";
pub const TEST_MIN_NOTIONAL: &str = "10";
pub const TEST_PRICE_PRECISION: i32 = 3;
pub const TEST_QUANTITY_PRECISION: i32 = 2;

/// Per-process `clientOrderId` salt: the suite's start timestamp
/// shifted left by 32 bits (occupying bits 32 through ~62 for any
/// realistic unix time; the high bit stays clear until ≈2106). The
/// shift width matters because `ParamsOfPlaceOrder` serializes
/// `client_order_id: u128` through `serde_json`, which rejects
/// values that do not fit in `u64`. A `<< 32` salt leaves room for
/// a 32-bit `base` in the low bits and keeps the whole value inside
/// u64.
pub fn salt() -> u128 {
    static S: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        (secs as u128) << 32
    })
}

pub fn fresh_coid(base: u32) -> u128 {
    salt() | (base as u128)
}

/// Connect to `TEST_DATABASE_URL`, run migrations, return `None` (with
/// a skip notice) when the env var is not set so `cargo test` without
/// docker still passes.
pub async fn db_pool() -> Option<(PgPool, Arc<Kek>)> {
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

/// Insert (or refresh) the just-deployed market into the read-model
/// and stamp it as fully reconciled. The handler resolves
/// `(marketAddress, symbol)` against this row; without it the request
/// would 404 with -1121 before reaching the chain.
pub async fn upsert_market(
    pool: &PgPool,
    market: &EphemeralMarket,
    market_name: &str,
    symbol: &str,
) {
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
    .bind(to_decimal_string(&market.event_id))
    .bind(to_decimal_string(&market.oracle_list_hash))
    .bind(&market.order_book_address)
    .bind(market.stake_start_unix)
    .bind(market.stake_end_unix)
    .bind(market.result_start_unix)
    .bind(market.result_end_unix)
    .bind(market.freeze_unix)
    .execute(pool)
    .await
    .expect("upsert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                max_batch_size)
           select id, $1, $2, $3, $4, $5, $6, $7, $8, $9, 5
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
    .bind(market.outcome_id as i32)
    .bind(&market.outcome_name)
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

/// Provision an account + api_key for `pn` so the e2e test can sign
/// HMAC requests against it. The api_key is UUID-suffixed per run so
/// repeat invocations never collide on the unique constraint.
pub async fn provision_account(pool: &PgPool, kek: &Kek, pn: &TestPn) -> (String, String) {
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

/// Decimal-encode the hex pubkey so it round-trips through the
/// `numeric(78,0)` column.
pub fn pubkey_hex_to_decimal(hex: &str) -> String {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    BigUint::parse_bytes(stripped.as_bytes(), 16)
        .unwrap_or_else(|| panic!("invalid pubkey hex: {hex}"))
        .to_str_radix(10)
}

/// Percent-encode RFC 3986 query-component bytes. Only the unreserved
/// set (`A-Z`, `a-z`, `0-9`, `-`, `.`, `_`, `~`) passes through; every
/// other byte becomes `%HH`.
///
/// Why this exists: the `auth_hoop` signs `raw_query_string` byte-for-
/// byte. A request whose `marketAddress` or `symbol` contains `:` will
/// be sent by `TestClient` with the colon percent-encoded, so we must
/// HMAC the same encoded bytes — not the raw string. Production
/// clients (`reqwest`, browsers) already encode at this granularity;
/// our test helper just makes the encoding explicit.
pub fn percent_encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Accept both `0x…` hex and pure decimal inputs; return the decimal
/// form `numeric(78,0)` expects. `deploy_ephemeral_market` returns
/// decimal; refreshed fixtures of either flavour just work.
pub fn to_decimal_string(value: &str) -> String {
    if let Some(stripped) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        return BigUint::parse_bytes(stripped.as_bytes(), 16)
            .unwrap_or_else(|| panic!("invalid hex: {value}"))
            .to_str_radix(10);
    }
    BigUint::parse_bytes(value.as_bytes(), 10)
        .unwrap_or_else(|| panic!("invalid decimal: {value}"))
        .to_str_radix(10)
}
