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
//! phase 1 bounds what the buyer is out: at least the ask's worth, at most
//! that plus any plausible fee. The quote it locked is more than twice that,
//! so the bound separates a returned remainder from a kept one without ever
//! naming the fee.
//!
//! A fourth phase covers the two things a market order may not carry — a
//! minimum fill size, and a buy worth less than the minimum notional. Neither
//! refusal is legible as an error: the note accepts the external message
//! before it validates, and the send does not wait for the transaction that
//! aborts. So the phase reads the refusal as the absence of its effects —
//! nothing rested, nothing locked, nothing spent — with `_opNonce` saying
//! *who* refused, since it only advances where the note dispatches to the
//! book. A valid order carrying the same client id closes the phase, because
//! "nothing happened" is equally true of a message that never arrived.
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
use crate::common::market::cancel_by_client;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::place_order_with_flags;
use crate::common::market::split_full_set;
use crate::common::market::try_place_order;
use crate::common::market::wait_owner_order;
use crate::common::market::FLAG_IOC;
use crate::common::market::FLAG_MARKET;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_ORDERS: u64 = 300;
const OUTCOME: u32 = 0;

/// Basis-point denominator, mirroring the contracts' `FULL_PERCENT`.
const FULL_PERCENT: u128 = 10_000;

/// The resting ask: 30 outcome tokens at 0.60, worth 18 NACKL.
const ASK_BPS: u128 = 6_000;
const ASK_AMOUNT: u128 = 30_000_000_000;

/// What that ask is worth — what the market buy below actually spends, as
/// against the much larger quote it locks.
const ASK_COST: u128 = ASK_AMOUNT * ASK_BPS / FULL_PERCENT;

/// The market buy, in **quote**. Comfortably more than the ask is worth, so a
/// remainder is guaranteed — which is the only reason this scenario can say
/// anything about what happens to one.
const MARKET_BUY_QUOTE: u128 = 40_000_000_000;

/// Collateral the seller splits to get something to sell.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

/// The bid the seller rests once it has been paid, so the market sell has
/// something to hit. At `BID_BPS` it is worth 12 NACKL.
const BID_BPS: u128 = 6_000;
const BID_AMOUNT: u128 = 20_000_000_000;

/// The market sell, in **base** — every token the buy above obtained, which
/// is more than the bid can absorb.
const MARKET_SELL_BASE: u128 = ASK_AMOUNT;

/// The IOC order sent into a book with nothing on either side.
const IOC_AMOUNT: u128 = 20_000_000_000;

// The sell has to exceed the bid, or there is no remainder and the scenario
// says nothing about what happens to one.
const _: () = assert!(MARKET_SELL_BASE > BID_AMOUNT);
// And the buy has to lock far more than the ask is worth, or "the unspent
// part came back" is a statement about a rounding error.
const _: () = assert!(MARKET_BUY_QUOTE > 2 * ASK_COST);

/// An upper bound on the taker fee, deliberately not the contract's rate
/// (0.045%): restating that here would check the implementation against
/// itself. A tenth of a percent is more than double the real rate and orders
/// of magnitude below the quote this phase asserts came back.
const FEE_CAP_PERMILLE: u128 = 1;

/// The 10 NACKL floor under any order's value, mirroring
/// `MIN_ORDER_NOTIONAL_NACKL`.
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// A market buy one unit under that floor.
const BELOW_MIN_NOTIONAL: u128 = MIN_ORDER_NOTIONAL - 1;

/// A minimum fill size of one lot, attached to an order that cannot carry one.
const MIN_FILL_ON_MARKET: u128 = 10_000_000;

/// The buy placed with a client id two refusals have already used. Worth
/// 12 NACKL at `BID_BPS`, so it is a valid order in its own right.
const REUSED_CID_AMOUNT: u128 = 20_000_000_000;

