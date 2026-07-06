// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// End-to-end coverage for the `MarketStatus::Cancelled` derivation path that
// must trigger when only the on-chain `isCancelled` flag (written by the
// reconciler) is set and the cancellation event has not been observed.
// Gated on TEST_DATABASE_URL — see crates/infrastructure/tests/reprojection.rs
// for the docker-compose harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::MarketsRequest;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::MarketStatus;
use dodex_domain::TerminalKind;
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

async fn purge_market(pool: &PgPool, pmp_address: &str) {
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge market");
}

async fn insert_market(
    pool: &PgPool,
    pmp_address: &str,
    market_name: &str,
    orderbook_address: &str,
    is_cancelled: bool,
    cancelled_at: Option<i64>,
) {
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                is_cancelled, cancelled_at,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $2, $2, 3, 'USDC',
                   1::numeric, 0::numeric, $3,
                   $4, $5,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   now())"#,
    )
    .bind(pmp_address)
    .bind(market_name)
    .bind(orderbook_address)
    .bind(is_cancelled)
    .bind(cancelled_at)
    .execute(pool)
    .await
    .expect("insert market");
}

#[tokio::test]
async fn cancelled_without_reason_fails_closed_single() {
    // docs/tech-specs/read-api.md §Fail-closed validation — `cancelReason`
    // MUST distinguish PMP_REJECTED_BY_ORACLE vs EVENT_CANCELLED. The
    // reconciler stamps `is_cancelled` from `getDetails()` but does not know
    // the cause; until the corresponding cancellation event is replayed,
    // `cancel_reason` stays NULL and the row violates the invariant. The API
    // must reject such rows rather than serialize them with `cancelReason: null`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_no_reason_single";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    // Reconciler-only path: is_cancelled = true, cancel_reason = NULL.
    insert_market(&pool, &pmp, &market_name, &orderbook, true, None).await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let err = repo.list_markets(&request).await.expect_err("must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn cancelled_without_reason_fails_closed_listing() {
    // Per docs/tech-specs/read-api.md §Fail-closed validation a single
    // inconsistent row fails the whole listing, not just hides itself —
    // silently dropping rows would mask indexer bugs and break the keyset
    // cursor's monotonic contract.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_no_reason_listing";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, true, None).await;

    let request = MarketsRequest::Listing(dodex_application::MarketsListing {
        filter: dodex_application::MarketsFilter {
            statuses: vec![MarketStatus::Cancelled],
            quote_asset: Some("USDC".into()),
            oracle_name: None,
            closing_before: None,
            resolves_from: None,
        },
        sort: dodex_application::MarketsSort::CreatedAtDesc,
        cursor: None,
        limit: 100,
        now: 1_700_000_150,
    });

    let err = repo.list_markets(&request).await.expect_err("listing must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn cancelled_with_reason_is_served() {
    // Sanity check the fail-closed path does not over-fire: a fully
    // consistent CANCELLED row (cancel_reason populated by either the
    // PMPRejected or EventCancelled projector) must serialize normally.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_with_reason";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, true, Some(1_700_000_050)).await;
    sqlx::query("update markets set cancel_reason = $1 where pmp_address = $2")
        .bind("PMP_REJECTED_BY_ORACLE")
        .bind(&pmp)
        .execute(&pool)
        .await
        .expect("stamp cancel_reason");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let page = repo.list_markets(&request).await.expect("consistent row serializes");
    let market = page.markets.first().expect("market row returned");

    assert_eq!(market.status, MarketStatus::Cancelled);
    let terminal = market.terminal.as_ref().expect("terminal populated");
    assert!(matches!(terminal.kind, TerminalKind::Cancelled));
    assert_eq!(terminal.at, 1_700_000_050);
    assert!(terminal.cancel_reason.is_some());
    // Snapshot: literals catch silent drift in the domain constants.
    assert_eq!(market.maker_commission, "-0.0003375");
    assert_eq!(market.taker_commission, "0.0004500");
}

#[tokio::test]
async fn reconciler_writes_cancelled_at_when_flag_flips() {
    let Some(pool) = setup().await else { return };

    let test = "markets_cancel_at_stamp";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;

    // Seed a non-cancelled market.
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;

    // Apply the same SET clause the reconciler emits (mirror of
    // `write_market_state` in reconciler.rs).
    sqlx::query(
        r#"update markets
              set is_cancelled = true,
                  cancelled_at = case
                      when true and cancelled_at is null then extract(epoch from now())::bigint
                      else cancelled_at
                  end
            where pmp_address = $1"#,
    )
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("simulate reconciler write");

    let cancelled_at: Option<i64> =
        sqlx::query_scalar("select cancelled_at from markets where pmp_address = $1")
            .bind(&pmp)
            .fetch_one(&pool)
            .await
            .expect("read cancelled_at");
    assert!(
        cancelled_at.is_some(),
        "reconciler must stamp cancelled_at when flipping is_cancelled and cancelled_at was null"
    );

    // A second pass with cancelled_at already populated must be a no-op
    // (idempotent — does not move the timestamp forward).
    let frozen = cancelled_at.unwrap();
    sqlx::query(
        r#"update markets
              set is_cancelled = true,
                  cancelled_at = case
                      when true and cancelled_at is null then extract(epoch from now())::bigint
                      else cancelled_at
                  end
            where pmp_address = $1"#,
    )
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("second reconciler write");

    let cancelled_at_after: Option<i64> =
        sqlx::query_scalar("select cancelled_at from markets where pmp_address = $1")
            .bind(&pmp)
            .fetch_one(&pool)
            .await
            .expect("read cancelled_at again");
    assert_eq!(
        cancelled_at_after,
        Some(frozen),
        "reconciler must not overwrite an existing cancelled_at"
    );
}

#[tokio::test]
async fn pending_status_when_reconciled_without_timings() {
    // Regression for the PENDING-not-modelled bug: the reconciler used to
    // unconditionally copy `stakeStart..resultEnd` from `getDetails()` even
    // on pre-TimingsSet PMPs (where the getter returns contract-default
    // zeros), making `derive_status` flip to AWAITING_FREEZE. After the fix
    // those columns stay NULL until `apply_timings_set` fires, and the row
    // surfaces with `status=PENDING` and `timings=null` per the spec
    // (docs/tech-specs/read-api.md §Status derivation, §Fail-closed validation).
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_pending_no_timings";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");

    purge_market(&pool, &pmp).await;
    // Mirror what reconciler writes when no TimingsSet has been observed:
    // `last_reconciled_at` populated, `orderbook_address` stamped from the
    // deterministic getter (the schema CHECK requires it whenever
    // `last_reconciled_at` is set), but `stake_*`/`result_*` left NULL.
    let orderbook = format!("0:{test}_book");
    sqlx::query(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                last_reconciled_at)
           values ($1, $2, $2, 3, 'USDC', 1::numeric, 0::numeric, $3, now())"#,
    )
    .bind(&pmp)
    .bind(&market_name)
    .bind(&orderbook)
    .execute(&pool)
    .await
    .expect("insert pre-timings market");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let page = repo.list_markets(&request).await.expect("list markets");
    let market = page.markets.first().expect("market row returned");

    assert_eq!(market.status, MarketStatus::Pending);
    assert!(
        market.timings.is_none(),
        "timings must be null for PENDING per docs/tech-specs/read-api.md §Status derivation"
    );
    assert!(market.terminal.is_none(), "PENDING is not a terminal status");
}

