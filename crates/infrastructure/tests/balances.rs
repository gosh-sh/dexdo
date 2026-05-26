// Integration tests for balance-side Postgres methods. Gated on
// TEST_DATABASE_URL like the rest of the infrastructure integration
// suites.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::ReferenceRepository;
use dodex_domain::MarketAddress;
use dodex_domain::Symbol;
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

#[tokio::test]
async fn lookup_ref_token_above_i32_max_fails_closed() {
    // The `ref_tokens.token_type` column is `integer` (signed i32). A u32
    // above `i32::MAX` is structurally impossible — chain ABI is uint32 but
    // the DB column cannot hold it. The repo lifts to MarketInconsistent so
    // the caller's log line for genuine-unknown does not blur with this
    // distinct corruption case.
    let Some(pool) = setup().await else { return };
    let refs = PostgresReferenceRepository::new(pool.clone());

    let err = refs.lookup_ref_token(u32::MAX).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
    // i32::MAX + 1 — the smallest value that triggers the branch.
    let just_above = (i32::MAX as u32) + 1;
    let err = refs.lookup_ref_token(just_above).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn lookup_ref_token_decimals_above_u8_max_fails_closed() {
    // `ref_tokens.decimals` is `integer` (i32 signed) but the domain caps
    // at u8 (chain ABI). A value above 255 — read-model drift — must lift
    // to MarketInconsistent rather than wrap or truncate.
    let Some(pool) = setup().await else { return };
    // Use a high token_type to avoid colliding with the seeded set and any
    // FK-referencing fixtures. Idempotent insert so reruns don't fight the
    // primary key.
    let oversize_tt: i32 = 90_001;
    sqlx::query(
        r#"insert into ref_tokens (
              token_type, token_code, decimals,
              min_notional, lot_size, tick_size_bps,
              price_precision, quantity_precision)
                values ($1, '__OVERSIZE_DEC__', 300,
                        0::numeric, 0::numeric, 0::numeric, 0, 0)
           on conflict (token_type) do update set decimals = excluded.decimals"#,
    )
    .bind(oversize_tt)
    .execute(&pool)
    .await
    .unwrap();

    let refs = PostgresReferenceRepository::new(pool.clone());
    let err = refs.lookup_ref_token(oversize_tt as u32).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
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
    for (oid, sym, name) in [(0i32, format!("{name}-NO"), "NO"), (1, format!("{name}-YES"), "YES")]
    {
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
        .execute(&pool)
        .await
        .unwrap();
    let (pmp, ob, _) = insert_market(&pool, "resolve-bal-1").await;
    let repo = PostgresReadModelRepository::new(pool.clone());

    let res = repo.resolve_market_for_balances(&MarketAddress(pmp.clone())).await.expect("ok");
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
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ('0:unrec-bal-pmp', 'x', 1, 'NACKL', 1::numeric, 1::numeric,
                   '0:unrec-bal-ob', 2, null)"#,
    )
    .execute(&pool)
    .await
    .unwrap();
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
        .execute(&pool)
        .await
        .unwrap();

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
    let sums = repo.sum_open_sell_remaining("0:no-such-ob", "0:no-such-pn").await.expect("ok");
    assert!(sums.is_empty());
}

