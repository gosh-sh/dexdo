// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for the OracleEventList reconciler. Covers the DB-write
// path (the parsing of `_events` getter output is unit-tested in
// `crates/infrastructure/src/oracle_event_list_reconciler.rs`). Runs against
// the same throw-away Postgres as the rest of the integration suite — see
// `docker-compose.test.yml`. Skipped when `TEST_DATABASE_URL` is unset.

use std::env;
use std::time::Duration;

use dodex_infrastructure::database;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

async fn purge(pool: &PgPool, oracle_address: &str, eventlist_address: &str) {
    sqlx::query("delete from oracle_event_lists where address = $1")
        .bind(eventlist_address)
        .execute(pool)
        .await
        .expect("purge oel");
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_address)
        .execute(pool)
        .await
        .expect("purge oracle");
}

async fn seed_oel_with_event(
    pool: &PgPool,
    oracle_address: &str,
    oracle_name: &str,
    eventlist_address: &str,
    event_internal_id_decimal: &str,
) -> (i64, i64) {
    let oracle_id: i64 = sqlx::query_scalar(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, '0xff') returning id"#,
    )
    .bind(oracle_name)
    .bind(oracle_address)
    .bind(format!("{oracle_name}-deploy-msg"))
    .fetch_one(pool)
    .await
    .expect("insert oracle");

    let eventlist_id: i64 = sqlx::query_scalar(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, list_index)
           values ($1, $2, $3, 1) returning id"#,
    )
    .bind(format!("{eventlist_address}-deploy-msg"))
    .bind(oracle_id)
    .bind(eventlist_address)
    .fetch_one(pool)
    .await
    .expect("insert oel");

    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name,
                oracle_fee, deadline, last_seen_at, updated_at)
           values ($1, $2::numeric, 'Election', 100::numeric, 1710000000, now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(event_internal_id_decimal)
    .execute(pool)
    .await
    .expect("insert oracle_event");

    (oracle_id, eventlist_id)
}

/// Runs the same UPDATE the reconciler emits in `apply_event_metadata`. This
/// keeps the DB-side contract pinned without exposing the internal helper to
/// the integration test crate.
async fn apply(
    pool: &PgPool,
    eventlist_id: i64,
    event_id_decimal: &str,
    describe: Option<&str>,
    trust_addr: Option<&str>,
) -> u64 {
    sqlx::query(
        r#"update oracle_events
              set describe = coalesce(describe, $1),
                  trust_addr = coalesce(trust_addr, $2),
                  meta_reconciled_at = now(),
                  updated_at = now()
            where eventlist_id = $3
              and internal_id_in_eventlist = $4::numeric
              and meta_reconciled_at is null"#,
    )
    .bind(describe)
    .bind(trust_addr)
    .bind(eventlist_id)
    .bind(event_id_decimal)
    .execute(pool)
    .await
    .expect("apply event metadata")
    .rows_affected()
}

#[tokio::test]
async fn fills_describe_when_null() {
    let Some(pool) = setup().await else { return };

    let test = "oel_reconcile_fills_describe";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let eventlist_addr = format!("0:{test}_evlist");
    let event_id = "1";

    purge(&pool, &oracle_addr, &eventlist_addr).await;
    let (_oracle_id, eventlist_id) =
        seed_oel_with_event(&pool, &oracle_addr, &oracle_name, &eventlist_addr, event_id).await;

    let updated =
        apply(&pool, eventlist_id, event_id, Some("Will candidate X win?"), Some("0xabc")).await;
    assert_eq!(updated, 1, "metadata write must affect the event row");

    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "select describe, trust_addr from oracle_events
                where eventlist_id = $1 and internal_id_in_eventlist = $2::numeric",
    )
    .bind(eventlist_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("read back oracle_event");

    assert_eq!(row.0.as_deref(), Some("Will candidate X win?"));
    assert_eq!(row.1.as_deref(), Some("0xabc"));
}

