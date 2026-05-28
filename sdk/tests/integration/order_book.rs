//! OrderBook integration tests against a pre-warmed market from
//! `ob_pool.json` (seeded by `cargo run --release --bin mint_ob_pool`).
//!
//! Tests use trader-`PrivateNote`s from `pn_pool.json` (seeded by
//! `mint_pn_pool`). Each trader calls `splitFullSet` against the market
//! to mint outcome tokens, then exercises the OrderBook public API
//! (place/cancel/match/fill). Tests claim slots 0..9 of the pool, so
//! seed with at least 10 fully-funded notes.
//!
//! These tests are `#[ignore]` by default — they need both pools and a
//! live shellnet endpoint. Run explicitly:
//!
//!   cargo test -p dodex-sdk --test integration order_book -- --ignored --nocapture
//!
//! Override pool paths via env:
//!
//!   OB_POOL_PATH=/path/to/ob_pool.json
//!   PN_POOL_PATH=/path/to/pn_pool.json

use ackinacki_kit::contracts::dex::order_book::OrderBookOrder;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfCancelOrderByClient;
use ackinacki_kit::contracts::dex::private_note::ParamsOfMergeFullSet;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceBatch;
use ackinacki_kit::contracts::dex::private_note::ParamsOfPlaceOrder;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSplitFullSet;
use ackinacki_kit::contracts::dex::private_note::ParamsOfStakeKey;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_sdk::Dex;
use dodex_sdk::OwnedOrder;

use crate::common::context::create_dex;
use crate::common::ob_pool;
use crate::common::ob_pool::ObMarket;
use crate::common::pn_pool;
use crate::common::pn_pool::PnNote;

/// Per-trader collateral for `splitFullSet`.
///
/// `PMP.splitFullSet` is NOT a 1:1 exchange. The contract computes
/// `t = collateral / Q` and `amounts[k] = t * u_k`, where `Q = sum(u_k)`
/// and `u_k = clean_pool[k] / gcd(clean_pool[*])` (frozen at the moment
/// of first post-stakeEnd split). For the seeder's symmetric pool
/// (`[100.2, 100.2]` NACKL), this yields `Q=2, u_k=1`, i.e. you get
/// **0.5 outcome-token of each outcome per NACKL of collateral**.
///
/// 100 NACKL split → 50 outcome-0 + 50 outcome-1 — comfortably above
/// the 30-NACKL sell amount used by single-order tests, with room for
/// multiple tests against the same trader-PN.
const TRADER_SPLIT_COLLATERAL: u128 = 100_000_000_000;

/// Standard order amount used by tests: 30 NACKL of outcome-0. At 5000 bps
/// that's 15 NACKL notional — comfortably above `MIN_ORDER_NOTIONAL_NACKL = 10`.
const ORDER_AMOUNT: u128 = 30_000_000_000;
const ORDER_PRICE_BPS: &str = "5000";

/// Order flags from `contracts/dex/modifiers/modifiers.sol`.
const FLAG_IOC: u8 = 0x01;
const FLAG_FOK: u8 = 0x02;
const FLAG_MARKET: u8 = 0x04;
const FLAG_POST_ONLY: u8 = 0x08;

/// Unix timestamp in seconds. Tests capture this at start to filter
/// `wait_for_order_*` events to "since-test-started" — without that,
/// reused `client_order_id` values would match historical events from
/// previous test runs and the assertion would race against state.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Per-process salt used to derive collision-free `client_order_id`
/// values across test runs.
///
/// `OrderBook.placeOrder` enforces `_clientOrderIds[depositHash][coid]`
/// uniqueness *per-trader*: a placement reusing a coid that's still
/// live (resting / partial / etc.) for the same trader is silently
/// rejected via `Rejected` (no `OrderPlaced` event). Tests that leave
/// orders behind on failure — or simply rest a maker without a
/// matching counter-order in this run — would block all future runs of
/// the same test on hard-coded coids.
///
/// `salted_coid(base)` packs a per-process salt (the suite's start
/// timestamp) into the high bits, so each `cargo test` invocation
/// generates fresh coids while keeping the low-bit `base` distinct
/// across tests in the same suite.
static COID_SALT: std::sync::LazyLock<u128> =
    std::sync::LazyLock::new(|| (now_unix() as u128) << 32);

fn salted_coid(base: u32) -> u128 {
    *COID_SALT | (base as u128)
}

/// Helper: split + wait for `onSplitAccepted` callback (PN no longer
/// busy). Polling-based — fixed sleep is unreliable under shellnet load
/// (3 parallel tests easily push round-trip past 5s, and `placeOrder`
/// then sees `stake.amount[outcomeId]=0` and rejects with ERR_LOW_VALUE).
async fn trader_split(dex: &Dex, market: &ObMarket, trader: &PnNote, collateral: u128) {
    dex.split_full_set(
        &trader.address,
        ParamsOfSplitFullSet {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            collateral,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("split_full_set");
    wait_not_busy(dex, trader, "split_full_set").await;
}

/// Poll `get_private_note_details` until the PN's `busy_address` clears
/// — i.e. the in-flight callback has settled.
///
/// Two-phase polling so we don't race past the "busy" window:
///   - Phase 1: poll up to 30s waiting for the chain to pick up our
///     external message and flip `busy_address` to `Some(...)`. Without
///     this, a fast caller observes the pre-message `None` and exits
///     immediately, then sends the next op before the previous one
///     has even landed.
///   - Phase 2: poll up to 90s waiting for `busy_address` to clear back
///     to `None`, signalling the callback (`onSplitAccepted` etc.)
///     finished and `_stakes` is updated.
///
/// If phase 1 never observes `Some`, the operation might have completed
/// between our 1s polls. That's fine — we treat it as already-done.
async fn wait_not_busy(dex: &Dex, trader: &PnNote, op: &str) {
    let mut saw_busy = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let d = dex.get_private_note_details(&trader.address).await.expect("pn details");
        if d.busy_address.is_some() {
            saw_busy = true;
            break;
        }
    }

    if !saw_busy {
        // Either the op landed + completed within polling resolution, or
        // chain hasn't seen our external message yet. Give it more time
        // either way before assuming success.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        return;
    }

    for _ in 0..90 {
        let d = dex.get_private_note_details(&trader.address).await.expect("pn details");
        if d.busy_address.is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    panic!("PN {} stayed busy after {op} for 90s", trader.address);
}

/// Helper: place a single limit order. `is_buy=false` ⇒ sell, `true` ⇒ buy.
async fn trader_place(
    dex: &Dex,
    market: &ObMarket,
    trader: &PnNote,
    outcome_id: u32,
    is_buy: bool,
    amount: u128,
    price: &str,
    client_order_id: u128,
) {
    trader_place_with_flags(
        dex,
        market,
        trader,
        outcome_id,
        is_buy,
        amount,
        price,
        client_order_id,
        0,
    )
    .await;
}

/// Helper: place a limit order with explicit `flags`. Used by IOC/FOK/
/// POST_ONLY/MARKET tests; plain limit orders should call `trader_place`
/// (which forwards `flags=0`).
#[allow(clippy::too_many_arguments)]
async fn trader_place_with_flags(
    dex: &Dex,
    market: &ObMarket,
    trader: &PnNote,
    outcome_id: u32,
    is_buy: bool,
    amount: u128,
    price: &str,
    client_order_id: u128,
    flags: u8,
) {
    dex.place_order(
        &trader.address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id,
            is_buy,
            price: price.to_string(),
            amount,
            flags,
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("place_order");
}

/// Poll `getOrdersByOwner` until an order with the given `client_order_id`
/// appears (returns it) or 60s elapse (panics).
async fn wait_owner_order(
    dex: &Dex,
    market: &ObMarket,
    trader: &PnNote,
    client_order_id: u128,
) -> OwnedOrder {
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let owned = dex
            .get_orders_by_owner(
                &market.order_book_address,
                trader.deposit_identifier_hash.clone(),
            )
            .await
            .expect("get_orders_by_owner");
        if let Some(o) = owned.orders.iter().find(|o| o.client_order_id == client_order_id) {
            return o.clone();
        }
    }
    panic!(
        "order client_order_id={client_order_id} for {} did not surface in getOrdersByOwner within 60s",
        trader.address,
    )
}

/// Poll `getOrdersByOwner` until the order with `client_order_id` is GONE
/// (returns) or 60s elapse (panics). Used to confirm a fill or cancel.
async fn wait_owner_order_gone(
    dex: &Dex,
    market: &ObMarket,
    trader: &PnNote,
    client_order_id: u128,
) {
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let owned = dex
            .get_orders_by_owner(
                &market.order_book_address,
                trader.deposit_identifier_hash.clone(),
            )
            .await
            .expect("get_orders_by_owner");
        if !owned.orders.iter().any(|o| o.client_order_id == client_order_id) {
            return;
        }
    }
    panic!(
        "order client_order_id={client_order_id} for {} did not disappear within 60s",
        trader.address,
    )
}

/// Smoke test: load fixtures, verify pools are alive and the OrderBook
/// is in a sane initial state. Doesn't place any orders — its job is to
/// make sure the seeders' output is consumable end-to-end.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_pool_smoke() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pn_pool = pn_pool::load();
    let (_trader_a, _trader_b) = pn_pool.take_pair(0);

    eprintln!(
        "[smoke] market: pmp={} ob={} (live for ~{}s more)",
        market.pmp_address,
        market.order_book_address,
        market.seconds_until_result_start(),
    );

    // OrderBook should be Active and bound to the right event/oracle list.
    let details = dex
        .get_order_book_details(&market.order_book_address)
        .await
        .expect("get_order_book_details");
    assert_eq!(details.event_id, market.event_id, "OB event_id matches market");
    assert_eq!(
        details.oracle_list_hash, market.oracle_list_hash,
        "OB oracle_list_hash matches market",
    );
    assert_eq!(details.token_type, market.token_type, "OB token_type matches");

    let shutdown =
        dex.get_order_book_shutdown_state(&market.order_book_address).await.expect("ob shutdown");
    assert!(!shutdown.shutting_down, "OB must not be shutting down");
    assert!(!shutdown.shutdown_pending, "OB must not be shutdown_pending");

    // queue empty by default for a freshly-spawned OB
    let qs = dex
        .get_order_book_queue_size(&market.order_book_address)
        .await
        .expect("queue size");
    eprintln!("[smoke] OB queue size = {qs}");

    // PMP must be approved + frozen post-stakeEnd
    let pmp = dex.get_pmp_details(&market.pmp_address).await.expect("pmp details");
    assert!(pmp.approved, "PMP approved");
    assert!(pmp.frozen, "PMP frozen (post-stakeEnd, after deployer's splitFullSet)");

    eprintln!(
        "[smoke] OK — OB live, deployer outcomes pre-split, traders={}",
        pn_pool.notes.len()
    );
}

