// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for account-scoped order reads via
// `PostgresReadModelRepository::list_orders`. Gated on TEST_DATABASE_URL
// like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::OrderStatusSet;
use dodex_application::OrdersCursor;
use dodex_application::OrdersMarketFilter;
use dodex_application::OrdersQuery;
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

struct Scope {
    owner: String,
    other_owner: String,
    pmp_yes: String,
    symbol_yes: String,
    book_yes: String,
    pmp_no: String,
    symbol_no: String,
    book_no: String,
}

impl Scope {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self {
            owner: format!("0:orders_{id}_owner"),
            other_owner: format!("0:orders_{id}_other"),
            pmp_yes: format!("0:orders_{id}_pmp_yes"),
            symbol_yes: format!("ORDERS_{id}_YES"),
            book_yes: format!("0:orders_{id}_book_yes"),
            pmp_no: format!("0:orders_{id}_pmp_no"),
            symbol_no: format!("ORDERS_{id}_NO"),
            book_no: format!("0:orders_{id}_book_no"),
        }
    }

    async fn cleanup(&self, pool: &PgPool) {
        for book in [&self.book_yes, &self.book_no] {
            sqlx::query("delete from live_orders where orderbook_address = $1")
                .bind(book)
                .execute(pool)
                .await
                .expect("purge live_orders");
        }
        for symbol in [&self.symbol_yes, &self.symbol_no] {
            sqlx::query("delete from market_outcomes where symbol = $1")
                .bind(symbol)
                .execute(pool)
                .await
                .expect("purge market_outcomes");
        }
        for pmp in [&self.pmp_yes, &self.pmp_no] {
            sqlx::query("delete from markets where pmp_address = $1")
                .bind(pmp)
                .execute(pool)
                .await
                .expect("purge markets");
        }
    }
}

/// Insert a market row with `last_reconciled_at = now()`.
async fn insert_market(pool: &PgPool, pmp: &str, symbol: &str, book: &str) {
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
    .bind(book)
    .fetch_one(pool)
    .await
    .expect("insert market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   3, 2, '0.001', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

/// Insert a market row WITHOUT `last_reconciled_at` (NULL). Used by test
/// `unreconciled_market_pair_returns_invalid_market_or_symbol`.
async fn insert_market_unreconciled(pool: &PgPool, pmp: &str, symbol: &str, book: &str) {
    let market_id: i64 = sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400)
           returning id"#,
    )
    .bind(pmp)
    .bind(book)
    .fetch_one(pool)
    .await
    .expect("insert unreconciled market");

    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional, max_batch_size)
           values ($1, $2, 1, 'YES', $3,
                   3, 2, '0.001', '0.01',
                   '1.00', 100)"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(symbol)
    .execute(pool)
    .await
    .expect("insert market_outcomes (unreconciled)");
}

/// General-purpose order inserter. `status` must be one of 'OPEN', 'FILLED',
/// 'CANCELLED' (the storage-side British spelling).
#[allow(clippy::too_many_arguments)]
async fn insert_order(
    pool: &PgPool,
    book: &str,
    order_id: i64,
    owner: Option<&str>,
    price: &str,
    amount_initial: &str,
    amount_remaining: &str,
    status: &str,
    created_sec: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, true, $3::numeric,
                   $4::numeric, $5::numeric, $6, $7, $8,
                   $9,
                   $10,
                   to_timestamp($11::bigint), to_timestamp(($11 + 1)::bigint),
                   to_timestamp($11::bigint), to_timestamp(($11 + 1)::bigint))"#,
    )
    .bind(book)
    .bind(order_id)
    .bind(price)
    .bind(amount_initial)
    .bind(amount_remaining)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(status)
    .bind(format!("5f800000000000{:06}", order_id))
    .bind(placed_chain_order)
    .bind(created_sec)
    .execute(pool)
    .await
    .expect("insert live_orders");
}

