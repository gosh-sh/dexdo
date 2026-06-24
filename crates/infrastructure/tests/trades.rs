// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

// Integration tests for PostgresReadModelRepository::get_trades — the read
// side of GET /api/v1/prediction/trades (docs/tech-specs/read-api.md#apiv1trades). Gated
// on TEST_DATABASE_URL; see crates/infrastructure/tests/reprojection.rs for the
// docker-compose harness. Each test uses a unique pmp/orderbook/symbol prefix
// so the suite can run concurrently against one database.

use std::env;
use std::time::Duration;

use dodex_application::MarketReadRepository;
use dodex_application::TradesLimit;
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

async fn purge(pool: &PgPool, pmp: &str, book: &str) {
    sqlx::query("delete from trades where orderbook_address = $1")
        .bind(book)
        .execute(pool)
        .await
        .expect("purge trades");
    sqlx::query("delete from market_outcomes where pmp_address = $1")
        .bind(pmp)
        .execute(pool)
        .await
        .expect("purge market_outcomes");
    sqlx::query("delete from markets where pmp_address = $1")
        .bind(pmp)
        .execute(pool)
        .await
        .expect("purge markets");
}

/// Insert a market (token_type 3 = USDC, decimals 6) and return its id.
/// `reconciled` controls `last_reconciled_at`; `book` is the orderbook
/// address (a whitespace-only value exercises the MarketInconsistent gap).
async fn insert_market(pool: &PgPool, pmp: &str, book: &str, reconciled: bool) -> i64 {
    let reconciled_at = if reconciled { Some(()) } else { None };
    sqlx::query_scalar(
        r#"insert into markets
               (pmp_address, market_id, name, token_type, token_code,
                event_id, oracle_list_hash, orderbook_address,
                stake_start, stake_end, result_start, result_end,
                last_reconciled_at)
           values ($1, $1, $1, 3, 'USDC',
                   1::numeric, 0::numeric, $2,
                   1700000100, 1700000200, 1700000300, 1700000400,
                   case when $3::int is null then null else now() end)
           returning id"#,
    )
    .bind(pmp)
    .bind(book)
    .bind(reconciled_at.map(|_| 1_i32))
    .fetch_one(pool)
    .await
    .expect("insert market")
}

async fn insert_outcome(
    pool: &PgPool,
    market_id: i64,
    pmp: &str,
    outcome_id: i32,
    symbol: &str,
    price_precision: i32,
    quantity_precision: i32,
) {
    sqlx::query(
        r#"insert into market_outcomes
               (market_id_fk, pmp_address, outcome_id, outcome_name, symbol,
                price_precision, quantity_precision, tick_size, step_size,
                min_notional)
           values ($1, $2, $3, $4, $5,
                   $6, $7, '0.001', '0.01',
                   '1.00')"#,
    )
    .bind(market_id)
    .bind(pmp)
    .bind(outcome_id)
    .bind(format!("OUT{outcome_id}"))
    .bind(symbol)
    .bind(price_precision)
    .bind(quantity_precision)
    .execute(pool)
    .await
    .expect("insert market_outcomes");
}

#[allow(clippy::too_many_arguments)]
async fn insert_trade(
    pool: &PgPool,
    trade_id: &str,
    book: &str,
    outcome_id: i32,
    price: &str,
    qty: &str,
    is_buyer_maker: bool,
    chain_secs: Option<f64>,
) {
    sqlx::query(
        r#"insert into trades
               (trade_id, orderbook_address, outcome_id, price, qty,
                is_buyer_maker, chain_time)
           values ($1, $2, $3, $4::numeric, $5::numeric, $6,
                   to_timestamp($7::double precision))"#,
    )
    .bind(trade_id)
    .bind(book)
    .bind(outcome_id)
    .bind(price)
    .bind(qty)
    .bind(is_buyer_maker)
    .bind(chain_secs)
    .execute(pool)
    .await
    .expect("insert trade");
}

#[tokio::test]
async fn unknown_pair_is_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let err = repo
        .get_trades(
            &MarketAddress("0:trades_unknown_pmp".into()),
            &Symbol("NOPE".into()),
            TradesLimit::from_const(20),
        )
        .await
        .expect_err("unknown pair must fail");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::InvalidMarketOrSymbol);
}

