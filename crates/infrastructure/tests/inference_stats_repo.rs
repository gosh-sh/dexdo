// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level integration tests for the inference stats aggregates. Gated on
// TEST_DATABASE_URL like the other read-model tests.
//
// The suite shares one test DB with other binaries that write `inference_trades` and
// `raw_events` — deliberately corrupt rows included. So every assertion on the shared
// `public` tables goes through the book-scoped `_for` variants, with per-test book
// addresses and `trade_id` / `msg_id` prefixes (both are GLOBAL keys). The real, unscoped
// statements run only inside a private schema nobody else writes to.

use std::env;
use std::time::Duration;

use dodex_application::InferenceReadRepository;
use dodex_domain::InferenceStatsPeriod;
use dodex_domain::InferenceStatsTotals;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::Executor;
use sqlx::PgPool;

const AS_OF: &str = "2026-10-23T12:00:00Z";

fn secs(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
}

fn as_of_ms() -> i64 {
    secs(AS_OF) * 1000
}

fn scope(obs: &[&str]) -> Vec<String> {
    obs.iter().map(|s| s.to_string()).collect()
}

fn t(ticks: &str, quote: &str, fills: u64) -> InferenceStatsTotals {
    InferenceStatsTotals { ticks: ticks.into(), quote: quote.into(), fills }
}

fn test_url() -> Option<String> {
    let _ = dotenvy::dotenv();
    match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("skipping: TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn setup() -> Option<PgPool> {
    let url = test_url()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("TEST_DATABASE_URL connect");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

/// A pool onto a private, freshly migrated schema of the test database.
/// `setup()` runs first so `public` is migrated and `pgcrypto` already lives there
/// (otherwise `create extension if not exists` would install it into the private schema
/// and dropping that schema would take the extension with it). The pool's `search_path`
/// is the private schema only, so `_sqlx_migrations` is looked up there and not found in
/// `public`.
async fn isolated_pool(name: &str) -> Option<PgPool> {
    let public = setup().await?;
    public.execute(format!("drop schema if exists {name} cascade").as_str()).await.unwrap();
    public.execute(format!("create schema {name}").as_str()).await.unwrap();
    let path = format!("set search_path to {name}");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                conn.execute(path.as_str()).await?;
                Ok(())
            })
        })
        .connect(&test_url()?)
        .await
        .expect("isolated pool connect");
    database::run_migrations(&pool).await.expect("run migrations in the private schema");
    Some(pool)
}

async fn drop_isolated_schema(name: &str) {
    let public = setup().await.expect("TEST_DATABASE_URL");
    public.execute(format!("drop schema if exists {name} cascade").as_str()).await.unwrap();
}

/// Deletes inference_trades by book, raw_events by src_address, inference_markets by book.
async fn purge(pool: &PgPool, obs: &[&str]) {
    let obs = scope(obs);
    sqlx::query("delete from inference_trades where orderbook_address = any($1)")
        .bind(&obs)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from raw_events where src_address = any($1)")
        .bind(&obs)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = any($1)")
        .bind(&obs)
        .execute(pool)
        .await
        .unwrap();
}

/// Reconciled book: SHELL (token_type 2, decimals 9), precision 9/0 — as in inference_trades_repo.rs.
async fn seed_market(pool: &PgPool, ob: &str) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, platform_fee_bps, quote_token_type,
                price_precision, quantity_precision, tick_size, step_size, min_notional,
                created_at_chain, last_reconciled_at)
           values ($1, null, 'r', 250, 2, 9, 0, '1', '1', '1',
                   to_timestamp(1700000000), now())
           on conflict (orderbook_address) do nothing"#,
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("seed market");
}

/// Skeleton row: only orderbook_address and created_at_chain; precision NULL.
async fn seed_skeleton(pool: &PgPool, ob: &str) {
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain)
         values ($1, to_timestamp(1700000000))",
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("seed skeleton");
}

/// Reconciled, then retired: precision kept, last_reconciled_at = null, superseded_at = now().
async fn seed_superseded(pool: &PgPool, ob: &str) {
    seed_market(pool, ob).await;
    sqlx::query(
        "update inference_markets set last_reconciled_at = null, superseded_at = now()
          where orderbook_address = $1",
    )
    .bind(ob)
    .execute(pool)
    .await
    .expect("supersede market");
}