#[tokio::test]
async fn resolved_without_freeze_fails_closed() {
    // docs/tech-specs/read-api.md §Fail-closed validation — RESOLVED implies
    // frozenAt != null ("resolution always follows freeze, see PMP.sol:1005").
    // If the indexer observed `PMP.Resolved` before `PMP.PoolsFrozen`
    // (out-of-order replay), the row is transiently inconsistent. The API
    // must fail the request closed instead of serializing with `frozenAt: null`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_resolved_no_freeze";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    // resolved_at set, frozen_at deliberately left NULL.
    sqlx::query(
        "update markets set resolved_at = $1, resolved_outcome_id = 1 where pmp_address = $2",
    )
    .bind(1_700_000_350_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp resolved without freeze");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_400 };
    let err = repo.list_markets(&request).await.expect_err("must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn resolved_with_freeze_is_served() {
    // Sanity check: a fully consistent RESOLVED row (frozen_at populated)
    // must serialize normally.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_resolved_with_freeze";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    sqlx::query(
        "update markets
            set frozen_at = $1,
                resolved_at = $2,
                resolved_outcome_id = 1
          where pmp_address = $3",
    )
    .bind(1_700_000_250_i64)
    .bind(1_700_000_350_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp resolved with freeze");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_400 };
    let page = repo.list_markets(&request).await.expect("consistent row serializes");
    let market = page.markets.first().expect("market row returned");

    assert_eq!(market.status, MarketStatus::Resolved);
    let timings = market.timings.as_ref().expect("timings populated for non-PENDING");
    assert_eq!(timings.frozen_at, Some(1_700_000_250));
}

#[tokio::test]
async fn resolved_without_outcome_id_fails_closed() {
    // api-spec.md:391: `resolvedOutcomeId` MUST be set when terminal.kind is
    // RESOLVED — "without it the client cannot know which side won." The
    // row-level check in `assemble_market` (resolved_at + frozen_at) was not
    // sufficient: a row with both set but `resolved_outcome_id = NULL` still
    // came through. The validator now catches this on the built DTO.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_resolved_no_outcome";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    // frozen_at and resolved_at set, but resolved_outcome_id deliberately NULL.
    sqlx::query(
        "update markets
            set frozen_at = $1, resolved_at = $2, resolved_outcome_id = null
          where pmp_address = $3",
    )
    .bind(1_700_000_250_i64)
    .bind(1_700_000_350_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp resolved without outcome_id");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_400 };
    let err = repo.list_markets(&request).await.expect_err("must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn cancelled_with_garbage_reason_fails_closed() {
    // `build_terminal` rewrites the raw string via `CancelReason::parse`. If the
    // column holds a string outside {PMP_REJECTED_BY_ORACLE, EVENT_CANCELLED} —
    // historical data, projector bug, manual SQL — parse returns None and
    // the API would surface `cancelReason: null`, violating
    // docs/tech-specs/read-api.md §Fail-closed validation. The row-level
    // `cancel_reason.is_none()` check missed this because the column is *not*
    // null. Validating the parsed `Terminal.cancel_reason` catches both shapes.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_garbage_reason";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, true, Some(1_700_000_050)).await;
    sqlx::query("update markets set cancel_reason = $1 where pmp_address = $2")
        .bind("MYSTERY_REASON")
        .bind(&pmp)
        .execute(&pool)
        .await
        .expect("stamp garbage cancel_reason");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let err = repo.list_markets(&request).await.expect_err("must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn unknown_market_address_returns_invalid_market_or_symbol() {
    // tech-specs/read-api.md error mapping: a single-market lookup for
    // an unknown / not-yet-reconciled `predictionMarketAddress` must surface as
    // `InvalidMarketOrSymbol` (→ HTTP 404), not an empty success page —
    // mirrors the /api/v1/prediction/depth contract.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "unknown";
    purge_market(&pool, pmp).await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.into()), now: 1_700_000_150 };
    let err = repo.list_markets(&request).await.expect_err("unknown market must error");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);
}

#[tokio::test]
async fn blank_orderbook_address_fails_closed_in_markets() {
    // Migration-0014 CHECK forbids NULL `orderbook_address` on reconciled
    // rows but a whitespace-only string slips past it. The depth path
    // already treats this as `MarketInconsistent`; `/api/v1/prediction/markets` must
    // fail closed too — silently serializing `predictionOrderBookAddress: null`
    // would break the public contract that visible markets always carry
    // the address.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_blank_orderbook_address";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = "   "; // blank — CHECK allows, business contract does not.

    purge_market(&pool, &pmp).await;
    // The blank-orderbook value is shared with
    // `depth.rs::blank_orderbook_address_fails_closed` (both tests pin the
    // same CHECK-allows-whitespace gap). The `markets_orderbook_address_unique`
    // partial index collides whichever test's row was left in the DB by the prior
    // run. Purging by orderbook_address here scrubs any sibling residue.
    sqlx::query("delete from markets where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge blank-orderbook residue");
    insert_market(&pool, &pmp, &market_name, orderbook, false, None).await;

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let err = repo.list_markets(&request).await.expect_err("blank orderbook must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn negative_resolved_outcome_id_fails_closed() {
    let Some(pool) = setup().await else { return };
    // Seed a market with last_reconciled_at + resolved_at + INVALID
    // resolved_outcome_id (negative). No DB CHECK enforces signedness.
    // Insert via raw SQL; markets table accepts the value, build_terminal's
    // try_into guard MUST surface it as MarketInconsistent.
    let pmp = "0:negative-resolved-outcome";
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, market_id, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at, frozen_at,
              resolved_at, resolved_outcome_id)
           values ($1, 'neg-outcome', 'neg-outcome', 1, 'NACKL', 1::numeric, 1::numeric,
                   '0:neg-ob', 2, now(),
                   extract(epoch from now())::bigint,
                   extract(epoch from now())::bigint,
                   -1)"#,
    )
    .bind(pmp)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
        as i64;
    let err = repo
        .list_markets(&dodex_application::MarketsRequest::One {
            market_address: MarketAddress(pmp.to_string()),
            now,
        })
        .await
        .unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolved_outcome_id_set_to_one_succeeds() {
    // Positive control for negative_resolved_outcome_id_fails_closed: confirms
    // the negative test fails because of the negative value, not because the
    // scaffolding is broken. Same row shape, only difference is resolved_outcome_id = 1.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_pos_resolved_outcome_id";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    sqlx::query(
        "update markets
            set frozen_at = $1, resolved_at = $2, resolved_outcome_id = 1
          where pmp_address = $3",
    )
    .bind(1_700_000_250_i64)
    .bind(1_700_000_350_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp fully consistent resolved row with outcome_id=1");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_400 };
    let page =
        repo.list_markets(&request).await.expect("positive resolved_outcome_id must succeed");
    let market = page.markets.first().expect("market row returned");
    assert_eq!(market.status, MarketStatus::Resolved);
    let terminal = market.terminal.as_ref().expect("terminal populated");
    assert_eq!(terminal.resolved_outcome_id, Some(1));
}

#[tokio::test]
async fn resolved_with_all_fields_set_succeeds() {
    // docs/tech-specs/read-api.md §Fail-closed validation — the build_terminal
    // RESOLVED arm has a guard for `resolved_at IS NULL`, but `derive_status`
    // derives the RESOLVED status from `resolved_at.is_some()`, so the only
    // reachable path through normal DB state has `resolved_at` populated.
    // This test verifies the happy path: with all fields set, the RESOLVED
    // arm renders normally. The defensive NULL guard remains in build_terminal
    // for future-proofing but cannot be triggered through derive_status today.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_resolved_ok_both_set";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    sqlx::query(
        "update markets
            set frozen_at = $1, resolved_at = $2, resolved_outcome_id = 1
          where pmp_address = $3",
    )
    .bind(1_700_000_250_i64)
    .bind(1_700_000_350_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp fully consistent resolved row");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_400 };
    let page = repo.list_markets(&request).await.expect("consistent row must succeed");
    let market = page.markets.first().expect("market row returned");
    assert_eq!(market.status, MarketStatus::Resolved);
    let terminal = market.terminal.as_ref().expect("terminal populated");
    assert_eq!(terminal.at, 1_700_000_350);
}

#[tokio::test]
async fn cancelled_without_cancelled_at_fails_closed() {
    // docs/tech-specs/read-api.md §Fail-closed validation — CANCELLED status
    // requires cancelled_at to be set. Substituting the request time would make
    // the response 'at' field wander between polls; fail closed instead.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancelled_no_cancelled_at";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    // cancelled_at = None so the column stays NULL.
    insert_market(&pool, &pmp, &market_name, &orderbook, true, None).await;
    sqlx::query("update markets set cancel_reason = $1 where pmp_address = $2")
        .bind("PMP_REJECTED_BY_ORACLE")
        .bind(&pmp)
        .execute(&pool)
        .await
        .expect("stamp cancel_reason");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let err = repo.list_markets(&request).await.expect_err("must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn expired_without_result_end_fails_closed() {
    // docs/tech-specs/read-api.md §Fail-closed validation — EXPIRED status
    // requires result_end to be set on the row. When result_end IS NULL,
    // derive_status still returns EXPIRED (it computes a fallback
    // `result_end = result_start` in Rust), but `build_terminal` reads
    // `row.result_end` directly and sees None — must return MarketInconsistent
    // rather than substituting the current request time.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_expired_no_result_end";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    // Set frozen_at so the post-freeze path is entered, and set stake/result
    // timings with result_end=NULL. With result_start=1_700_000_300 and
    // result_end NULL, derive_status falls back to result_end=result_start and
    // returns EXPIRED when now >= 1_700_000_300. build_terminal then sees
    // row.result_end=None and must fail closed.
    sqlx::query(
        "update markets
            set frozen_at = $1, stake_start = $2, stake_end = $3,
                result_start = $4, result_end = null
          where pmp_address = $5",
    )
    .bind(1_700_000_250_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(&pmp)
    .execute(&pool)
    .await
    .expect("stamp frozen with null result_end");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_500 };
    let err = repo.list_markets(&request).await.expect_err("null result_end must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn negative_price_precision_fails_closed() {
    // market_outcomes.price_precision is `integer` with no CHECK constraint
    // enforcing non-negative. A negative value from a projector bug or manual
    // SQL must fail closed via the try_into::<u8>() guard in fetch_outcomes.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_neg_price_precision";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, false, None).await;
    let market_id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(&pmp)
        .fetch_one(&pool)
        .await
        .expect("market id");
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, 0, 'NO', $3, -1, 2, '0.001', '0.01', '1')"#,
    )
    .bind(market_id)
    .bind(&pmp)
    .bind(format!("{test}-NO"))
    .execute(&pool)
    .await
    .expect("insert outcome with negative price_precision");

    let request =
        MarketsRequest::One { market_address: MarketAddress(pmp.clone()), now: 1_700_000_150 };
    let err =
        repo.list_markets(&request).await.expect_err("negative price_precision must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}