#[tokio::test]
async fn unreconciled_pair_is_invalid_market_or_symbol() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_unreconciled_pmp";
    let book = "0:trades_unreconciled_book";
    let symbol = "TRADES_UNRECONCILED_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, false).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;

    let err = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect_err("a market that never reconciled is reported like an unknown pair");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::InvalidMarketOrSymbol);
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn blank_orderbook_address_fails_closed() {
    // A whitespace orderbook_address satisfies the
    // markets_orderbook_address_set_after_reconcile CHECK (which only forbids
    // NULL on reconciled rows) but is unusable; it must surface as
    // MarketInconsistent (503), never an empty tape — mirrors depth's
    // blank-orderbook contract.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_blank_book_pmp";
    let blank = " \t ";
    let symbol = "TRADES_BLANK_BOOK_YES";
    purge(&pool, pmp, blank).await;
    sqlx::query("delete from markets where orderbook_address = $1")
        .bind(blank)
        .execute(&pool)
        .await
        .expect("purge blank-orderbook residue");
    let market_id = insert_market(&pool, pmp, blank, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;

    let err = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect_err("blank orderbook on a reconciled market must fail closed");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::MarketInconsistent);
    purge(&pool, pmp, blank).await;
}

#[tokio::test]
async fn reconciled_market_without_trades_returns_empty_tape() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_empty_pmp";
    let book = "0:trades_empty_book";
    let symbol = "TRADES_EMPTY_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("reconciled market with no trades is a bare empty tape, not an error");
    assert!(tape.is_empty(), "no matched trades yet must return []");
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn returns_trades_newest_first_and_respects_limit() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_order_pmp";
    let book = "0:trades_order_book";
    let symbol = "TRADES_ORDER_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;
    // Lex-sortable chain-order keys; insertion order is deliberately not the
    // sort order, and ord-0004 carries the smallest chain_time so accidental
    // ORDER BY chain_time DESC would put it last.
    insert_trade(&pool, "ord-0002", book, 1, "6150", "1000000", true, Some(1_700_000_002.0)).await;
    insert_trade(&pool, "ord-0004", book, 1, "6150", "1000000", true, Some(1_700_000_000.0)).await;
    insert_trade(&pool, "ord-0001", book, 1, "6150", "1000000", true, Some(1_700_000_001.0)).await;
    insert_trade(&pool, "ord-0003", book, 1, "6150", "1000000", true, Some(1_700_000_003.0)).await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    let ids: Vec<&str> = tape.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(ids, ["ord-0004", "ord-0003", "ord-0002", "ord-0001"], "strict trade_id DESC");

    let limited = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(2))
        .await
        .expect("get_trades limited");
    let limited_ids: Vec<&str> = limited.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(limited_ids, ["ord-0004", "ord-0003"], "limit keeps the newest N");
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn tape_is_scoped_to_one_outcome_and_one_orderbook() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_scope_pmp";
    let book = "0:trades_scope_book";
    let other_pmp = "0:trades_scope_other_pmp";
    let other_book = "0:trades_scope_other_book";
    let yes = "TRADES_SCOPE_YES";
    let no = "TRADES_SCOPE_NO";
    let other_yes = "TRADES_SCOPE_OTHER_YES";
    purge(&pool, pmp, book).await;
    purge(&pool, other_pmp, other_book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, yes, 3, 2).await;
    insert_outcome(&pool, market_id, pmp, 2, no, 3, 2).await;
    // Second market whose outcome shares outcome_id = 1: a dropped
    // `orderbook_address = $1` predicate would only fail through this row,
    // not through the sibling-outcome one.
    let other_market_id = insert_market(&pool, other_pmp, other_book, true).await;
    insert_outcome(&pool, other_market_id, other_pmp, 1, other_yes, 3, 2).await;
    insert_trade(&pool, "scope-y1", book, 1, "6150", "1000000", true, Some(1_700_000_001.0)).await;
    insert_trade(&pool, "scope-n1", book, 2, "3850", "1000000", false, Some(1_700_000_002.0)).await;
    insert_trade(&pool, "scope-x1", other_book, 1, "5000", "1000000", true, Some(1_700_000_003.0))
        .await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(yes.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    let ids: Vec<&str> = tape.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(
        ids,
        ["scope-y1"],
        "neither a sibling outcome's trades nor another orderbook's same-id outcome may leak"
    );
    purge(&pool, pmp, book).await;
    purge(&pool, other_pmp, other_book).await;
}

