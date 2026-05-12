// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// End-to-end test for `seed::seed_accounts`. Verifies the hard-coded
// JSON inserts every row on a fresh DB and is a no-op on a re-run
// (idempotency). Gated on TEST_DATABASE_URL — see
// crates/infrastructure/tests/reprojection.rs for the docker-compose
// harness.

use std::env;
use std::time::Duration;

use dodex_infrastructure::crypto::Kek;
use dodex_infrastructure::database;
use dodex_infrastructure::seed;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

async fn purge_seed_rows(pool: &PgPool) {
    // Drop everything seeded so the test starts from a clean slate
    // even if a previous failing run left rows behind.
    sqlx::query("delete from api_keys where api_key like 'dk_live_test_%'")
        .execute(pool)
        .await
        .expect("purge api_keys");
    sqlx::query(
        r#"delete from accounts
           where label like 'test-mm-%'
              or pn_address in (
                '0:42781640a51593054f3cdaba2dc9f9bcd746a7847828864a7bec0a84c1a9a4ab',
                '0:18f7d7f71f9c3235c295bb87006ab4850285ca55dd72dbe627c470f979adb5c0',
                '0:2032f18e320f24791ace5804087b1b6ca7d34a73983b3b594dd3c9d9085da4a7',
                '0:f0289b2052e384b01a34afc31c5a31ac08639626c57676e050d29c169846896f',
                '0:be4d08affd3bab2ab63ac34e730bd4e40455819248f4f708282250eeecf2feb3',
                '0:f6693cca08a1189ed0751e72034331fc6c618fd6b48b8d9f4bcaec8cdc02acef',
                '0:16db530b3946eb5af86ec4782bede0eac128a022b2f4cc51ee2b29850f467c13',
                '0:e621ce9f99ad514db84eee4b444c869535be30a9f3b0e8f31cc81e77bbee4a7c',
                '0:131093824ee79d800fe91c5b1e65db452129170d238e2876413315b02e49286b',
                '0:053d69da328ed15fff87a9cfaac2ffb8d6c5b9f4f3027420eeac3b08b3b8dfb9'
              )"#,
    )
    .execute(pool)
    .await
    .expect("purge accounts");
}

#[tokio::test]
async fn seed_inserts_all_accounts_on_fresh_db() {
    let Some(pool) = setup().await else { return };
    purge_seed_rows(&pool).await;
    let kek = test_kek();

    let report = seed::seed_accounts(&pool, &kek).await.expect("seed");
    assert_eq!(report.accounts_inserted, 10);
    assert_eq!(report.accounts_skipped, 0);
    assert_eq!(report.api_keys_inserted, 10);
    assert_eq!(report.api_keys_skipped, 0);

    let account_count: i64 =
        sqlx::query_scalar("select count(*) from accounts where label like 'test-mm-%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(account_count, 10);

    let api_key_count: i64 =
        sqlx::query_scalar("select count(*) from api_keys where api_key like 'dk_live_test_%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(api_key_count, 10);

    // Cleanup for the next run.
    purge_seed_rows(&pool).await;
}

#[tokio::test]
async fn seed_is_idempotent_on_rerun() {
    let Some(pool) = setup().await else { return };
    purge_seed_rows(&pool).await;
    let kek = test_kek();

    let first = seed::seed_accounts(&pool, &kek).await.expect("seed first");
    let second = seed::seed_accounts(&pool, &kek).await.expect("seed second");

    assert_eq!(first.accounts_inserted, 10);
    assert_eq!(first.api_keys_inserted, 10);
    // Second run: every row is a conflict, every counter shows skip.
    assert_eq!(second.accounts_inserted, 0);
    assert_eq!(second.accounts_skipped, 10);
    assert_eq!(second.api_keys_inserted, 0);
    assert_eq!(second.api_keys_skipped, 10);

    let account_count: i64 =
        sqlx::query_scalar("select count(*) from accounts where label like 'test-mm-%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(account_count, 10, "rerun must not produce duplicates");

    purge_seed_rows(&pool).await;
}