/// Place a simple sell order from trader A and verify it shows up in the
/// OrderBook owner-index. Uses `getOrdersByOwner` for the assertion (no
/// event subscription yet — adds that in A3).
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_place_single_sell_order() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pn_pool = pn_pool::load();
    let (trader, _other) = pn_pool.take_pair(0);

    eprintln!("[place] using trader {} on market {}", trader.address, market.pmp_address);

    // 1. Trader splits NACKL collateral into outcome tokens. Required —
    //    OB orders are denominated in outcome tokens, not NACKL.
    dex.split_full_set(
        &trader.address,
        ParamsOfSplitFullSet {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            collateral: TRADER_SPLIT_COLLATERAL,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("split_full_set");

    // Wait for split callback (`onSplitAccepted`) to land — PN clears
    // `busy_address` once it processes the callback.
    wait_not_busy(&dex, &trader, "split_full_set").await;

    // 2. Place a sell of 30 outcome-0 tokens at price 5000 bps.
    //    Notional = amount × price / FULL_PERCENT = 30 × 0.5 = 15 NACKL,
    //    above the contract-fixed `MIN_ORDER_NOTIONAL_NACKL = 10 NACKL`
    //    (ERR_ORDER_TOO_SMALL=160 if you go under).
    //    `client_order_id` is the trader's own counter — used later to
    //    look up the chain-assigned `order_id` via `getOrderIdByClient`.
    let client_order_id: u128 = salted_coid(1);
    let amount: u128 = 30_000_000_000; // 30 NACKL of outcome-0
    let price = "5000".to_string(); // basis points (50%)
    dex.place_order(
        &trader.address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id: 0,
            is_buy: false,
            price: price.clone(),
            amount,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("place_order");

    // 3. Poll the OB owner-index until the order surfaces.
    let mut found = None;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let owned = dex
            .get_orders_by_owner(
                &market.order_book_address,
                trader.deposit_identifier_hash.clone(),
            )
            .await
            .expect("get_orders_by_owner");
        if let Some(o) = owned.orders.iter().find(|o| o.client_order_id == client_order_id) {
            found = Some(o.clone());
            break;
        }
    }

    let order = found.expect("placed order should appear in getOrdersByOwner within 60s");
    assert_eq!(order.outcome_id, 0);
    assert!(!order.is_buy);
    assert_eq!(order.amount, amount);
    eprintln!(
        "[place] OK — order_id={} client_order_id={} amount={} price={}",
        order.order_id, order.client_order_id, order.amount, order.price,
    );
}

/// Place a sell, then cancel it via `cancelOrderByClient`. Verifies that
/// the order disappears from the owner-index after the cancel callback.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_cancel_by_client_id() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    // trader[1] — distinct from place_test (trader[0]) so suite-level
    // parallelism doesn't collide on the same PN's outcome-token balance.
    let trader = pool.take_n(1, 1).into_iter().next().expect("pool has trader[1]");

    eprintln!("[cancel] using trader {} on market {}", trader.address, market.pmp_address);

    trader_split(&dex, &market, &trader, TRADER_SPLIT_COLLATERAL).await;

    let client_order_id: u128 = salted_coid(1001);
    // outcome 0 — same book as place_test, but each trader's owner-index
    // is partitioned by deposit_identifier_hash so we don't see other
    // traders' residue when polling.
    trader_place(&dex, &market, &trader, 0, false, ORDER_AMOUNT, ORDER_PRICE_BPS, client_order_id)
        .await;

    let placed = wait_owner_order(&dex, &market, &trader, client_order_id).await;
    eprintln!(
        "[cancel] placed: order_id={} amount={} — now cancelling",
        placed.order_id, placed.amount,
    );

    dex.cancel_order_by_client(
        &trader.address,
        ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("cancel_order_by_client");

    wait_owner_order_gone(&dex, &market, &trader, client_order_id).await;
    eprintln!("[cancel] OK — order client_order_id={client_order_id} removed");
}

/// Two traders, crossing limit orders at the same price → immediate
/// match. Verifies that both orders disappear from `getOrdersByOwner`
/// (i.e. each was fully filled, not just resting). Also verifies the
/// OrderBook's `next_order_id` advanced by 2.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_match_crossing_orders() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    // trader[2] (sell) + trader[3] (buy) — separate from place/cancel tests.
    let traders = pool.take_n(2, 2);
    let seller = &traders[0];
    let buyer = &traders[1];

    eprintln!(
        "[match] sell={} buy={} on market {}",
        seller.address, buyer.address, market.pmp_address,
    );

    // Both traders need outcome tokens so they can settle either side
    // of the trade. `splitFullSet` is idempotent in the sense that you
    // can call it multiple times to add more — running it once per
    // trader here is enough.
    trader_split(&dex, &market, seller, TRADER_SPLIT_COLLATERAL).await;
    trader_split(&dex, &market, buyer, TRADER_SPLIT_COLLATERAL).await;

    let next_id_before = dex
        .get_order_book_details(&market.order_book_address)
        .await
        .expect("ob details before")
        .next_order_id;

    // Trade on outcome-1 — place/cancel tests use outcome-0, so the two
    // OB books are isolated and our buyer can't accidentally cross
    // residue from a previous test in the same suite.
    let outcome_id: u32 = 1;

    // Seller posts first — this becomes the resting maker order.
    let seller_coid: u128 = salted_coid(2001);
    let buyer_coid: u128 = salted_coid(2002);
    trader_place(
        &dex,
        &market,
        seller,
        outcome_id,
        false,
        ORDER_AMOUNT,
        ORDER_PRICE_BPS,
        seller_coid,
    )
    .await;
    let resting = wait_owner_order(&dex, &market, seller, seller_coid).await;
    eprintln!(
        "[match] sell resting: order_id={} amount={} price={}",
        resting.order_id, resting.amount, resting.price,
    );

    // Buyer crosses at the same price → full fill expected.
    trader_place(&dex, &market, buyer, outcome_id, true, ORDER_AMOUNT, ORDER_PRICE_BPS, buyer_coid)
        .await;

    // Both sides should disappear from the owner-index after the fill.
    wait_owner_order_gone(&dex, &market, seller, seller_coid).await;
    wait_owner_order_gone(&dex, &market, buyer, buyer_coid).await;

    let details_after =
        dex.get_order_book_details(&market.order_book_address).await.expect("ob details after");
    // Strict `Δ == 2` doesn't hold under parallel tests — they all share
    // the same OB.next_order_id counter. Just verify it grew by AT LEAST
    // 2 (our own two placements). Authoritative correctness checks live
    // in `wait_owner_order_gone` above (both sides fully filled).
    let advanced = details_after.next_order_id.saturating_sub(next_id_before);
    assert!(
        advanced >= 2,
        "next_order_id must advance by ≥ 2 (one per our place_order), got Δ={advanced} \
         (before={next_id_before}, after={})",
        details_after.next_order_id,
    );
    eprintln!(
        "[match] OK — both orders filled. next_order_id {next_id_before} → {} (Δ={advanced}), \
         total_maker_rebates_paid={}, total_protocol_fees={}",
        details_after.next_order_id,
        details_after.total_maker_rebates_paid,
        details_after.total_protocol_fees,
    );
}

