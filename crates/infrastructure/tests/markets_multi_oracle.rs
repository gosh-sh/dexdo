// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Regression coverage for the multi-oracle PMP listing bug. A PMP can be
// confirmed against multiple `OracleEventList` contracts
// (`PrivateNote.PMPDeployed.oracleEventLists: address[]`), so a single
// market can have N rows in `oracle_events` keyed by the same
// `confirmed_pmp_address`. The old `LEFT JOIN oracle_events ON
// confirmed_pmp_address = m.pmp_address` produced N duplicate market rows.
// `fetch_oracle_events` now collapses those into one market entry with an
// `event.oracles[]` array and validates the hash-derived invariant that
// `event_name`/`description` agree across rows.
//
// Gated on TEST_DATABASE_URL — see crates/infrastructure/tests/reprojection.rs.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
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

async fn purge_market_chain(pool: &PgPool, pmp_address: &str) {
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge market");
    sqlx::query("delete from oracle_events where confirmed_pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge oracle_events");
}

async fn purge_oracle(pool: &PgPool, oracle_address: &str) {
    sqlx::query("delete from oracles where address = $1")
        .bind(oracle_address)
        .execute(pool)
        .await
        .expect("purge oracle");
}

async fn insert_market(pool: &PgPool, pmp_address: &str, orderbook: &str) -> i64 {
    sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())
           returning id"#,
    )
    .bind(pmp_address)
    .bind(orderbook)
    .fetch_one(pool)
    .await
    .expect("insert markets")
}

async fn insert_market_outcome(pool: &PgPool, market_id: i64, pmp_address: &str, symbol: &str) {
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, 1, 'YES', $3,
                   2, 2, '0.01', '0.01',
                   '1.00')"#,
    )
    .bind(market_id)
    .bind(pmp_address)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

struct OracleSeed<'a> {
    oracle_name: &'a str,
    oracle_address: &'a str,
    eventlist_address: &'a str,
    event_name: &'a str,
    description: &'a str,
    oracle_fee: &'a str,
    /// Used as `internal_id_in_eventlist` and ordering tiebreaker.
    internal_id: i64,
    confirmed_at_offset: i64,
}

async fn seed_oracle_confirmation(pool: &PgPool, pmp_address: &str, seed: OracleSeed<'_>) {
    let oracle_id: i64 = sqlx::query_scalar(
        r#"insert into oracles (name, address, deploy_msg_id)
           values ($1, $2, $3)
           returning id"#,
    )
    .bind(seed.oracle_name)
    .bind(seed.oracle_address)
    .bind(format!("{}-deploy", seed.oracle_name))
    .fetch_one(pool)
    .await
    .expect("insert oracles");
    let eventlist_id: i64 = sqlx::query_scalar(
        r#"insert into oracle_event_lists (msg_id, oracle_id, address, description)
           values ($1, $2, $3, '')
           returning id"#,
    )
    .bind(format!("{}-list-msg", seed.eventlist_address))
    .bind(oracle_id)
    .bind(seed.eventlist_address)
    .fetch_one(pool)
    .await
    .expect("insert oracle_event_lists");
    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name,
                oracle_fee, deadline, describe, confirmed_pmp_address,
                confirmed_at)
           values ($1, $2::numeric, $3, $4::numeric, 1700000400, $5, $6,
                   now() + ($7 || ' seconds')::interval)"#,
    )
    .bind(eventlist_id)
    .bind(seed.internal_id)
    .bind(seed.event_name)
    .bind(seed.oracle_fee)
    .bind(seed.description)
    .bind(pmp_address)
    .bind(seed.confirmed_at_offset.to_string())
    .execute(pool)
    .await
    .expect("insert oracle_events");
}

