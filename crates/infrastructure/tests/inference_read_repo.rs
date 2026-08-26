// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Repo-level integration tests for the inference read path. Gated on
// TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

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
async fn seed_market(
    pool: &PgPool,
    ob: &str,
    model_ref: Option<&str>,
    reference_price: Option<&str>,
    created_at_chain_secs: Option<i64>,
) {
    sqlx::query(
        r#"insert into inference_markets
               (orderbook_address, model_hash, model_ref,
                platform_fee_bps, quote_token_type, price_precision, quantity_precision,
                tick_size, step_size, min_notional, reference_price,
                created_at_chain, last_reconciled_at)
           values ($1, null, $2,
                   250, 2, 9, 0,
                   '0.000000001', '1', '0.000000001', $3::numeric,
                   case when $4::bigint is null then null else to_timestamp($4::double precision) end,
                   now())"#,
    )
    .bind(ob)
    .bind(model_ref)
    .bind(reference_price)
    .bind(created_at_chain_secs)
    .execute(pool)
    .await
    .expect("seed inference_markets");
}

/// A chain time above every other fixture in this shared database, so rows
/// seeded with it occupy the head of the newest-first listing. Below the
/// `created_at_chain` clamp (4_102_444_800), which the overflow test seeds at.
const HEAD_OF_LISTING: i64 = 4_102_444_000;

/// The first `pages` pages of the listing, newest first.
///
/// BOUNDED on purpose. The listing has no filter any more — `producer` went
/// with the parsed model-name parts it read — and the test database is shared
/// with suites that seed reconciled rows carrying no trading rules, which the
/// read model refuses (`MarketInconsistent`) rather than rendering. Walking to
/// the end would reach them. A test seeds at [`HEAD_OF_LISTING`] instead and
/// looks only at the head, where its own rows are.
async fn listing_head(
    repo: &PostgresReadModelRepository,
    page_size: u16,
    pages: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = None;
    for _ in 0..pages {
        let page = repo
            .list_inference_markets(&InferenceMarketsRequest::Listing(InferenceMarketsListing {
                sort: InferenceMarketsSort::CreatedAtDesc,
                cursor: cursor.take(),
                limit: page_size,
            }))
            .await
            .expect("listing page");
        out.extend(page.markets.iter().map(|m| m.orderbook_address.clone()));
        if !page.has_more {
            break;
        }
        cursor = Some(page.next_cursor.expect("has_more implies a cursor"));
    }
    out
}

/// Index of `ob` in a listing slice, or a panic naming what was actually there.
fn position_of(all: &[String], ob: &str) -> usize {
    all.iter()
        .position(|a| a == ob)
        .unwrap_or_else(|| panic!("{ob} missing from the listing head: {all:?}"))
}

#[tokio::test]
async fn one_market_renders_fees_identity_and_refprice() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_one";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("qwen--qwen2.5-32b--instruct"), Some("1010"), Some(1_700_000_000))
        .await;
    // The `version` column holds the CONTRACT version (getVersion, supersede); set
    // it to a value distinct from the model version so the assertion below proves
    // `model.version` renders from `model_version`, not from `version`.
    sqlx::query("update inference_markets set version='4.0.14' where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .expect("one market");

    assert_eq!(page.markets.len(), 1);
    let m = &page.markets[0];
    // The name reaches the API whole, undecomposed.
    assert_eq!(m.model_ref_name, "qwen--qwen2.5-32b--instruct");
    // `contractVersion` renders from the `version` column and is the CONTRACT's,
    // proving the two are surfaced independently.
    assert_eq!(m.contract_version.as_deref(), Some("4.0.14"));
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
    seed_market(&pool, ob, None, None, Some(1_700_000_000)).await;
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
    assert_eq!(m.model_ref_name, "9942"); // falls back to model_hash
    assert!(m.reference_price.is_none()); // dry book
    assert!(m.contract_version.is_none()); // version column unset -> null

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
    let visible = "0:inf_repo_gate_visible";
    let skeleton = "0:inf_repo_gate_skeleton";
    for ob in [visible, skeleton] {
        purge(&pool, ob).await;
    }
    // A reconciled (visible) row...
    seed_market(&pool, visible, Some("v"), None, Some(HEAD_OF_LISTING)).await;
    // ...and a skeleton row that exists but was never reconciled
    // (last_reconciled_at NULL) — exactly what the discovery pre-step inserts.
    sqlx::query(
        "insert into inference_markets (orderbook_address, created_at_chain) \
         values ($1, to_timestamp(1700000200))",
    )
    .bind(skeleton)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Single lookup of the unreconciled row is a miss, not a 503/inconsistent.
    let err = repo
        .list_inference_markets(&InferenceMarketsRequest::One {
            orderbook_address: skeleton.into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::InvalidMarketOrSymbol)
    ));

    // The listing carries the reconciled row and NOT the skeleton — the latter
    // must not leak through the `last_reconciled_at IS NOT NULL` gate. Both were
    // seeded at the head, so one page is enough to decide it.
    let head = listing_head(&repo, 2, 1).await;
    assert!(head.iter().any(|a| a == visible), "reconciled row missing from the listing");
    assert!(!head.iter().any(|a| a == skeleton), "unreconciled skeleton leaked into the listing");

    for ob in [visible, skeleton] {
        purge(&pool, ob).await;
    }
}