#[tokio::test]
async fn resolve_market_for_balances_outcome_count_mismatch_fails_closed() {
    // Seed a market with num_outcomes = 3 but only insert 2 market_outcomes
    // rows. The guard in resolve_market_for_balances detects
    // the discrepancy and returns MarketInconsistent rather than silently
    // serving a partial outcome list.
    let Some(pool) = setup().await else { return };
    let pmp = "0:mismatch-bal-pmp";
    let ob = "0:mismatch-bal-ob";
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, 'mismatch-test', 1, 'NACKL', 42::numeric, 24::numeric, $2, 3, now())"#,
    )
    .bind(pmp)
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(pmp)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Insert only 2 rows despite num_outcomes = 3.
    for (oid, sym, name) in [(0i32, "mismatch-NO", "NO"), (1, "mismatch-YES", "YES")] {
        sqlx::query(
            r#"insert into market_outcomes (
                  market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                  price_precision, quantity_precision, tick_size, step_size,
                  min_notional, max_batch_size)
               values ($1, $2, $3, $4, $5, 3, 2, '0.001', '0.01', '1', 5)"#,
        )
        .bind(id)
        .bind(pmp)
        .bind(oid)
        .bind(name)
        .bind(sym)
        .execute(&pool)
        .await
        .unwrap();
    }

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.resolve_market_for_balances(&MarketAddress(pmp.to_string())).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_market_for_balances_null_oracle_list_hash_fails_closed() {
    // `oracle_list_hash` is nullable at the schema level but the visibility
    // gate `last_reconciled_at IS NOT NULL` is supposed to guarantee a
    // non-null value at read time. Seeding a row that violates that
    // contract exercises the explicit guard in resolve_market_for_balances.
    let Some(pool) = setup().await else { return };
    let pmp = "0:null-orahash-bal-pmp";
    let ob = "0:null-orahash-bal-ob";
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, 'null-orahash', 1, 'NACKL', 42::numeric, null,
                   $2, 1, now())"#,
    )
    .bind(pmp)
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.resolve_market_for_balances(&MarketAddress(pmp.to_string())).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_market_for_balances_blank_orderbook_address_fails_closed() {
    // A reconciled market with a NULL/blank `orderbook_address` cannot
    // serve balances because `/api/v1/account/balances` joins live_orders
    // by orderbook. The guard treats this as read-model corruption.
    let Some(pool) = setup().await else { return };
    let pmp = "0:blank-ob-bal-pmp";
    // Clean up by pmp_address (this test's marker) AND by any prior leftover
    // row holding the literal whitespace orderbook value — the partial
    // UNIQUE index on `markets.orderbook_address` would otherwise reject
    // re-running this test if a prior panic skipped teardown.
    sqlx::query("delete from markets where pmp_address = $1 or orderbook_address = '   '")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, 'blank-ob', 1, 'NACKL', 42::numeric, 24::numeric,
                   '   ', 1, now())"#,
    )
    .bind(pmp)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.resolve_market_for_balances(&MarketAddress(pmp.to_string())).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_market_for_balances_negative_token_type_fails_closed() {
    // `markets.token_type` is `integer` (signed) but the chain ABI is
    // uint32. A negative value is read-model drift; the repo lifts it
    // to MarketInconsistent rather than handing a poisoned token_type
    // to downstream consumers.
    let Some(pool) = setup().await else { return };
    let pmp = "0:neg-tt-bal-pmp";
    let ob = "0:neg-tt-bal-ob";
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    // markets.token_type has an FK to ref_tokens; idempotently seed a
    // sentinel -1 row so the markets insert below doesn't violate the FK.
    // All `not null` columns must be filled — the values themselves are
    // never read by the code under test.
    sqlx::query(
        r#"insert into ref_tokens (
              token_type, token_code, decimals,
              min_notional, lot_size, tick_size_bps,
              price_precision, quantity_precision)
                values (-1, '__NEG_TT__', 0,
                        0::numeric, 0::numeric, 0::numeric, 0, 0)
           on conflict (token_type) do nothing"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, 'neg-tt', -1, '__NEG_TT__', 42::numeric, 24::numeric,
                   $2, 1, now())"#,
    )
    .bind(pmp)
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo.resolve_market_for_balances(&MarketAddress(pmp.to_string())).await.unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}

#[tokio::test]
async fn resolve_for_new_order_negative_outcome_id_fails_closed() {
    // market_outcomes.outcome_id has no CHECK constraint enforcing non-negative.
    // The try_into::<u32>() guard in resolve_for_new_order must surface a negative
    // value as MarketInconsistent rather than silently wrapping or panicking.
    // Mirrors negative_price_precision_fails_closed which exercises fetch_outcomes.
    let Some(pool) = setup().await else { return };
    let pmp = "0:neg-outcome-id-placement-pmp";
    let ob = "0:neg-outcome-id-placement-ob";
    sqlx::query("delete from live_orders where orderbook_address = $1")
        .bind(ob)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"insert into markets (
              pmp_address, name, token_type, token_code, event_id, oracle_list_hash,
              orderbook_address, num_outcomes, last_reconciled_at)
           values ($1, 'neg-oid-test', 1, 'NACKL', 42::numeric, 24::numeric, $2, 1, now())"#,
    )
    .bind(pmp)
    .bind(ob)
    .execute(&pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("select id from markets where pmp_address = $1")
        .bind(pmp)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Insert outcome with outcome_id = -1 — negative value the DB accepts but
    // the domain guard must reject.
    sqlx::query(
        r#"insert into market_outcomes (
              market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
              price_precision, quantity_precision, tick_size, step_size,
              min_notional, max_batch_size)
           values ($1, $2, -1, 'NO', $3, 3, 2, '0.001', '0.01', '1', 5)"#,
    )
    .bind(id)
    .bind(pmp)
    .bind(format!("{}-NO", pmp))
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresReadModelRepository::new(pool.clone());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
        as i64;
    let err = repo
        .resolve_for_new_order(&MarketAddress(pmp.to_string()), &Symbol(format!("{}-NO", pmp)), now)
        .await
        .unwrap_err();
    let dom = err.downcast_ref::<dodex_domain::DomainError>().expect("DomainError");
    assert!(matches!(dom, dodex_domain::DomainError::MarketInconsistent));
}