const _: () = assert!(REUSED_CID_AMOUNT * BID_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

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

    place_limit(
        dex,
        &seller,
        &market.key,
        OUTCOME,
        false,
        &ASK_BPS.to_string(),
        ASK_AMOUNT,
        ask_coid,
    )
    .await;
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
    // And it paid for the fill and only the fill. The quote locked is more
    // than twice what the ask was worth, so the two readings are far apart:
    // a book that kept the unmatched part would leave the buyer short by
    // `MARKET_BUY_QUOTE`, not by `ASK_COST`. The upper bound carries the fee
    // rather than restating it.
    let buyer_free_after = pn_balance(&r, &buyer.note.address).await;
    let buyer_paid = (buyer_free_before + buyer_locked_before)
        - (buyer_free_after + pn_locked(&r, &buyer.note.address).await);
    assert!(
        buyer_paid >= ASK_COST,
        "the buyer paid {buyer_paid} for tokens worth {ASK_COST}"
    );
    assert!(
        buyer_paid <= ASK_COST + ASK_COST * FEE_CAP_PERMILLE / 1000,
        "the buyer is out {buyer_paid} on a fill worth {ASK_COST}, against the \
         {MARKET_BUY_QUOTE} its market order locked — the quote it did not spend was not \
         returned"
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
    place_limit(
        dex,
        &seller,
        &market.key,
        OUTCOME,
        true,
        &BID_BPS.to_string(),
        BID_AMOUNT,
        bid_coid,
    )
    .await;
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
        &BID_BPS.to_string(),
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

    // ── and the two a market order may not carry ──────────────────────────
    //
    // Both rules are enforced twice — once by the note before it sends
    // anything, once by the book before it queues the entry — because a bad
    // entry that reached the queue would stall every order behind it. Which
    // of the two answered is not a matter of taste: `_opNonce` advances only
    // where the note dispatches a batch to the book, so a nonce that has not
    // moved says the note refused on its own and the book never heard of it.
    //
    // The refusal cannot be read as an error. The note accepts the external
    // message before it validates anything, so a `require` that fires
    // afterwards leaves an aborted transaction — and the send does not wait
    // for a transaction to report on. What it does leave is the absence of
    // every effect a placement has, which is what this reads. "Nothing
    // happened" is equally true of a message that never arrived, so a valid
    // order carrying the same client id closes the phase as the control.
    let refused_coid = ask_coid + 13;
    let held_before = pn_balance(&r, &buyer.note.address).await
        + pn_locked(&r, &buyer.note.address).await;
    let nonce_before = op_nonce(&r, &buyer.note.address).await;

    // A minimum fill size is stated in tokens; a market buy's amount is
    // collateral. There is no exchange rate between the two until a fill
    // picks one, so the combination is refused rather than interpreted.
    let _ = try_place_order(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        "0",
        MARKET_BUY_QUOTE,
        FLAG_MARKET,
        MIN_FILL_ON_MARKET,
        refused_coid,
    )
    .await;
    wait_not_busy(dex, &buyer.note.address, "a market order with a minimum fill size").await;

    // And a market buy's amount *is* its value, so the minimum notional
    // applies to it directly rather than through a price. Sent with the same
    // client id as the one above: the note reserves that id before it
    // validates anything, so an id still held here would refuse this order
    // for the wrong reason — and the control at the end would then fail.
    let _ = try_place_order(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        "0",
        BELOW_MIN_NOTIONAL,
        FLAG_MARKET,
        0,
        refused_coid,
    )
    .await;
    wait_not_busy(dex, &buyer.note.address, "a market buy under the minimum notional").await;

    // Neither reached the book.
    assert_eq!(
        op_nonce(&r, &buyer.note.address).await,
        nonce_before,
        "the note dispatched a batch for an order it should have refused itself"
    );
    let after_refusals = dex
        .get_orders_by_owner(&market.order_book, buyer.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        after_refusals.orders.is_empty(),
        "a refused order rested anyway: {} on the book",
        after_refusals.orders.len()
    );

    // And neither cost anything: no lock taken, nothing spent, no order the
    // note believes in.
    assert_eq!(
        pn_balance(&r, &buyer.note.address).await + pn_locked(&r, &buyer.note.address).await,
        held_before,
        "a refused order moved collateral"
    );
    assert_eq!(
        open_orders(&r, &buyer.note.address).await,
        0,
        "the note counts an open order it never placed"
    );

    // The control. Same client id, same path, one valid order — it rests, so
    // the two above were refused rather than lost, and the id they reserved
    // was handed back.
    place_limit(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        &BID_BPS.to_string(),
        REUSED_CID_AMOUNT,
        refused_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &buyer.note.dih_dec, refused_coid, true).await;
    assert!(
        op_nonce(&r, &buyer.note.address).await > nonce_before,
        "a valid order left the note's nonce where the refused ones did — the placement path \
         itself is not working, and the readings above say nothing"
    );
    cancel_by_client(dex, &buyer, &market.key, refused_coid).await;

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

/// The note's order-operation counter — `_opNonce`, bumped once per batch it
/// dispatches to a book and never otherwise. An order refused during
/// validation never reaches the bump, so this separates "the note said no"
/// from "the book said no", which no balance reading can.
async fn op_nonce(r: &chain_reader::ChainReader, pn_address: &str) -> u64 {
    invariant::pn_op_nonce(r, pn_address).await.expect("read the note's op nonce")
}