/// Price-axis twins of the qty fail-closed tests
/// (quantity_precision_above_quote_decimals_fails_closed,
/// negative_raw_qty_fails_closed, off_grid_qty_fails_closed): the price drop
/// is computed from `PRICE_BPS_DECIMALS - price_precision` (a different input
/// pair than the qty drop), so a sign or operand error in one axis would not
/// surface through the other's test.
#[tokio::test]
async fn price_axis_corruption_fails_closed() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());

    // (suffix, price_precision, raw price) — three corruption modes:
    // display precision finer than the basis-point scale, a negative raw
    // price, and an on-chain price off the display grid.
    let cases: &[(&str, i32, &str)] =
        &[("pprec", 5, "6150"), ("negp", 3, "-5"), ("offgp", 2, "6150")];
    for (suffix, price_precision, raw_price) in cases {
        let pmp = format!("0:trades_{suffix}_pmp");
        let book = format!("0:trades_{suffix}_book");
        let symbol = format!("TRADES_{}_YES", suffix.to_uppercase());
        purge(&pool, &pmp, &book).await;
        let market_id = insert_market(&pool, &pmp, &book, true).await;
        insert_outcome(&pool, market_id, &pmp, 1, &symbol, *price_precision, 2).await;
        insert_trade(
            &pool,
            &format!("{suffix}-1"),
            &book,
            1,
            raw_price,
            "1000000",
            true,
            Some(1_700_000_001.0),
        )
        .await;

        let err = repo
            .get_trades(
                &MarketAddress(pmp.clone()),
                &Symbol(symbol.clone()),
                TradesLimit::from_const(20),
            )
            .await
            .expect_err("corrupt price axis must fail closed");
        assert_eq!(
            *err.downcast_ref::<DomainError>().unwrap(),
            DomainError::MarketInconsistent,
            "case {suffix}",
        );
        purge(&pool, &pmp, &book).await;
    }
}

#[tokio::test]
async fn decodes_price_qty_quote_qty_time_and_direction() {
    // The worked example from docs/api-spec.md §Recent Trades: USDC decimals
    // 6, price_precision 3, quantity_precision 2.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_decode_pmp";
    let book = "0:trades_decode_book";
    let symbol = "TRADES_DECODE_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;
    // price 6150 bps -> 0.615; qty 1_000_000 atoms -> 1.00;
    // quoteQty = 6150 * 1_000_000 / 10_000 = 615_000 atoms -> 0.615000.
    insert_trade(&pool, "dec-2", book, 1, "6150", "1000000", true, Some(1_710_000_008.0)).await;
    // qty 500_000 -> 0.50; quoteQty = 6150 * 500_000 / 10_000 = 307_500 -> 0.307500.
    insert_trade(&pool, "dec-1", book, 1, "6150", "500000", false, Some(1_710_000_004.0)).await;
    // Fractional chain seconds: `time` must truncate the microsecond store
    // to whole milliseconds, not round or pass sub-ms digits through.
    insert_trade(&pool, "dec-3", book, 1, "6150", "1000000", true, Some(1_710_000_009.123_456))
        .await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    assert_eq!(tape.len(), 3);

    let fractional = &tape[0];
    assert_eq!(fractional.trade_id, "dec-3");
    assert_eq!(
        fractional.time, 1_710_000_009_123,
        "sub-second chain time truncates to milliseconds"
    );

    let whole = &tape[1];
    assert_eq!(whole.trade_id, "dec-2");
    assert_eq!(whole.price, "0.615");
    assert_eq!(whole.qty, "1.00");
    assert_eq!(whole.quote_qty, "0.615000");
    assert_eq!(whole.time, 1_710_000_008_000);
    assert!(whole.is_buyer_maker, "is_buyer_maker passes through verbatim");

    let older = &tape[2];
    assert_eq!(older.qty, "0.50");
    assert_eq!(older.quote_qty, "0.307500");
    assert!(!older.is_buyer_maker, "false direction passes through verbatim");
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn quote_qty_floors_after_multiplying() {
    // Distinguishes the contract's `price * qty / FULL_PERCENT` (multiply, then
    // integer-divide) from `round(price_decimal * qty_decimal)`. With
    // price_precision 4 (no price digits dropped) and quantity_precision 6 =
    // decimals (no qty digits dropped), price 6150 bps * 1 atom = 6150, and
    // 6150 / 10_000 floors to 0 -> quoteQty "0.000000". A decimal round would
    // yield 0.000001 instead.
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_floor_pmp";
    let book = "0:trades_floor_book";
    let symbol = "TRADES_FLOOR_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 4, 6).await;
    insert_trade(&pool, "floor-1", book, 1, "6150", "1", true, Some(1_700_000_001.0)).await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    assert_eq!(tape.len(), 1);
    assert_eq!(tape[0].price, "0.6150");
    assert_eq!(tape[0].qty, "0.000001");
    assert_eq!(tape[0].quote_qty, "0.000000", "notional floors after multiplying");
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn quote_qty_handles_large_notional_exactly() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_large_notional_pmp";
    let book = "0:trades_large_notional_book";
    let symbol = "TRADES_LARGE_NOTIONAL_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 4, 6).await;

    let qty = "1000000000000000000000000000000";
    insert_trade(&pool, "large-1", book, 1, "9999", qty, true, Some(1_700_000_001.0)).await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    assert_eq!(tape.len(), 1);
    assert_eq!(tape[0].price, "0.9999");
    assert_eq!(tape[0].qty, "1000000000000000000000000.000000");
    assert_eq!(
        tape[0].quote_qty, "999900000000000000000000.000000",
        "large notional must stay in integer arithmetic"
    );
    purge(&pool, pmp, book).await;
}

#[tokio::test]
async fn rows_without_chain_time_are_excluded() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_nulltime_pmp";
    let book = "0:trades_nulltime_book";
    let symbol = "TRADES_NULLTIME_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;
    insert_trade(&pool, "nt-keep", book, 1, "6150", "1000000", true, Some(1_700_000_002.0)).await;
    // "nt-z-drop" sorts lexicographically AFTER "nt-keep", so the NULL row is
    // the newest on the tape. This pins the filter into the SQL itself: a
    // "fetch `limit` rows, then filter in Rust" refactor would return nothing
    // for the limit=1 probe below.
    insert_trade(&pool, "nt-z-drop", book, 1, "6150", "1000000", true, None).await;

    let tape = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect("get_trades");
    let ids: Vec<&str> = tape.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(ids, ["nt-keep"], "a row with NULL chain_time is filtered out of the tape");

    let probe = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(1))
        .await
        .expect("get_trades limit=1");
    let probe_ids: Vec<&str> = probe.iter().map(|t| t.trade_id.as_str()).collect();
    assert_eq!(
        probe_ids,
        ["nt-keep"],
        "the NULL-chain_time filter must apply before LIMIT, not after the fetch"
    );
    purge(&pool, pmp, book).await;
}

