// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// Integration tests for account-scoped open order reads. Gated on
// TEST_DATABASE_URL like the other read-model tests.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::OpenOrdersMarketFilter;
use dodex_application::OpenOrdersQuery;
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
            owner: format!("0:open_orders_{id}_owner"),
            other_owner: format!("0:open_orders_{id}_other"),
            pmp_yes: format!("0:open_orders_{id}_pmp_yes"),
            symbol_yes: format!("OPEN_ORDERS_{id}_YES"),
            book_yes: format!("0:open_orders_{id}_book_yes"),
            pmp_no: format!("0:open_orders_{id}_pmp_no"),
            symbol_no: format!("OPEN_ORDERS_{id}_NO"),
            book_no: format!("0:open_orders_{id}_book_no"),
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

#[tokio::test]
async fn all_markets_open_orders_are_owner_scoped_sorted_and_scaled() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;
    insert_market(&pool, &scope.pmp_no, &scope.symbol_no, &scope.book_no).await;

    insert_order(
        &pool,
        &scope.book_yes,
        2,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_000,
        &format!("5f80000000000000{:06}", 2),
    )
    .await;
    insert_order(
        &pool,
        &scope.book_no,
        1,
        Some(&scope.owner),
        "20000",
        "500",
        "300",
        "OPEN",
        1_700_000_000,
        &format!("5f80000000000000{:06}", 1),
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        3,
        Some(&scope.other_owner),
        "11111",
        "900",
        "900",
        "OPEN",
        1_700_000_001,
        &format!("5f80000000000000{:06}", 3),
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        4,
        Some(&scope.owner),
        "11111",
        "900",
        "0",
        "FILLED",
        1_700_000_002,
        &format!("5f80000000000000{:06}", 4),
    )
    .await;
    insert_order(
        &pool,
        &scope.book_yes,
        5,
        Some(&scope.owner),
        "11111",
        "900",
        "0",
        "CANCELLED",
        1_700_000_003,
        &format!("5f80000000000000{:06}", 5),
    )
    .await;

    let page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list open orders");

    assert_eq!(page.orders.len(), 2);
    assert_eq!(page.orders[0].order_id, "1");
    assert_eq!(page.orders[0].market_address.0, scope.pmp_no);
    assert_eq!(page.orders[0].status.as_str(), "PARTIALLY_FILLED");
    assert_eq!(page.orders[0].price, "20.000");
    assert_eq!(page.orders[0].orig_qty, "5.00");
    assert_eq!(page.orders[0].executed_qty, "2.00");

    assert_eq!(page.orders[1].order_id, "2");
    assert_eq!(page.orders[1].market_address.0, scope.pmp_yes);
    assert_eq!(page.orders[1].status.as_str(), "NEW");
    assert_eq!(page.orders[1].price, "12.345");
    assert_eq!(page.orders[1].orig_qty, "10.00");
    assert_eq!(page.orders[1].executed_qty, "0.00");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn market_symbol_filter_and_empty_results_work() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;
    insert_market(&pool, &scope.pmp_no, &scope.symbol_no, &scope.book_no).await;
    insert_order(
        &pool,
        &scope.book_yes,
        10,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_010,
        &format!("5f80000000000000{:06}", 10),
    )
    .await;
    insert_order(
        &pool,
        &scope.book_no,
        11,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_011,
        &format!("5f80000000000000{:06}", 11),
    )
    .await;

    let filtered = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: Some(OpenOrdersMarketFilter {
                market_address: MarketAddress(scope.pmp_yes.clone()),
                symbol: Symbol(scope.symbol_yes.clone()),
            }),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("filtered open orders");
    assert_eq!(filtered.orders.len(), 1);
    assert_eq!(filtered.orders[0].order_id, "10");
    assert!(filtered.next_cursor.is_none());

    let empty = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.other_owner.clone(),
            market: Some(OpenOrdersMarketFilter {
                market_address: MarketAddress(scope.pmp_yes.clone()),
                symbol: Symbol(scope.symbol_yes.clone()),
            }),
            limit: 100,
            cursor: None,
        })
        .await
        .expect("empty open orders");
    assert!(empty.orders.is_empty());
    assert!(empty.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn unknown_market_symbol_returns_domain_error() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    let err = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: Some(OpenOrdersMarketFilter {
                market_address: MarketAddress(scope.pmp_yes.clone()),
                symbol: Symbol(format!("{}_UNKNOWN", scope.symbol_yes)),
            }),
            limit: 100,
            cursor: None,
        })
        .await
        .expect_err("unknown pair must fail");
    let domain = err.downcast_ref::<DomainError>().expect("typed DomainError");
    assert_eq!(*domain, DomainError::InvalidMarketOrSymbol);

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn cursor_returns_subsequent_page_in_order() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for (i, t) in (1..=3).zip([1_700_000_010, 1_700_000_020, 1_700_000_030]) {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "12345",
            "1000",
            "1000",
            "OPEN",
            t,
            &format!("5f80000000000000{:06}", i),
        )
        .await;
    }

    let first = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("first page");
    assert_eq!(first.orders.len(), 2);
    assert_eq!(first.orders[0].order_id, "1");
    assert_eq!(first.orders[1].order_id, "2");
    let cursor = first.next_cursor.expect("next_cursor present after partial page");

    let second = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: Some(cursor),
        })
        .await
        .expect("second page");
    assert_eq!(second.orders.len(), 1);
    assert_eq!(second.orders[0].order_id, "3");
    assert!(second.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn cursor_stable_under_concurrent_fills() {
    // Mid-pagination rows may leave the OPEN set. Because the cursor is the
    // last returned placed_chain_order and the next-page predicate is strict
    // `>`, the second page must not duplicate already-returned rows or skip
    // rows that remain open past the cursor.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    for (i, t) in (1..=4).zip([1_700_000_010, 1_700_000_020, 1_700_000_030, 1_700_000_040]) {
        insert_order(
            &pool,
            &scope.book_yes,
            i,
            Some(&scope.owner),
            "12345",
            "1000",
            "1000",
            "OPEN",
            t,
            &format!("5f80000000000000{:06}", i),
        )
        .await;
    }

    let first = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("first page");
    assert_eq!(first.orders.len(), 2);
    assert_eq!(first.orders[0].order_id, "1");
    assert_eq!(first.orders[1].order_id, "2");
    let cursor = first.next_cursor.expect("next_cursor present");

    // Between pages, order 3 fully fills and order 2 (already returned) cancels.
    sqlx::query(
        "update live_orders set status = 'FILLED', amount_remaining = 0
              where orderbook_address = $1 and order_id = 3::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("fill order 3");
    sqlx::query(
        "update live_orders set status = 'CANCELLED', amount_remaining = 0
              where orderbook_address = $1 and order_id = 2::numeric",
    )
    .bind(&scope.book_yes)
    .execute(&pool)
    .await
    .expect("cancel order 2");

    let second = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: Some(cursor),
        })
        .await
        .expect("second page");
    assert_eq!(second.orders.len(), 1, "only order 4 remains open past the cursor");
    assert_eq!(second.orders[0].order_id, "4");
    assert!(second.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn repo_returns_empty_page_for_limit_zero() {
    // limit-range validation lives in the use case, not the repo. This test
    // pins the repo's behaviour when supplied `limit = 0` directly: it issues
    // a sane SQL (no rows, no next_cursor). The use-case bound check that
    // rejects `limit = 0` with -1102 before reaching the repo is asserted in
    // the HTTP layer tests.
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
        1_700_000_010,
        &format!("5f80000000000000{:06}", 1),
    )
    .await;

    let page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 0,
            cursor: None,
        })
        .await
        .expect("limit=0 yields a clean page");
    assert!(page.orders.is_empty());
    // The HTTP layer rejects `limit = 0` before reaching the repo; this test
    // just pins the repo's behaviour for the unreachable-in-prod case. With
    // limit 0 the SQL still fetches 1 row to detect "more available", but
    // `truncate(0)` then empties the result and `.last()` returns `None`, so
    // `next_cursor` is `None` regardless of whether matching rows existed.
    assert!(page.next_cursor.is_none());
    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn rows_with_null_chain_timestamps_are_excluded() {
    // Regression: `live_orders.chain_created_at` / `chain_updated_at` are
    // nullable (the projector can bind NULL if an EventNode arrives without
    // a chain timestamp). Such a row would have caused the SELECT to decode
    // NULL into `OpenOrderRow.chain_created_at_ms: i64` and surface as
    // -1000/500. The query now skips them via the partial-index predicate
    // mirrored in the WHERE clause.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Seed one normal open order (visible) and one with NULL chain
    // timestamps (invisible). Both belong to the same owner and book so the
    // only differentiator is the chain-time nullability.
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_010,
        &format!("5f80000000000000{:06}", 1),
    )
    .await;
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order,
                placed_chain_order,
                chain_created_at, chain_updated_at,
                created_at, updated_at)
           values ($1, 2::numeric, 1, true, 12345::numeric,
                   1000::numeric, 1000::numeric, $2,
                   'client-null-ts', 'OPEN', '5f80000000000000000002',
                   '5f80000000000000000002',
                   NULL, NULL,
                   to_timestamp(1700000020), to_timestamp(1700000020))"#,
    )
    .bind(&scope.book_yes)
    .bind(&scope.owner)
    .execute(&pool)
    .await
    .expect("insert null-chain-ts row");

    let page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 100,
            cursor: None,
        })
        .await
        .expect("list page must not error on NULL chain timestamps");
    assert_eq!(page.orders.len(), 1, "NULL-chain row must be omitted");
    assert_eq!(page.orders[0].order_id, "1");
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn sub_millisecond_chain_timestamps_are_display_only_for_cursor_pagination() {
    // `chain_created_at` may carry sub-millisecond precision, but openOrders
    // cursors no longer depend on it. The API renders timestamps in whole
    // milliseconds, while pagination must continue from the last returned
    // placed_chain_order.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Two open orders in the same book whose timestamps share a millisecond
    // but differ by 499 microseconds. Build the fractional-second f64 from
    // an exact microsecond i64 so the f64 literal isn't rejected by clippy's
    // excessive-precision lint.
    for (order_id, us) in [(1_i64, 1_700_000_000_500_500_i64), (2_i64, 1_700_000_000_500_999_i64)] {
        let fractional_secs = (us as f64) / 1_000_000.0;
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_initial, amount_remaining, owner_pn_address,
                    client_order_id, status, last_chain_order,
                    placed_chain_order,
                    chain_created_at, chain_updated_at,
                    created_at, updated_at)
               values ($1, $2::numeric, 1, true, 12345::numeric,
                       1000::numeric, 1000::numeric, $3,
                       $4, 'OPEN', $5,
                       $5,
                       to_timestamp($6::double precision), to_timestamp($6::double precision),
                       now(), now())"#,
        )
        .bind(&scope.book_yes)
        .bind(order_id)
        .bind(&scope.owner)
        .bind(format!("client-{order_id}"))
        .bind(format!("5f8000000000000{:05}", order_id))
        .bind(fractional_secs)
        .execute(&pool)
        .await
        .expect("insert sub-ms row");
    }

    let first = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 1,
            cursor: None,
        })
        .await
        .expect("first page");
    assert_eq!(first.orders.len(), 1);
    assert_eq!(first.orders[0].order_id, "1");
    assert_eq!(first.orders[0].time, 1_700_000_000_500);
    let cursor = first.next_cursor.expect("next_cursor present");
    assert_eq!(cursor.0, format!("5f8000000000000{:05}", 1));

    let second = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 1,
            cursor: Some(cursor),
        })
        .await
        .expect("second page");
    assert_eq!(second.orders.len(), 1, "second page must advance by placed_chain_order");
    assert_eq!(second.orders[0].order_id, "2");
    assert_eq!(second.orders[0].time, 1_700_000_000_500);
    assert!(second.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn cross_book_tie_does_not_lose_orders_across_pages() {
    // Under placed_chain_order ordering, two cross-book rows are globally
    // distinguished by their msg_chain_order (gateway-unique), so a tied
    // (chain_time, order_id) no longer collapses two rows under a single
    // cursor. This test pins the all-markets variant: two open orders on
    // different orderbooks paginate without losing either row.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;
    insert_market(&pool, &scope.pmp_no, &scope.symbol_no, &scope.book_no).await;

    // Two orders, two books, identical chain time and identical order_id.
    // Under the old (chain_created_at, order_id, orderbook_address) sort the
    // tie-breaker was orderbook_address; under the new placed_chain_order
    // sort the rows just need globally distinct chain_order values (which
    // the gateway guarantees in production). We assign them explicitly here
    // so the all-markets query can paginate across both rows.
    let chain_seconds = 1_700_000_050;
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        chain_seconds,
        "5f80000000000000_yes_001",
    )
    .await;
    insert_order(
        &pool,
        &scope.book_no,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        chain_seconds,
        "5f80000000000000_zno_001",
    )
    .await;

    let first = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 1,
            cursor: None,
        })
        .await
        .expect("first page");
    assert_eq!(first.orders.len(), 1);
    let cursor = first.next_cursor.expect("next_cursor must be set on tied page");

    let second = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 1,
            cursor: Some(cursor),
        })
        .await
        .expect("second page");
    assert_eq!(
        second.orders.len(),
        1,
        "the second order at the same (chain_time, order_id) must not be skipped"
    );
    assert_ne!(
        first.orders[0].market_address.0, second.orders[0].market_address.0,
        "the two pages must surface different books"
    );
    assert!(second.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn sort_uses_placed_chain_order_independent_of_chain_created_at() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Three orders, all sharing the same chain second, with strictly
    // increasing placed_chain_order values. With the old (chain_created_at,
    // order_id) sort, ties would fall back to numeric order_id; with the
    // new placed_chain_order sort, ordering is driven solely by the chain
    // event sequence. We assign placed_chain_order in reverse of order_id
    // to prove the sort no longer follows order_id.
    insert_order(
        &pool,
        &scope.book_yes,
        1,
        Some(&scope.owner),
        "12345",
        "1000",
        "1000",
        "OPEN",
        1_700_000_500,
        "5f80000000000000_C", // last
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
        1_700_000_500,
        "5f80000000000000_A", // first
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
        1_700_000_500,
        "5f80000000000000_B", // middle
    )
    .await;

    let page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 10,
            cursor: None,
        })
        .await
        .expect("list_open_orders");

    let order_ids: Vec<_> = page.orders.iter().map(|o| o.order_id.as_str()).collect();
    assert_eq!(
        order_ids,
        vec!["2", "3", "1"],
        "rows must sort by placed_chain_order ascending, not by order_id or chain_created_at"
    );
    assert!(page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn cursor_paginates_same_second_orders_without_duplicates_or_skips() {
    // Central same-second pagination case: one user places multiple orders
    // on one market within a single chain second. The cursor must split
    // them across pages by placed_chain_order alone — no duplicates, no
    // skips, no fallback to order_id (contracts/OrderBook.sol:697 only
    // guarantees order_id uniqueness, not monotonicity). placed_chain_order
    // is assigned in a scrambled order vs order_id so a regression to
    // order_id-based sort would surface as a wrong-order assertion.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    let same_second = 1_700_000_500_i64;
    let rows: [(i64, &str); 4] = [
        (10, "5f80000000000000_D"), // 4th in placed_chain_order lex
        (20, "5f80000000000000_A"), // 1st
        (30, "5f80000000000000_C"), // 3rd
        (40, "5f80000000000000_B"), // 2nd
    ];
    for (order_id, placed) in rows {
        insert_order(
            &pool,
            &scope.book_yes,
            order_id,
            Some(&scope.owner),
            "12345",
            "1000",
            "1000",
            "OPEN",
            same_second,
            placed,
        )
        .await;
    }

    // Page 1, limit=2 → expected [20, 40] (lex "_A" then "_B").
    let page1 = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: None,
        })
        .await
        .expect("page 1");
    let page1_ids: Vec<_> = page1.orders.iter().map(|o| o.order_id.clone()).collect();
    assert_eq!(page1_ids, vec!["20", "40"]);
    let cursor1 = page1.next_cursor.expect("cursor present after partial page");

    // Page 2, same limit → expected [30, 10] ("_C" then "_D"), no further page.
    let page2 = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 2,
            cursor: Some(cursor1),
        })
        .await
        .expect("page 2");
    let page2_ids: Vec<_> = page2.orders.iter().map(|o| o.order_id.clone()).collect();
    assert_eq!(page2_ids, vec!["30", "10"]);
    assert!(page2.next_cursor.is_none(), "no more pages");

    // Every order returned exactly once, in the expected lex order.
    let returned: Vec<_> = page1_ids.into_iter().chain(page2_ids).collect();
    assert_eq!(returned, vec!["20", "40", "30", "10"]);

    scope.cleanup(&pool).await;
}

