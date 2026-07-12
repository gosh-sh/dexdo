// Canary for the DB-gated integration suite. Every other DB test self-skips
// when TEST_DATABASE_URL is unset, so a suite run without the test Postgres
// looks green while executing nothing. This test fails instead and names the
// command that fixes it. CI always provides TEST_DATABASE_URL and a Postgres
// service (.github/workflows/pr-tests.yml), so it can only fire locally.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn test_database_is_reachable() {
    let _ = dotenvy::dotenv();
    let url = match std::env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => panic!(
            "TEST_DATABASE_URL is not set: the DB-gated integration tests silently skipped. \
             Run `make test-db-up` (or see README.md#test-postgres)."
        ),
    };
    if let Err(e) = PgPoolOptions::new().acquire_timeout(Duration::from_secs(5)).connect(&url).await
    {
        panic!(
            "TEST_DATABASE_URL is set but Postgres is unreachable ({e}). \
             Run `make test-db-up` (or see README.md#test-postgres)."
        );
    }
}
