// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for PostgresReadModelRepository::get_depth focused on the
// "OrderBook not deployed yet" empty-book contract documented in
// services/api/README.md. Gated on TEST_DATABASE_URL — see
// crates/infrastructure/tests/reprojection.rs for the docker-compose harness.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_domain::DomainError;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;
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
async fn blank_orderbook_address_fails_closed() {
    // Migration-0014 CHECK forbids NULL `orderbook_address` on reconciled
    // rows. A whitespace-only string slips past the constraint but still
    // violates the depth invariant: a reconciled market must have a usable
    // orderbook address. The depth handler must surface this as
    // `MarketInconsistent` (HTTP 503), not silently serve an empty book.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_blank_orderbook_pmp";
    let symbol = "DEPTH_BLANK_ORDERBOOK_YES";
    let blank_orderbook = "   ";
    purge_market(&pool, pmp, symbol).await;
    // The blank-orderbook value is shared with
    // `markets_status.rs::blank_orderbook_address_fails_closed_in_markets`
    // (both tests pin the same CHECK-allows-whitespace gap). The
    // `markets_orderbook_address_unique` partial index
    // collides whichever test's row was left in the DB by the prior run.
    // Purging by orderbook_address here scrubs any sibling residue.
    sqlx::query("delete from markets where orderbook_address = $1")
        .bind(blank_orderbook)
        .execute(&pool)
        .await
        .expect("purge blank-orderbook residue");
    insert_market_with_outcome(&pool, pmp, symbol, Some(blank_orderbook)).await;

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("blank orderbook_address on a reconciled market must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn fresh_orderbook_without_orders_returns_empty_book() {
    // Legitimate empty-book case: a reconciled market has its deterministic
    // `orderbook_address` stamped on the first pass, but no
    // `OrderBook.OrderPlaced` events have landed yet. The depth response shape
    // (empty bids/asks, `lastUpdateId = ""`) must still work — it stems from
    // "no rows in live_orders" rather than "no address resolved".
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_fresh_orderbook_pmp";
    let symbol = "DEPTH_FRESH_ORDERBOOK_YES";
    let orderbook = "0:depth_fresh_orderbook_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("fresh orderbook with no orders must serve empty book");

    assert_eq!(depth.last_update_id, "");
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
    // Market: price_precision=2, quantity_precision=2, USDC decimals=6.
    // live_orders mirrors the chain: price in basis points, amount in atoms.
    //   6100 bps -> 0.61 ; 100_000_000 atoms -> 100.00.
    // Two bids (so depth has something to sort) plus one ask.
    let levels = [
        (true, "6100", "100000000"), // bid: price 0.61, qty 100.00
        (true, "6000", "25500000"),  // bid: price 0.60, qty 25.50
        (false, "6200", "50000000"), // ask: price 0.62, qty 50.00
    ];
    for (idx, (is_buy, price, amount)) in levels.iter().enumerate() {
        let chain_order = format!("5f8000000000{:06}", idx);
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_initial, amount_remaining, status,
                    last_chain_order, placed_chain_order)
               values ($1, $2::numeric, 1, $3, $4::numeric,
                       $5::numeric, $5::numeric, 'OPEN',
                       $6, $6)"#,
        )
        .bind(orderbook)
        .bind(idx as i64 + 1)
        .bind(*is_buy)
        .bind(*price)
        .bind(*amount)
        .bind(&chain_order)
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
    assert_eq!(depth.bids[0].price, "0.61");
    assert_eq!(depth.bids[0].quantity, "100.00");
    assert_eq!(depth.bids[1].price, "0.60");
    assert_eq!(depth.bids[1].quantity, "25.50");
    assert_eq!(depth.asks[0].price, "0.62");
    assert_eq!(depth.asks[0].quantity, "50.00");
}

#[tokio::test]
async fn last_update_id_is_scoped_per_outcome() {
    // Regression: lastUpdateId used to aggregate `max(last_chain_order)`
    // across the whole orderbook, so a quiet outcome would surface the
    // cursor from a sibling outcome's activity. The fix scopes the
    // aggregate to (orderbook_address, outcome_id); this test pins that
    // contract.
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
    // leaks across outcomes, YES will pick up the NO-side chain_order.
    let no_chain_order = "5f8000000000000bb8";
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, 1::numeric, 2, true, 500::numeric,
                   1000000::numeric, 1000000::numeric, 'OPEN',
                   $2, $2)"#,
    )
    .bind(orderbook)
    .bind(no_chain_order)
    .execute(&pool)
    .await
    .expect("insert NO-side order");

    let yes_depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(yes_symbol.into()), 100)
        .await
        .expect("get_depth YES");
    assert_eq!(
        yes_depth.last_update_id, "",
        "YES outcome has no orders, so its lastUpdateId must not borrow from NO"
    );
    assert!(yes_depth.bids.is_empty());
    assert!(yes_depth.asks.is_empty());

    let no_depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(no_symbol.into()), 100)
        .await
        .expect("get_depth NO");
    assert_eq!(no_depth.last_update_id, no_chain_order);
}

