// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level integration tests for the inference read path. Gated on
// TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_application::InferenceMarketsFilter;
use dodex_application::InferenceMarketsListing;
use dodex_application::InferenceMarketsRequest;
use dodex_application::InferenceMarketsSort;
use dodex_application::InferenceReadRepository;
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
    sqlx::query("delete from inference_orders where orderbook_address = $1")
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

/// Seed a reconciled inference market. `reference_price` and `created_at_chain`
/// are passed raw (None => SQL NULL). fee=250, precision 9/0. `model_hash` is
/// seeded NULL (exempt from the `inference_markets_model_hash_idx` unique index,
/// so parallel tests never collide); `model_ref` supplies `ref`. Tests that need
/// the model-hash fallback set it with an explicit UPDATE to a distinct value.
#[allow(clippy::too_many_arguments)]
async fn seed_market(
    pool: &PgPool,
    ob: &str,
    model_ref: Option<&str>,
    producer: Option<&str>,
    model_name: Option<&str>,
    version: Option<&str>,
    reference_price: Option<&str>,
    created_at_chain_secs: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref, producer, model_name, version,
                platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, reference_price,
                created_at_chain, last_reconciled_at)
           values ($1, null, $2, $3, $4, $5,
                   250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001', $6::numeric,
                   case when $7::bigint is null then null else to_timestamp($7::double precision) end,
                   now())"#,
    )
    .bind(ob)
    .bind(model_ref)
    .bind(producer)
    .bind(model_name)
    .bind(version)
    .bind(reference_price)
    .bind(created_at_chain_secs)
    .execute(pool)
    .await
    .expect("seed inference_markets");
}

#[tokio::test]
async fn one_market_renders_fees_identity_and_refprice() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_one";
    purge(&pool, ob).await;
    seed_market(
        &pool,
        ob,
        Some("qwen--qwen2.5-32b--instruct"),
        Some("qwen"),
        Some("qwen2.5-32b"),
        Some("instruct"),
        Some("1010"),
        Some(1_700_000_000),
    )
    .await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .expect("one market");

    assert_eq!(page.markets.len(), 1);
    let m = &page.markets[0];
    assert_eq!(m.model.model_ref, "qwen--qwen2.5-32b--instruct");
    assert_eq!(m.model.producer.as_deref(), Some("qwen"));
    assert_eq!(m.quote_asset, "SHELL");
    assert_eq!(m.taker_commission, "0.025"); // 250 bps
    assert_eq!(m.maker_commission, "-0.02"); // -REBATE_MAX_BPS
    assert_eq!(m.price_precision, 9);
    assert_eq!(m.quantity_precision, 0);
    // reference_price raw "1010" scaled by price_precision 9.
    assert_eq!(m.reference_price.as_deref(), Some("0.000001010"));
    assert_eq!(m.created_at, 1_700_000_000);

    purge(&pool, ob).await;
}

#[tokio::test]
async fn ref_falls_back_to_model_hash_when_model_ref_null() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_reffallback";
    purge(&pool, ob).await;
    seed_market(&pool, ob, None, None, None, None, None, Some(1_700_000_000)).await;
    // model_ref is NULL; give this row a distinct model_hash so `ref` falls back
    // to it. (9942 is unique among the suite's seeds, which otherwise use NULL.)
    sqlx::query("update inference_markets set model_hash = 9942 where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .unwrap();
    let m = &page.markets[0];
    assert_eq!(m.model.model_ref, "9942"); // falls back to model_hash
    assert!(m.model.producer.is_none());
    assert!(m.reference_price.is_none()); // dry book

    purge(&pool, ob).await;
}

#[tokio::test]
async fn unknown_address_is_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One {
            orderbook_address: "0:does_not_exist".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::InvalidMarketOrSymbol)
    ));
}

#[tokio::test]
async fn unreconciled_market_is_hidden_by_the_visibility_gate() {
    let Some(pool) = setup().await else { return };
    let prod = "inf_repo_gate";
    let visible = "0:inf_repo_gate_visible";
    let skeleton = "0:inf_repo_gate_skeleton";
    for ob in [visible, skeleton] {
        purge(&pool, ob).await;
    }
    // A reconciled (visible) row...
    seed_market(&pool, visible, Some("v"), Some(prod), None, None, None, Some(1_700_000_100)).await;
    // ...and a skeleton row that exists but was never reconciled
    // (last_reconciled_at NULL) — exactly what the discovery pre-step inserts.
    sqlx::query(
        "insert into inference_markets (orderbook_address, producer, created_at_chain) \
         values ($1, $2, to_timestamp(1700000200))",
    )
    .bind(skeleton)
    .bind(prod)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Single lookup of the unreconciled row is a miss, not a 503/inconsistent.
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: skeleton.into() })
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::InvalidMarketOrSymbol)
    ));

    // Listing (producer-isolated) returns only the reconciled row — the
    // skeleton must not leak through the `last_reconciled_at IS NOT NULL` gate.
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::Listing(InferenceMarketsListing {
            filter: InferenceMarketsFilter { producer: Some(prod.to_string()) },
            sort: InferenceMarketsSort::CreatedAtDesc,
            cursor: None,
            limit: 50,
        }))
        .await
        .unwrap();
    let addrs: Vec<&str> = page.markets.iter().map(|m| m.orderbook_address.as_str()).collect();
    assert_eq!(addrs, vec![visible]);

    for ob in [visible, skeleton] {
        purge(&pool, ob).await;
    }
}

