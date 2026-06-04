// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for `PostgresReadModelRepository::list_oracles`
// (GET /api/v1/oracles). Runs against the throwaway Postgres from
// docker-compose.test.yml; skipped when TEST_DATABASE_URL is unset.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::OraclesFilter;
use dodex_application::OraclesRequest;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const NOW: i64 = 1_710_000_000;
const FUTURE: i64 = 1_710_999_999; // > NOW
const PAST: i64 = 1_709_000_000; // < NOW

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
        .expect("connect");
    database::run_migrations(&pool).await.expect("migrations");
    Some(pool)
}

async fn purge(pool: &PgPool, oracle_addr: &str) {
    // Cascades to oracle_event_lists and oracle_events.
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_addr)
        .execute(pool)
        .await
        .unwrap();
}

/// Seed one oracle with one event list and one *available* event
/// (reconciled, future deadline, not deleted). Returns (oracle_id, eventlist_id).
async fn seed_available(
    pool: &PgPool,
    oracle_addr: &str,
    oracle_name: &str,
    eventlist_addr: &str,
    list_index: i64,
    event_internal_id_decimal: &str,
    deadline: i64,
    description: Option<&str>,
    outcomes: serde_json::Value,
) -> (i64, i64) {
    let oracle_id: i64 = sqlx::query_scalar(
        r#"insert into oracles (name, address, deploy_msg_id, pubkey)
           values ($1, $2, $3, '0xff') returning id"#,
    )
    .bind(oracle_name)
    .bind(oracle_addr)
    .bind(format!("{oracle_name}-deploy"))
    .fetch_one(pool)
    .await
    .unwrap();

    let eventlist_id: i64 = sqlx::query_scalar(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, list_index, description)
           values ($1, $2, $3, $4, $5) returning id"#,
    )
    .bind(format!("{eventlist_addr}-deploy"))
    .bind(oracle_id)
    .bind(eventlist_addr)
    .bind(list_index)
    .bind(description)
    .fetch_one(pool)
    .await
    .unwrap();

    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name, oracle_fee,
                deadline, describe, trust_addr, outcome_names_jsonb,
                meta_reconciled_at, last_seen_at, updated_at)
           values ($1, $2::numeric, 'Election', 100::numeric, $3, 'Will X win?',
                   '0xabc', $4::jsonb, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(event_internal_id_decimal)
    .bind(deadline)
    .bind(&outcomes)
    .execute(pool)
    .await
    .unwrap();

    (oracle_id, eventlist_id)
}

fn req(filter: OraclesFilter, cursor: Option<String>, limit: u16) -> OraclesRequest {
    OraclesRequest { filter, cursor, limit, now: NOW }
}

