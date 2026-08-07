// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level integration tests for the inference trade tape. Gated on
// TEST_DATABASE_URL like the other read-model tests.
//
// `inference_trades.trade_id` is a GLOBAL primary key and the whole suite shares one test
// DB, so every seeded id here carries a per-test prefix. Reusing a plain `co-01` across
// tests deadlocks or unique-violates once nextest runs two binaries at once, and the
// per-book purge does not protect against it.

use std::env;
use std::time::Duration;

use dodex_application::InferenceReadRepository;
use dodex_domain::DomainError;
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

async fn purge(pool: &PgPool, ob: &str) {
    sqlx::query("delete from inference_trades where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from inference_markets where orderbook_address = $1")
        .bind(ob)
        .execute(pool)
        .await
        .unwrap();
}

/// Reconciled book: quote SHELL (token_type 2, decimals 9), precision 9/0.
/// `version` is deliberately not seeded — the tape does not read it (no `contractVersion`
/// in a bare-array response).
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

async fn seed_trade(
    pool: &PgPool,
    ob: &str,
    trade_id: &str,
    price: &str,
    qty: &str,
    is_buyer_maker: bool,
    chain_secs: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_trades
               (trade_id, orderbook_address, price, qty, is_buyer_maker, chain_time)
           values ($1, $2, $3::numeric, $4::numeric, $5,
                   case when $6::bigint is null then null
                        else to_timestamp($6::double precision) end)"#,
    )
    .bind(trade_id)
    .bind(ob)
    .bind(price)
    .bind(qty)
    .bind(is_buyer_maker)
    .bind(chain_secs)
    .execute(pool)
    .await
    .expect("seed trade");
}

#[tokio::test]
async fn tape_is_newest_first_and_scaled() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_tape_repo_scale";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    // price = 1.5 SHELL per tick (1_500_000_000 base units), 4 ticks => 6 SHELL notional.
    seed_trade(&pool, ob, "rsc-1", "1500000000", "4", true, Some(1_700_000_000)).await;
    seed_trade(&pool, ob, "rsc-2", "2000000000", "3", false, Some(1_700_000_060)).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let tape = repo.list_inference_trades(ob, 20).await.expect("tape");

    assert_eq!(tape.len(), 2);
    assert_eq!(tape[0].trade_id, "rsc-2", "newest (lex-greatest trade_id) first");
    assert_eq!(tape[0].price, "2.000000000");
    assert_eq!(tape[0].qty, "3");
    assert_eq!(tape[0].quote_qty, "6.000000000");
    assert!(!tape[0].is_buyer_maker);
    assert_eq!(tape[0].time, 1_700_000_060_000, "Unix milliseconds");
    assert_eq!(tape[1].trade_id, "rsc-1");
    assert_eq!(tape[1].quote_qty, "6.000000000");
    assert!(tape[1].is_buyer_maker);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn tape_applies_limit_and_scopes_to_the_book() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_tape_repo_limit";
    let other = "0:inf_tape_repo_other";
    purge(&pool, ob).await;
    purge(&pool, other).await;
    seed_market(&pool, ob).await;
    seed_market(&pool, other).await;
    for i in 1..=3 {
        seed_trade(&pool, ob, &format!("rlim-{i}"), "1000000000", "1", true, Some(1_700_000_000))
            .await;
    }
    seed_trade(&pool, other, "rlim-oth-1", "1000000000", "1", true, Some(1_700_000_000)).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let tape = repo.list_inference_trades(ob, 2).await.expect("tape");
    assert_eq!(tape.len(), 2, "limit cuts the tape");
    assert_eq!(tape[0].trade_id, "rlim-3");
    assert_eq!(tape[1].trade_id, "rlim-2");

    purge(&pool, ob).await;
    purge(&pool, other).await;
}

#[tokio::test]
async fn tape_hides_rows_without_chain_time() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_tape_repo_notime";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_trade(&pool, ob, "rnt-1", "1000000000", "1", true, Some(1_700_000_000)).await;
    seed_trade(&pool, ob, "rnt-2", "1000000000", "1", true, None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let tape = repo.list_inference_trades(ob, 20).await.expect("tape");
    assert_eq!(tape.len(), 1, "a NULL chain_time row is excluded before LIMIT");
    assert_eq!(tape[0].trade_id, "rnt-1");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn reconciled_book_with_no_matches_is_empty() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_tape_repo_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let tape = repo.list_inference_trades(ob, 20).await.expect("tape");
    assert!(tape.is_empty(), "a book that has not traded reads as an empty tape, not an error");

    purge(&pool, ob).await;
}

#[tokio::test]
async fn unknown_or_unreconciled_book_is_invalid_market() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let err = repo.list_inference_trades("0:inf_tape_repo_missing", 20).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));

    let ob = "0:inf_tape_repo_unreconciled";
    purge(&pool, ob).await;
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain)
         values ($1, to_timestamp(1700000000))",
    )
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let err = repo.list_inference_trades(ob, 20).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::InvalidMarketOrSymbol)));

    purge(&pool, ob).await;
}

#[tokio::test]
async fn undecodable_raw_price_is_market_inconsistent() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_tape_repo_badprice";
    purge(&pool, ob).await;
    seed_market(&pool, ob).await;
    seed_trade(&pool, ob, "rbad-1", "1000000000", "1", true, Some(1_700_000_000)).await;
    // numeric(78,0) has no CHECK; a negative value is unsigned-undecodable -> fail closed.
    sqlx::query("update inference_trades set price = -1 where trade_id = 'rbad-1'")
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.list_inference_trades(ob, 20).await.unwrap_err();
    assert!(matches!(err.downcast_ref::<DomainError>(), Some(DomainError::MarketInconsistent)));

    purge(&pool, ob).await;
}
