//! Orders that must not rest, on all three paths through the branch that
//! decides it.
//!
//! Every order the suite had placed was a plain limit order. `OrderBook`
//! treats an unfilled remainder in exactly one place — inserted into the book
//! for a limit order, returned to the caller for a market, IOC or FOK one —
//! and nothing exercised the returning side. The failure it guards against is
//! silent and permanent: an order that rested when it should not would sit
//! there holding escrow its owner believes it got back.
//!
//! One market, three phases, each reaching that branch differently:
//!
//! 1. **Market buy over a resting ask.** `amount` is denominated in quote, and
//!    the unfilled part comes back unscaled by any price.
//! 2. **Market sell over a resting bid.** `amount` is in base instead, and the
//!    remainder returns through the book's collateral conversion rather than
//!    verbatim — the same branch, different arithmetic on either side of it.
//! 3. **IOC into a book with nothing on either side.** The degenerate case:
//!    nothing to fill, so the whole order is returned. A book that rested it
//!    instead is indistinguishable from one that simply found no match, right
//!    up until the escrow reading.
//!
//! Each phase asserts the same shape — the sender's owner index is empty
//! afterwards, and its `_lockedInOrders` is exactly what it was before the
//! order. Escrow is the half that matters: an order that rested would hold
//! collateral, and an order that vanished without refunding would spend it.
//!
//! What the fills *cost* is never asserted against a re-derivation of the
//! contract's fee arithmetic — that would check the implementation against
//! itself. Phases 1 and 2 assert the fill in tokens, which is exact, and
//! phase 1 additionally asserts that the buyer paid *something*, since a
//! market order that matched nothing would satisfy the escrow reading too.
//!
//! The three phases share one market and one pair of notes on purpose: they
//! test one branch from three sides, and a scenario each would cost three
//! notes and a pipeline step apiece for no extra coverage.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::at;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::place_order_with_flags;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::market::FLAG_IOC;
use crate::common::market::FLAG_MARKET;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_ORDERS: u64 = 240;
const OUTCOME: u32 = 0;

/// The resting ask: 30 outcome tokens at 0.60, worth 18 NACKL.
const ASK_BPS: &str = "6000";
const ASK_AMOUNT: u128 = 30_000_000_000;

/// The market buy, in **quote**. Comfortably more than the ask is worth, so a
/// remainder is guaranteed — which is the only reason this scenario can say
/// anything about what happens to one.
const MARKET_BUY_QUOTE: u128 = 40_000_000_000;

/// Collateral the seller splits to get something to sell.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

/// The bid the seller rests once it has been paid, so the market sell has
/// something to hit. At `BID_BPS` it is worth 12 NACKL.
const BID_BPS: &str = "6000";
const BID_AMOUNT: u128 = 20_000_000_000;

/// The market sell, in **base** — every token the buy above obtained, which
/// is more than the bid can absorb.
const MARKET_SELL_BASE: u128 = ASK_AMOUNT;

/// The IOC order sent into a book with nothing on either side.
const IOC_AMOUNT: u128 = 20_000_000_000;