#[tokio::test]
async fn depth_fails_closed_when_price_precision_exceeds_bps_scale() {
    // price_precision above the basis-point scale (PRICE_BPS_DECIMALS = 4)
    // clears the range guard but asks for finer price detail than the chain
    // carries. The descale step must fail closed (503), never round.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_price_precision_overflow_pmp";
    let symbol = "DEPTH_PP_OVERFLOW_YES";
    let orderbook = "0:depth_pp_overflow_book";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;
    sqlx::query("update market_outcomes set price_precision = 5 where symbol = $1")
        .bind(symbol)
        .execute(&pool)
        .await
        .expect("widen price_precision past the bps scale");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("price_precision finer than the bps scale must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn depth_fails_closed_when_quantity_precision_exceeds_decimals() {
    // quantity_precision above the quote asset's decimals (USDC = 6) asks for
    // more amount detail than the chain carries — a negative drop, fail closed.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_qty_precision_overflow_pmp";
    let symbol = "DEPTH_QP_OVERFLOW_YES";
    let orderbook = "0:depth_qp_overflow_book";
    purge_market(&pool, pmp, symbol).await;
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;
    sqlx::query("update market_outcomes set quantity_precision = 7 where symbol = $1")
        .bind(symbol)
        .execute(&pool)
        .await
        .expect("widen quantity_precision past the quote decimals");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("quantity_precision finer than quote decimals must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn depth_fails_closed_on_off_grid_price() {
    // Both precisions are in range (so the negative-drop guards pass), but a
    // stored bid price sits off the chain tick grid: at price_precision=2 the
    // descale drops two basis-point digits, and "6105" leaves a nonzero "5".
    // descale_pow10 must reject it at the per-level step rather than re-grid —
    // a depth snapshot with silently rounded levels would misprice the book.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_off_grid_price_pmp";
    let symbol = "DEPTH_OFF_GRID_PRICE_YES";
    let orderbook = "0:depth_off_grid_price_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;

    // amount is on-grid (100_000_000 atoms drops "0000" at amount drop 4); only
    // the price is off the tick grid, so the failure is attributable to it.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, 1::numeric, 1, true, 6105::numeric,
                   100000000::numeric, 100000000::numeric, 'OPEN',
                   '5f800000000000', '5f800000000000')"#,
    )
    .bind(orderbook)
    .execute(&pool)
    .await
    .expect("insert off-grid live_orders");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("off-grid chain price must fail the depth request closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn depth_fails_closed_on_off_grid_amount() {
    // The amount axis is held to the lot grid just as the price axis is held to
    // the tick grid: price is on the tick grid here but the level amount is off
    // the lot grid (last atom digit nonzero), so the per-level quantity descale
    // must reject it rather than re-grid the book.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_off_grid_amount_pmp";
    let symbol = "DEPTH_OFF_GRID_AMOUNT_YES";
    let orderbook = "0:depth_off_grid_amount_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;

    // price 6100 is on the grid (drops "00" at price drop 2); amount
    // 100_000_001 leaves a nonzero "1" after the drop-4 lot descale.
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, status,
                last_chain_order, placed_chain_order)
           values ($1, 1::numeric, 1, true, 6100::numeric,
                   100000001::numeric, 100000001::numeric, 'OPEN',
                   '5f800000000000', '5f800000000000')"#,
    )
    .bind(orderbook)
    .execute(&pool)
    .await
    .expect("insert off-grid live_orders");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("off-grid level amount must fail the depth request closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn depth_aggregates_across_owners_into_single_level() {
    // Depth is global (all open orders) and per-price aggregated. Two
    // regressions could deanonymize it:
    //   (1) adding `owner_pn_address` to GROUP BY would split same-price
    //       orders by owner, letting a client count distinct owners at a
    //       price level by counting same-price entries;
    //   (2) adding `owner_pn_address IS NOT NULL` to WHERE would hide rows
    //       in the eventual-consistency window between OrderBook.OrderPlaced
    //       and PrivateNote.OrderPlacedConfirmed (read-api.md §
    //       "Visibility / eventual consistency").
    // This test pins both invariants: three bid orders at the same price —
    // one with owner A, one with owner B, one with NULL owner — must
    // collapse into a single PriceLevel whose quantity is the sum.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_cross_owner_aggregation_pmp";
    let symbol = "DEPTH_CROSS_OWNER_AGG_YES";
    let orderbook = "0:depth_cross_owner_aggregation_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");
    insert_market_with_outcome(&pool, pmp, symbol, Some(orderbook)).await;

    // Three bids at the SAME chain price (5000 bps → "0.50"), three owner
    // states (amounts in USDC atoms, decimals=6, quantity_precision=2):
    //   owner A:    1_000_000 atoms → "1.00"
    //   owner B:    2_000_000 atoms → "2.00"
    //   NULL owner: 3_000_000 atoms → "3.00"
    // Aggregated quantity = 6_000_000 atoms → "6.00".
    let rows: [(i64, Option<&str>, &str); 3] = [
        (1, Some("0:depth_cross_owner_a"), "1000000"),
        (2, Some("0:depth_cross_owner_b"), "2000000"),
        (3, None, "3000000"),
    ];
    for (order_id, owner, amount) in rows {
        let chain_order = format!("5f8000000000{:06}", order_id);
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_initial, amount_remaining, owner_pn_address, status,
                    last_chain_order, placed_chain_order)
               values ($1, $2::numeric, 1, true, 5000::numeric,
                       $3::numeric, $3::numeric, $4, 'OPEN',
                       $5, $5)"#,
        )
        .bind(orderbook)
        .bind(order_id)
        .bind(amount)
        .bind(owner)
        .bind(&chain_order)
        .execute(&pool)
        .await
        .expect("insert live_orders");
    }

    let depth = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect("get_depth");

    assert_eq!(
        depth.bids.len(),
        1,
        "same-price orders from different owners must aggregate into one level"
    );
    assert_eq!(depth.bids[0].price, "0.50");
    assert_eq!(
        depth.bids[0].quantity, "6.00",
        "level quantity must include ALL same-price open orders \
         (owner A 1.00 + owner B 2.00 + NULL-owner 3.00 = 6.00); a regression \
         that filters by owner or groups by owner would change this sum"
    );
    assert!(depth.asks.is_empty());
}