/// Variant of [`insert_order`] that lets the caller pick `is_buy`. The
/// default helper hardcodes BUY because most tests don't care about
/// side, but the `is_buy` → `OrderSide` mapping is load-bearing and
/// must be pinned by at least one test.
#[allow(clippy::too_many_arguments)]
async fn insert_order_with_side(
    pool: &PgPool,
    book: &str,
    order_id: i64,
    owner: Option<&str>,
    is_buy: bool,
    price: &str,
    amount_initial: &str,
    amount_remaining: &str,
    status: &str,
    created_sec: i64,
    placed_chain_order: &str,
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, $2::numeric, 1, $3, $4::numeric,
                   $5::numeric, $6::numeric, $7, $8, $9,
                   $10,
                   $11,
                   to_timestamp($12::bigint), to_timestamp(($12 + 1)::bigint),
                   to_timestamp($12::bigint), to_timestamp(($12 + 1)::bigint))"#,
    )
    .bind(book)
    .bind(order_id)
    .bind(is_buy)
    .bind(price)
    .bind(amount_initial)
    .bind(amount_remaining)
    .bind(owner)
    .bind(format!("client-{order_id}"))
    .bind(status)
    .bind(format!("5f800000000000{:06}", order_id))
    .bind(placed_chain_order)
    .bind(created_sec)
    .execute(pool)
    .await
    .expect("insert live_orders with side");
}

/// Build a default `OrdersQuery` with a custom owner — no market filter, no
/// cursor, no status filter, limit 100.
fn query_all(owner: &str) -> OrdersQuery {
    OrdersQuery {
        owner_pn_address: owner.to_string(),
        market: None,
        status: OrderStatusSet::all(),
        limit: 100,
        cursor: None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Owner scoping across all statuses.
/// Seed owner-1 with one row per status (NEW=OPEN full, PARTIALLY_FILLED=OPEN
/// partial, FILLED, CANCELLED). Seed owner-2 NEW row that must NOT appear.
/// Assert: page returns exactly 4 owner-1 rows, in DESC placed_chain_order.
#[tokio::test]
async fn returns_only_owner_rows_across_all_statuses() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // owner-1: NEW (OPEN, amount_remaining == amount_initial)
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    // owner-1: PARTIALLY_FILLED (OPEN, 0 < amount_remaining < amount_initial)
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "1000",
        "1000",
        "500",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    // owner-1: FILLED
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_003,
        "003",
    )
    .await;
    // owner-1: CANCELLED
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_004,
        "004",
    )
    .await;
    // owner-2: NEW — must NOT appear in owner-1 results
    insert_order(
        &pool,
        &scope.book_yes,
        5,
        Some(&scope.other_owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_005,
        "005",
    )
    .await;

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert_eq!(page.orders.len(), 4, "exactly four owner-1 rows");
    // DESC placed_chain_order: "004" > "003" > "002" > "001"
    let ids: Vec<&str> = page.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids, vec!["4", "3", "2", "1"], "DESC placed_chain_order order");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Default status (is_all) returns all non-rejected buckets.
/// Seed owner-1 with NEW + PARTIALLY_FILLED + FILLED + CANCELLED.
/// No status filter. Assert: all four rows returned.
#[tokio::test]
async fn default_status_returns_all_non_rejected_buckets() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "1000",
        "1000",
        "500",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_003,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_004,
        "004",
    )
    .await;

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert_eq!(page.orders.len(), 4, "all four rows returned by default filter");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Empty owner baseline: a new account with no attributed rows receives an
/// empty terminal page, without market joins or cursor state getting involved.
#[tokio::test]
async fn owner_with_no_orders_returns_empty_page() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert!(page.orders.is_empty());
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Rows missing either chain timestamp must be filtered in SQL before the
/// mapper decodes them into non-null `time` / `updateTime` fields. This
/// covers both duplicated `list_orders` SQL blocks.
#[tokio::test]
async fn filters_rows_missing_chain_timestamps_before_decoding() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for i in 1_i64..=3 {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "1000",
            "1000",
            "1000",
            "OPEN",
            1_700_000_000 + i,
            &format!("{:03}", i),
        )
        .await;
    }

    sqlx::query(
        "update live_orders set chain_created_at = null
              where orderbook_address = $1 and order_id = 2::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("clear chain_created_at");

    sqlx::query(
        "update live_orders set chain_updated_at = null
              where orderbook_address = $1 and order_id = 3::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("clear chain_updated_at");

    let page_all = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders all");
    let ids_all: Vec<&str> = page_all.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids_all, vec!["1"], "unfiltered query drops both NULL timestamp rows");

    let page_market = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: Some(OrdersMarketFilter {
                market_address: MarketAddress(scope.pmp_yes.clone()),
                symbol: Symbol(scope.symbol_yes.clone()),
            }),
            status: OrderStatusSet::all(),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders market-filtered");
    let ids_market: Vec<&str> = page_market.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids_market, vec!["1"], "market-filtered query drops both NULL timestamp rows");

    scope.cleanup(&pool).await;
}