#[tokio::test]
async fn multi_oracle_market_returns_single_row_with_array() {
    // Regression: the old LEFT JOIN duplicated this market into N rows.
    // After the fix the listing emits one entry whose `event.oracles[]`
    // surfaces every confirming oracle deterministically (ordered by
    // `confirmed_at, id`).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:multi_oracle_listing_pmp";
    let orderbook = "0:multi_oracle_listing_book";
    let symbol = "MULTI_ORACLE_LISTING_YES";
    purge_market_chain(&pool, pmp).await;
    purge_oracle(&pool, "0:multi_oracle_listing_oracle_a").await;
    purge_oracle(&pool, "0:multi_oracle_listing_oracle_b").await;

    let market_id = insert_market(&pool, pmp, orderbook).await;
    insert_market_outcome(&pool, market_id, pmp, symbol).await;

    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_listing_oracle_a",
            oracle_address: "0:multi_oracle_listing_oracle_a",
            eventlist_address: "0:multi_oracle_listing_list_a",
            event_name: "shared event name",
            description: "shared description",
            oracle_fee: "100",
            internal_id: 1,
            confirmed_at_offset: 1,
        },
    )
    .await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_listing_oracle_b",
            oracle_address: "0:multi_oracle_listing_oracle_b",
            eventlist_address: "0:multi_oracle_listing_list_b",
            event_name: "shared event name",
            description: "shared description",
            oracle_fee: "200",
            internal_id: 2,
            confirmed_at_offset: 2,
        },
    )
    .await;

    // Scope the listing to our market only (tests share the test DB; an
    // inconsistent multi-oracle row left by sibling tests would otherwise
    // surface as `MarketInconsistent` and fail this test). The
    // `oracleName=oracle_a` filter is also the regression target for the
    // duplication bug: pre-fix, this listing returned the market twice.
    let request = MarketsRequest::Listing(MarketsListing {
        filter: MarketsFilter {
            statuses: vec![],
            quote_asset: Some("USDC".into()),
            oracle_name: Some("multi_oracle_listing_oracle_a".into()),
            closing_before: None,
            resolves_from: None,
        },
        sort: MarketsSort::CreatedAtDesc,
        cursor: None,
        limit: 100,
        now: 1_700_000_150,
    });
    let page = repo.list_markets(&request).await.expect("listing must succeed");

    let our_markets: Vec<_> = page.markets.iter().filter(|m| m.market_address.0 == pmp).collect();
    assert_eq!(
        our_markets.len(),
        1,
        "multi-oracle market must appear exactly once in the listing, got {}",
        our_markets.len()
    );
    let m = our_markets[0];
    assert_eq!(m.event.event_name.as_deref(), Some("shared event name"));
    assert_eq!(m.event.description.as_deref(), Some("shared description"));
    assert_eq!(m.event.oracles.len(), 2, "both oracle confirmations must surface in event.oracles");
    let names: Vec<&str> = m.event.oracles.iter().filter_map(|o| o.name.as_deref()).collect();
    assert!(names.contains(&"multi_oracle_listing_oracle_a"));
    assert!(names.contains(&"multi_oracle_listing_oracle_b"));
    let fees: Vec<&str> = m.event.oracles.iter().filter_map(|o| o.fee.as_deref()).collect();
    assert!(fees.contains(&"100"), "oracle A fee preserved");
    assert!(fees.contains(&"200"), "oracle B fee preserved");
}

#[tokio::test]
async fn multi_oracle_oracle_name_filter_uses_exists() {
    // The `?oracleName=X` filter must match the market whenever X appears in
    // any of its confirmations — the EXISTS subquery in build_listing_query
    // is the regression target.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:multi_oracle_filter_pmp";
    let orderbook = "0:multi_oracle_filter_book";
    let symbol = "MULTI_ORACLE_FILTER_YES";
    purge_market_chain(&pool, pmp).await;
    purge_oracle(&pool, "0:multi_oracle_filter_oracle_a").await;
    purge_oracle(&pool, "0:multi_oracle_filter_oracle_b").await;

    let market_id = insert_market(&pool, pmp, orderbook).await;
    insert_market_outcome(&pool, market_id, pmp, symbol).await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_filter_oracle_a",
            oracle_address: "0:multi_oracle_filter_oracle_a",
            eventlist_address: "0:multi_oracle_filter_list_a",
            event_name: "name",
            description: "desc",
            oracle_fee: "100",
            internal_id: 1,
            confirmed_at_offset: 1,
        },
    )
    .await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_filter_oracle_b",
            oracle_address: "0:multi_oracle_filter_oracle_b",
            eventlist_address: "0:multi_oracle_filter_list_b",
            event_name: "name",
            description: "desc",
            oracle_fee: "200",
            internal_id: 2,
            confirmed_at_offset: 2,
        },
    )
    .await;

    // Filter by the SECOND oracle's name. Before the fix this could miss the
    // market entirely (filtering against the joined row picked from the
    // first match by Postgres). EXISTS makes the match symmetric.
    let request = MarketsRequest::Listing(MarketsListing {
        filter: MarketsFilter {
            statuses: vec![],
            quote_asset: Some("USDC".into()),
            oracle_name: Some("multi_oracle_filter_oracle_b".into()),
            closing_before: None,
            resolves_from: None,
        },
        sort: MarketsSort::CreatedAtDesc,
        cursor: None,
        limit: 100,
        now: 1_700_000_150,
    });
    let page = repo.list_markets(&request).await.expect("listing must succeed");

    let our: Vec<_> = page.markets.iter().filter(|m| m.market_address.0 == pmp).collect();
    assert_eq!(our.len(), 1, "filter must match the multi-oracle market exactly once");
    assert_eq!(our[0].event.oracles.len(), 2, "both oracles must still surface in the response");
}