#[tokio::test]
async fn corrupt_price_precision_fails_closed() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_badprec";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("r"), None, None, None, None, Some(1)).await;
    sqlx::query("update inference_markets set price_precision = -1 where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::MarketInconsistent)
    ));

    purge(&pool, ob).await;
}

#[tokio::test]
async fn negative_platform_fee_bps_fails_closed() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_negfee";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("r"), None, None, None, None, Some(1)).await;
    sqlx::query("update inference_markets set platform_fee_bps = -50 where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::MarketInconsistent)
    ));

    purge(&pool, ob).await;
}

// --- Remaining fail-closed matrix (spec §6.1 / D5): every corrupt input on a
// reconciled row is -1500, including on a dry book where the refprice guard
// never runs. Helper to seed + corrupt one column, then assert MarketInconsistent.
async fn assert_corrupt_market_is_inconsistent(pool: &PgPool, ob: &str, corrupt_sql: &str) {
    purge(pool, ob).await;
    seed_market(pool, ob, Some("r"), None, None, None, None, Some(1)).await;
    sqlx::query(corrupt_sql).bind(ob).execute(pool).await.unwrap();
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<dodex_domain::DomainError>(),
            Some(dodex_domain::DomainError::MarketInconsistent)
        ),
        "expected MarketInconsistent for corruption: {corrupt_sql}"
    );
    purge(pool, ob).await;
}

#[tokio::test]
async fn null_platform_fee_bps_fails_closed() {
    let Some(pool) = setup().await else { return };
    assert_corrupt_market_is_inconsistent(
        &pool,
        "0:inf_repo_nullfee",
        "update inference_markets set platform_fee_bps = null where orderbook_address = $1",
    )
    .await;
}

#[tokio::test]
async fn negative_reference_price_fails_closed() {
    let Some(pool) = setup().await else { return };
    // reference_price is numeric(78,0) with no CHECK; -1 is unsigned-undecodable.
    assert_corrupt_market_is_inconsistent(
        &pool,
        "0:inf_repo_negref",
        "update inference_markets set reference_price = -1 where orderbook_address = $1",
    )
    .await;
}

#[tokio::test]
async fn corrupt_quantity_precision_on_dry_book_fails_closed() {
    let Some(pool) = setup().await else { return };
    // Dry book (reference_price NULL from seed): the refprice guard never runs,
    // proving precision is validated unconditionally while rendering the market.
    assert_corrupt_market_is_inconsistent(
        &pool,
        "0:inf_repo_badqprec",
        "update inference_markets set quantity_precision = -1 where orderbook_address = $1",
    )
    .await;
}

#[tokio::test]
async fn listing_paginates_null_chain_time_last() {
    let Some(pool) = setup().await else { return };
    // Unique producer isolates the three seeded rows in the shared test DB so
    // the assertions are exact regardless of other visible markets.
    let prod = "pgtest_inf";
    let a = "0:inf_repo_pg_a"; // newest chain time
    let b = "0:inf_repo_pg_b"; // older chain time
    let c = "0:inf_repo_pg_c"; // NULL chain time -> sorts last (epoch 0)
    for ob in [a, b, c] {
        purge(&pool, ob).await;
    }
    seed_market(&pool, a, Some("a"), Some(prod), None, None, None, Some(1_700_000_200)).await;
    seed_market(&pool, b, Some("b"), Some(prod), None, None, None, Some(1_700_000_100)).await;
    seed_market(&pool, c, Some("c"), Some(prod), None, None, None, None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let request = |cursor: Option<String>| {
        InferenceMarketsRequest::Listing(InferenceMarketsListing {
            filter: InferenceMarketsFilter { producer: Some(prod.to_string()) },
            sort: InferenceMarketsSort::CreatedAtDesc,
            cursor,
            limit: 2,
        })
    };

    // Page 1: newest-first, the NULL-time row must NOT appear here.
    let p1 = repo.list_inference_markets(&request(None)).await.unwrap();
    let addrs1: Vec<&str> = p1.markets.iter().map(|m| m.orderbook_address.as_str()).collect();
    assert_eq!(addrs1, vec![a, b]);
    assert!(p1.has_more);
    let cursor = p1.next_cursor.expect("page 1 has a next cursor");

    // Page 2: the NULL-time row sorts last and is reached across the boundary,
    // with createdAt coalesced to 0 — not skipped, not duplicated.
    let p2 = repo.list_inference_markets(&request(Some(cursor))).await.unwrap();
    let addrs2: Vec<&str> = p2.markets.iter().map(|m| m.orderbook_address.as_str()).collect();
    assert_eq!(addrs2, vec![c]);
    assert_eq!(p2.markets[0].created_at, 0);
    assert!(!p2.has_more);

    for ob in [a, b, c] {
        purge(&pool, ob).await;
    }
}