/// Status CSV filter narrows results, one bucket at a time.
/// Seed NEW + PARTIALLY_FILLED + FILLED + CANCELLED, then query each
/// non-default status in isolation. Pins every disjunct in
/// `build_status_predicate` against the same fixture:
///   - `NEW` → only the row with `amount_remaining == amount_initial`;
///   - `PARTIALLY_FILLED` → only the row with `0 < amount_remaining < amount_initial`;
///   - `FILLED,CANCELED` → only the closed buckets, neither OPEN bucket.
#[tokio::test]
async fn status_csv_filter_narrows_results() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "1000",
        "1000",
        "500",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_003,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_004,
        "004",
    )
    .await;

    // NEW alone — pins `(status='OPEN' AND amount_remaining = amount_initial)`.
    let page_new = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::from_csv(Some("NEW")).expect("valid NEW filter"),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders with NEW filter");
    assert_eq!(page_new.orders.len(), 1, "only the NEW row");
    assert_eq!(page_new.orders[0].order_id, "1");
    assert_eq!(page_new.orders[0].status.as_str(), "NEW");

    // PARTIALLY_FILLED alone — pins `(status='OPEN' AND amount_remaining < amount_initial AND amount_remaining > 0)`.
    let page_pf = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::from_csv(Some("PARTIALLY_FILLED"))
                .expect("valid PARTIALLY_FILLED filter"),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders with PARTIALLY_FILLED filter");
    assert_eq!(page_pf.orders.len(), 1, "only the PARTIALLY_FILLED row");
    assert_eq!(page_pf.orders[0].order_id, "2");
    assert_eq!(page_pf.orders[0].status.as_str(), "PARTIALLY_FILLED");

    // FILLED,CANCELED together — pins the two closed-state predicates,
    // and that the OPEN predicates do NOT leak in.
    let page_closed = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::from_csv(Some("FILLED,CANCELED")).expect("valid CSV"),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders with FILLED,CANCELED filter");
    assert_eq!(page_closed.orders.len(), 2, "only FILLED and CANCELLED rows");
    let statuses: Vec<&str> = page_closed.orders.iter().map(|o| o.status.as_str()).collect();
    // DESC placed_chain_order puts the CANCELLED row (004) before FILLED (003).
    assert_eq!(statuses, vec!["CANCELED", "FILLED"]);
    assert!(page_closed.next_cursor.is_none());

    let page_filled = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::from_csv(Some("FILLED")).expect("valid FILLED filter"),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders with FILLED filter");
    assert_eq!(page_filled.orders.len(), 1, "only the FILLED row");
    assert_eq!(page_filled.orders[0].order_id, "3");
    assert_eq!(page_filled.orders[0].status.as_str(), "FILLED");

    let page_canceled = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::from_csv(Some("CANCELED")).expect("valid CANCELED filter"),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders with CANCELED filter");
    assert_eq!(page_canceled.orders.len(), 1, "only the CANCELED row");
    assert_eq!(page_canceled.orders[0].order_id, "4");
    assert_eq!(page_canceled.orders[0].status.as_str(), "CANCELED");

    scope.cleanup(&pool).await;
}

/// REJECTED filter narrows the result to rows whose stored status is
/// REJECTED. The `live_orders.status` CHECK constraint (see
/// `migrations/0001_initial.sql`) currently excludes `REJECTED`, so
/// against this fixture the filter returns an empty page; the test
/// pins the no-leakage-of-non-REJECTED-rows property.
/// Do NOT attempt to insert a 'REJECTED' row — it would violate the CHECK.
#[tokio::test]
async fn rejected_filter_returns_only_rejected_rows() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Seed the usual four non-rejected rows — none should surface under REJECTED filter.
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "1000",
        "1000",
        "500",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_003,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_004,
        "004",
    )
    .await;

    let status = OrderStatusSet::from_csv(Some("REJECTED")).expect("valid CSV");
    let page = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status,
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list_orders REJECTED filter");

    assert!(page.orders.is_empty(), "no REJECTED rows exist yet");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Canceled partial fill reports non-zero executed_qty.