/// Place + cancel asserted via `OrderPlaced` / `OrderCancelled` ext-out
/// events instead of polling getters. Events are the contract's
/// authoritative signal that an action settled — cleaner than racing
/// against indexer latency on `getOrdersByOwner`.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_events_place_and_cancel() {
    let test_start = now_unix();
    let dex = create_dex();
    // market[1] — separate from market[0] used by place/cancel/match tests
    // so the OrderPlaced/OrderCancelled stream stays clean for our queries.
    let market = ob_pool::nth_live_market(1);
    let pool = pn_pool::load();
    // trader[4] — distinct from place/cancel/match tests.
    let trader = pool.take_n(4, 1).into_iter().next().expect("pool has trader[4]");

    eprintln!("[events-pc] using trader {} on ob {}", trader.address, market.order_book_address);

    trader_split(&dex, &market, &trader, TRADER_SPLIT_COLLATERAL).await;

    let coid: u128 = salted_coid(3001);
    trader_place(&dex, &market, &trader, 0, false, ORDER_AMOUNT, ORDER_PRICE_BPS, coid).await;

    // Wait for the placed event — payload carries the chain-assigned
    // order_id, which is what we'll match the cancel event against.
    let placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced event for placed order");
    assert_eq!(placed.client_order_id, coid);
    assert_eq!(placed.outcome_id, 0);
    assert!(!placed.is_buy);
    assert_eq!(placed.amount, ORDER_AMOUNT);
    eprintln!(
        "[events-pc] OrderPlaced: order_id={} client_order_id={} amount={} price={}",
        placed.order_id, placed.client_order_id, placed.amount, placed.price,
    );

    dex.cancel_order_by_client(
        &trader.address,
        ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: coid,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("cancel_order_by_client");

    let cancelled = dex
        .wait_for_order_cancelled(
            &market.order_book_address,
            coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderCancelled event for cancelled order");
    assert_eq!(cancelled.client_order_id, coid);
    assert_eq!(
        cancelled.order_id, placed.order_id,
        "cancel event must reference the same order_id as the place event",
    );
    eprintln!(
        "[events-pc] OrderCancelled: order_id={} client_order_id={}",
        cancelled.order_id, cancelled.client_order_id,
    );
}

/// Crossing match asserted via `OrderFilled` events. Verifies both sides
/// see a fill, sum to the original amount, fees match the resting=maker /
/// crossing=taker model, and clearing_price comes out at the expected level.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_events_match_fills() {
    let test_start = now_unix();
    let dex = create_dex();
    // market[1] — same as events_place_and_cancel, separate from
    // place/cancel/match. Different traders+outcome+coid keep us isolated
    // within market[1].
    let market = ob_pool::nth_live_market(1);
    let pool = pn_pool::load();
    // traders[5..7] — distinct from previous tests.
    let traders = pool.take_n(5, 2);
    let seller = &traders[0];
    let buyer = &traders[1];

    eprintln!("[events-fill] sell={} buy={}", seller.address, buyer.address);

    trader_split(&dex, &market, seller, TRADER_SPLIT_COLLATERAL).await;
    trader_split(&dex, &market, buyer, TRADER_SPLIT_COLLATERAL).await;

    // Use outcome_id=1 to stay isolated from the place/cancel tests'
    // outcome-0 book.
    let outcome_id: u32 = 1;
    let seller_coid: u128 = salted_coid(4001);
    let buyer_coid: u128 = salted_coid(4002);

    trader_place(
        &dex,
        &market,
        seller,
        outcome_id,
        false,
        ORDER_AMOUNT,
        ORDER_PRICE_BPS,
        seller_coid,
    )
    .await;
    let seller_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            seller_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for seller");

    trader_place(&dex, &market, buyer, outcome_id, true, ORDER_AMOUNT, ORDER_PRICE_BPS, buyer_coid)
        .await;
    let buyer_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            buyer_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for buyer");

    // Both sides should produce an OrderFilled event referencing their
    // own chain-assigned order_id. Seller is maker (rested first), buyer
    // is taker (crossed it).
    let seller_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            seller_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for seller (maker)");
    let buyer_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            buyer_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for buyer (taker)");

    assert!(!seller_fill.is_taker, "seller posted first → maker");
    assert!(buyer_fill.is_taker, "buyer crossed → taker");
    assert_eq!(seller_fill.filled_amount, ORDER_AMOUNT);
    assert_eq!(buyer_fill.filled_amount, ORDER_AMOUNT);
    assert_eq!(
        seller_fill.clearing_price, buyer_fill.clearing_price,
        "both sides clear at the same price",
    );

    // 30 NACKL × 5000 bps / FULL_PERCENT = 15 NACKL notional.
    let notional: u128 = 15_000_000_000;
    let expected_maker_fee = notional * 15 / 100_000; // MAKER_FEE_RATE = 0.015%
    let expected_taker_fee = notional * 45 / 100_000; // TAKER_FEE_RATE = 0.045%
    assert_eq!(seller_fill.fee_amount, expected_maker_fee, "maker fee = 0.015% of notional");
    assert_eq!(buyer_fill.fee_amount, expected_taker_fee, "taker fee = 0.045% of notional");

    eprintln!(
        "[events-fill] OK — maker_fill: order_id={} amount={} fee={} taker={}; \
         taker_fill: order_id={} amount={} fee={} taker={}; clearing_price={}",
        seller_fill.order_id,
        seller_fill.filled_amount,
        seller_fill.fee_amount,
        seller_fill.is_taker,
        buyer_fill.order_id,
        buyer_fill.filled_amount,
        buyer_fill.fee_amount,
        buyer_fill.is_taker,
        seller_fill.clearing_price,
    );
}

