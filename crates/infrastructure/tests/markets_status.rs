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
    // tech-spec.md:103 — `cancelReason` MUST distinguish PMP_CANCELLED vs
    // EVENT_CANCELLED. The reconciler stamps `is_cancelled` from
    // `getDetails()` but does not know the cause; until the corresponding
    // cancellation event is replayed, `cancel_reason` stays NULL and the
    // row violates the invariant. Per tech-spec.md:113 the API must reject
    // such rows rather than serialize them with `cancelReason: null`.
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
    // Per docs/tech-spec.md:113 a single inconsistent row fails the whole
    // listing, not just hides itself — silently dropping rows would mask
    // indexer bugs and break the keyset cursor's monotonic contract.
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
    // PMPCancelled or EventCancelled projector) must serialize normally.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let test = "markets_cancel_with_reason";
    let pmp = format!("0:{test}_pmp");
    let market_name = format!("{test}-market");
    let orderbook = format!("0:{test}_ob");

    purge_market(&pool, &pmp).await;
    insert_market(&pool, &pmp, &market_name, &orderbook, true, Some(1_700_000_050)).await;
    sqlx::query("update markets set cancel_reason = $1 where pmp_address = $2")
        .bind("PMP_CANCELLED")
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
    // (tech-spec.md:73 and invariant #3 at tech-spec.md:109).
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
        "timings must be null for PENDING per api-spec.md:328 and invariant #3"
    );
    assert!(market.terminal.is_none(), "PENDING is not a terminal status");
}

#[tokio::test]
async fn resolved_without_freeze_fails_closed() {
    // tech-spec.md:110 invariant #4 — RESOLVED implies frozenAt != null
    // ("resolution always follows freeze, see PMP.sol:1005"). If the
    // indexer observed `PMP.Resolved` before `PMP.PoolsFrozen` (out-of-
    // order replay), the row is transiently inconsistent. Per
    // tech-spec.md:113 we must fail the request closed instead of
    // serializing it with `frozenAt: null`.
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
    // `build_terminal` runs
    // `row.cancel_reason.as_deref().and_then(CancelReason::parse)`. If the
    // column holds a string outside {PMP_CANCELLED, EVENT_CANCELLED} —
    // historical data, projector bug, manual SQL — parse returns None and
    // the API would surface `cancelReason: null`, violating
    // tech-spec.md:103. The row-level `cancel_reason.is_none()` check missed
    // this because the column is *not* null. Validating the parsed
    // `Terminal.cancel_reason` catches both shapes.
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
    // an unknown / not-yet-reconciled `marketAddress` must surface as
    // `InvalidMarketOrSymbol` (→ HTTP 404), not an empty success page —
    // mirrors the /api/v1/depth contract.
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
    // already treats this as `MarketInconsistent`; `/api/v1/markets` must
    // fail closed too — silently serializing `orderBookAddress: null`
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
