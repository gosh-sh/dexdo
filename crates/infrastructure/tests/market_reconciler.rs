// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the MarketReconciler's observed-only contract on
// `orderbook_address` (tech-spec.md invariant #5). The full reconciler path
// requires graphql/decoder plumbing; here we exercise just the SQL the
// reconciler emits — the SELECT predicate that decides which rows enter the
// queue, and the UPDATE that decides whether to stamp the column. Mirrors
// the same "test the SQL contract" approach as
// `crates/infrastructure/tests/oel_reconciler.rs`.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use sqlx::Row;

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
           where (last_reconciled_at is null
                  or (frozen_at is not null and orderbook_address is null))
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
                  orderbook_address = case
                      when frozen_at is not null then $1
                      else orderbook_address
                  end,
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
async fn pre_freeze_reconcile_does_not_stamp_orderbook() {
    // tech-spec.md invariant #5: `orderBookAddress` MUST be null until the
    // OrderBook is observed on-chain. PoolsFrozen is that signal (see
    // dex-events-routing.md:77 — "after deploy OrderBook"). A reconcile pass
    // on a row without `frozen_at` must leave the column null even though
    // `PMP.getOrderBookAddress()` would happily return the precomputed value.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_pre_freeze";
    purge(&pool, pmp).await;
    let id = seed_market(&pool, pmp, false, None, None).await;

    reconcile_update(&pool, id, "0:deterministic_precomputed").await;

    assert_eq!(
        read_orderbook_address(&pool, id).await,
        None,
        "reconciler must not stamp orderbook_address before PoolsFrozen"
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
        "once frozen_at is set the reconciler must stamp orderbook_address"
    );
}

#[tokio::test]
async fn already_reconciled_row_is_requeued_when_freeze_lands() {
    // The regression this pins: a market is reconciled before PoolsFrozen
    // (so `last_reconciled_at` is set and `orderbook_address` is null).
    // Then PoolsFrozen lands and stamps `frozen_at`. The widened SELECT
    // predicate must re-queue this row so the next sweep can stamp
    // `orderbook_address`. Without the widening the row would stay in the
    // "already reconciled" pool forever and `orderBookAddress` would never
    // surface on the API.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_requeue";
    purge(&pool, pmp).await;
    // First reconcile pass happened, but pre-freeze — orderbook null.
    let id = seed_market(&pool, pmp, true, None, None).await;

    // No freeze yet → row stays out of the queue.
    assert!(
        !pending_ids(&pool).await.contains(&id),
        "fully-reconciled row without freeze must not be in the queue"
    );

    // PoolsFrozen lands.
    sqlx::query("update markets set frozen_at = $1 where id = $2")
        .bind(1_700_000_250_i64)
        .bind(id)
        .execute(&pool)
        .await
        .expect("set frozen_at");

    assert!(
        pending_ids(&pool).await.contains(&id),
        "row with frozen_at set and orderbook_address null must be re-queued"
    );

    // Second pass stamps the address.
    reconcile_update(&pool, id, "0:deployed_orderbook").await;
    assert_eq!(read_orderbook_address(&pool, id).await.as_deref(), Some("0:deployed_orderbook"));

    // After the stamp the row leaves the queue.
    assert!(
        !pending_ids(&pool).await.contains(&id),
        "row with orderbook_address stamped must drop out of the queue"
    );
}

#[tokio::test]
async fn migration_0013_backfills_legacy_pre_freeze_orderbook() {
    // Migration 0013 backfills NULL into `orderbook_address` for rows that
    // were stamped under the old behaviour (pre-freeze, precomputed
    // address). The migration runs once at boot via `database::run_migrations`,
    // so by the time `setup()` returns the column is already cleared on any
    // legacy state. To exercise the backfill we re-introduce the legacy
    // shape and run the same UPDATE the migration ships.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_backfill";
    purge(&pool, pmp).await;
    let id = seed_market(&pool, pmp, true, None, Some("0:legacy_precomputed")).await;

    sqlx::query(
        "update markets
            set orderbook_address = null
          where frozen_at is null
            and orderbook_address is not null",
    )
    .execute(&pool)
    .await
    .expect("apply migration 0013 backfill");

    assert_eq!(
        read_orderbook_address(&pool, id).await,
        None,
        "migration 0013 must clear pre-freeze orderbook_address values"
    );
}

#[tokio::test]
async fn migration_0013_leaves_post_freeze_orderbook_alone() {
    // The "once non-null it is stable" half of invariant #5: post-freeze
    // rows must not be touched by the backfill.
    let Some(pool) = setup().await else { return };

    let pmp = "0:reconciler_backfill_keep";
    purge(&pool, pmp).await;
    let id = seed_market(
        &pool,
        pmp,
        true,
        Some(1_700_000_250),
        Some("0:legitimately_observed"),
    )
    .await;

    sqlx::query(
        "update markets
            set orderbook_address = null
          where frozen_at is null
            and orderbook_address is not null",
    )
    .execute(&pool)
    .await
    .expect("apply migration 0013 backfill");

    assert_eq!(
        read_orderbook_address(&pool, id).await.as_deref(),
        Some("0:legitimately_observed"),
        "migration must not clear post-freeze (observed) addresses"
    );
}