/// Place a batch of 3 sell orders at distinct prices in a single
/// `placeBatch` call, verify each lands in the OB via OrderPlaced events
/// and `getOrdersByOwner`. Exercises the batch-place path which goes
/// through `OrderBook.executeBatch` (different code path from single
/// `placeOrder`) and concludes with `onBatchComplete` on the PN.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_place_batch_sells() {
    let test_start = now_unix();
    let dex = create_dex();
    // market[0] — same as place/cancel/match (separate from event tests'
    // market[1] which we don't want flooded with batch orders during
    // their event polls).
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    // trader[7] — distinct from all prior tests.
    let trader = pool.take_n(7, 1).into_iter().next().expect("pool has trader[7]");

    eprintln!("[batch-place] using trader {} on ob {}", trader.address, market.order_book_address);

    // 200 NACKL split → 100 outcome-0 + 100 outcome-1 (formula: 0.5
    // outcome-token per NACKL on this pool — see TRADER_SPLIT_COLLATERAL
    // doc above). Enough for 3 × 25 NACKL sells with comfortable margin.
    trader_split(&dex, &market, &trader, 200_000_000_000).await;

    // 3 sells at distinct prices, all on outcome 0. Distinct prices
    // guarantee none accidentally cross another batch order, and the
    // 6000–7000 bps band stays clear of other tests' 5000 bps level.
    let outcome_id: u32 = 0;
    let amount: u128 = 25_000_000_000; // 25 NACKL → notional 15 NACKL @ 6000 bps
    let prices = ["6000", "6500", "7000"];
    let coids: [u128; 3] = [salted_coid(5001), salted_coid(5002), salted_coid(5003)];

    let orders: Vec<OrderBookOrder> = coids
        .iter()
        .zip(prices.iter())
        .map(|(coid, price)| OrderBookOrder {
            outcome_id,
            is_buy: false,
            flags: 0,
            price: (*price).to_string(),
            amount,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: *coid,
        })
        .collect();

    dex.place_batch(
        &trader.address,
        ParamsOfPlaceBatch {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            orders,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("place_batch");

    // Each order in the batch produces its own OrderPlaced event.
    let mut placed_events = Vec::with_capacity(coids.len());
    for coid in coids {
        let p = dex
            .wait_for_order_placed(
                &market.order_book_address,
                coid,
                test_start,
                std::time::Duration::from_secs(90),
            )
            .await
            .unwrap_or_else(|e| panic!("OrderPlaced for client_order_id={coid}: {e}"));
        placed_events.push(p);
    }

    for ev in &placed_events {
        assert_eq!(ev.outcome_id, outcome_id);
        assert!(!ev.is_buy);
        assert_eq!(ev.amount, amount);
    }
    eprintln!(
        "[batch-place] all 3 OrderPlaced events captured: order_ids=[{}, {}, {}]",
        placed_events[0].order_id, placed_events[1].order_id, placed_events[2].order_id,
    );

    // Confirm via owner-index too — `getOrdersByOwner` should return at
    // least our 3 orders. Other tests may have parallel placements so we
    // assert ≥ 3 with our coids present, not equality.
    let owned = dex
        .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
        .await
        .expect("get_orders_by_owner");
    for coid in coids {
        assert!(
            owned.orders.iter().any(|o| o.client_order_id == coid),
            "owned orders should include client_order_id={coid}; got {:?}",
            owned.orders.iter().map(|o| o.client_order_id).collect::<Vec<_>>(),
        );
    }
    eprintln!("[batch-place] OK — 3/3 orders present in getOrdersByOwner");
}

/// Place a batch, then cancel the same orders by `order_id` via
/// `cancelBatch`. Verifies each `OrderCancelled` event fires.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_cancel_batch() {
    let test_start = now_unix();
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    // trader[8] — distinct.
    let trader = pool.take_n(8, 1).into_iter().next().expect("pool has trader[8]");

    eprintln!(
        "[batch-cancel] using trader {} on ob {}",
        trader.address, market.order_book_address,
    );

    trader_split(&dex, &market, &trader, 200_000_000_000).await;

    // Use a price band that doesn't collide with the place_batch test
    // (6000–7000) or place/cancel/match (5000) — pick 7500–8500.
    let outcome_id: u32 = 0;
    let amount: u128 = 25_000_000_000;
    let prices = ["7500", "8000", "8500"];
    let coids: [u128; 3] = [salted_coid(6001), salted_coid(6002), salted_coid(6003)];

    let orders: Vec<OrderBookOrder> = coids
        .iter()
        .zip(prices.iter())
        .map(|(coid, price)| OrderBookOrder {
            outcome_id,
            is_buy: false,
            flags: 0,
            price: (*price).to_string(),
            amount,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: *coid,
        })
        .collect();

    dex.place_batch(
        &trader.address,
        ParamsOfPlaceBatch {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            orders,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("place_batch");

    // Resolve each client_order_id to a chain-assigned order_id via the
    // OrderPlaced event payload.
    let mut order_ids = Vec::with_capacity(coids.len());
    for coid in coids {
        let p = dex
            .wait_for_order_placed(
                &market.order_book_address,
                coid,
                test_start,
                std::time::Duration::from_secs(90),
            )
            .await
            .unwrap_or_else(|e| panic!("OrderPlaced for client_order_id={coid}: {e}"));
        order_ids.push(p.order_id);
    }
    eprintln!("[batch-cancel] placed: order_ids={order_ids:?}");

    // PN must be idle before we send the next external (cancelBatch).
    // place_batch's onBatchComplete eventually clears _busy.
    wait_not_busy(&dex, &trader, "place_batch").await;

    dex.cancel_batch(
        &trader.address,
        ParamsOfCancelBatch {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            order_ids: order_ids.clone(),
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("cancel_batch");

    // Each cancel produces an OrderCancelled event matching its
    // client_order_id (the cancel callback reuses the original coid).
    for coid in coids {
        dex.wait_for_order_cancelled(
            &market.order_book_address,
            coid,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap_or_else(|e| panic!("OrderCancelled for client_order_id={coid}: {e}"));
    }
    eprintln!("[batch-cancel] OK — all 3 OrderCancelled events captured");
}

/// `POST_ONLY` (`flags = 0x08`) end-to-end test, both paths in one
/// sequential test to share a single trader-PN slot:
///
///   PHASE A — HAPPY: place POST_ONLY sell at a price no one would
///   cross (high sell, no buys waiting that high) → order rests like a
///   plain limit order. `OrderPlaced` fires; order shows up in
///   `getOrdersByOwner`. Then we cancel it for cleanup.
///
///   PHASE B — REJECT: deployer-PN posts a normal resting sell at a
///   LOW price; trader posts a POST_ONLY buy at a HIGHER price (would
///   cross). Contract emits `OrderPlaced` (order_id assigned BEFORE
///   the POST_ONLY check at PrivateNote.sol:758) immediately followed
///   by `OrderCancelled` from `_cancelNoBookEntryTo`. The would-be
///   POST_ONLY order never enters the book; the resting sell is
///   untouched (POST_ONLY rejects BEFORE any matching). Cleanup
///   cancels the deployer's resting sell.
///
/// One test (not two) so phase B doesn't race phase A on the same
/// trader-PN's `_busy` flag — combined sequential flow keeps the
/// trader idle between phases via `wait_not_busy`.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_post_only_rest_and_reject() {
    let test_start = now_unix();
    let dex = create_dex();
    // market[1] — same as event tests, but distinct outcome+price
    // bands keep us isolated.
    let market = ob_pool::nth_live_market(1);
    let pool = pn_pool::load();
    // trader[9] — last unused slot in the 10-PN pool.
    let trader = pool.take_n(9, 1).into_iter().next().expect("pool has trader[9]");

    eprintln!(
        "[post-only] using trader {} on ob {}",
        trader.address, market.order_book_address,
    );

    trader_split(&dex, &market, &trader, TRADER_SPLIT_COLLATERAL).await;

    // ── PHASE A: POST_ONLY rests (no opposite at crossing price) ────

    // outcome-1 + price 9500 bps: well above the 5000 bps cluster used
    // by other tests, so no resting buy will cross us.
    let happy_coid: u128 = salted_coid(7000);
    trader_place_with_flags(
        &dex,
        &market,
        &trader,
        1, // outcome
        false,
        ORDER_AMOUNT,
        "9500",
        happy_coid,
        FLAG_POST_ONLY,
    )
    .await;

    let happy_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            happy_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for POST_ONLY sell (no cross)");
    assert_eq!(happy_placed.flags, FLAG_POST_ONLY, "OrderPlaced payload echoes flags");
    eprintln!(
        "[post-only:A] rests as maker: order_id={} flags=0x{:02x}",
        happy_placed.order_id, happy_placed.flags,
    );

    // POST_ONLY that doesn't cross goes into the book exactly like a
    // plain limit order — must show up in getOrdersByOwner.
    let owned = dex
        .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        owned.orders.iter().any(|o| o.client_order_id == happy_coid),
        "POST_ONLY sell with no opposite to cross must rest in book",
    );

    // Cleanup phase A — also gates phase B by clearing _busy.
    dex.cancel_order_by_client(
        &trader.address,
        ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: happy_coid,
        },
        Signer::Keys { keys: trader.keys() },
    )
    .await
    .expect("cleanup cancel happy");
    dex.wait_for_order_cancelled(
        &market.order_book_address,
        happy_coid,
        test_start,
        std::time::Duration::from_secs(60),
    )
    .await
    .expect("cleanup cancel event");
    wait_not_busy(&dex, &trader, "post_only_happy_cleanup").await;

    // ── PHASE B: POST_ONLY rejected (would cross resting opposite) ──

    // Resting counterparty: deployer-PN posts a sell on outcome-0 at
    // price 3500 bps (notional = 30 × 3500/10000 = 10.5 NACKL — just
    // above MIN_ORDER_NOTIONAL_NACKL=10). Stays clear of all other
    // tests' price bands (5000+ standard, 6000–8500 batch).
    let resting_coid: u128 = salted_coid(7200);
    let deployer_keys = market.deployer_keys();
    dex.place_order(
        &market.deployer_pn_address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id: 0,
            is_buy: false,
            price: "3500".to_string(),
            amount: ORDER_AMOUNT,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: resting_coid,
        },
        Signer::Keys { keys: deployer_keys.clone() },
    )
    .await
    .expect("deployer resting sell");
    let _resting = dex
        .wait_for_order_placed(
            &market.order_book_address,
            resting_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for resting sell");

    // Trader attempts POST_ONLY BUY at higher price (4000 > 3500) →
    // would cross the resting sell. Contract should reject. Notional =
    // 30 × 4000 / 10000 = 12 NACKL, above the 10-NACKL minimum.
    let buyer_coid: u128 = salted_coid(7201);
    trader_place_with_flags(
        &dex,
        &market,
        &trader,
        0,    // outcome
        true, // buy
        ORDER_AMOUNT,
        "4000",
        buyer_coid,
        FLAG_POST_ONLY,
    )
    .await;

    // OrderPlaced fires — order_id assigned BEFORE the POST_ONLY check.
    let buy_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            buyer_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for POST_ONLY buy (announced before reject)");
    eprintln!(
        "[post-only:B] announced: order_id={} flags=0x{:02x}",
        buy_placed.order_id, buy_placed.flags,
    );

    // OrderCancelled follows — POST_ONLY check failed, contract cancels.
    let buy_cancelled = dex
        .wait_for_order_cancelled(
            &market.order_book_address,
            buyer_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderCancelled (POST_ONLY rejection)");
    assert_eq!(
        buy_cancelled.order_id, buy_placed.order_id,
        "rejection cancel must reference the same order_id as the announced place",
    );

    // Order MUST NOT be in book — POST_ONLY rejected before any book entry.
    let owned = dex
        .get_orders_by_owner(&market.order_book_address, trader.deposit_identifier_hash.clone())
        .await
        .expect("get_orders_by_owner buyer");
    assert!(
        !owned.orders.iter().any(|o| o.client_order_id == buyer_coid),
        "rejected POST_ONLY must not appear in book",
    );

    // Resting deployer sell must STILL be there — POST_ONLY reject
    // happens BEFORE any matching, so the book is untouched.
    let deployer_owned = dex
        .get_orders_by_owner(
            &market.order_book_address,
            market.deployer_deposit_identifier_hash.clone(),
        )
        .await
        .expect("get_orders_by_owner deployer");
    assert!(
        deployer_owned.orders.iter().any(|o| o.client_order_id == resting_coid),
        "resting deployer sell must remain after POST_ONLY rejection",
    );

    // Cleanup: cancel deployer's resting sell.
    dex.cancel_order_by_client(
        &market.deployer_pn_address,
        ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: resting_coid,
        },
        Signer::Keys { keys: deployer_keys },
    )
    .await
    .expect("cleanup deployer cancel");

    eprintln!("[post-only] OK — A: rested + cleaned, B: rejected, book unchanged");
}

// Silence unused-flag warnings until MARKET tests land.
const _: u8 = FLAG_MARKET;

/// `FOK` (`flags = 0x02`) — Fill-Or-Kill, both paths in one sequential
/// test:
///
///   PHASE A — REJECT: resting maker offers 30 outcome-tokens; FOK
///   taker requests 50 → contract walks the entire crossing region in
///   the FOK precheck, accumulates 30 (< 50 minFill), rejects via
///   `_cancelNoBookEntryTo` BEFORE entering the match loop. Events:
///     1. `OrderPlaced` for taker (line 758, before precheck)
///     2. `OrderCancelled` for taker (precheck reject path)
///   No `OrderFilled` for either side. Resting remains in book intact —
///   FOK reject is non-destructive to the rest of the book.
///
///   PHASE B — HAPPY: resting maker offers 30; FOK taker requests 30
///   (exact match) → precheck passes (accum == minFill), match loop
///   runs, both sides fully fill. Same event flow as a plain crossing
///   match (`OrderPlaced` + `OrderFilled` on both sides), since the
///   taker fully consumed there's no remainder cancel.
///
/// One test (not two) so phase B's resting placement happens AFTER
/// phase A cleaned up — avoids the FOK precheck walking phase A's
/// leftover resting along with phase B's.
///
/// Resting maker: deployer of the market (already split by seeder, no
/// extra setup). FOK taker: pool trader[1] — they only need NACKL
/// collateral for the buy side. Uses outcome 1 + price 3500: notional
/// 10.5 NACKL (just above the 10-NACKL minimum), and price 3500 is
/// below all other tests' bands on outcome 1 (events_match=5000,
/// post_only_happy=9500), so no other test's resting can leak into
/// our FOK precheck.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_fok_reject_and_happy() {
    let test_start = now_unix();
    let dex = create_dex();
    let market = ob_pool::nth_live_market(1);
    let pool = pn_pool::load();
    // trader[1] — only used by cancel test (short-lived); for FOK we
    // only need NACKL collateral on the buy side, no split.
    let taker = pool.take_n(1, 1).into_iter().next().expect("pool has trader[1]");
    let deployer_keys = market.deployer_keys();

    eprintln!(
        "[fok] resting={} (deployer); taker={} (trader[1])",
        market.deployer_pn_address, taker.address,
    );

    let outcome_id: u32 = 1;
    let price = "3500"; // notional 30 × 3500 / 10000 = 10.5 NACKL ≥ 10

    // ── PHASE A — REJECT (FOK requests more than the book has) ──────

    let a_resting_coid: u128 = salted_coid(9001);
    dex.place_order(
        &market.deployer_pn_address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id,
            is_buy: false,
            price: price.to_string(),
            amount: ORDER_AMOUNT,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: a_resting_coid,
        },
        Signer::Keys { keys: deployer_keys.clone() },
    )
    .await
    .expect("phase A resting sell");
    let a_resting_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            a_resting_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for phase A resting sell");
    eprintln!("[fok:A] resting placed: order_id={}", a_resting_placed.order_id);

    // FOK taker: buy 50 outcome-1 @ 3500 — more than the resting 30.
    let a_taker_amount: u128 = 50_000_000_000;
    let a_taker_coid: u128 = salted_coid(9002);
    trader_place_with_flags(
        &dex,
        &market,
        &taker,
        outcome_id,
        true,
        a_taker_amount,
        price,
        a_taker_coid,
        FLAG_FOK,
    )
    .await;
    let a_taker_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            a_taker_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for FOK taker (announced before precheck)");
    assert_eq!(a_taker_placed.flags, FLAG_FOK, "OrderPlaced echoes FOK flag");
    eprintln!(
        "[fok:A] taker placed: order_id={} flags=0x{:02x}",
        a_taker_placed.order_id, a_taker_placed.flags,
    );

    // Precheck fails → OrderCancelled. No OrderFilled for either side.
    let a_taker_cancelled = dex
        .wait_for_order_cancelled(
            &market.order_book_address,
            a_taker_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderCancelled for FOK precheck reject");
    assert_eq!(a_taker_cancelled.order_id, a_taker_placed.order_id);

    // Resting must STILL be in book — FOK reject is non-destructive.
    let owned_deployer = dex
        .get_orders_by_owner(
            &market.order_book_address,
            market.deployer_deposit_identifier_hash.clone(),
        )
        .await
        .expect("get_orders_by_owner deployer (phase A)");
    assert!(
        owned_deployer.orders.iter().any(|o| o.client_order_id == a_resting_coid),
        "FOK reject must NOT consume the resting maker",
    );
    eprintln!("[fok:A] OK — taker rejected, resting maker untouched");

    // Cleanup phase A — cancel the resting so it doesn't interfere with
    // phase B's precheck (which walks the whole crossing region).
    dex.cancel_order_by_client(
        &market.deployer_pn_address,
        ParamsOfCancelOrderByClient {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            client_order_id: a_resting_coid,
        },
        Signer::Keys { keys: deployer_keys.clone() },
    )
    .await
    .expect("phase A cleanup cancel");
    dex.wait_for_order_cancelled(
        &market.order_book_address,
        a_resting_coid,
        test_start,
        std::time::Duration::from_secs(60),
    )
    .await
    .expect("phase A cleanup cancel event");

    // ── PHASE B — HAPPY (FOK exact match) ───────────────────────────

    let b_resting_coid: u128 = salted_coid(9011);
    dex.place_order(
        &market.deployer_pn_address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id,
            is_buy: false,
            price: price.to_string(),
            amount: ORDER_AMOUNT,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: b_resting_coid,
        },
        Signer::Keys { keys: deployer_keys },
    )
    .await
    .expect("phase B resting sell");
    let b_resting_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            b_resting_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for phase B resting");

    // FOK taker: buy 30 outcome-1 @ 3500 — exact match.
    let b_taker_coid: u128 = salted_coid(9012);
    trader_place_with_flags(
        &dex,
        &market,
        &taker,
        outcome_id,
        true,
        ORDER_AMOUNT,
        price,
        b_taker_coid,
        FLAG_FOK,
    )
    .await;
    let b_taker_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            b_taker_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for FOK taker (happy)");

    // Both sides fully filled.
    let b_taker_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            b_taker_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for FOK taker");
    assert!(b_taker_fill.is_taker);
    assert_eq!(b_taker_fill.filled_amount, ORDER_AMOUNT);

    let b_maker_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            b_resting_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for FOK maker");
    assert!(!b_maker_fill.is_taker);
    assert_eq!(b_maker_fill.filled_amount, ORDER_AMOUNT);
    assert_eq!(
        b_taker_fill.clearing_price, b_maker_fill.clearing_price,
        "both clear at same price",
    );

    eprintln!(
        "[fok:B] OK — exact match: taker fill={} maker fill={} clearing_price={}",
        b_taker_fill.filled_amount, b_maker_fill.filled_amount, b_taker_fill.clearing_price,
    );
}

