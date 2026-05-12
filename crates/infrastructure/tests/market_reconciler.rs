// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the MarketReconciler SQL contract after the
// migration-0014 invariant flip: `orderbook_address` is stamped on the first
// successful reconcile (the PMP getter is deterministic) and the CHECK
// constraint forbids reconciled rows from having a NULL address. The full
// reconciler path requires graphql/decoder plumbing; here we exercise just
// the SQL the reconciler emits — the SELECT predicate that decides which
// rows enter the queue and the UPDATE that writes the address. Mirrors the
// "test the SQL contract" approach used in
// `crates/infrastructure/tests/oel_reconciler.rs`.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Row;

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
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

async fn purge(pool: &PgPool, pmp_address: &str) {
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge market");
}

async fn seed_market(
    pool: &PgPool,
    pmp_address: &str,
    last_reconciled: bool,
    frozen_at: Option<i64>,
    orderbook_address: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                frozen_at,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   $3,
                   case when $4 then now() else null end)
           returning id"#,
    )
    .bind(pmp_address)
    .bind(orderbook_address)
    .bind(frozen_at)
    .bind(last_reconciled)
    .fetch_one(pool)
    .await
    .expect("insert market")
}

/// Mirror of the SELECT in `MarketReconciler::run_once` (reconciler.rs).
/// Returns the ids that the next sweep would pick up.
async fn pending_ids(pool: &PgPool) -> Vec<i64> {
    let rows = sqlx::query(
        r#"select id from markets
           where last_reconciled_at is null
             and (last_reconcile_failed_at is null
                  or last_reconcile_failed_at < now() - interval '5 minutes')
           order by last_reconcile_failed_at nulls first, id asc"#,
    )
    .fetch_all(pool)
    .await
    .expect("select pending");
    rows.iter().map(|r| r.get::<i64, _>("id")).collect()
}

/// Mirror of the UPDATE in `reconciler.rs::write_market_state`. Only the
/// orderbook-related field matters here; other fields are stable for the
/// test fixtures.
async fn reconcile_update(pool: &PgPool, market_id: i64, orderbook_address: &str) {
    sqlx::query(
        r#"update markets
              set market_id = pmp_address,
                  name = pmp_address,
                  approved = true,
                  is_cancelled = false,
                  num_outcomes = 2,
                  orderbook_address = $1,
                  last_reconciled_at = now()
            where id = $2"#,
    )
    .bind(orderbook_address)
    .bind(market_id)
    .execute(pool)
    .await
    .expect("reconcile update");
}

async fn read_orderbook_address(pool: &PgPool, market_id: i64) -> Option<String> {
    sqlx::query_scalar("select orderbook_address from markets where id = $1")
        .bind(market_id)
        .fetch_one(pool)
        .await
        .expect("read orderbook_address")
}

#[tokio::test]
async fn pre_freeze_reconcile_stamps_orderbook() {
    // `PMP.getOrderBookAddress()` is deterministic (contracts/PMP.sol:1360) and
    // returns the precomputed address even before PoolsFrozen lands. The
    // reconciler stamps that address on the first pass; the public api-spec.md
    // contract requires `orderBookAddress` to be present for any reconciled
    // market. Migration 0014's CHECK constraint pins this invariant.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_pre_freeze";
    purge(&pool, pmp).await;
    let id = seed_market(&pool, pmp, false, None, None).await;

    reconcile_update(&pool, id, "0:deterministic_precomputed").await;

    assert_eq!(
        read_orderbook_address(&pool, id).await.as_deref(),
        Some("0:deterministic_precomputed"),
        "reconciler must stamp the deterministic orderbook_address even pre-freeze"
    );
}

#[tokio::test]
async fn post_freeze_reconcile_stamps_orderbook() {
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_post_freeze";
    purge(&pool, pmp).await;
    let id = seed_market(&pool, pmp, false, Some(1_700_000_250), None).await;

    reconcile_update(&pool, id, "0:deployed_orderbook").await;

    assert_eq!(
        read_orderbook_address(&pool, id).await.as_deref(),
        Some("0:deployed_orderbook"),
        "post-freeze reconcile stamps orderbook_address (same path as pre-freeze)"
    );
}

#[tokio::test]
async fn reconciled_row_drops_out_of_queue_permanently() {
    // After migration 0014 the queue predicate is just `last_reconciled_at IS
    // NULL`: the deterministic getter returns the same address on every pass,
    // so there is no later re-queue trigger. PoolsFrozen landing afterwards
    // must NOT pull the row back into the queue.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_no_requeue";
    purge(&pool, pmp).await;

    // Never-reconciled row sits in the queue.
    let id = seed_market(&pool, pmp, false, None, None).await;
    assert!(pending_ids(&pool).await.contains(&id), "fresh row must be queued");

    // First reconcile pass stamps the address and last_reconciled_at.
    reconcile_update(&pool, id, "0:deterministic_precomputed").await;
    assert!(
        !pending_ids(&pool).await.contains(&id),
        "first reconcile must drop the row from the queue"
    );

    // PoolsFrozen lands later — must not re-queue.
    sqlx::query("update markets set frozen_at = $1 where id = $2")
        .bind(1_700_000_250_i64)
        .bind(id)
        .execute(&pool)
        .await
        .expect("set frozen_at");
    assert!(
        !pending_ids(&pool).await.contains(&id),
        "PoolsFrozen does not re-queue a row whose address is already stamped"
    );
}

#[tokio::test]
async fn check_constraint_blocks_reconciled_row_without_orderbook() {
    // The migration-0014 CHECK constraint is the schema-level pin of the
    // "reconciled ⇒ has orderbook_address" invariant. Stamping
    // `last_reconciled_at` on a row that still has a NULL address must fail
    // with a CHECK violation, so the depth fail-closed path can rely on the
    // invariant rather than re-validating it on every read.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_check_constraint";
    purge(&pool, pmp).await;
    let id = seed_market(&pool, pmp, false, None, None).await;

    let err = sqlx::query("update markets set last_reconciled_at = now() where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect_err("CHECK must block reconcile-without-address");
    let message = err.to_string();
    assert!(
        message.contains("markets_orderbook_address_set_after_reconcile"),
        "expected CHECK constraint violation, got: {message}"
    );
}