/// `chain` is an RFC 3339 instant, or None for a NULL chain_time.
async fn seed_trade(
    pool: &PgPool,
    ob: &str,
    trade_id: &str,
    price: &str,
    qty: &str,
    chain: Option<&str>,
) {
    sqlx::query(
        r#"insert into inference_trades
               (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time)
           values ($1, $2, $3::numeric, $4::numeric, false, $5::text::timestamptz)"#,
    )
    .bind(trade_id)
    .bind(ob)
    .bind(price)
    .bind(qty)
    .bind(chain)
    .execute(pool)
    .await
    .expect("seed trade");
}

/// Unprojected raw event: processed_at NULL, decoded '{}', chain_order = msg_id.
async fn seed_pending_event(
    pool: &PgPool,
    ob: &str,
    msg_id: &str,
    event_type: Option<&str>,
    chain: Option<&str>,
) {
    sqlx::query(
        r#"insert into raw_events
               (msg_id, src_address, event_type, chain_order, decoded, processed_at,
                created_at_chain)
           values ($1, $2, $3, $1, '{}'::jsonb, null, $4::text::timestamptz)"#,
    )
    .bind(msg_id)
    .bind(ob)
    .bind(event_type)
    .bind(chain)
    .execute(pool)
    .await
    .expect("seed pending event");
}

/// Reference totals from a plain query over the given books and the half-open interval
/// `[from, to)` (`from` None = unbounded), independent of the production statements. Every
/// book in this suite is SHELL with quantity precision 0, so the raw notional divided by
/// 10^9 and trimmed is the canonical decimal.
async fn reference_totals(
    pool: &PgPool,
    obs: &[&str],
    from: Option<&str>,
    to: &str,
) -> InferenceStatsTotals {
    let (ticks, quote, fills): (String, String, i64) = sqlx::query_as(
        "select coalesce(sum(qty), 0)::text, \
                coalesce(trim_scale(sum(price * qty) / 1000000000), 0)::text, \
                count(*) \
           from inference_trades \
          where orderbook_address = any($1) and chain_time is not null \
            and chain_time < $2::text::timestamptz \
            and ($3::text is null or chain_time >= $3::text::timestamptz)",
    )
    .bind(scope(obs))
    .bind(to)
    .bind(from)
    .fetch_one(pool)
    .await
    .unwrap();
    InferenceStatsTotals { ticks, quote, fills: fills as u64 }
}

/// The nine trades of the 14-day scenario, ids `<prefix>-1` .. `<prefix>-9`.
async fn seed_w14(pool: &PgPool, ob: &str, prefix: &str) {
    let rows: [(&str, &str, Option<&str>); 9] = [
        // before the previous period: evidence only
        ("1000000000", "1", Some("2026-09-25T23:59:59Z")),
        // previous, first instant
        ("2000000000", "2", Some("2026-09-26T00:00:00Z")),
        // previous, last instant
        ("1000000000", "3", Some("2026-10-09T23:59:59Z")),
        // selected, first instant
        ("1500000001", "3", Some("2026-10-10T00:00:00Z")),
        ("1000000000", "1", Some("2026-10-22T23:59:59Z")),
        ("1000000000", "5", Some("2026-10-23T00:00:00Z")),
        ("12000000000", "9", Some("2026-10-23T11:59:59Z")),
        // == asOf: excluded
        ("1000000000", "7", Some("2026-10-23T12:00:00Z")),
        // NULL chain_time: excluded
        ("1000000000", "4", None),
    ];
    for (i, (price, qty, chain)) in rows.iter().enumerate() {
        seed_trade(pool, ob, &format!("{prefix}-{}", i + 1), price, qty, *chain).await;
    }
}

