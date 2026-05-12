// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// End-to-end tests for `seed::apply_seed`. Each test generates a fresh
// per-run prefix and feeds `apply_seed` a synthetic `SeedData` with
// that prefix, so concurrent tests (even within a binary running with
// parallel threads) never share rows and never need cross-test
// cleanup coordination. The hardcoded production `SEED_DATA` and
// `seed::seed_accounts` are covered by unit tests in `seed.rs`; here
// we exercise the DB-side pipeline.
//
// Gated on TEST_DATABASE_URL — see
// crates/infrastructure/tests/reprojection.rs for the docker-compose
// harness.

use std::env;
use std::time::Duration;

use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::database;
use dodex_infrastructure::seed;
use dodex_infrastructure::seed::SeedAccount;
use dodex_infrastructure::seed::SeedApiKey;
use dodex_infrastructure::seed::SeedData;
use num_bigint::BigUint;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

async fn setup() -> Option<PgPool> {
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

fn test_kek() -> Kek {
    Kek::from_hex(&"ab".repeat(32)).unwrap()
}

/// Per-test fixture scope. Holds the UUID prefix used to namespace all
/// inserted rows and yields the label / address patterns the test
/// asserts and cleans up against.
struct Scope {
    label_prefix: String,
    address_prefix: String,
    api_key_prefix: String,
}

impl Scope {
    fn new() -> Self {
        let id = Uuid::new_v4().simple().to_string();
        Self {
            label_prefix: format!("seedtest-{id}-"),
            address_prefix: format!("0:seedtest-{id}-"),
            api_key_prefix: format!("dk_seedtest_{id}_"),
        }
    }

    /// Build `n` accounts, each with one api_key, all prefixed by this
    /// scope's UUID. `pn_pubkey_dec` and `pn_dih_dec` derive from a
    /// fresh UUID per account so the global UNIQUE constraints on
    /// those columns are never hit across parallel runs.
    fn synth(&self, n: usize) -> SeedData {
        let accounts = (0..n)
            .map(|i| {
                let pubkey = uuid_to_dec(Uuid::new_v4());
                let dih = uuid_to_dec(Uuid::new_v4());
                SeedAccount {
                    label: Some(format!("{}{i:03}", self.label_prefix)),
                    pn_address: format!("{}{i:03}", self.address_prefix),
                    pn_pubkey_dec: pubkey,
                    pn_seckey_hex: "00".repeat(32),
                    pn_dih_dec: dih,
                    api_keys: vec![SeedApiKey {
                        api_key: format!("{}{i:03}", self.api_key_prefix),
                        api_secret_hex: "00".repeat(32),
                        permissions: vec!["USER_DATA".into(), "TRADE".into()],
                    }],
                }
            })
            .collect();
        SeedData { accounts }
    }

    async fn cleanup(&self, pool: &PgPool) {
        // api_keys go first (FK back to accounts), then accounts.
        let api_pattern = format!("{}%", self.api_key_prefix);
        let label_pattern = format!("{}%", self.label_prefix);
        sqlx::query("delete from api_keys where api_key like $1")
            .bind(&api_pattern)
            .execute(pool)
            .await
            .expect("purge api_keys");
        sqlx::query("delete from accounts where label like $1")
            .bind(&label_pattern)
            .execute(pool)
            .await
            .expect("purge accounts");
    }
}

fn uuid_to_dec(uuid: Uuid) -> String {
    // 128-bit unsigned integer view of the UUID bytes. Fits well within
    // numeric(78,0) (max ~256 bits) and gives globally-unique decimals
    // for the `pn_pubkey`/`pn_dih` columns without colliding with the
    // baked production seed values.
    BigUint::from_bytes_be(uuid.as_bytes()).to_str_radix(10)
}

#[tokio::test]
async fn apply_seed_inserts_all_rows_on_fresh_scope() {
    let Some(pool) = setup().await else { return };
    let kek = test_kek();
    let scope = Scope::new();

    let report = seed::apply_seed(&pool, &kek, scope.synth(5)).await.expect("apply_seed");
    assert_eq!(report.accounts_inserted, 5);
    assert_eq!(report.accounts_skipped, 0);
    assert_eq!(report.api_keys_inserted, 5);
    assert_eq!(report.api_keys_skipped, 0);

    let account_count: i64 =
        sqlx::query_scalar("select count(*) from accounts where label like $1")
            .bind(format!("{}%", scope.label_prefix))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(account_count, 5);

    let api_key_count: i64 =
        sqlx::query_scalar("select count(*) from api_keys where api_key like $1")
            .bind(format!("{}%", scope.api_key_prefix))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(api_key_count, 5);

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn apply_seed_is_idempotent_on_rerun() {
    let Some(pool) = setup().await else { return };
    let kek = test_kek();
    let scope = Scope::new();

    let first = seed::apply_seed(&pool, &kek, scope.synth(3)).await.expect("apply first");
    // Same payload twice → second run sees only conflicts.
    let second = seed::apply_seed(&pool, &kek, scope.synth(3)).await.expect("apply second");

    // The two `synth` calls produce different per-account pubkeys/dihs
    // (each picks a fresh inner Uuid), but the same `pn_address` and
    // `api_key` strings — that is what the ON CONFLICT targets use.
    assert_eq!(first.accounts_inserted, 3);
    assert_eq!(first.api_keys_inserted, 3);
    assert_eq!(second.accounts_inserted, 0);
    assert_eq!(second.accounts_skipped, 3);
    assert_eq!(second.api_keys_inserted, 0);
    assert_eq!(second.api_keys_skipped, 3);

    let account_count: i64 =
        sqlx::query_scalar("select count(*) from accounts where label like $1")
            .bind(format!("{}%", scope.label_prefix))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(account_count, 3, "rerun must not produce duplicates");

    scope.cleanup(&pool).await;
}