#[tokio::test]
async fn shared_client_order_id_across_owners_does_not_leak_rows() {
    // client_order_id is user-supplied and per-owner. There is no DB-level
    // uniqueness constraint on the column and the openOrders SQL filters
    // exclusively on owner_pn_address — two users can legitimately submit
    // orders with the same client_order_id (e.g. "my-cool-order") and each
    // must see only their own row. Regressions to watch: client_order_id
    // accidentally added to DISTINCT/GROUP BY, the owner predicate
    // weakened or removed, or any logic treating client_order_id as a
    // uniqueness key.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let scope = Scope::new();
    scope.cleanup(&pool).await;
    insert_market(&pool, &scope.pmp_yes, &scope.symbol_yes, &scope.book_yes).await;

    // Two open orders on the same orderbook, different owners, identical
    // client_order_id. order_id differs (chain-assigned, unique per book).
    let shared_client_order_id = "shared-cool-order";
    for (order_id, owner, placed_chain_order) in [
        (1_i64, &scope.owner, "5f80000000000000_alpha"),
        (2_i64, &scope.other_owner, "5f80000000000000_bravo"),
    ] {
        sqlx::query(
            r#"insert into live_orders
                   (orderbook_address, order_id, outcome_id, is_buy, price,
                    amount_initial, amount_remaining, owner_pn_address,
                    client_order_id, status, last_chain_order,
                    placed_chain_order,
                    chain_created_at, chain_updated_at,
                    created_at, updated_at)
               values ($1, $2::numeric, 1, true, 12345::numeric,
                       1000::numeric, 1000::numeric, $3,
                       $4, 'OPEN', $5,
                       $5,
                       to_timestamp(1700000500::bigint), to_timestamp(1700000501::bigint),
                       to_timestamp(1700000500::bigint), to_timestamp(1700000501::bigint))"#,
        )
        .bind(&scope.book_yes)
        .bind(order_id)
        .bind(owner)
        .bind(shared_client_order_id)
        .bind(placed_chain_order)
        .execute(&pool)
        .await
        .expect("insert shared-client_order_id row");
    }

    // Owner sees exactly one row, with the shared clientOrderId.
    let owner_page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.owner.clone(),
            market: None,
            limit: 10,
            cursor: None,
        })
        .await
        .expect("owner page");
    assert_eq!(owner_page.orders.len(), 1, "owner must see exactly their own row");
    assert_eq!(owner_page.orders[0].order_id, "1");
    assert_eq!(owner_page.orders[0].client_order_id, shared_client_order_id);
    assert!(owner_page.next_cursor.is_none());

    // Other owner sees exactly their row, with the same clientOrderId.
    let other_page = repo
        .list_open_orders(&OpenOrdersQuery {
            owner_pn_address: scope.other_owner.clone(),
            market: None,
            limit: 10,
            cursor: None,
        })
        .await
        .expect("other owner page");
    assert_eq!(other_page.orders.len(), 1, "other owner must see exactly their own row");
    assert_eq!(other_page.orders[0].order_id, "2");
    assert_eq!(other_page.orders[0].client_order_id, shared_client_order_id);
    assert!(other_page.next_cursor.is_none());

    scope.cleanup(&pool).await;
}