/// A `quantity_precision` finer than the quote asset's on-chain `decimals` is
/// read-model misconfiguration: the display grid would need digits the chain
/// never recorded. `get_trades` fails closed with `MarketInconsistent`
/// instead of inventing precision.
#[tokio::test]
async fn quantity_precision_above_quote_decimals_fails_closed() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_qprec_pmp";
    let book = "0:trades_qprec_book";
    let symbol = "TRADES_QPREC_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    // USDC carries 6 on-chain decimals; 8 display digits cannot be derived.
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 8).await;
    insert_trade(&pool, "qprec-1", book, 1, "6150", "1000000", true, Some(1_700_000_001.0)).await;

    let err = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect_err("quantity_precision above quote decimals must fail closed");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::MarketInconsistent);
    purge(&pool, pmp, book).await;
}

/// `NUMERIC(78,0)` admits a negative value the projector would never write;
/// a tape row carrying one is read-model corruption. `get_trades` fails
/// closed with `MarketInconsistent` rather than rendering it.
#[tokio::test]
async fn negative_raw_qty_fails_closed() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_negqty_pmp";
    let book = "0:trades_negqty_book";
    let symbol = "TRADES_NEGQTY_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;
    insert_trade(&pool, "negq-1", book, 1, "6150", "-5", true, Some(1_700_000_001.0)).await;

    let err = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect_err("a negative raw qty must fail closed");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::MarketInconsistent);
    purge(&pool, pmp, book).await;
}

/// A qty whose dropped digits are nonzero sits off the display grid the
/// chain lattice guarantees (step-size multiples); rounding it would be
/// confidently wrong, so `get_trades` fails closed with `MarketInconsistent`.
#[tokio::test]
async fn off_grid_qty_fails_closed() {
    let Some(pool) = setup().await else { return };
    let repo = PostgresReadModelRepository::new(pool.clone());
    let pmp = "0:trades_offgrid_pmp";
    let book = "0:trades_offgrid_book";
    let symbol = "TRADES_OFFGRID_YES";
    purge(&pool, pmp, book).await;
    let market_id = insert_market(&pool, pmp, book, true).await;
    insert_outcome(&pool, market_id, pmp, 1, symbol, 3, 2).await;
    // quantity_precision 2 against 6 quote decimals drops 4 digits; the
    // trailing "0001" is off-grid and must not be rounded away.
    insert_trade(&pool, "offg-1", book, 1, "6150", "1000001", true, Some(1_700_000_001.0)).await;

    let err = repo
        .get_trades(&MarketAddress(pmp.into()), &Symbol(symbol.into()), TradesLimit::from_const(20))
        .await
        .expect_err("an off-grid qty must fail closed");
    assert_eq!(*err.downcast_ref::<DomainError>().unwrap(), DomainError::MarketInconsistent);
    purge(&pool, pmp, book).await;
}
