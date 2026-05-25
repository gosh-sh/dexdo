// Integration tests for balance-side Postgres methods. Gated on
// TEST_DATABASE_URL like the rest of the infrastructure integration
// suites.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::ReferenceRepository;
use dodex_domain::MarketAddress;
use dodex_infrastructure::database;
use dodex_infrastructure::postgres_repo::PostgresReadModelRepository;
use dodex_infrastructure::postgres_repo::PostgresReferenceRepository;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup() -> Option<PgPool> {
    let _ = dotenvy::dotenv();
    let url = env::var("TEST_DATABASE_URL").ok().filter(|s| !s.is_empty())?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    database::run_migrations(&pool).await.expect("run migrations");
    Some(pool)
}

#[tokio::test]
async fn lookup_ref_token_returns_seeded_rows() {
    let Some(pool) = setup().await else { return };
    let refs = PostgresReferenceRepository::new(pool.clone());

    let nackl = refs.lookup_ref_token(1).await.expect("ok");
    assert!(nackl.is_some());
    let nackl = nackl.unwrap();
    assert_eq!(nackl.token_code, "NACKL");
    assert_eq!(nackl.decimals, 9);

    let usdc = refs.lookup_ref_token(3).await.expect("ok");
    assert!(usdc.is_some());
    assert_eq!(usdc.unwrap().token_code, "USDC");

    let unknown = refs.lookup_ref_token(99).await.expect("ok");
    assert!(unknown.is_none());
}

async fn insert_market(pool: &PgPool, name: &str) -> (String, String, i64) {
    let pmp = format!("0:{name}-pmp");
    let ob = format!("0:{name}-ob");
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, $2, 1, 'NACKL', 42::numeric, 24::numeric, $3, 2, now())"#,
    )
    .bind(&pmp)
    .bind(name)
    .bind(&ob)
    .execute(pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(&pmp)
        .fetch_one(pool)
        .await
        .unwrap();
    for (oid, sym, name) in [(0i32, format!("{name}-NO"), "NO"), (1, format!("{name}-YES"), "YES")] {
        sqlx::query(
            r#"insert into market_outcomes (
                  market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                  price_precision, quantity_precision, tick_size, step_size,
                  min_notional, max_batch_size)
               values ($1, $2, $3, $4, $5, 3, 2, '0.001', '0.01', '1', 5)"#,
        )
        .bind(id)
        .bind(&pmp)
        .bind(oid)
        .bind(name)
        .bind(&sym)
        .execute(pool)
        .await
        .unwrap();
    }
    (pmp, ob, id)
}

#[tokio::test]
async fn resolve_market_for_balances_returns_reconciled_row() {
    let Some(pool) = setup().await else { return };
    sqlx::query("delete from markets where pmp_address like '0:resolve-bal-%'")
        .execute(&pool).await.unwrap();
    let (pmp, ob, _) = insert_market(&pool, "resolve-bal-1").await;
    let repo = PostgresReadModelRepository::new(pool.clone());

    let res = repo
        .resolve_market_for_balances(&MarketAddress(pmp.clone()))
        .await
        .expect("ok");
    assert_eq!(res.event_id, "42");
    assert_eq!(res.oracle_list_hash, "24");
    assert_eq!(res.token_type, 1);
    assert_eq!(res.orderbook_address, ob);
    assert_eq!(res.num_outcomes, 2);
    assert_eq!(res.outcomes.len(), 2);
    assert_eq!(res.outcomes[0].outcome_id, 0);
    assert_eq!(res.outcomes[1].outcome_id, 1);
}

#[tokio::test]
async fn resolve_market_for_balances_unknown_returns_invalid_market() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    let err = repo
        .resolve_market_for_balances(&MarketAddress("0:does-not-exist".to_string()))
        .await
        .unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::InvalidMarketOrSymbol));
}

#[tokio::test]
async fn resolve_market_for_balances_unreconciled_returns_invalid_market() {
    let Some(pool) = setup().await else { return };
    sqlx::query("delete from markets where pmp_address = '0:unrec-bal-pmp'")
        .execute(&pool).await.unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ('0:unrec-bal-pmp', 'x', 1, 'NACKL', 1::numeric, 1::numeric,
                   '0:unrec-bal-ob', 2, null)"#,
    )
    .execute(&pool).await.unwrap();
    let repo = PostgresReadModelRepository::new(pool.clone());

    let err = repo
        .resolve_market_for_balances(&MarketAddress("0:unrec-bal-pmp".to_string()))
        .await
        .unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::InvalidMarketOrSymbol));
}

async fn insert_live_order(
    pool: &PgPool,
    orderbook: &str,
    order_id: i64,
    outcome_id: i32,
    is_buy: bool,
    owner: Option<&str>,
    status: &str,
    amount_remaining: &str,
) {
    sqlx::query(
        r#"insert into live_orders (
              orderbook_address, order_id, outcome_id, is_buy, price,
              amount_initial, amount_remaining, status, last_chain_order,
              placed_chain_order, owner_pn_address)
           values ($1, $2::numeric, $3, $4, '500'::numeric, $7::numeric, $7::numeric,
                   $5, '0', '0', $6)"#,
    )
    .bind(orderbook)
    .bind(order_id)
    .bind(outcome_id)
    .bind(is_buy)
    .bind(status)
    .bind(owner)
    .bind(amount_remaining)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn sum_open_sell_remaining_groups_by_outcome_and_filters() {
    let Some(pool) = setup().await else { return };
    sqlx::query("delete from live_orders where orderbook_address = '0:sum-sell-ob'")
        .execute(&pool).await.unwrap();

    let ob = "0:sum-sell-ob";
    let me = "0:my-pn";
    // me / outcome 0 OPEN SELL 10 → counted
    insert_live_order(&pool, ob, 1, 0, false, Some(me), "OPEN", "10").await;
    // me / outcome 0 OPEN SELL 5 → counted (total = 15)
    insert_live_order(&pool, ob, 2, 0, false, Some(me), "OPEN", "5").await;
    // me / outcome 1 OPEN SELL 100 → counted
    insert_live_order(&pool, ob, 3, 1, false, Some(me), "OPEN", "100").await;
    // me / outcome 0 OPEN BUY → excluded (is_buy = true)
    insert_live_order(&pool, ob, 4, 0, true, Some(me), "OPEN", "999").await;
    // me / outcome 0 FILLED SELL → excluded (status != OPEN)
    insert_live_order(&pool, ob, 5, 0, false, Some(me), "FILLED", "777").await;
    // me / outcome 0 CANCELLED SELL → excluded
    insert_live_order(&pool, ob, 6, 0, false, Some(me), "CANCELLED", "888").await;
    // other owner / outcome 0 OPEN SELL → excluded
    insert_live_order(&pool, ob, 7, 0, false, Some("0:other"), "OPEN", "111").await;
    // NULL owner / outcome 0 OPEN SELL → excluded (depth-only row)
    insert_live_order(&pool, ob, 8, 0, false, None, "OPEN", "222").await;

    let repo = PostgresReadModelRepository::new(pool.clone());
    let sums = repo.sum_open_sell_remaining(ob, me).await.expect("ok");
    assert_eq!(sums.get(&0u32), Some(&"15".to_string()));
    assert_eq!(sums.get(&1u32), Some(&"100".to_string()));
    assert_eq!(sums.len(), 2);
}

#[tokio::test]
async fn sum_open_sell_remaining_empty_when_no_match() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let sums = repo
        .sum_open_sell_remaining("0:no-such-ob", "0:no-such-pn")
        .await
        .expect("ok");
    assert!(sums.is_empty());
}