#[tokio::test]
async fn mismatched_event_name_fails_closed() {
    // event_id = hash(eventName, description, deadline, outcomeNames), so
    // every confirmation row for the same PMP must agree on event_name. A
    // divergence is an indexer/contract corruption and must surface as
    // MarketInconsistent (HTTP 503) instead of an arbitrary pick.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:multi_oracle_mismatch_name_pmp";
    let orderbook = "0:multi_oracle_mismatch_name_book";
    let symbol = "MULTI_ORACLE_MISMATCH_NAME_YES";
    purge_market_chain(&pool, pmp).await;
    purge_oracle(&pool, "0:multi_oracle_mismatch_name_oracle_a").await;
    purge_oracle(&pool, "0:multi_oracle_mismatch_name_oracle_b").await;

    let market_id = insert_market(&pool, pmp, orderbook).await;
    insert_market_outcome(&pool, market_id, pmp, symbol).await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_mismatch_name_oracle_a",
            oracle_address: "0:multi_oracle_mismatch_name_oracle_a",
            eventlist_address: "0:multi_oracle_mismatch_name_list_a",
            event_name: "name A",
            description: "shared desc",
            oracle_fee: "100",
            internal_id: 1,
            confirmed_at_offset: 1,
        },
    )
    .await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_mismatch_name_oracle_b",
            oracle_address: "0:multi_oracle_mismatch_name_oracle_b",
            eventlist_address: "0:multi_oracle_mismatch_name_list_b",
            event_name: "name B",
            description: "shared desc",
            oracle_fee: "200",
            internal_id: 2,
            confirmed_at_offset: 2,
        },
    )
    .await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.into()), now: 1_700_000_150 };
    let err =
        repo.list_markets(&request).await.expect_err("mismatched event_name must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn mismatched_description_fails_closed() {
    // Same hash invariant, descriptor-side branch of unify_optional.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:multi_oracle_mismatch_desc_pmp";
    let orderbook = "0:multi_oracle_mismatch_desc_book";
    let symbol = "MULTI_ORACLE_MISMATCH_DESC_YES";
    purge_market_chain(&pool, pmp).await;
    purge_oracle(&pool, "0:multi_oracle_mismatch_desc_oracle_a").await;
    purge_oracle(&pool, "0:multi_oracle_mismatch_desc_oracle_b").await;

    let market_id = insert_market(&pool, pmp, orderbook).await;
    insert_market_outcome(&pool, market_id, pmp, symbol).await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_mismatch_desc_oracle_a",
            oracle_address: "0:multi_oracle_mismatch_desc_oracle_a",
            eventlist_address: "0:multi_oracle_mismatch_desc_list_a",
            event_name: "name",
            description: "desc A",
            oracle_fee: "100",
            internal_id: 1,
            confirmed_at_offset: 1,
        },
    )
    .await;
    seed_oracle_confirmation(
        &pool,
        pmp,
        OracleSeed {
            oracle_name: "multi_oracle_mismatch_desc_oracle_b",
            oracle_address: "0:multi_oracle_mismatch_desc_oracle_b",
            eventlist_address: "0:multi_oracle_mismatch_desc_list_b",
            event_name: "name",
            description: "desc B",
            oracle_fee: "200",
            internal_id: 2,
            confirmed_at_offset: 2,
        },
    )
    .await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.into()), now: 1_700_000_150 };
    let err =
        repo.list_markets(&request).await.expect_err("mismatched description must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn market_without_oracle_confirmation_returns_empty_array() {
    // A reconciled market with no confirmed `oracle_events` rows must still
    // serialize — `event_name`/`description` null, `oracles: []` — rather
    // than fail the listing. This mirrors the pre-fix LEFT-JOIN behaviour
    // for a market whose oracle confirmations have not landed yet.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:multi_oracle_no_confirmations_pmp";
    let orderbook = "0:multi_oracle_no_confirmations_book";
    let symbol = "MULTI_ORACLE_NO_CONFIRMATIONS_YES";
    purge_market_chain(&pool, pmp).await;
    let market_id = insert_market(&pool, pmp, orderbook).await;
    insert_market_outcome(&pool, market_id, pmp, symbol).await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.into()), now: 1_700_000_150 };
    let page = repo.list_markets(&request).await.expect("listing must succeed");
    let m = page.markets.first().expect("single market");

    assert!(m.event.event_name.is_none());
    assert!(m.event.description.is_none());
    assert!(m.event.oracles.is_empty(), "no confirmations means empty oracles array");
}