// The sell has to exceed the bid, or there is no remainder and the scenario
// says nothing about what happens to one.
const _: () = assert!(MARKET_SELL_BASE > BID_AMOUNT);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn orders_that_must_not_rest_never_rest_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "market_orders").expect("rent the deployer note");
    let seller = alloc.rent(PnProfile::Trd, "market_orders").expect("rent the seller note");
    let buyer = alloc.rent(PnProfile::Trd, "market_orders").expect("rent the buyer note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_ORDERS).await;

    split_full_set(dex, &seller, &market.key, SPLIT_COLLATERAL).await;

    let ask_coid = nonce as u128 * 10;
    let buy_coid = ask_coid + 1;

    place_limit(dex, &seller, &market.key, OUTCOME, false, ASK_BPS, ASK_AMOUNT, ask_coid).await;
    // The ask has to be on the book *before* the market buy is sent, or the
    // buy finds nothing, fills nothing, and every assertion below still holds
    // for the wrong reason.
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, true).await;

    let buyer_free_before = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_before = pn_locked(&r, &buyer.note.address).await;
    let buyer_tokens_before = outcome_tokens(dex, &buyer).await;

    // A market buy: price is ignored, and `amount` is the collateral to spend.
    place_order_with_flags(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        "0",
        MARKET_BUY_QUOTE,
        FLAG_MARKET,
        buy_coid,
    )
    .await;

    // The ask leaving the seller's index is what proves the buy was matched
    // against it rather than ignored.
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, false).await;
    wait_not_busy(dex, &buyer.note.address, "market buy").await;
    wait_not_busy(dex, &seller.note.address, "ask filled").await;

    // The claim: nothing of the market order stayed on the book.
    let buyer_orders = dex
        .get_orders_by_owner(&market.order_book, buyer.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        buyer_orders.orders.is_empty(),
        "the market buy left {} order(s) resting; a market order's remainder is returned, \
         never inserted",
        buyer_orders.orders.len()
    );
    assert_eq!(
        open_orders(&r, &buyer.note.address).await,
        0,
        "the buyer's note still counts an open order after a market buy"
    );

    // It filled the whole ask and no more.
    let buyer_tokens_after = outcome_tokens(dex, &buyer).await;
    assert_eq!(
        at(&buyer_tokens_after, OUTCOME),
        at(&buyer_tokens_before, OUTCOME) + ASK_AMOUNT,
        "the market buy did not take exactly the {ASK_AMOUNT} tokens the ask offered"
    );

    // The escrow half of "never rests": a returned remainder leaves nothing
    // locked, whatever the fill itself cost.
    assert_eq!(
        pn_locked(&r, &buyer.note.address).await,
        buyer_locked_before,
        "the market buy left collateral locked, which only a resting order should do"
    );
    // And it did pay: without this the reading above is equally true of a
    // market order that never matched anything.
    let buyer_free_after = pn_balance(&r, &buyer.note.address).await;
    assert!(
        buyer_free_after < buyer_free_before,
        "the buyer's free collateral did not fall ({buyer_free_before} -> {buyer_free_after}), \
         so nothing was paid for the tokens it holds"
    );

    // ── the same branch from the sell side ────────────────────────────────
    //
    // A market sell denominates `amount` in outcome tokens rather than quote,
    // and its unfilled part comes back through `_collateralFor` rather than
    // being returned unscaled. Same branch in the book, different arithmetic
    // on either side of it, so the buy above does not cover this.
    //
    // The seller was paid in collateral for the ask; it now bids some of that
    // back, giving the market sell something to hit.
    let bid_coid = ask_coid + 10;
    place_limit(dex, &seller, &market.key, OUTCOME, true, BID_BPS, BID_AMOUNT, bid_coid).await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, bid_coid, true).await;

    let sell_tokens_before = outcome_tokens(dex, &buyer).await;
    let sell_coid = ask_coid + 11;

    // More tokens than the bid can absorb, so there is again a remainder to
    // account for.
    place_order_with_flags(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        false,
        "0",
        MARKET_SELL_BASE,
        FLAG_MARKET,
        sell_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, bid_coid, false).await;
    wait_not_busy(dex, &buyer.note.address, "market sell").await;
    wait_not_busy(dex, &seller.note.address, "bid filled").await;

    let after_sell = dex
        .get_orders_by_owner(&market.order_book, buyer.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        after_sell.orders.is_empty(),
        "the market sell left {} order(s) resting",
        after_sell.orders.len()
    );
    assert_eq!(
        at(&outcome_tokens(dex, &buyer).await, OUTCOME),
        at(&sell_tokens_before, OUTCOME) - BID_AMOUNT,
        "the market sell gave up something other than exactly the {BID_AMOUNT} the bid took; \
         the unsold remainder must come back rather than rest"
    );

    // ── and with no liquidity at all ──────────────────────────────────────
    //
    // Both sides of the book are empty now. An IOC order takes the same
    // never-rest branch, and this is the degenerate case of it: nothing to
    // fill, so the whole order is returned. A book that rested it instead
    // would look identical to one that simply found no match — until the
    // escrow reading below.
    let ioc_free_before = pn_balance(&r, &buyer.note.address).await;
    let ioc_locked_before = pn_locked(&r, &buyer.note.address).await;
    let ioc_coid = ask_coid + 12;

    place_order_with_flags(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        BID_BPS,
        IOC_AMOUNT,
        FLAG_IOC,
        ioc_coid,
    )
    .await;
    wait_not_busy(dex, &buyer.note.address, "ioc into an empty book").await;

    let after_ioc = dex
        .get_orders_by_owner(&market.order_book, buyer.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        after_ioc.orders.is_empty(),
        "the IOC order rested in an empty book instead of being returned"
    );
    assert_eq!(
        pn_locked(&r, &buyer.note.address).await,
        ioc_locked_before,
        "the IOC order kept collateral locked after filling nothing"
    );
    assert_eq!(
        pn_balance(&r, &buyer.note.address).await,
        ioc_free_before,
        "an IOC order that filled nothing still cost the note collateral"
    );

    seller.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    buyer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// The note's free NACKL — `_balance`, without what its resting orders hold.
async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

/// The note's NACKL held against its resting orders — `_lockedInOrders`.
async fn pn_locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

/// How many orders the note believes it has resting.
async fn open_orders(r: &chain_reader::ChainReader, pn_address: &str) -> u32 {
    invariant::pn_open_order_count(r, pn_address).await.expect("read open order count")
}