/// `mergeFullSet` round-trip: split N NACKL of collateral into outcome
/// tokens, then merge them back. Verifies the inverse identity:
///
///   split(F) → +t·u_k per outcome,  -F NACKL
///   merge(amounts) → burn t·u_k per outcome, +t·Q NACKL
///
/// where `Q = sum(u_k)` and `t·Q = F` for an exact round-trip on
/// symmetric pools (our seeder produces `Q=2, u=[1,1]`, so `F=50`
/// gives `t=25` and merging back `[25, 25]` returns 50 NACKL).
///
/// Uses the market's **deployer-PN** instead of a pool trader because:
/// - deployer is already split (seeder did 100 NACKL split), so we add
///   to existing _stakes without race vs another test's split
/// - no concurrent test does split/merge on the deployer-PN —
///   IOC/POST_ONLY/FOK use it ONLY as resting maker for orders, which
///   doesn't touch NACKL balance on placement (only fees on fill, ≤
///   tens of milli-NACKL — well below our 50-NACKL signal)
///
/// Asserts via NACKL balance deltas: split debits at least F, merge
/// credits at least F. Concurrent IOC fills on the deployer can only
/// ADD positive NACKL credits, so `≥` semantics hold.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_merge_full_set_roundtrip() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();

    eprintln!(
        "[merge] using deployer-PN {} on market {} (pool Q=2, u=[1,1] ⇒ 0.5 outcome / NACKL)",
        market.deployer_pn_address, market.pmp_address,
    );

    // Make sure the PN is idle — concurrent IOC/POST_ONLY/FOK tests may
    // be holding it as a resting maker.
    wait_addr_not_busy(&dex, &market.deployer_pn_address, "merge_setup_idle").await;
    let pn_addr = market.deployer_pn_address.clone();
    let pn_signer = || Signer::Keys { keys: market.deployer_keys() };

    // SPLIT: read NACKL balance before, split, read after, assert delta.
    const SPLIT_F: u128 = 50_000_000_000; // 50 NACKL
    const PER_OUTCOME_T: u128 = 25_000_000_000; // = SPLIT_F / Q where Q=2

    let before_split = read_nackl_balance(&dex, &pn_addr).await;
    eprintln!("[merge] NACKL before split: {before_split}");

    dex.split_full_set(
        &pn_addr,
        ParamsOfSplitFullSet {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            collateral: SPLIT_F,
        },
        pn_signer(),
    )
    .await
    .expect("split_full_set");
    wait_addr_not_busy(&dex, &pn_addr, "split_full_set").await;

    let after_split = read_nackl_balance(&dex, &pn_addr).await;
    let split_delta = before_split as i128 - after_split as i128;
    eprintln!("[merge] NACKL after split: {after_split} (Δ = {split_delta})");

    // MERGE: ask for [t, t] back → contract burns t·u_k per outcome and
    // returns t·Q = 50 NACKL.
    dex.merge_full_set(
        &pn_addr,
        ParamsOfMergeFullSet {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            amount: vec![PER_OUTCOME_T, PER_OUTCOME_T],
        },
        pn_signer(),
    )
    .await
    .expect("merge_full_set");
    wait_addr_not_busy(&dex, &pn_addr, "merge_full_set").await;

    let after_merge = read_nackl_balance(&dex, &pn_addr).await;
    let merge_delta = after_merge as i128 - after_split as i128;
    let roundtrip_drift = before_split as i128 - after_merge as i128;
    eprintln!(
        "[merge] NACKL after merge: {after_merge} (Δ from after_split = {merge_delta}, \
         roundtrip drift from before_split = {roundtrip_drift})",
    );

    // Strict equality on amounts is unreliable here — `trader[2]` is
    // shared with the match test, which can run a split (-100 NACKL) +
    // sell (+~15 NACKL after fee) cycle on the same PN inside our
    // measurement window. wait_not_busy serialises individual external
    // calls but doesn't fence whole multi-step phases, so concurrent
    // split/sell/merge by another test slips between our balance reads.
    //
    // Asserting DIRECTIONS (split debits, merge credits, both ≥ our
    // F = 50 NACKL contribution) catches the real bugs (no debit on
    // split, no credit on merge, mis-sized return) without flaking on
    // suite parallelism. The diagnostic eprintln above lets a human
    // verify the roundtrip drift is within the small NACKL fee+match
    // proceeds bucket, not pathological.
    assert!(
        split_delta >= SPLIT_F as i128,
        "split must debit at LEAST F NACKL (got delta={split_delta})",
    );
    assert!(
        merge_delta >= SPLIT_F as i128,
        "merge must credit at LEAST t·Q = F NACKL (got delta={merge_delta})",
    );

    eprintln!("[merge] OK — split debited ≥F, merge credited ≥F NACKL on the same PN");
}