#[tokio::test]
async fn window_14d_day_boundaries_cutoff_and_previous_period() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_w14";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    seed_w14(&pool, OB, "w14").await;
    let repo = PostgresReadModelRepository::new(pool.clone());

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h.as_of_ms, as_of_ms());
    assert_eq!(h.per_day.len(), 14);
    assert_eq!(h.per_day[0].day, "2026-10-10");
    assert_eq!(h.per_day[0].totals, t("3", "4.500000003", 1));
    assert_eq!(h.per_day[12].totals, t("1", "1", 1));
    assert_eq!(h.per_day[13].day, "2026-10-23");
    assert_eq!(h.per_day[13].totals, t("14", "113", 2));
    assert!(h.per_day[1..12].iter().all(|x| x.totals == t("0", "0", 0)));
    assert_eq!(h.totals, t("18", "118.500000003", 4));
    let p = h.previous_period.clone().expect("bracketed");
    assert_eq!((p.from.as_str(), p.to.as_str()), ("2026-09-26", "2026-10-10"));
    assert_eq!(p.totals, t("5", "7", 2));

    // Reference: a plain query over the same rows and intervals agrees.
    assert_eq!(h.totals, reference_totals(&pool, &[OB], Some("2026-10-10T00:00:00Z"), AS_OF).await);
    assert_eq!(
        p.totals,
        reference_totals(&pool, &[OB], Some("2026-09-26T00:00:00Z"), "2026-10-10T00:00:00Z").await
    );

    // A recovered chain_time lands on its own historical day.
    sqlx::query(
        "update inference_trades set chain_time = '2026-10-15T08:00:00Z' where trade_id = 'w14-9'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let h2 = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h2.per_day[5].day, "2026-10-15");
    assert_eq!(h2.per_day[5].totals, t("4", "4", 1));
    assert_eq!(h2.totals, t("22", "122.500000003", 5));

    // A replayed insert of an existing trade_id changes nothing.
    sqlx::query(
        "insert into inference_trades (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time) \
         values ('w14-7', $1, 12000000000, 9, false, '2026-10-23T11:59:59Z') \
         on conflict (trade_id) do nothing",
    )
    .bind(OB)
    .execute(&pool)
    .await
    .unwrap();
    let h3 = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h3.totals, h2.totals);

    purge(&pool, &[OB]).await;
}

#[tokio::test]
async fn window_30d_boundaries() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_w30";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    let trades = [
        ("w30-1", "2026-08-24T23:59:59Z"), // before
        ("w30-2", "2026-08-25T00:00:00Z"), // previous
        ("w30-3", "2026-09-23T23:59:59Z"), // previous
        ("w30-4", "2026-09-24T00:00:00Z"), // selected
        ("w30-5", "2026-10-23T11:00:00Z"), // selected
    ];
    for (id, at) in trades {
        seed_trade(&pool, OB, id, "1000000000", "1", Some(at)).await;
    }
    let repo = PostgresReadModelRepository::new(pool.clone());

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days30, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h.per_day.len(), 30);
    assert_eq!(h.per_day[0].day, "2026-09-24");
    assert_eq!(h.per_day[0].totals, t("1", "1", 1));
    assert_eq!(h.totals, t("2", "2", 2));
    let p = h.previous_period.unwrap();
    assert_eq!((p.from.as_str(), p.to.as_str()), ("2026-08-25", "2026-09-24"));
    assert_eq!(p.totals, t("2", "2", 2));
    assert_eq!(h.totals, reference_totals(&pool, &[OB], Some("2026-09-24T00:00:00Z"), AS_OF).await);
    assert_eq!(
        p.totals,
        reference_totals(&pool, &[OB], Some("2026-08-25T00:00:00Z"), "2026-09-24T00:00:00Z").await
    );

    purge(&pool, &[OB]).await;
}

#[tokio::test]
async fn all_period_spans_first_execution_to_today_and_empty_scope_is_empty() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_all";
    const EMPTY: &str = "0:inf_stats_all_empty";
    purge(&pool, &[OB, EMPTY]).await;
    seed_market(&pool, OB).await;
    seed_trade(&pool, OB, "all-1", "1000000000", "1", Some("2026-10-01T10:00:00Z")).await;
    seed_trade(&pool, OB, "all-2", "1000000000", "1", Some("2026-10-03T10:00:00Z")).await;
    let repo = PostgresReadModelRepository::new(pool.clone());
    let cutoff = secs("2026-10-05T09:00:00Z") * 1000;

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::All, cutoff, &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(
        h.per_day.iter().map(|x| x.day.as_str()).collect::<Vec<_>>(),
        ["2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04", "2026-10-05"]
    );
    assert_eq!(h.per_day[4].totals, t("0", "0", 0));
    assert!(h.previous_period.is_none());
    assert_eq!(h.totals, reference_totals(&pool, &[OB], None, "2026-10-05T09:00:00Z").await);
    // a scope with no trades at all
    let e = repo
        .inference_stats_history_for(InferenceStatsPeriod::All, cutoff, &scope(&[EMPTY]))
        .await
        .unwrap();
    assert!(e.per_day.is_empty());
    assert_eq!(e.totals, t("0", "0", 0));

    purge(&pool, &[OB, EMPTY]).await;
}