#[tokio::test]
async fn corrupt_price_precision_fails_closed() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_badprec";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("r"), None, Some(1)).await;
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
    seed_market(&pool, ob, Some("r"), None, Some(1)).await;
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
    seed_market(pool, ob, Some("r"), None, Some(1)).await;
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
    let a = "0:inf_repo_pg_a"; // newest chain time
    let b = "0:inf_repo_pg_b"; // older chain time
    let c = "0:inf_repo_pg_c"; // older still
    let d = "0:inf_repo_pg_d"; // NULL chain time -> coalesces to epoch 0, sorts last
    for ob in [a, b, c, d] {
        purge(&pool, ob).await;
    }
    seed_market(&pool, a, Some("a"), None, Some(HEAD_OF_LISTING)).await;
    seed_market(&pool, b, Some("b"), None, Some(HEAD_OF_LISTING - 1)).await;
    seed_market(&pool, c, Some("c"), None, Some(HEAD_OF_LISTING - 2)).await;
    seed_market(&pool, d, Some("d"), None, None).await;

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Two at a time, so the three dated rows cannot land in one page and every
    // boundary between them is actually crossed.
    let head = listing_head(&repo, 2, 3).await;

    let unique = head.iter().collect::<std::collections::HashSet<_>>().len();
    assert_eq!(unique, head.len(), "the keyset repeated a row across a page boundary");
    assert!(position_of(&head, a) < position_of(&head, b), "newer chain time must sort first");
    assert!(position_of(&head, b) < position_of(&head, c), "the keyset must stay ordered");

    // The NULL-time row coalesces to epoch 0 and sorts behind every dated row.
    // Asserted only when the window actually reaches it: whether it does depends
    // on how many other markets this shared database holds, and a test must not
    // depend on that. When the window is short the property is unobservable, not
    // violated.
    if let Some(pos_d) = head.iter().position(|x| x == d) {
        assert!(pos_d > position_of(&head, c), "a NULL chain time must sort after every dated row");
    }

    // And it renders with that coalesced timestamp rather than failing to decode.
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: d.into() })
        .await
        .unwrap();
    assert_eq!(page.markets[0].created_at, 0);

    for ob in [a, b, c, d] {
        purge(&pool, ob).await;
    }
}

async fn seed_order(
    pool: &PgPool,
    ob: &str,
    order_id: i64,
    is_buy: bool,
    price: &str,
    amount: &str,
    chain_order: &str,
) {
    sqlx::query(
        r#"insert into inference_orders
               (orderbook_address, order_id, is_buy, price, amount_initial, amount_remaining,
                status, last_chain_order)
           values ($1, $2::numeric, $3, $4::numeric, $5::numeric, $5::numeric, 'OPEN', $6)"#,
    )
    .bind(ob)
    .bind(order_id)
    .bind(is_buy)
    .bind(price)
    .bind(amount)
    .bind(chain_order)
    .execute(pool)
    .await
    .expect("seed inference_order");
}