/// Read the NACKL balance (token_type=1) from a PN's `getDetails`.
/// `_balance` map is keyed by token_type as a string.
async fn read_nackl_balance(dex: &Dex, pn_address: &str) -> u128 {
    let details = dex.get_private_note_details(pn_address).await.expect("pn details");
    *details.balance.get("1").unwrap_or(&0)
}

/// Variant of `wait_not_busy` keyed by raw PN address (not `PnNote`).
/// Used by tests operating on a market deployer-PN, whose keys live in
/// `ob_pool.json` rather than the trader pool.
async fn wait_addr_not_busy(dex: &Dex, address: &str, op: &str) {
    let mut saw_busy = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let d = dex.get_private_note_details(address).await.expect("pn details");
        if d.busy_address.is_some() {
            saw_busy = true;
            break;
        }
    }
    if !saw_busy {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        return;
    }
    for _ in 0..90 {
        let d = dex.get_private_note_details(address).await.expect("pn details");
        if d.busy_address.is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    panic!("PN {address} stayed busy after {op} for 90s");
}

/// Negative-validation test: drives `placeOrder` and `placeBatch` with
/// inputs that violate contract-side invariants and asserts the exact
/// `exit_code` we get back via `AppError.error_code`. Documents the
/// error-codes-on-the-wire contract for SDK consumers.
///
/// Cases covered:
///   - lot quantisation: `amount % LOT_SIZE_NACKL (10_000_000) != 0`
///     → `ERR_AMOUNT_NOT_LOT_MULTIPLE = 163`
///   - tick quantisation: `price % TICK_SIZE (10) != 0`
///     → `ERR_PRICE_NOT_TICK_MULTIPLE = 164`
///   - notional minimum: `amount × price / FULL_PERCENT < 10 NACKL`
///     → `ERR_ORDER_TOO_SMALL = 160`
///   - batch size: `orders.length > MAX_BATCH_SIZE (10)`
///     → `ERR_BATCH_TOO_LARGE = 161`
///
/// All checks fire on the PN side BEFORE state mutation, so the
/// trader's `_busy` flag never gets set and consecutive negative
/// calls don't race against each other. We still call
/// `wait_not_busy` at the start in case a concurrent test (e.g.
/// match) is mid-flight on the same trader-PN.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_negative_validation() {
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    // trader[3] — match-test buyer; used briefly, low concurrent risk.
    let trader = pool.take_n(3, 1).into_iter().next().expect("pool has trader[3]");

    eprintln!("[neg] using trader {} on market {}", trader.address, market.pmp_address);

    wait_not_busy(&dex, &trader, "neg_setup_idle").await;

    let signer = || Signer::Keys { keys: trader.keys() };
    let base_params = |amount: u128, price: &str, flags: u8| ParamsOfPlaceOrder {
        event_id: market.event_id.clone(),
        oracle_list_hash: market.oracle_list_hash.clone(),
        token_type: market.token_type,
        outcome_id: 0,
        is_buy: true, // buy avoids the stake-amount check, isolating to
        // lot/tick/notional rules
        flags,
        price: price.to_string(),
        amount,
        min_amount: 0,
        epoch_id: 0,
        client_order_id: salted_coid(10000), // never lands, no collision
    };

    // wait_not_busy before every call: the match test reuses trader[3]
    // as a buyer, and concurrent runs can hold `_busy` between our
    // sequential negative calls. ERR_NOTE_BUSY (121) would mask the
    // contract-validation codes (163/164/160/161) we're trying to
    // surface.

    // 1. ERR_AMOUNT_NOT_LOT_MULTIPLE = 163: amount = 30 NACKL + 1 wei.
    //    LOT_SIZE_NACKL = 10_000_000 (0.01 NACKL), so 30_000_000_001
    //    leaves remainder 1.
    wait_not_busy(&dex, &trader, "neg_pre_1").await;
    let r1 = dex
        .place_order(&trader.address, base_params(30_000_000_001, "5000", 0), signer())
        .await;
    assert_err_code(&r1, "163", "amount not lot-multiple");

    // 2. ERR_PRICE_NOT_TICK_MULTIPLE = 164: price = 5005 (TICK_SIZE = 10
    //    bps, so 5005 % 10 == 5).
    wait_not_busy(&dex, &trader, "neg_pre_2").await;
    let r2 = dex
        .place_order(&trader.address, base_params(30_000_000_000, "5005", 0), signer())
        .await;
    assert_err_code(&r2, "164", "price not tick-multiple");

    // 3. ERR_ORDER_TOO_SMALL = 160: notional below 10 NACKL.
    //    20 NACKL × 1000 bps / 10000 = 2 NACKL, well below the floor.
    wait_not_busy(&dex, &trader, "neg_pre_3").await;
    let r3 = dex
        .place_order(&trader.address, base_params(20_000_000_000, "1000", 0), signer())
        .await;
    assert_err_code(&r3, "160", "notional too small");

    // 4. ERR_BATCH_TOO_LARGE = 161: 11 orders, MAX_BATCH_SIZE = 10.
    //    Each individual order is well-formed — we want the size check
    //    to fire, not the per-order checks.
    wait_not_busy(&dex, &trader, "neg_pre_4").await;
    let oversized: Vec<OrderBookOrder> = (0..11)
        .map(|i| OrderBookOrder {
            outcome_id: 0,
            is_buy: true,
            flags: 0,
            price: "5000".to_string(),
            amount: 30_000_000_000,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: salted_coid(10100 + i),
        })
        .collect();
    let r4 = dex
        .place_batch(
            &trader.address,
            ParamsOfPlaceBatch {
                event_id: market.event_id.clone(),
                oracle_list_hash: market.oracle_list_hash.clone(),
                token_type: market.token_type,
                orders: oversized,
            },
            signer(),
        )
        .await;
    assert_err_code(&r4, "161", "batch size > MAX_BATCH_SIZE");

    eprintln!("[neg] OK — all 4 invariant violations rejected with documented codes");
}