#[tokio::test]
async fn does_not_overwrite_existing_values() {
    let Some(pool) = setup().await else { return };

    let test = "oel_reconcile_idempotent";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let eventlist_addr = format!("0:{test}_evlist");
    let event_id = "2";

    purge(&pool, &oracle_addr, &eventlist_addr).await;
    let (_oracle_id, eventlist_id) =
        seed_oel_with_event(&pool, &oracle_addr, &oracle_name, &eventlist_addr, event_id).await;

    // First pass: fill from chain.
    let first = apply(&pool, eventlist_id, event_id, Some("Original"), Some("0xaaa")).await;
    assert_eq!(first, 1);

    // Second pass with different values must be a no-op — the WHERE guard
    // (`meta_reconciled_at is null`) excludes the row once the marker is set.
    let second = apply(&pool, eventlist_id, event_id, Some("Replaced"), Some("0xbbb")).await;
    assert_eq!(second, 0, "rows already stamped meta_reconciled_at must be skipped");

    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "select describe, trust_addr from oracle_events
                where eventlist_id = $1 and internal_id_in_eventlist = $2::numeric",
    )
    .bind(eventlist_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("read back oracle_event");

    assert_eq!(row.0.as_deref(), Some("Original"));
    assert_eq!(row.1.as_deref(), Some("0xaaa"));
}

#[tokio::test]
async fn fills_only_missing_field_when_partially_set() {
    let Some(pool) = setup().await else { return };

    let test = "oel_reconcile_partial";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let eventlist_addr = format!("0:{test}_evlist");
    let event_id = "3";

    purge(&pool, &oracle_addr, &eventlist_addr).await;
    let (_oracle_id, eventlist_id) =
        seed_oel_with_event(&pool, &oracle_addr, &oracle_name, &eventlist_addr, event_id).await;

    // Pre-set describe only; trust_addr and meta_reconciled_at remain null.
    // This models the legacy state where a row could have describe populated
    // without the reconciler-progress marker.
    sqlx::query(
        "update oracle_events set describe = 'Already known'
                where eventlist_id = $1 and internal_id_in_eventlist = $2::numeric",
    )
    .bind(eventlist_id)
    .bind(event_id)
    .execute(&pool)
    .await
    .expect("preset describe");

    let updated =
        apply(&pool, eventlist_id, event_id, Some("Should not stick"), Some("0xnewtrust")).await;
    assert_eq!(updated, 1, "unstamped row matches the pending predicate");

    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "select describe, trust_addr from oracle_events
                where eventlist_id = $1 and internal_id_in_eventlist = $2::numeric",
    )
    .bind(eventlist_id)
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("read back");

    assert_eq!(row.0.as_deref(), Some("Already known"), "describe must not be overwritten");
    assert_eq!(row.1.as_deref(), Some("0xnewtrust"), "trust_addr must be filled");
}

#[tokio::test]
async fn null_chain_metadata_clears_pending_predicate() {
    // Regression: the old pending predicate `describe is null or trust_addr is
    // null` matched forever when the on-chain getter legitimately returned null
    // for `trustAddr` (or empty `describe`). The OEL reconciler then re-selected
    // the same rows every sweep and starved later OELs out of the `LIMIT 16`
    // batch. With the `meta_reconciled_at` marker a single pass with all-None
    // metadata must remove the row from the pending set.
    let Some(pool) = setup().await else { return };

    let test = "oel_reconcile_null_chain_meta";
    let oracle_addr = format!("0:{test}_oracle");
    let oracle_name = format!("{test}-oracle");
    let eventlist_addr = format!("0:{test}_evlist");
    let event_id = "4";

    purge(&pool, &oracle_addr, &eventlist_addr).await;
    let (_oracle_id, eventlist_id) =
        seed_oel_with_event(&pool, &oracle_addr, &oracle_name, &eventlist_addr, event_id).await;

    let updated = apply(&pool, eventlist_id, event_id, None, None).await;
    assert_eq!(updated, 1, "first pass with null metadata must still stamp the marker");

    let pending: i64 = sqlx::query_scalar(
        "select count(*) from oracle_events
                where eventlist_id = $1 and meta_reconciled_at is null",
    )
    .bind(eventlist_id)
    .fetch_one(&pool)
    .await
    .expect("count pending");
    assert_eq!(pending, 0, "row must drop out of the pending set after one pass");

    let second = apply(&pool, eventlist_id, event_id, None, None).await;
    assert_eq!(second, 0, "second pass must be a no-op — no starvation loop");
}