#[tokio::test]
async fn books_in_every_state_are_counted() {
    let Some(pool) = setup().await else { return };
    const REC: &str = "0:inf_stats_state_rec";
    const SUP: &str = "0:inf_stats_state_sup";
    const SKE: &str = "0:inf_stats_state_ske";
    const NON: &str = "0:inf_stats_state_none";
    let obs = [REC, SUP, SKE, NON];
    purge(&pool, &obs).await;
    seed_market(&pool, REC).await;
    seed_superseded(&pool, SUP).await;
    seed_skeleton(&pool, SKE).await;
    for (i, ob) in obs.iter().enumerate() {
        seed_trade(
            &pool,
            ob,
            &format!("state-{i}"),
            "1000000000",
            "1",
            Some("2026-10-20T10:00:00Z"),
        )
        .await;
    }
    let repo = PostgresReadModelRepository::new(pool.clone());

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&obs))
        .await
        .unwrap();
    assert_eq!(h.per_day[10].day, "2026-10-20");
    assert_eq!(h.per_day[10].totals, t("4", "4", 4));

    purge(&pool, &obs).await;
}

#[tokio::test]
async fn more_than_1000_executions_in_one_book_are_all_counted() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_many";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    sqlx::query(
        "insert into inference_trades (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time) \
         select 'many-' || g, $1, 1000000000, 1, false, '2026-10-21T10:00:00Z'::timestamptz \
           from generate_series(1, 1200) g",
    )
    .bind(OB)
    .execute(&pool)
    .await
    .unwrap();
    let repo = PostgresReadModelRepository::new(pool.clone());

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h.per_day[11].totals, t("1200", "1200", 1200));

    purge(&pool, &[OB]).await;
}

#[tokio::test]
async fn sums_at_the_top_of_the_numeric_range_stay_exact() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_top";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    // price = 1 followed by 77 zeros, qty = 1 followed by 38 zeros
    let price = format!("1{}", "0".repeat(77));
    let qty = format!("1{}", "0".repeat(38));
    seed_trade(&pool, OB, "top-1", &price, &qty, Some("2026-10-21T10:00:00Z")).await;
    let repo = PostgresReadModelRepository::new(pool.clone());

    let h = repo
        .inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
        .await
        .unwrap();
    assert_eq!(h.per_day[11].totals.ticks, format!("1{}", "0".repeat(38)));
    assert_eq!(h.per_day[11].totals.quote, format!("1{}", "0".repeat(106))); // 10^115 at scale 9

    purge(&pool, &[OB]).await;
}

#[tokio::test]
async fn volume_24h_uses_exact_timestamps() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_vol";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    let trades = [
        ("vol-1", "2026-10-22T11:59:59Z"), // out
        ("vol-2", "2026-10-22T12:00:00Z"), // in
        ("vol-3", "2026-10-23T11:59:59Z"), // in
        ("vol-4", "2026-10-23T12:00:00Z"), // out, == asOf
    ];
    for (id, at) in trades {
        seed_trade(&pool, OB, id, "1000000000", "1", Some(at)).await;
    }
    let repo = PostgresReadModelRepository::new(pool.clone());

    let v = repo.inference_stats_volume_24h_for(as_of_ms(), &scope(&[OB])).await.unwrap();
    assert_eq!(v.as_of_ms, as_of_ms());
    assert_eq!(v.totals, t("2", "2", 2));
    assert_eq!(v.totals, reference_totals(&pool, &[OB], Some("2026-10-22T12:00:00Z"), AS_OF).await);

    purge(&pool, &[OB]).await;
}