/// `submitResolve` + OB shutdown drain + `claim` end-to-end on a stale
/// market (one whose `result_start` has already passed). Verifies the
/// closing-stage of the market lifecycle — which doesn't fit the
/// "live trading window" model the rest of the suite operates in.
///
/// Sequence:
///   1. Find a market past `result_start` and not yet resolved.
///   2. Oracle calls `PMP.submitResolve(outcome=0)` (oracle keys come
///      from `ob_pool.json`).
///   3. Poll PMP until `resolved_outcome == Some(0)`.
///   4. Poll PMP until `getShutdownState.order_book_done == true` —
///      the OB drained its queue and reported back via
///      `onOrderBookShutdownComplete`. `PMP.claim` is gated on this
///      flag (`require(_orderBookDone, ERR_ORDERBOOK_NOT_SHUTDOWN)`).
///   5. Deployer-PN calls `claim(eventId, oracleListHash, tokenType)`.
///      The deployer staked `[100, 100]` NACKL initially + `[0.2, 0.2]`
///      regular + holds 50/50 outcome tokens from the seeder split, so
///      they have actual claimable value regardless of which outcome
///      wins.
///   6. Verify deployer's NACKL balance increased — exact payout depends
///      on full pool composition and is too noisy to assert; the
///      directional check (`after > before`) catches "claim returned
///      nothing" regressions cleanly.
///
/// One-shot test: it consumes a market irreversibly. Run only when a
/// stale market is available in `ob_pool.json` (i.e. one of the seeded
/// markets has aged past its `result_start`). For repeated runs,
/// re-seed with `mint_ob_pool` between invocations.
#[tokio::test]
#[ignore = "requires a stale market in ob_pool.json + shellnet"]
async fn test_ob_resolve_and_claim() {
    let dex = create_dex();

    // Pick the first stale-but-not-resolved market by querying chain
    // state. The JSON has no "resolved" flag, so we have to ask each
    // candidate's PMP directly. Skip cancelled markets too.
    let candidates = ob_pool::post_result_start_markets();
    if candidates.is_empty() {
        eprintln!(
            "[resolve] SKIP — no market past result_start in the pool. \
             Seed a short-lifetime market to exercise the resolve+claim path: \
             cargo run --release --bin mint_ob_pool -- --count 1 --lifetime 5m"
        );
        return;
    }

    let mut market = None;
    for cand in candidates {
        let d = dex
            .get_pmp_details(&cand.pmp_address)
            .await
            .expect("pmp details for resolve candidate");
        if d.approved && d.resolved_outcome.is_none() && !d.is_cancelled {
            market = Some(cand);
            break;
        } else {
            eprintln!(
                "[resolve] skip pmp={} (resolved={:?}, cancelled={}, approved={})",
                cand.pmp_address, d.resolved_outcome, d.is_cancelled, d.approved,
            );
        }
    }
    let market = match market {
        Some(m) => m,
        None => {
            eprintln!(
                "[resolve] SKIP — all stale markets in the pool are already resolved \
                 or cancelled. Re-seed a short-lifetime market for this test: \
                 cargo run --release --bin mint_ob_pool -- --count 1 --lifetime 5m"
            );
            return;
        }
    };
    eprintln!(
        "[resolve] market: pmp={} ob={} result_start={} (age={}s)",
        market.pmp_address,
        market.order_book_address,
        market.result_start_unix,
        now_unix().saturating_sub(market.result_start_unix),
    );

    let winning_outcome: u32 = 0;
    eprintln!("[resolve] submitResolve(outcome={winning_outcome}) from oracle keys…");
    dex.submit_resolve(
        &market.pmp_address,
        ParamsOfSubmitResolve { outcome_id: winning_outcome },
        Signer::Keys { keys: market.oracle_keys() },
    )
    .await
    .expect("submit_resolve");

    // Wait for resolution to apply — single-oracle market, so quorum is
    // immediate, but the chain still needs a few blocks.
    let mut resolved_seen = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let d = dex.get_pmp_details(&market.pmp_address).await.expect("pmp resolve poll");
        if d.resolved_outcome == Some(winning_outcome) {
            resolved_seen = true;
            break;
        }
    }
    assert!(resolved_seen, "PMP did not reach resolved state within 60s");
    eprintln!("[resolve] PMP resolved to outcome={winning_outcome}");

    // Wait for OB shutdown drain — `claim` is gated on this.
    let mut ob_drained = false;
    for _ in 0..60 {
        let s = dex
            .get_pmp_shutdown_state(&market.pmp_address)
            .await
            .expect("pmp shutdown state");
        if s.order_book_done {
            ob_drained = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    assert!(ob_drained, "OB shutdown did not complete within 120s — claim would be gated");
    eprintln!("[resolve] OB shutdown complete (order_book_done=true)");

    // Snapshot deployer NACKL balance, claim, snapshot again.
    let before_claim = read_nackl_balance(&dex, &market.deployer_pn_address).await;
    eprintln!("[resolve] deployer NACKL before claim: {before_claim}");

    dex.claim(
        &market.deployer_pn_address,
        ParamsOfStakeKey {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
        },
        Signer::Keys { keys: market.deployer_keys() },
    )
    .await
    .expect("claim");
    wait_addr_not_busy(&dex, &market.deployer_pn_address, "claim").await;

    let after_claim = read_nackl_balance(&dex, &market.deployer_pn_address).await;
    let payout = after_claim.saturating_sub(before_claim);
    eprintln!(
        "[resolve] deployer NACKL after claim: {after_claim} (payout = +{payout})",
    );
    assert!(
        payout > 0,
        "claim should pay out something to the deployer (initial stakes + outcome tokens)",
    );

    // Stake record should be cleared after claim — re-claiming would
    // be a no-op or fail.
    let stakes_after =
        dex.get_stakes(&market.deployer_pn_address).await.expect("stakes after claim");
    eprintln!(
        "[resolve] deployer stakes after claim: {} entry(ies)",
        stakes_after.stakes.len(),
    );

    eprintln!("[resolve] OK — resolve + shutdown + claim end-to-end");
}

/// Assert that `result` is an `Err` whose `AppError.error_code` equals
/// `expected`. Panics otherwise.
fn assert_err_code<T: std::fmt::Debug>(
    result: &Result<T, dodex_sdk::errors::AppError>,
    expected: &str,
    label: &str,
) {
    match result {
        Err(e) if e.error_code.as_deref() == Some(expected) => {
            eprintln!("[neg] {label}: ✓ exit_code={expected}");
        }
        Err(e) => panic!(
            "{label}: expected error_code={expected}, got {:?} (full err: {e:?})",
            e.error_code,
        ),
        Ok(v) => panic!("{label}: expected failure with code {expected}, got Ok({v:?})"),
    }
}

/// `IOC` (`flags = 0x01`) — Immediate-Or-Cancel.
///
/// Semantics: fill as much as the book has crossing liquidity for *now*,
/// cancel the remainder — never rest the taker.
///
/// This test exercises the partial-fill path: resting maker offers 30
/// outcome-tokens, IOC taker requests 60. Expected event sequence:
///
///   1. `OrderPlaced` for the IOC taker (line 758, before matching)
///   2. `OrderFilled` for the IOC taker, `is_taker=true`,
///      `filled_amount=30` (matches the maker's full size)
///   3. `OrderFilled` for the resting maker, `is_taker=false`, same
///      `filled_amount=30`
///   4. `OrderCancelled` for the IOC taker, covering the 30-token
///      remainder (`_cancelNoBookEntryTo` after the match loop, see
///      OrderBook.sol `if (isIoc || isFok || isMarket)` branch)
///
/// Neither side rests — the maker is fully consumed; the taker's remainder
/// was rejected from book entry. `getOrdersByOwner` returns nothing for
/// the IOC taker.
///
/// Roles:
///   - Resting maker: deployer-PN of the market under test (already split
///     50 outcome-each by the seeder, no extra setup).
///   - IOC taker: pool trader[0] — buying doesn't require split (only
///     NACKL collateral), and trader[0] is idle most of the suite's
///     wall-clock so contention is minimal.
///
/// Outcome 1 + price 5500 sit in a price band no other test crosses:
/// match/events_match operate at 5000 (won't cross our 5500), batch
/// tests are on outcome 0.
#[tokio::test]
#[ignore = "requires ob_pool.json + pn_pool[_1].json + shellnet"]
async fn test_ob_ioc_partial_fill() {
    let test_start = now_unix();
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    let taker = pool.take_n(0, 1).into_iter().next().expect("pool has trader[0]");

    eprintln!(
        "[ioc] resting={} (deployer); taker={} (trader[0])",
        market.deployer_pn_address, taker.address,
    );

    // 1. Resting maker: deployer sells 30 outcome-1 @ 5500.
    //    notional = 30 × 5500 / 10000 = 16.5 NACKL, above 10-NACKL min.
    let outcome_id: u32 = 1;
    let resting_amount = ORDER_AMOUNT;
    let resting_price = "5500";
    let resting_coid: u128 = salted_coid(8001);
    let deployer_keys = market.deployer_keys();
    dex.place_order(
        &market.deployer_pn_address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id,
            is_buy: false,
            price: resting_price.to_string(),
            amount: resting_amount,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: resting_coid,
        },
        Signer::Keys { keys: deployer_keys.clone() },
    )
    .await
    .expect("deployer resting sell");
    let resting_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            resting_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for resting sell");
    eprintln!(
        "[ioc] resting maker placed: order_id={} amount={}",
        resting_placed.order_id, resting_placed.amount,
    );

    // 2. IOC taker: buy 60 outcome-1 @ 5500 (twice the resting amount).
    let ioc_amount: u128 = 60_000_000_000;
    let ioc_coid: u128 = salted_coid(8002);
    trader_place_with_flags(
        &dex,
        &market,
        &taker,
        outcome_id,
        true, // buy
        ioc_amount,
        resting_price,
        ioc_coid,
        FLAG_IOC,
    )
    .await;

    // 3. IOC's OrderPlaced fires (regardless of fill outcome — line 758).
    let ioc_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            ioc_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for IOC buy");
    assert_eq!(ioc_placed.flags, FLAG_IOC, "OrderPlaced echoes IOC flag");
    eprintln!(
        "[ioc] taker placed: order_id={} amount={} flags=0x{:02x}",
        ioc_placed.order_id, ioc_placed.amount, ioc_placed.flags,
    );

    // 4. OrderFilled for the IOC taker — `is_taker=true`, filled_amount
    //    matches the resting amount (matched everything available).
    let taker_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            ioc_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for IOC taker");
    assert!(taker_fill.is_taker, "IOC = taker");
    assert_eq!(
        taker_fill.filled_amount, resting_amount,
        "IOC fills exactly the available resting size",
    );

    // 5. OrderFilled for the resting maker — `is_taker=false`, full
    //    consumption.
    let maker_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            resting_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for resting maker");
    assert!(!maker_fill.is_taker, "resting = maker");
    assert_eq!(maker_fill.filled_amount, resting_amount);

    // 6. OrderCancelled for the IOC taker — covers the 30-token remainder
    //    that didn't find liquidity. `_cancelNoBookEntryTo` runs in the
    //    `if (isIoc || isFok || isMarket)` tail of OB.placeOrder.
    let ioc_cancelled = dex
        .wait_for_order_cancelled(
            &market.order_book_address,
            ioc_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderCancelled for IOC remainder");
    assert_eq!(
        ioc_cancelled.order_id, ioc_placed.order_id,
        "IOC remainder cancel must reference taker's order_id",
    );

    // 7. IOC must NOT rest — the cancel covers the remainder, no book entry.
    let owned_taker = dex
        .get_orders_by_owner(&market.order_book_address, taker.deposit_identifier_hash.clone())
        .await
        .expect("get_orders_by_owner taker");
    assert!(
        !owned_taker.orders.iter().any(|o| o.client_order_id == ioc_coid),
        "IOC remainder must not appear in book",
    );

    // 8. Resting maker must NOT be in book — fully consumed by the fill.
    let owned_deployer = dex
        .get_orders_by_owner(
            &market.order_book_address,
            market.deployer_deposit_identifier_hash.clone(),
        )
        .await
        .expect("get_orders_by_owner deployer");
    assert!(
        !owned_deployer.orders.iter().any(|o| o.client_order_id == resting_coid),
        "resting maker must be consumed (full fill)",
    );

    eprintln!(
        "[ioc] OK — taker fill {} + cancel remainder, maker fully consumed",
        taker_fill.filled_amount,
    );
}

/// Diagnostic-only: dump live orders from market[1] for inspection.
/// Helpful to debug POST_ONLY false-rejects (figure out who's in the
/// book at high prices). Not a real test — always asserts ok.
#[tokio::test]
#[ignore = "diagnostic; run explicitly"]
async fn diag_dump_market1_orders() {
    let dex = create_dex();
    let market = ob_pool::nth_live_market(1);
    let details =
        dex.get_order_book_details(&market.order_book_address).await.expect("ob details");
    eprintln!(
        "[diag] OB {} next_order_id={} order_count={}",
        market.order_book_address, details.next_order_id, details.order_count,
    );
    for id in 1..details.next_order_id {
        if let Ok(o) = dex.get_order(&market.order_book_address, id).await {
            eprintln!(
                "  order_id={id} outcome={} is_buy={} price={} amount={} dih={}",
                o.outcome_id, o.is_buy, o.price, o.amount, o.deposit_identifier_hash,
            );
        }
    }

    let pool = pn_pool::load();
    let trader9 = &pool.notes[9];
    eprintln!(
        "[diag] trader[9].address={} deposit_identifier_hash={}",
        trader9.address, trader9.deposit_identifier_hash,
    );
    let owned = dex
        .get_orders_by_owner(&market.order_book_address, trader9.deposit_identifier_hash.clone())
        .await
        .expect("get_orders_by_owner trader9");
    eprintln!(
        "[diag] getOrdersByOwner(trader[9].dih) → {} order(s)",
        owned.orders.len(),
    );
    for o in &owned.orders {
        eprintln!(
            "  owned: order_id={} client_order_id={} outcome={} is_buy={} amount={}",
            o.order_id, o.client_order_id, o.outcome_id, o.is_buy, o.amount,
        );
    }
}

/// Demo: deployer-PN (which was staked on both outcomes during mint_ob_pool's
/// bidding window) places a resting sell; trader[0] from pn_pool splits and
/// crosses with a matching buy → both sides fully fill. The test writes a
/// JSON artifact at `dodex_sdk/demo_artifacts.json` capturing the deployer-PN
/// credentials (staked + traded), the counter-party trader, the PMP and the
/// OrderBook addresses. BOC snapshots are dumped externally by the wrapper
/// script (see /tmp/watch_pmp_boc.sh for the post-stake snapshot).
///
/// Run with a freshly-seeded `ob_pool.json` (1h lifetime) and `pn_pool.json`:
///   cargo test -p dodex-sdk --test integration \
///     test_ob_demo_deployer_staked_and_trades -- --ignored --nocapture
#[tokio::test]
#[ignore = "demo: requires ob_pool.json + pn_pool.json + shellnet"]
async fn test_ob_demo_deployer_staked_and_trades() {
    let test_start = now_unix();
    let dex = create_dex();
    let market = ob_pool::first_live_market();
    let pool = pn_pool::load();
    let trader = pool.take_n(0, 1).into_iter().next().expect("pn_pool has trader[0]");

    eprintln!(
        "[demo] market: pmp={} ob={} result_start_in={}s",
        market.pmp_address,
        market.order_book_address,
        market.seconds_until_result_start(),
    );
    eprintln!("[demo] deployer (already staked + split): pn={}", market.deployer_pn_address);
    eprintln!("[demo] counter-party trader: pn={}", trader.address);

    // Trader splits NACKL → outcome tokens (deployer was pre-split in mint_ob_pool).
    trader_split(&dex, &market, &trader, TRADER_SPLIT_COLLATERAL).await;

    let outcome_id: u32 = 0;
    let deployer_coid: u128 = salted_coid(20001);
    let trader_coid: u128 = salted_coid(20002);
    let deployer_keys = market.deployer_keys();

    // Deployer places resting sell.
    dex.place_order(
        &market.deployer_pn_address,
        ParamsOfPlaceOrder {
            event_id: market.event_id.clone(),
            oracle_list_hash: market.oracle_list_hash.clone(),
            token_type: market.token_type,
            outcome_id,
            is_buy: false,
            price: ORDER_PRICE_BPS.to_string(),
            amount: ORDER_AMOUNT,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: deployer_coid,
        },
        Signer::Keys { keys: deployer_keys },
    )
    .await
    .expect("deployer place sell");

    let deployer_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            deployer_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for deployer sell");
    eprintln!("[demo] deployer sell rested: order_id={}", deployer_placed.order_id);

    // Trader crosses with matching buy.
    trader_place(&dex, &market, &trader, outcome_id, true, ORDER_AMOUNT, ORDER_PRICE_BPS, trader_coid)
        .await;
    let trader_placed = dex
        .wait_for_order_placed(
            &market.order_book_address,
            trader_coid,
            test_start,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("OrderPlaced for trader buy");

    let deployer_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            deployer_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for deployer (maker)");
    let trader_fill = dex
        .wait_for_order_filled(
            &market.order_book_address,
            trader_placed.order_id,
            test_start,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("OrderFilled for trader (taker)");

    assert!(!deployer_fill.is_taker, "deployer rested first → maker");
    assert!(trader_fill.is_taker, "trader crossed → taker");
    assert_eq!(deployer_fill.filled_amount, ORDER_AMOUNT);
    assert_eq!(trader_fill.filled_amount, ORDER_AMOUNT);

    eprintln!(
        "[demo] FILLED — deployer order_id={} filled={} fee={}; trader order_id={} filled={} fee={}; clearing_price={}",
        deployer_fill.order_id,
        deployer_fill.filled_amount,
        deployer_fill.fee_amount,
        trader_fill.order_id,
        trader_fill.filled_amount,
        trader_fill.fee_amount,
        deployer_fill.clearing_price,
    );

    // Persist artifacts as JSON for the wrapper to consume (BOC dumps land
    // adjacent, named via the addresses below).
    let artifacts = serde_json::json!({
        "endpoint": "shellnet.ackinacki.org",
        "market": {
            "pmp_address": market.pmp_address,
            "order_book_address": market.order_book_address,
            "event_id": market.event_id,
            "oracle_list_hash": market.oracle_list_hash,
            "token_type": market.token_type,
            "stake_start_unix": market.stake_start_unix,
            "stake_end_unix": market.stake_end_unix,
            "result_start_unix": market.result_start_unix,
            "result_end_unix": market.result_end_unix,
            "freeze_unix": market.freeze_unix,
        },
        "staked_and_traded_pn": {
            "role": "deployer-PN: staked initial + setStake on both outcomes during bidding window, splitFullSet after freeze, sold 30 outcome-0 @ 5000bps as maker",
            "address": market.deployer_pn_address,
            "owner_public_key_hex": market.deployer_pn_pubkey_hex,
            "owner_secret_key_hex": market.deployer_pn_secret_hex,
            "deposit_identifier_hash": market.deployer_deposit_identifier_hash,
        },
        "counter_party_pn": {
            "role": "trader[0] from pn_pool: splitFullSet, bought 30 outcome-0 @ 5000bps as taker",
            "address": trader.address,
            "owner_public_key_hex": trader.owner_public_key_hex,
            "owner_secret_key_hex": trader.owner_secret_key_hex,
            "deposit_identifier_hash": trader.deposit_identifier_hash,
        },
        "trade": {
            "outcome_id": outcome_id,
            "price_bps": ORDER_PRICE_BPS,
            "amount_raw": ORDER_AMOUNT,
            "clearing_price": deployer_fill.clearing_price.to_string(),
            "deployer_order_id": deployer_fill.order_id.to_string(),
            "deployer_fee_raw": deployer_fill.fee_amount.to_string(),
            "trader_order_id": trader_fill.order_id.to_string(),
            "trader_fee_raw": trader_fill.fee_amount.to_string(),
        },
        "boc_snapshots": {
            "pmp_after_stake": "pmp_after_stake.boc",
            "pmp_after_trades": "pmp_after_trades.boc",
            "orderbook_after_trades": "orderbook_after_trades.boc",
        },
    });
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out = std::path::Path::new(&manifest).join("demo_artifacts.json");
    std::fs::write(&out, serde_json::to_string_pretty(&artifacts).unwrap())
        .expect("write demo_artifacts.json");
    eprintln!("[demo] wrote artifacts → {}", out.display());
}

