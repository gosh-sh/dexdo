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
) {
    sqlx::query(
        r#"insert into live_orders
               (orderbook_address, order_id, outcome_id, is_buy, price,
                amount_initial, amount_remaining, owner_pn_address,
                client_order_id, status, last_chain_order, created_at, updated_at)
           values ($1, $2::numeric, 1, true, $3::numeric,
                   $4::numeric, $5::numeric, $6, $7, $8,
                   $9, to_timestamp($10), to_timestamp($10 + 1))"#,
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
    )
    .await;

    let page = repo
        .list_open_orders(&OpenOrdersQuery { owner_pn_address: scope.owner.clone(), market: None, limit: 100, cursor: None })
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