/// Seed: amount_initial=1000, amount_remaining=300, status='CANCELLED'.
/// quantity_precision=2 → origQty="10.00", executedQty="7.00".
#[tokio::test]
async fn canceled_partial_fill_reports_nonzero_executed_qty() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // amount_initial=1000, amount_remaining=300: executed = 1000 - 300 = 700.
    // quantity_precision=2 → scale by /100: origQty="10.00", executedQty="7.00".
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "300",
        "CANCELLED",
        1_700_000_001,
        "001",
    )
    .await;

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert_eq!(page.orders.len(), 1);
    let order = &page.orders[0];
    assert_eq!(order.status.as_str(), "CANCELED", "public status is CANCELED (American spelling)");
    assert_eq!(order.orig_qty, "10.00", "origQty scaled by 2 decimals");
    assert_eq!(order.executed_qty, "7.00", "executedQty = (1000-300)/100 = 7.00");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// `is_buy` column maps to `OrderSide` per-row, not via a hardcoded constant.
/// Other tests in this file use the BUY-only helper, which leaves the
/// `if row.is_buy { Buy } else { Sell }` branch in `order_from_row`
/// unexercised on the SELL side; this test pins both branches.
#[tokio::test]
async fn order_side_renders_buy_or_sell_per_is_buy_column() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Two rows: one BUY (is_buy=true), one SELL (is_buy=false).
    insert_order_with_side(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        true,
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order_with_side(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        false,
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert_eq!(page.orders.len(), 2);
    let by_id: std::collections::HashMap<&str, &str> = page
        .orders
        .iter()
        .map(|o| {
            (
                o.order_id.as_str(),
                match o.side {
                    dodex_domain::OrderSide::Buy => "BUY",
                    dodex_domain::OrderSide::Sell => "SELL",
                },
            )
        })
        .collect();
    assert_eq!(by_id.get("1").copied(), Some("BUY"), "is_buy=true → BUY");
    assert_eq!(by_id.get("2").copied(), Some("SELL"), "is_buy=false → SELL");

    scope.cleanup(&pool).await;
}

/// Descending placed_chain_order sort.
/// Seed 3 rows with placed_chain_order "001", "002", "003".
/// Assert: response order is "003", "002", "001".
#[tokio::test]
async fn descending_placed_chain_order_sort() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for i in 1_i64..=3 {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "1000",
            "1000",
            "1000",
            "OPEN",
            1_700_000_000 + i,
            &format!("{:03}", i),
        )
        .await;
    }

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");

    assert_eq!(page.orders.len(), 3);
    let placed: Vec<&str> = page.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(placed, vec!["3", "2", "1"], "DESC placed_chain_order: 003 > 002 > 001");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Cursor advances strictly below last returned.
/// Seed 6 rows with placed_chain_order "001".."006".
/// Page 1 limit=4 → "006","005","004","003", next_cursor="003".
/// Page 2 with cursor → "002","001", next_cursor=None.
#[tokio::test]
async fn cursor_advances_strictly_below_last_returned() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for i in 1_i64..=6 {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "1000",
            "1000",
            "1000",
            "OPEN",
            1_700_000_000 + i,
            &format!("{:03}", i),
        )
        .await;
    }

    let page1 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 4,
            cursor: None,
        })
        .await
        .expect("page 1");

    let ids1: Vec<&str> = page1.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids1, vec!["6", "5", "4", "3"], "page 1 DESC");
    let cursor = page1.next_cursor.expect("next_cursor set after partial page");
    assert_eq!(cursor.as_str(), "003", "cursor is the placed_chain_order of the last returned row");

    let page2 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 4,
            cursor: Some(cursor.clone()),
        })
        .await
        .expect("page 2");

    let ids2: Vec<&str> = page2.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids2, vec!["2", "1"], "page 2 completes the set");
    assert!(page2.next_cursor.is_none(), "no further pages");

    scope.cleanup(&pool).await;
}