#[tokio::test]
async fn depth_aggregates_scales_and_reports_last_update_id() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_depth";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("r"), None, Some(1)).await;
    // The depth response echoes the book's contract version from `version`.
    sqlx::query("update inference_markets set version='4.0.30' where orderbook_address=$1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    // price_precision 9 -> raw "1000000000" => "1.000000000"; quantity_precision 0 -> raw passes through.
    seed_order(&pool, ob, 1, true, "1000000000", "5", "co-01").await;
    seed_order(&pool, ob, 2, true, "990000000", "3", "co-02").await;
    seed_order(&pool, ob, 3, false, "1050000000", "7", "co-03").await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let snap = repo.get_inference_depth(ob, 100).await.expect("depth");

    assert_eq!(snap.orderbook_address, ob);
    assert_eq!(snap.contract_version.as_deref(), Some("4.0.30"));
    assert_eq!(snap.last_update_id, "co-03"); // max chain order
    assert_eq!(
        snap.bids,
        vec![
            dodex_domain::PriceLevel { price: "1.000000000".into(), quantity: "5".into() },
            dodex_domain::PriceLevel { price: "0.990000000".into(), quantity: "3".into() },
        ]
    );
    assert_eq!(
        snap.asks,
        vec![dodex_domain::PriceLevel { price: "1.050000000".into(), quantity: "7".into() }]
    );

    purge(&pool, ob).await;
}

#[tokio::test]
async fn depth_empty_book_is_ok_with_blank_last_update_id() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_depth_empty";
    purge(&pool, ob).await;
    seed_market(&pool, ob, Some("r"), None, Some(1)).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let snap = repo.get_inference_depth(ob, 100).await.unwrap();
    assert!(snap.bids.is_empty());
    assert!(snap.asks.is_empty());
    assert_eq!(snap.last_update_id, "");
    assert!(snap.contract_version.is_none()); // version column unset -> null

    purge(&pool, ob).await;
}

#[tokio::test]
async fn depth_unknown_book_is_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.get_inference_depth("0:inf_repo_depth_missing", 100).await.unwrap_err();
    assert!(matches!(
        err.downcast_ref::<dodex_domain::DomainError>(),
        Some(dodex_domain::DomainError::InvalidMarketOrSymbol)
    ));
}

// Regression test: a far-future `created_at_chain` (epoch seconds above the
// i64 overflow threshold when multiplied by 1_000_000) must not cause a
// Postgres "bigint out of range" error. Before the epoch-clamp fix every query
// that evaluated `(extract(epoch from created_at_chain) * 1000000)::bigint`
// over such a row would raise ERROR and surface as HTTP 500 — including the
// listing ORDER BY which runs across ALL candidate rows before LIMIT.
//
// Seed value: 9_223_372_036_855 seconds (one above the overflow boundary
// 9_223_372_036_854). to_timestamp() accepts it (Postgres timestamptz max ≈
// year 294276). After the clamp (cap = 4_102_444_800 = 2100-01-01 UTC) the
// micros expression returns 4_102_444_800_000_000 — well within i64 range.
#[tokio::test]
async fn far_future_created_at_chain_does_not_overflow() {
    let Some(pool) = setup().await else { return };
    let ob = "0:inf_repo_ff_overflow";
    purge(&pool, ob).await;

    // Seed a reconciled, visible market with created_at_chain in the overflow band.
    seed_market(&pool, ob, Some("far-future-model"), None, Some(9_223_372_036_855_i64)).await;

    let repo = PostgresReadModelRepository::new(pool.clone());

    // Single-lookup must succeed (before the fix: Err from bigint overflow).
    let page = repo
        .list_inference_markets(&InferenceMarketsRequest::One { orderbook_address: ob.into() })
        .await
        .expect("single-lookup of far-future market must not overflow");
    assert_eq!(page.markets.len(), 1);
    assert_eq!(page.markets[0].orderbook_address, ob);

    // The listing must also survive it: the ORDER BY evaluates the micros
    // expression over every candidate row before LIMIT, so one overflowing row
    // used to fail the whole query.
    let head = listing_head(&repo, 2, 1).await;
    assert!(head.iter().any(|a| a == ob), "far-future market missing from the listing head");

    purge(&pool, ob).await;
}