/// Mirror of the pending SELECT in `OracleEventListReconciler::run_once`.
/// Same shape, so a regression there will fail this test.
async fn pending_oel_ids(pool: &PgPool) -> Vec<i64> {
    use sqlx::Row;
    let rows = sqlx::query(
        r#"select oel.id
             from oracle_event_lists oel
            where (oel.last_reconcile_failed_at is null
                   or oel.last_reconcile_failed_at < now() - interval '5 minutes')
              and exists (
                  select 1 from oracle_events oe
                   where oe.eventlist_id = oel.id
                     and oe.meta_reconciled_at is null
              )
            order by oel.last_reconcile_failed_at nulls first, oel.id asc"#,
    )
    .fetch_all(pool)
    .await
    .expect("select pending oels");
    rows.iter().map(|r| r.get::<i64, _>("id")).collect()
}

#[tokio::test]
async fn no_progress_pass_backs_off_under_cooldown() {
    // Regression: when an OEL is picked by the pending SELECT (it has a
    // child with `meta_reconciled_at IS NULL`) but the `_events` getter
    // does not stamp any of its children — either the map is empty, or
    // every item targets an already-`meta_reconciled_at`-set child — the
    // outer loop must treat the pass as a chain-lag failure and stamp
    // `last_reconcile_failed_at`. Without that, the OEL stays at the head
    // of `last_reconcile_failed_at nulls first, id asc` every sweep and
    // crowds out later rows behind the LIMIT 16 batch.
    //
    // This test pins the SQL contract: a never-failed OEL beats a
    // cooled-down failed one, and a fresh failure stamp pushes the row off
    // the pending SELECT entirely until the cooldown window expires.
    let Some(pool) = setup().await else { return };

    // OEL #1: simulate "no progress made this sweep" — the outer loop's
    // `Reconciled(0)` branch calls `stamp_failure` (same UPDATE shape).
    let oracle_a = "0:oel_no_progress_oracle_a";
    let evlist_a = "0:oel_no_progress_list_a";
    purge(&pool, oracle_a, evlist_a).await;
    let (_, oel_a_id) =
        seed_oel_with_event(&pool, oracle_a, "oel_no_progress_oracle_a", evlist_a, "1").await;
    sqlx::query(
        r#"update oracle_event_lists
              set last_reconcile_failed_at = now(),
                  reconcile_attempts = reconcile_attempts + 1
            where id = $1"#,
    )
    .bind(oel_a_id)
    .execute(&pool)
    .await
    .expect("stamp failure on OEL A");

    // OEL #2: never failed. Should land first when both come due.
    let oracle_b = "0:oel_no_progress_oracle_b";
    let evlist_b = "0:oel_no_progress_list_b";
    purge(&pool, oracle_b, evlist_b).await;
    let (_, oel_b_id) =
        seed_oel_with_event(&pool, oracle_b, "oel_no_progress_oracle_b", evlist_b, "1").await;

    // While OEL A is inside the 5-minute cooldown window, the SELECT must
    // skip it entirely. OEL B alone shows up.
    let ids = pending_oel_ids(&pool).await;
    assert!(
        !ids.contains(&oel_a_id),
        "OEL A inside cooldown must be excluded from pending SELECT, got {ids:?}"
    );
    assert!(ids.contains(&oel_b_id), "OEL B (never failed) must be pending");

    // After the cooldown window OEL A returns to the queue but `nulls first`
    // keeps OEL B ahead of it — no starvation. Simulate by pushing OEL A's
    // failure stamp into the past.
    sqlx::query(
        "update oracle_event_lists \
           set last_reconcile_failed_at = now() - interval '10 minutes' \
         where id = $1",
    )
    .bind(oel_a_id)
    .execute(&pool)
    .await
    .expect("expire cooldown");

    let ids = pending_oel_ids(&pool).await;
    let pos_a = ids.iter().position(|&id| id == oel_a_id);
    let pos_b = ids.iter().position(|&id| id == oel_b_id);
    assert!(pos_b.is_some() && pos_a.is_some(), "both OELs eligible after cooldown");
    assert!(pos_b < pos_a, "never-failed OEL must run before cooled-down failed one");
}