/// A page whose size exactly equals the remaining row count is terminal:
/// the `LIMIT + 1` lookahead must use `rows.len() > limit`, not `>=`.
#[tokio::test]
async fn limit_equal_to_row_count_has_no_next_cursor() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for i in 1_i64..=3 {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "1000",
            "1000",
            "1000",
            "OPEN",
            1_700_000_000 + i,
            &format!("{:03}", i),
        )
        .await;
    }

    let page = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 3,
            cursor: None,
        })
        .await
        .expect("exact-size page");

    let ids: Vec<&str> = page.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids, vec!["3", "2", "1"]);
    assert!(page.next_cursor.is_none(), "exact-size page is terminal");

    scope.cleanup(&pool).await;
}

/// A cursor below every row in scope returns an empty terminal page, not an
/// error. This lets clients tolerate stale cursors after history compaction
/// or owner-scoped data changes.
#[tokio::test]
async fn out_of_range_cursor_returns_empty_page() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "010",
    )
    .await;

    let page = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 100,
            cursor: Some(OrdersCursor::new("001".into()).expect("valid cursor")),
        })
        .await
        .expect("out-of-range cursor");

    assert!(page.orders.is_empty());
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Client order ids are owner-scoped. Two accounts can reuse the same coid;
/// account order reads must still apply owner_pn_address first.
#[tokio::test]
async fn shared_client_order_id_across_owners_does_not_leak_rows() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.other_owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    sqlx::query(
        "update live_orders set client_order_id = 'shared-coid'
              where orderbook_address = $1 and order_id in (1::numeric, 2::numeric)",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("share client_order_id");

    let page = repo.list_orders(&query_all(&scope.owner)).await.expect("list_orders");
    assert_eq!(page.orders.len(), 1);
    assert_eq!(page.orders[0].order_id, "1");
    assert_eq!(page.orders[0].client_order_id, "shared-coid");

    scope.cleanup(&pool).await;
}

/// Status filter and cursor pagination compose without leakage.
/// Seed six mixed-status rows; filter `FILLED,CANCELED` selects four of
/// them. With limit=2 the SQL must produce a coherent two-page result
/// where the cursor's strict-`<` predicate and the status disjunction
/// AND together correctly — a composition bug (wrong predicate
/// precedence, dropped guard) would either leak OPEN rows or skip a
/// matching row across the page boundary.
#[tokio::test]
async fn status_filter_combines_with_cursor_pagination() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Six rows interleaving OPEN and closed statuses by placed_chain_order.
    // Filter FILLED,CANCELED matches rows 6, 4, 3, 1 (DESC by placed_chain_order).
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_001,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "1000",
        "1000",
        "1000",
        "OPEN",
        1_700_000_002,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_003,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "FILLED",
        1_700_000_004,
        "004",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        5,
        Some(&scope.owner),
        "1000",
        "1000",
        "500",
        "OPEN",
        1_700_000_005,
        "005",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        6,
        Some(&scope.owner),
        "1000",
        "1000",
        "0",
        "CANCELLED",
        1_700_000_006,
        "006",
    )
    .await;

    let status = OrderStatusSet::from_csv(Some("FILLED,CANCELED")).expect("valid CSV");

    // Page 1: limit=2 → DESC tail of the filtered set, "006" then "004".
    let page1 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: status.clone(),
            limit: 2,
            cursor: None,
        })
        .await
        .expect("page 1 filtered");
    let ids1: Vec<&str> = page1.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids1, vec!["6", "4"], "filtered page 1 skips OPEN row 005");
    let cursor = page1.next_cursor.expect("cursor after partial filtered page");
    assert_eq!(cursor.as_str(), "004");

    // Page 2: same filter + cursor → must skip OPEN row 002, return "003" then "001".
    let page2 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status,
            limit: 2,
            cursor: Some(cursor),
        })
        .await
        .expect("page 2 filtered");
    let ids2: Vec<&str> = page2.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids2, vec!["3", "1"], "filtered page 2 skips OPEN row 002");
    let statuses2: Vec<&str> = page2.orders.iter().map(|o| o.status.as_str()).collect();
    assert!(
        statuses2.iter().all(|s| *s == "FILLED" || *s == "CANCELED"),
        "no OPEN rows leak across the boundary: {statuses2:?}"
    );
    assert!(page2.next_cursor.is_none(), "filtered set is exhausted");

    scope.cleanup(&pool).await;
}