#[tokio::test]
async fn previous_period_coverage() {
    let Some(pool) = setup().await else { return };
    const OB: &str = "0:inf_stats_cov";
    const FILLED: &str = "InferenceOrderBook.InferenceFilled";
    purge(&pool, &[OB]).await;
    seed_market(&pool, OB).await;
    let repo = PostgresReadModelRepository::new(pool.clone());
    let prev = || async {
        repo.inference_stats_history_for(InferenceStatsPeriod::Days14, as_of_ms(), &scope(&[OB]))
            .await
            .unwrap()
            .previous_period
    };
    let delete_event = |msg_id: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("delete from raw_events where msg_id = $1")
                .bind(msg_id)
                .execute(&pool)
                .await
                .unwrap();
        }
    };

    // 1. only a selected-period trade: no execution before the previous interval
    seed_trade(&pool, OB, "cov-1", "1000000000", "1", Some("2026-10-20T10:00:00Z")).await;
    assert!(prev().await.is_none());

    // 2. + a trade before the previous interval: bracketed, nothing inside it
    seed_trade(&pool, OB, "cov-2", "1000000000", "1", Some("2026-09-20T10:00:00Z")).await;
    assert_eq!(prev().await.expect("bracketed").totals, t("0", "0", 0));

    // 3. + a trade inside the previous interval
    seed_trade(&pool, OB, "cov-3", "1000000000", "1", Some("2026-10-01T10:00:00Z")).await;
    assert_eq!(prev().await.expect("bracketed").totals, t("1", "1", 1));

    // 4. + a pending InferenceFilled inside the previous interval withholds it
    seed_pending_event(&pool, OB, "cov-ev-1", Some(FILLED), Some("2026-10-02T10:00:00Z")).await;
    assert!(prev().await.is_none());

    // 5. once processed, the event no longer counts
    sqlx::query("update raw_events set processed_at = now() where msg_id = 'cov-ev-1'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(prev().await.is_some());

    // 6. a pending event of unknown type under this book, inside the interval
    seed_pending_event(&pool, OB, "cov-ev-2", None, Some("2026-10-02T10:00:00Z")).await;
    assert!(prev().await.is_none());
    delete_event("cov-ev-2").await;
    assert!(prev().await.is_some());

    // 7. a pending InferenceFilled with an unknown date
    seed_pending_event(&pool, OB, "cov-ev-3", Some(FILLED), None).await;
    assert!(prev().await.is_none());
    delete_event("cov-ev-3").await;
    assert!(prev().await.is_some());

    // 8. a pending InferenceFilled inside the selected period is not the previous interval
    seed_pending_event(&pool, OB, "cov-ev-4", Some(FILLED), Some("2026-10-21T10:00:00Z")).await;
    assert!(prev().await.is_some());

    // 9. no execution left in the selected period
    sqlx::query("delete from inference_trades where trade_id = 'cov-1'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(prev().await.is_none());

    purge(&pool, &[OB]).await;
}

/// The real, unscoped statements — the ones production runs — in a schema nobody else writes to.
/// The shared `public` tables cannot host this: a neighbouring suite's corrupt row would turn
/// the all-time aggregate into MarketInconsistent at random.
#[tokio::test]
async fn unscoped_statements_in_an_isolated_schema() {
    const SCHEMA: &str = "inference_stats_iso_unscoped";
    const OB: &str = "0:inf_stats_iso";
    let Some(pool) = isolated_pool(SCHEMA).await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    // Same nine trades as the 14-day scenario (fresh ids), one reconciled book.
    // No scope anywhere: these are the trait methods.
    seed_market(&pool, OB).await;
    seed_w14(&pool, OB, "iso").await;

    let h = InferenceReadRepository::inference_stats_history(
        &repo,
        InferenceStatsPeriod::Days14,
        as_of_ms(),
    )
    .await
    .unwrap();
    assert_eq!(h.per_day.len(), 14);
    assert_eq!(h.totals, t("18", "118.500000003", 4));
    assert_eq!(h.previous_period.clone().unwrap().totals, t("5", "7", 2));

    let all = InferenceReadRepository::inference_stats_history(
        &repo,
        InferenceStatsPeriod::All,
        as_of_ms(),
    )
    .await
    .unwrap();
    assert_eq!(all.per_day.first().unwrap().day, "2026-09-25");
    assert_eq!(all.per_day.last().unwrap().day, "2026-10-23");
    assert_eq!(all.per_day.len(), 29);
    assert_eq!(all.totals, t("24", "126.500000003", 7));
    assert!(all.previous_period.is_none());

    let v = InferenceReadRepository::inference_stats_volume_24h(&repo, as_of_ms()).await.unwrap();
    assert_eq!(v.totals, t("15", "114", 3)); // 2026-10-22T23:59:59Z + the two of 2026-10-23

    // The unscoped coverage probe: a pending fill inside the previous interval withholds it.
    seed_pending_event(
        &pool,
        OB,
        "iso-pend-1",
        Some("InferenceOrderBook.InferenceFilled"),
        Some("2026-10-02T00:00:00Z"),
    )
    .await;
    let h = InferenceReadRepository::inference_stats_history(
        &repo,
        InferenceStatsPeriod::Days14,
        as_of_ms(),
    )
    .await
    .unwrap();
    assert!(h.previous_period.is_none());

    pool.close().await;
    drop_isolated_schema(SCHEMA).await;
}