#[tokio::test]
async fn negative_price_precision_fails_closed() {
    // `market_outcomes.price_precision` is `integer` (signed) with no CHECK
    // constraint. A negative value coming through the depth path is
    // read-model corruption — the response must surface as
    // MarketInconsistent rather than silently coerce the scale to zero
    // and serve raw integer prices to the client.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_neg_price_precision_pmp";
    let symbol = "DEPTH_NEG_PRICE_PRECISION_YES";
    let orderbook = "0:depth_neg_price_precision_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");

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
    .bind(pmp)
    .bind(orderbook)
    .fetch_one(&pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   -1, 2, '0.01', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(&pool)
    .await
    .expect("insert outcome with negative precision");

    // Need at least one live_orders row so the precision branch runs;
    // an empty book short-circuits before scaling.
    sqlx::query(
        r#"insert into live_orders (
              orderbook_address, order_id, outcome_id, is_buy, price,
              amount_initial, amount_remaining, status, last_chain_order,
              placed_chain_order, owner_pn_address)
           values ($1, 1::numeric, 1, true, '100'::numeric, '1'::numeric, '1'::numeric,
                   'OPEN', '0', '0', '0:owner')"#,
    )
    .bind(orderbook)
    .execute(&pool)
    .await
    .expect("insert live_orders");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("negative precision must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}

#[tokio::test]
async fn oversized_price_precision_fails_closed() {
    // The scale itself feeds scale_uint_to_decimal's "0".repeat(scale),
    // so an unbounded positive value would OOM the API process on the
    // first scaled level. The depth path mirrors precision_to_scale's
    // MAX_DECIMAL_PRECISION cap (= NUMERIC(38, …)): values above the
    // cap lift to MarketInconsistent.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let pmp = "0:depth_oversize_price_precision_pmp";
    let symbol = "DEPTH_OVERSIZE_PRICE_PRECISION_YES";
    let orderbook = "0:depth_oversize_price_precision_book";
    purge_market(&pool, pmp, symbol).await;
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(orderbook)
        .execute(&pool)
        .await
        .expect("purge live_orders");

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
    .bind(pmp)
    .bind(orderbook)
    .fetch_one(&pool)
    .await
    .expect("insert market");

    // 100_000_000 fits in i32 and would survive a try_from-only guard, but
    // would expand `"0".repeat(100_000_000)` per level — must reject before
    // touching scale_uint_to_decimal.
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   100000000, 2, '0.01', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(&pool)
    .await
    .expect("insert outcome with oversized precision");

    sqlx::query(
        r#"insert into live_orders (
              orderbook_address, order_id, outcome_id, is_buy, price,
              amount_initial, amount_remaining, status, last_chain_order,
              placed_chain_order, owner_pn_address)
           values ($1, 1::numeric, 1, true, '100'::numeric, '1'::numeric, '1'::numeric,
                   'OPEN', '0', '0', '0:owner')"#,
    )
    .bind(orderbook)
    .execute(&pool)
    .await
    .expect("insert live_orders");

    let err = repo
        .get_depth(&MarketAddress(pmp.into()), &Symbol(symbol.into()), 100)
        .await
        .expect_err("precision above MAX_DECIMAL_PRECISION must fail closed");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError surfaced");
    assert_eq!(*domain, DomainError::MarketInconsistent);
}