/// Cursor stable when open row transitions to FILLED between pages.
/// Seed 4 OPEN rows; fetch page 1 with limit=2 (gets the two highest
/// placed_chain_order). Mutate row at logical position 3 to FILLED.
/// Fetch page 2 with the cursor — default status filter must still surface
/// that row (FILLED is in the default set).
#[tokio::test]
async fn cursor_stable_when_open_row_transitions_to_filled_between_pages() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Rows with placed_chain_order "001".."004".
    for i in 1_i64..=4 {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "1000",
            "1000",
            "1000",
            "OPEN",
            1_700_000_000 + i,
            &format!("{:03}", i),
        )
        .await;
    }

    // Page 1 (limit=2): gets rows "004" and "003" (DESC).
    let page1 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 2,
            cursor: None,
        })
        .await
        .expect("page 1");

    let ids1: Vec<&str> = page1.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids1, vec!["4", "3"], "page 1 has rows 4 and 3");
    let cursor = page1.next_cursor.expect("cursor present");
    assert_eq!(cursor.as_str(), "003");

    // Mutate row 2 (logical position 3 in DESC order) to FILLED.
    // placed_chain_order is NOT changed — cursor stability depends on this.
    sqlx::query(
        "update live_orders set status = 'FILLED', amount_remaining = 0
              where orderbook_address = $1 and order_id = 2::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("transition row 2 to FILLED");

    // Page 2 with cursor — default status (all) must include FILLED row.
    let page2 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 2,
            cursor: Some(cursor),
        })
        .await
        .expect("page 2");

    assert_eq!(page2.orders.len(), 2, "rows 2 and 1 appear on page 2");
    let ids2: Vec<&str> = page2.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(ids2, vec!["2", "1"], "page 2 DESC: row 2 then row 1");
    // Confirm the transitioned row has FILLED status in the response.
    let transitioned = page2.orders.iter().find(|o| o.order_id == "2").unwrap();
    assert_eq!(transitioned.status.as_str(), "FILLED", "transitioned row shows FILLED");
    assert!(page2.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Unknown (marketAddress, symbol) pair returns InvalidMarketOrSymbol.
#[tokio::test]
async fn pair_unknown_returns_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    // Insert one valid market so the test is not vacuously trivial.
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    let err = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: Some(OrdersMarketFilter {
                market_address: MarketAddress("0:nonexistent_market_address".to_string()),
                symbol: Symbol("NONEXISTENT_SYMBOL".to_string()),
            }),
            status: OrderStatusSet::all(),
            limit: 100,
            cursor: None,
        })
        .await
        .expect_err("unknown pair must fail");

    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);

    scope.cleanup(&pool).await;
}

/// Unreconciled market pair returns InvalidMarketOrSymbol.
/// Insert a market row WITHOUT `last_reconciled_at` (NULL).
/// Query with that pair. Assert: DomainError::InvalidMarketOrSymbol.
#[tokio::test]
async fn unreconciled_market_pair_returns_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market_unreconciled(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    let err = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: Some(OrdersMarketFilter {
                market_address: MarketAddress(scope.pmp_yes.clone()),
                symbol: Symbol(scope.symbol_yes.clone()),
            }),
            status: OrderStatusSet::all(),
            limit: 100,
            cursor: None,
        })
        .await
        .expect_err("unreconciled pair must fail");

    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);

    scope.cleanup(&pool).await;
}