#[tokio::test]
async fn lists_available_event_with_fields() {
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_basic";
    purge(&pool, oracle).await;
    seed_available(
        &pool,
        oracle,
        "oracles-it-basic",
        "0:oracles_it_basic_list",
        0,
        "1",
        FUTURE,
        Some("Election markets."),
        serde_json::json!({ "0": "NO", "1": "YES" }),
    )
    .await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_oracles(&req(
            OraclesFilter { oracle_address: Some(oracle.into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .expect("list_oracles");

    assert_eq!(page.oracles.len(), 1);
    let o = &page.oracles[0];
    assert_eq!(o.name, "oracles-it-basic");
    assert_eq!(o.event_lists.len(), 1);
    let l = &o.event_lists[0];
    assert_eq!(l.index, 0);
    assert_eq!(l.description.as_deref(), Some("Election markets."));
    assert_eq!(l.events.len(), 1);
    let e = &l.events[0];
    assert_eq!(e.event_id, "0x0000000000000000000000000000000000000000000000000000000000000001");
    assert_eq!(e.event_name, "Election");
    assert_eq!(e.oracle_fee.asset, "SHELL");
    assert_eq!(e.oracle_fee.amount, "100");
    assert_eq!(e.deadline, FUTURE);
    assert_eq!(e.outcomes.len(), 2);
    assert_eq!(e.outcomes[0].outcome_id, 0);
    assert_eq!(e.outcomes[0].outcome_name, "NO");

    purge(&pool, oracle).await;
}

#[tokio::test]
async fn hides_unavailable_events() {
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_hidden";
    purge(&pool, oracle).await;

    // Available event so the oracle/list exist, plus deleted + past-deadline
    // siblings that must not surface.
    let (oracle_id, eventlist_id) = seed_available(
        &pool,
        oracle,
        "oracles-it-hidden",
        "0:oracles_it_hidden_list",
        0,
        "1",
        FUTURE,
        None,
        serde_json::json!({ "0": "NO" }),
    )
    .await;
    let _ = oracle_id;

    // Past-deadline.
    sqlx::query(
        r#"insert into oracle_events (eventlist_id, internal_id_in_eventlist, event_name,
               oracle_fee, deadline, meta_reconciled_at, last_seen_at, updated_at)
           values ($1, 2::numeric, 'Past', 1::numeric, $2, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(PAST)
    .execute(&pool)
    .await
    .unwrap();
    // Deleted.
    sqlx::query(
        r#"insert into oracle_events (eventlist_id, internal_id_in_eventlist, event_name,
               oracle_fee, deadline, is_deleted, meta_reconciled_at, last_seen_at, updated_at)
           values ($1, 3::numeric, 'Deleted', 1::numeric, $2, true, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(FUTURE)
    .execute(&pool)
    .await
    .unwrap();
    // Unreconciled.
    sqlx::query(
        r#"insert into oracle_events (eventlist_id, internal_id_in_eventlist, event_name,
               oracle_fee, deadline, last_seen_at, updated_at)
           values ($1, 4::numeric, 'Unreconciled', 1::numeric, $2, now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(FUTURE)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_oracles(&req(
            OraclesFilter { oracle_address: Some(oracle.into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .expect("list_oracles");

    assert_eq!(page.oracles.len(), 1);
    assert_eq!(page.oracles[0].event_lists[0].events.len(), 1, "only the available event surfaces");
    assert_eq!(page.oracles[0].event_lists[0].events[0].event_name, "Election");

    purge(&pool, oracle).await;
}

#[tokio::test]
async fn event_id_filter_narrows_to_one_event() {
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_eventid";
    purge(&pool, oracle).await;
    let (_oid, eventlist_id) = seed_available(
        &pool,
        oracle,
        "oracles-it-eventid",
        "0:oracles_it_eventid_list",
        0,
        "1",
        FUTURE,
        None,
        serde_json::json!({ "0": "NO" }),
    )
    .await;
    // Second available event in the same list.
    sqlx::query(
        r#"insert into oracle_events (eventlist_id, internal_id_in_eventlist, event_name,
               oracle_fee, deadline, outcome_names_jsonb, meta_reconciled_at, last_seen_at, updated_at)
           values ($1, 2::numeric, 'Second', 1::numeric, $2, '{}'::jsonb, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(FUTURE)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    // eventId hex for decimal 2.
    let hex2 = "0x0000000000000000000000000000000000000000000000000000000000000002";
    let page = repo
        .list_oracles(&req(
            OraclesFilter { event_id: Some(hex2.into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .expect("list_oracles");

    let events: Vec<_> = page
        .oracles
        .iter()
        .flat_map(|o| o.event_lists.iter())
        .flat_map(|l| l.events.iter())
        .filter(|e| e.event_name == "Second" || e.event_name == "Election")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_name, "Second");

    purge(&pool, oracle).await;
}

#[tokio::test]
async fn deadline_before_excludes_later_events() {
    // Two available events with different deadlines; deadlineBefore set
    // between them must surface only the earlier one and drop the later.
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_deadline_before";
    purge(&pool, oracle).await;
    let (_oid, eventlist_id) = seed_available(
        &pool,
        oracle,
        "oracles-it-deadline-before",
        "0:oracles_it_deadline_before_list",
        0,
        "1",
        NOW + 100,
        None,
        serde_json::json!({ "0": "NO" }),
    )
    .await;
    // Second available event with a much later deadline, same list.
    sqlx::query(
        r#"insert into oracle_events (eventlist_id, internal_id_in_eventlist, event_name,
               oracle_fee, deadline, outcome_names_jsonb, meta_reconciled_at, last_seen_at, updated_at)
           values ($1, 2::numeric, 'Later', 1::numeric, $2, '{}'::jsonb, now(), now(), now())"#,
    )
    .bind(eventlist_id)
    .bind(NOW + 10_000)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_oracles(&req(
            OraclesFilter {
                oracle_address: Some(oracle.into()),
                deadline_before: Some(NOW + 1_000),
                ..Default::default()
            },
            None,
            50,
        ))
        .await
        .expect("list_oracles");

    let events: Vec<_> = page
        .oracles
        .iter()
        .flat_map(|o| o.event_lists.iter())
        .flat_map(|l| l.events.iter())
        .collect();
    assert_eq!(events.len(), 1, "only the event before deadlineBefore should surface");
    assert_eq!(events[0].event_name, "Election");

    purge(&pool, oracle).await;
}

#[tokio::test]
async fn invalid_event_id_hex_is_invalid_parameter() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_oracles(&req(
            OraclesFilter { event_id: Some("0xZZZ".into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::InvalidParameter)
    ));
}

#[tokio::test]
async fn paginates_by_oracle_with_cursor() {
    let Some(pool) = setup().await else { return };
    // Three oracles with names that sort A < B < C.
    let addrs = ["0:oracles_it_pg_a", "0:oracles_it_pg_b", "0:oracles_it_pg_c"];
    let names = ["oracles-it-pg-a", "oracles-it-pg-b", "oracles-it-pg-c"];
    for a in addrs {
        purge(&pool, a).await;
    }
    for i in 0..3 {
        seed_available(
            &pool,
            addrs[i],
            names[i],
            &format!("{}_list", addrs[i]),
            0,
            "1",
            FUTURE,
            None,
            serde_json::json!({ "0": "NO" }),
        )
        .await;
    }

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Restrict to our three oracles by name prefix is not supported; instead
    // page globally with limit 2 and walk until our cursor advances. To keep
    // the test hermetic, filter the asserted set to our known names.
    let page1 = repo.list_oracles(&req(OraclesFilter::default(), None, 2)).await.expect("p1");
    assert!(page1.has_more);
    assert!(page1.next_cursor.is_some());
    let page2 = repo
        .list_oracles(&req(OraclesFilter::default(), page1.next_cursor.clone(), 2))
        .await
        .expect("p2");
    // The cursor must strictly advance: no oracle appears on both pages.
    let p1: std::collections::HashSet<_> =
        page1.oracles.iter().map(|o| o.address.clone()).collect();
    for o in &page2.oracles {
        assert!(!p1.contains(&o.address), "cursor must not re-list {}", o.address);
    }

    for a in addrs {
        purge(&pool, a).await;
    }
}

#[tokio::test]
async fn fails_closed_on_malformed_outcome_names() {
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_badjson";
    purge(&pool, oracle).await;
    let (_oid, eventlist_id) = seed_available(
        &pool,
        oracle,
        "oracles-it-badjson",
        "0:oracles_it_badjson_list",
        0,
        "1",
        FUTURE,
        None,
        serde_json::json!({ "0": "NO" }),
    )
    .await;
    // Overwrite outcomes with a JSON array (not an object) → MarketInconsistent.
    sqlx::query(
        "update oracle_events set outcome_names_jsonb = '[\"NO\"]'::jsonb where eventlist_id = $1",
    )
    .bind(eventlist_id)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_oracles(&req(
            OraclesFilter { oracle_address: Some(oracle.into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::MarketInconsistent)
    ));

    purge(&pool, oracle).await;
}

#[tokio::test]
async fn omits_oracle_with_only_unavailable_events() {
    // An oracle whose every event is unavailable (here: reconciled but past
    // deadline) must be absent from the response entirely — this is the
    // Phase-1 EXISTS gate, not Phase-2 row filtering. If the EXISTS were
    // dropped, the oracle head would still be selected and surface with an
    // empty `eventLists`, which would also corrupt `has_more`/cursor counts.
    let Some(pool) = setup().await else { return };
    let oracle = "0:oracles_it_only_unavail";
    purge(&pool, oracle).await;
    seed_available(
        &pool,
        oracle,
        "oracles-it-only-unavail",
        "0:oracles_it_only_unavail_list",
        0,
        "1",
        PAST,
        None,
        serde_json::json!({ "0": "NO" }),
    )
    .await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_oracles(&req(
            OraclesFilter { oracle_address: Some(oracle.into()), ..Default::default() },
            None,
            50,
        ))
        .await
        .expect("list_oracles");

    assert!(
        page.oracles.is_empty(),
        "oracle with only unavailable events must be omitted entirely, got {} oracle(s)",
        page.oracles.len()
    );

    purge(&pool, oracle).await;
}
