//! What happens to orders that are still resting when the market closes.
//!
//! `proof_money` reaches `resultStart` with an empty book — its one trade
//! filled both sides — so the shutdown it exercises has nothing to clean up.
//! The interesting case is the opposite one: a market resolves while orders
//! are still on the book, and their escrow has to come back. If it does not,
//! nobody notices. The book is destroyed in the same message that reports the
//! drain complete, the orders are gone from every index, and the collateral
//! and outcome tokens behind them are simply missing from notes that have no
//! record of ever having lost them.
//!
//! ## What it asserts
//!
//! One maker rests an ask, one taker rests a bid below it — deliberately not
//! crossing, so both survive to `resultStart` — and the market is then
//! resolved:
//!
//! - the **bid's collateral** returns to the taker: `_lockedInOrders` back to
//!   its pre-order value and `_balance` back to its own. The contract refunds
//!   the authoritative lock verbatim (`_orderLocks[ob][orderId]`), so this is
//!   an equality, not a bound;
//! - the **ask's outcome tokens** return to the maker's stake record;
//! - both notes' `_openOrderCount` falls back to zero, because the note-side
//!   counter and the book's resting set are updated by different contracts
//!   and a drain that refunded without decrementing would strand the note.
//!
//! ## What it does not assert, and why
//!
//! `claim` before the drain finishes is supposed to fail with
//! `ERR_ORDERBOOK_NOT_SHUTDOWN`. It is not asserted here: the guard sits after
//! `tvm.accept()` but leaves no trace a caller can read — no `_busy`, no state
//! change — so "the claim was rejected" and "the claim never arrived" are the
//! same observation from outside. A check that only re-read unchanged balances
//! would pass whether or not the gate exists, which is worse than no check.
//!
//! Note that the gate is not academic. `onOrderCancelled`'s sell branch drops
//! returned outcome tokens **silently** when the stake record is already gone,
//! so a claim that landed between the drain's completion report and its cancel
//! callbacks would destroy exactly what this scenario watches come back. This
//! scenario never claims, so it observes the intended path; proving the gate
//! itself needs delivery-order control the stand does not have.
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
use crate::common::market::resolve_and_drain;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::now_unix;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Distance to `resultStart`. A tenth of it is the staking window the fixture
/// waits out; the rest has to cover a split and two placements with room to
/// spare, and whatever is left over is spent idling until the market closes.
const STAKE_PERIOD_SHUTDOWN: u64 = 180;

/// How much budget must remain once both orders are resting. Below this the
/// stand is too slow for the market this scenario sizes, and saying so beats
/// a revert later that looks like a contract defect.
const RESTING_MARGIN_SECS: u64 = 20;

const OUTCOME: u32 = 0;

/// Bid strictly below ask: neither fills, so both are still there to be
/// cleaned up when the market closes. That is the whole setup.
const BID_BPS: &str = "6000";
const ASK_BPS: &str = "8000";
const ORDER_AMOUNT: u128 = 30_000_000_000;

/// Collateral the maker splits to get something to offer.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_drain_refunds_orders_that_were_still_resting_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "shutdown_orders").expect("rent the deployer note");
    let maker = alloc.rent(PnProfile::Trd, "shutdown_orders").expect("rent the maker note");
    let taker = alloc.rent(PnProfile::Trd, "shutdown_orders").expect("rent the taker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_SHUTDOWN).await;

    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;

    // Baselines before either order exists. Read after placement they would
    // describe the escrowed state, and the comparison after the drain would
    // be a tautology.
    let maker_tokens_before = outcome_tokens(dex, &maker).await;
    let taker_free_before = pn_balance(&r, &taker.note.address).await;
    let taker_locked_before = pn_locked(&r, &taker.note.address).await;

    let ask_coid = nonce as u128 * 10;
    let bid_coid = ask_coid + 1;

    place_limit(dex, &maker, &market.key, OUTCOME, false, ASK_BPS, ORDER_AMOUNT, ask_coid).await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, ask_coid, true).await;
    place_limit(dex, &taker, &market.key, OUTCOME, true, BID_BPS, ORDER_AMOUNT, bid_coid).await;
    wait_owner_order(dex, &market.order_book, &taker.note.dih_dec, bid_coid, true).await;
    wait_not_busy(dex, &maker.note.address, "rest ask").await;
    wait_not_busy(dex, &taker.note.address, "rest bid").await;

    assert!(
        now_unix() + RESTING_MARGIN_SECS < market.result_start,
        "only {}s left before the market closes, less than the {RESTING_MARGIN_SECS}s margin — \
         the stand is too slow for a {STAKE_PERIOD_SHUTDOWN}s market, and the orders may not \
         have been resting for the whole window this scenario claims to test",
        market.result_start.saturating_sub(now_unix())
    );

    // Both orders really did escrow something; without this the refunds
    // asserted below could all be no-ops.
    assert!(
        pn_locked(&r, &taker.note.address).await > taker_locked_before,
        "the bid locked no collateral"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME) + ORDER_AMOUNT,
        at(&maker_tokens_before, OUTCOME),
        "the ask did not take the maker's outcome tokens"
    );

    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, OUTCOME).await;
    wait_not_busy(dex, &maker.note.address, "ask refunded").await;
    wait_not_busy(dex, &taker.note.address, "bid refunded").await;

    // The collateral behind the bid came back, exactly.
    assert_eq!(
        pn_locked(&r, &taker.note.address).await,
        taker_locked_before,
        "the drain left the bid's collateral locked on a book that no longer exists"
    );
    assert_eq!(
        pn_balance(&r, &taker.note.address).await,
        taker_free_before,
        "the bid's collateral did not return to the taker's free balance"
    );

    // And the outcome tokens behind the ask.
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME),
        at(&maker_tokens_before, OUTCOME),
        "the drain did not return the ask's outcome tokens to the maker's stake"
    );

    // The note-side counters agree that nothing is resting. They are written
    // by the note, the book's index by the book: a drain that refunded but
    // never told the notes would leave both permanently unable to withdraw.
    assert_eq!(open_orders(&r, &maker.note.address).await, 0, "the maker still counts an order");
    assert_eq!(open_orders(&r, &taker.note.address).await, 0, "the taker still counts an order");

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
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