/// Cursor stays unstuck when the row at the page tail fails
/// the mapper (projector-bug scenario: OPEN row with `amount_remaining = 0`).
///
/// `list_orders` builds `next_cursor` from the last truncated raw row
/// BEFORE `filter_map(order_from_row)` drops corrupt rows. Without
/// this ordering, a corrupt row at the page boundary would freeze
/// pagination: if `next_cursor` were taken from the last
/// successfully-rendered row instead, the next page would re-query the
/// corrupt row, drop it again, advance the cursor to the same place,
/// and loop indefinitely. This test pins the live behaviour by seeding
/// a corrupt row at the boundary and verifying:
///   - page 1 returns the two healthy rows above the corrupt boundary;
///   - `next_cursor` equals the corrupt row's `placed_chain_order`
///     (so the strict-`<` predicate on page 2 skips past it);
///   - page 2 returns the one row strictly below the corrupt boundary
///     with `next_cursor = None`.
#[tokio::test]
async fn cursor_advances_past_corrupt_row_at_page_tail() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Four rows in ascending placed_chain_order; row 002 is the corrupt one.
    // DESC sort puts the tail of page 1 (limit=3) at row 002.
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_000,
        "001",
    )
    .await;
    // Row 002: OPEN + amount_remaining=0 — projector bug; the mapper
    // logs and returns None, the row is filtered out of the response.
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "12345",
        "1000",
        "0",
        "OPEN",
        1_700_000_001,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_002,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_003,
        "004",
    )
    .await;

    // Page 1: limit=3. DESC raw fetch order: 004, 003, 002, 001.
    // limit+1 lookahead returns all 4; has_more=true; truncate to
    // [004, 003, 002]; next_cursor = "002" (last of truncated).
    // filter_map drops 002 → response orders = [004, 003].
    let page1 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 3,
            cursor: None,
        })
        .await
        .expect("page 1");
    assert_eq!(page1.orders.len(), 2);
    assert_eq!(page1.orders[0].order_id, "4");
    assert_eq!(page1.orders[1].order_id, "3");
    let cursor = page1.next_cursor.expect("next_cursor set");
    assert_eq!(cursor.as_str(), "002");

    // Page 2: strict-< against "002" → returns only "001".
    let page2 = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 3,
            cursor: Some(cursor),
        })
        .await
        .expect("page 2");
    assert_eq!(page2.orders.len(), 1);
    assert_eq!(page2.orders[0].order_id, "1");
    assert!(page2.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

/// Cursor stays unstuck when a page-tail row carries an unrecognised raw
/// status. The CHECK constraint normally prevents this; the test drops and
/// restores it in a private scope to model schema drift/read-model corruption.
#[tokio::test]
async fn cursor_advances_past_unknown_status_row_at_page_tail() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_000,
        "001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_001,
        "002",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_002,
        "003",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_003,
        "004",
    )
    .await;

    sqlx::query("alter table live_orders drop constraint if exists live_orders_status_check")
        .execute(&pool)
        .await
        .expect("drop status check");
    sqlx::query(
        "update live_orders set status = 'SOMETHING_ELSE'
              where orderbook_address = $1 and order_id = 2::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("corrupt status");

    let page1_result = repo
        .list_orders(&OrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            status: OrderStatusSet::all(),
            limit: 3,
            cursor: None,
        })
        .await;

    let page2_result = match &page1_result {
        Ok(page1) => page1.next_cursor.clone().map(|cursor| async {
            repo.list_orders(&OrdersQuery {
                owner_pn_address: scope.owner.clone(),
                market: None,
                status: OrderStatusSet::all(),
                limit: 3,
                cursor: Some(cursor),
            })
            .await
        }),
        Err(_) => None,
    };
    let page2_result = match page2_result {
        Some(fut) => Some(fut.await),
        None => None,
    };

    sqlx::query("update live_orders set status = 'OPEN' where status not in ('OPEN', 'FILLED', 'CANCELLED')")
        .execute(&pool)
        .await
        .expect("clear corrupt status before restoring check");

    sqlx::query(
        "alter table live_orders add constraint live_orders_status_check
             check (status in ('OPEN', 'FILLED', 'CANCELLED'))",
    )
    .execute(&pool)
    .await
    .expect("restore status check");

    let page1 = page1_result.expect("page 1");
    assert_eq!(page1.orders.len(), 2);
    assert_eq!(page1.orders[0].order_id, "4");
    assert_eq!(page1.orders[1].order_id, "3");
    let cursor = page1.next_cursor.expect("next_cursor set");
    assert_eq!(cursor.as_str(), "002");

    let page2 = page2_result.expect("page 2 attempted").expect("page 2");
    assert_eq!(page2.orders.len(), 1);
    assert_eq!(page2.orders[0].order_id, "1");
    assert!(page2.next_cursor.is_none());

    scope.cleanup(&pool).await;
}
