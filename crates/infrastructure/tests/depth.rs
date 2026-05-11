// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::get_depth focused on the
// "OrderBook not deployed yet" empty-book contract documented in
// services/api/README.md. Gated on TEST_DATABASE_URL — see
// crates/infrastructure/tests/reprojection.rs for the docker-compose harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup() -> Option<PgPool> {
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

async fn purge_market(pool: &PgPool, pmp_address: &str, symbol: &str) {
    sqlx::query("delete from market_outcomes where symbol = $1")
        .bind(symbol)
        .execute(pool)
        .await
        .expect("purge market_outcomes");
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp_address)
        .execute(pool)
        .await
        .expect("purge markets");
}

async fn insert_market_with_outcome(
    pool: &PgPool,
    pmp_address: &str,
    symbol: &str,
    orderbook_address: Option<&str>,
) {
    let market_id: i64 = sqlx::query_scalar(
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
    .bind(orderbook_address)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   2, 2, '0.01', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp_address)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

#[tokio::test]
async fn null_orderbook_address_returns_empty_book() {
    // services/api/README.md contracts that an existing market whose
    // orderbook address has not been resolved must yield an empty book with
    // lastUpdateId = 0. markets.orderbook_address is nullable in migration
    // 0001, so a literal NULL must take the same path as a blank string -
    // not surface as a sqlx decode error.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_null_orderbook_pmp";
    let symbol = "DEPTH_NULL_ORDERBOOK_YES";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, None).await;

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth must not error on NULL orderbook_address");

    assert_eq!(depth.last_update_id, 0);
    assert!(depth.bids.is_empty(), "bids must be empty for not-yet-deployed orderbook");
    assert!(depth.asks.is_empty(), "asks must be empty for not-yet-deployed orderbook");
}

#[tokio::test]
async fn blank_orderbook_address_returns_empty_book() {
    // Same contract as the NULL case but for a whitespace-only string. Kept
    // separate to pin both branches of `Option::and_then(filter_orderbook)`.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_blank_orderbook_pmp";
    let symbol = "DEPTH_BLANK_ORDERBOOK_YES";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, Some("   ")).await;

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth must not error on blank orderbook_address");

    assert_eq!(depth.last_update_id, 0);
    assert!(depth.bids.is_empty());
    assert!(depth.asks.is_empty());
}

#[tokio::test]
async fn depth_returns_human_decimal_levels() {
    // live_orders stores raw uint128/uint256 integers as the contract emits
    // them; the API spec (docs/api-spec.md:54, :440) requires DECIMAL strings
    // ("0.614", "100.00"). Pin the scaling through (price|quantity)_precision
    // from market_outcomes so a regression to raw `price::text` would fail.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_decimal_levels_pmp";
    let symbol = "DEPTH_DECIMAL_LEVELS_YES";
    let orderbook = "0:depth_decimal_levels_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;

    // market_outcomes inserted by insert_market_with_outcome uses
    // price_precision = 2, quantity_precision = 2. raw 614 -> "6.14",
    // raw 10000 -> "100.00". Two bids (so depth has something to sort) plus
    // one ask to cover both branches.
    let levels = [
        (true, "614", "10000"), // bid: price 6.14, qty 100.00
        (true, "613", "2550"),  // bid: price 6.13, qty 25.50
        (false, "616", "5000"), // ask: price 6.16, qty 50.00
    ];
    for (idx, (is_buy, price, amount)) in levels.iter().enumerate() {
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_remaining, status, last_event_lt)
               values ($1, $2::numeric, 1, $3, $4::numeric, $5::numeric, 'OPEN', $6)"#,
        )
        .bind(orderbook)
        .bind(idx as i64 + 1)
        .bind(*is_buy)
        .bind(*price)
        .bind(*amount)
        .bind(1_700_000_000_i64 + idx as i64)
        .execute(&pool)
        .await
        .expect("insert live_orders");
    }

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth");

    assert_eq!(depth.bids.len(), 2);
    assert_eq!(depth.asks.len(), 1);
    // Bids descending by price.
    assert_eq!(depth.bids[0].price, "6.14");
    assert_eq!(depth.bids[0].quantity, "100.00");
    assert_eq!(depth.bids[1].price, "6.13");
    assert_eq!(depth.bids[1].quantity, "25.50");
    assert_eq!(depth.asks[0].price, "6.16");
    assert_eq!(depth.asks[0].quantity, "50.00");
}

#[tokio::test]
async fn last_update_id_is_scoped_per_outcome() {
    // Regression: lastUpdateId used to aggregate `max(last_event_lt)` across
    // the whole orderbook, so a quiet outcome would surface the sequence
    // number from a sibling outcome's activity. The fix scopes the aggregate
    // to (orderbook_address, outcome_id); this test pins that contract.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_last_update_per_outcome_pmp";
    let yes_symbol = "DEPTH_LAST_UPDATE_PER_OUTCOME_YES";
    let no_symbol = "DEPTH_LAST_UPDATE_PER_OUTCOME_NO";
    let orderbook = "0:depth_last_update_per_outcome_book";

    // Purge both outcome rows and any leftover orders before re-seeding.
    for symbol in [yes_symbol, no_symbol] {
        purge_market(&pool, pmp, symbol).await;
    }
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");

    // Insert the market plus the YES outcome (outcome_id = 1) via the
    // existing helper, then add a sibling NO outcome (outcome_id = 2) on the
    // same orderbook.
    insert_market_with_outcome(&pool, pmp, yes_symbol, Some(orderbook)).await;
    let market_id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(pmp)
        .fetch_one(&pool)
        .await
        .expect("fetch market id");
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 2, 'NO', $3,
                   2, 2, '0.01', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(no_symbol)
    .execute(&pool)
    .await
    .expect("insert NO outcome");

    // Only the NO outcome (outcome_id = 2) has activity. If the aggregate
    // leaks across outcomes, YES will pick up last_event_lt = 1_800_000_000.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_remaining, status, last_event_lt)
           values ($1, 1::numeric, 2, true, 500::numeric, 100::numeric,
                   'OPEN', 1800000000)"#,
    )
    .bind(orderbook)
    .execute(&pool)
    .await
    .expect("insert NO-side order");

    let yes_depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(yes_symbol.into()), 100)
        .await
        .expect("get_depth YES");
    assert_eq!(
        yes_depth.last_update_id, 0,
        "YES outcome has no orders, so its lastUpdateId must not borrow from NO"
    );
    assert!(yes_depth.bids.is_empty());
    assert!(yes_depth.asks.is_empty());

    let no_depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(no_symbol.into()), 100)
        .await
        .expect("get_depth NO");
    assert_eq!(no_depth.last_update_id, 1_800_000_000);
}
