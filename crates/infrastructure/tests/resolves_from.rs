// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Read-path coverage for the `resolvesFrom` block + `?resolvesFrom=` filter on
// /api/v1/prediction/markets. A numeric range market (oracle_events.range_ob_address
// set) surfaces the settling InferenceOrderBook + joined model; non-range markets
// carry `None`. Gated on TEST_DATABASE_URL — see reprojection.rs for the harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::MarketsFilter;
use dodex_application::MarketsListing;
use dodex_application::MarketsRequest;
use dodex_application::MarketsSort;
use dodex_domain::MarketAddress;
use dodex_domain::ResolvesFromMetric;
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

/// Seed a reconciled (API-visible) market + one confirming oracle event. When
/// `range_ob` is `Some`, the event is a range event pointing at that inference
/// book. When `model_ref` is `Some`, a matching `inference_markets` row exists.
async fn seed(
    pool: &PgPool,
    tag: &str,
    range_ob: Option<&str>,
    model_ref: Option<&str>,
    event_id: i64,
) {
    let pmp = format!("0:{tag}_pmp");
    let oracle = format!("0:{tag}_oracle");
    // Cascade-clean any residue.
    sqlx::query("delete from markets where pmp_address = $1").bind(&pmp).execute(pool).await.unwrap();
    sqlx::query("delete from oracle_events where confirmed_pmp_address = $1").bind(&pmp).execute(pool).await.unwrap();
    sqlx::query("delete from oracles where address = $1").bind(&oracle).execute(pool).await.unwrap();
    if let Some(ob) = range_ob {
        sqlx::query("delete from inference_markets where orderbook_address = $1").bind(ob).execute(pool).await.unwrap();
    }

    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end, last_reconciled_at)
           values ($1, $1, $1, 1, 'NACKL', $2::numeric, 0::numeric, $3,
                   1700000100, 1700000200, 1700000300, 1700000400, now())
           returning id"#,
    )
    .bind(&pmp)
    .bind(event_id)
    .bind(format!("0:{tag}_book"))
    .fetch_one(pool)
    .await
    .expect("insert market");
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size, min_notional)
           values ($1, $2, 1, 'YES', $3, 2, 2, '0.01', '0.01', '1.00')"#,
    )
    .bind(market_id)
    .bind(&pmp)
    .bind(format!("{tag}-YES"))
    .execute(pool)
    .await
    .expect("insert outcome");

    let oracle_id: i64 = sqlx::query_scalar(
        "insert into oracles (name, address, deploy_msg_id) values ($1, $2, $3) returning id",
    )
    .bind(format!("{tag}-oracle"))
    .bind(&oracle)
    .bind(format!("{tag}-deploy"))
    .fetch_one(pool)
    .await
    .expect("insert oracle");
    let eventlist_id: i64 = sqlx::query_scalar(
        "insert into oracle_event_lists (msg_id, oracle_id, address, description) values ($1, $2, $3, '') returning id",
    )
    .bind(format!("0:{tag}_oel-msg"))
    .bind(oracle_id)
    .bind(format!("0:{tag}_oel"))
    .fetch_one(pool)
    .await
    .expect("insert oel");
    sqlx::query(
        r#"insert into oracle_events
               (eventlist_id, internal_id_in_eventlist, event_name, deadline,
                confirmed_pmp_address, confirmed_at, range_ob_address)
           values ($1, $2::numeric, 'E', 1700000400, $3, now(), $4)"#,
    )
    .bind(eventlist_id)
    .bind(event_id)
    .bind(&pmp)
    .bind(range_ob)
    .execute(pool)
    .await
    .expect("insert oracle_event");

    if let (Some(ob), model) = (range_ob, model_ref) {
        sqlx::query(
            "insert into inference_markets (orderbook_address, model_ref) values ($1, $2)",
        )
        .bind(ob)
        .bind(model)
        .execute(pool)
        .await
        .expect("insert inference_market");
    }
}

async fn one(pool: &PgPool, tag: &str) -> dodex_domain::Market {
    let repo = PostgresReadModelRepository::new(pool.clone());
    let req = MarketsRequest::One {
        market_address: MarketAddress(format!("0:{tag}_pmp")),
        now: 1_700_000_150,
    };
    repo.list_markets(&req).await.expect("list").markets.into_iter().next().expect("one market")
}

#[tokio::test]
async fn range_market_exposes_resolves_from_with_model() {
    let Some(pool) = setup().await else { return };
    let ob = "0:resolves_from_with_model_infbook";
    seed(&pool, "rf_with_model", Some(ob), Some("qwen--qwen3--32b"), 42).await;

    let market = one(&pool, "rf_with_model").await;
    let rf = market.resolves_from.as_ref().expect("resolvesFrom present for a range market");
    assert_eq!(rf.inference_order_book_address, ob);
    assert_eq!(rf.model.as_deref(), Some("qwen--qwen3--32b"));
    assert!(matches!(rf.metric, ResolvesFromMetric::WeeklyMedianPrice));
}

#[tokio::test]
async fn non_range_market_has_no_resolves_from() {
    let Some(pool) = setup().await else { return };
    seed(&pool, "rf_none", None, None, 43).await;
    let market = one(&pool, "rf_none").await;
    assert!(market.resolves_from.is_none(), "plain event market must carry resolvesFrom = null");
}

#[tokio::test]
async fn range_market_without_reconciled_inference_book_degrades_model_to_none() {
    let Some(pool) = setup().await else { return };
    let ob = "0:resolves_from_unreconciled_infbook";
    // range_ob set, but NO inference_markets row for it.
    seed(&pool, "rf_degrade", Some(ob), None, 44).await;

    let market = one(&pool, "rf_degrade").await;
    let rf = market.resolves_from.as_ref().expect("market is not hidden when inference book missing");
    assert_eq!(rf.inference_order_book_address, ob);
    assert!(rf.model.is_none(), "model degrades to null when the inference book is unreconciled");
}

#[tokio::test]
async fn resolves_from_filter_matches_only_that_inference_book() {
    let Some(pool) = setup().await else { return };
    let ob_a = "0:resolves_from_filter_book_a";
    let ob_b = "0:resolves_from_filter_book_b";
    seed(&pool, "rf_filter_a", Some(ob_a), Some("model-a"), 45).await;
    seed(&pool, "rf_filter_b", Some(ob_b), Some("model-b"), 46).await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let req = MarketsRequest::Listing(MarketsListing {
        filter: MarketsFilter { resolves_from: Some(ob_a.to_string()), ..MarketsFilter::default() },
        sort: MarketsSort::CreatedAtDesc,
        cursor: None,
        limit: 100,
        now: 1_700_000_150,
    });
    let page = repo.list_markets(&req).await.expect("listing");

    let a = page.markets.iter().any(|m| m.market_address.0 == "0:rf_filter_a_pmp");
    let b = page.markets.iter().any(|m| m.market_address.0 == "0:rf_filter_b_pmp");
    assert!(a, "?resolvesFrom=ob_a must include the market settling from ob_a");
    assert!(!b, "?resolvesFrom=ob_a must exclude the market settling from ob_b");
}
