//! An event that never happens: everyone gets their stake back and the market
//! closes with nothing left over.
//!
//! A market can end two ways. Every scenario so far has taken the first —
//! oracles resolve an outcome, winners claim, the market pays out. This is the
//! second: the oracles decide the event cannot be settled at all, and the
//! market has to hand every stake back instead of paying anyone.
//!
//! ## The half that was untested
//!
//! Cancelling a market whose staking window is still open is the easy case,
//! and the live SDK suite already covers it: there is no order book, no
//! escrow, nothing in flight. The case with teeth is cancelling **after the
//! freeze**, when an `OrderBook` exists and holds collateral:
//!
//! - `cancelEvent` sets `_isCancelled` and, through `ensureBalance`, that flag
//!   alone triggers the order-book shutdown — the drain runs for a reason
//!   other than `resultStart` for the first time here;
//! - `PMP.cancelStake` refuses until `_orderBookDone`, because refunding a
//!   stake while a sell still rests would delete the record a later
//!   `onOrderCancelled` needs to credit outcome tokens back to;
//! - `PrivateNote.cancelStake` refuses again while the note's own
//!   `_openOrdersByEvent` counter is non-zero — the book reports "done" when
//!   it has finished *sending* the cancels, which is not when the note has
//!   finished receiving them.
//!
//! Three gates in a row, each guarding a different silent loss. A scenario
//! that cancels before the freeze passes through none of them.
//!
//! ## Closing with nothing left over
//!
//! The market self-destructs once its pools have decayed to the forfeited mass
//! — here, to nothing — and `_finalizeResidualClose` sweeps whatever
//! `_totalUnclaimedBalance` is left to the creator on the way out. So "every
//! refund was exact" and "the market closed" are the same claim, and the place
//! to make it is **before** the closing call: read the unclaimed balance while
//! only the creator's own refund is still owed, and assert it is exactly that.
//! Read afterwards it would race the residual transfer, which can still be in
//! flight when the account is already gone — a check that would pass on a
//! market that quietly overpaid.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`.

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::at;
use crate::common::market::cancel_event_and_wait;
use crate::common::market::cancel_stake;
use crate::common::market::freeze_prepared_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::stake;
use crate::common::market::wait_order_book_done;
use crate::common::market::wait_owner_order;
use crate::common::misc::now_unix;
use crate::common::misc::poll_until;

/// Long enough that the tenth of it the contract makes the staking window is
/// still a workable window for one acknowledged stake.
const STAKE_PERIOD_CANCELLED: u64 = 300;

/// The margin the cancel vote must still have against `resultStart`. Past that
/// deadline the book shuts down on its own, and a drain that had already
/// started for that reason would let this scenario credit the cancel with it.
const RESULT_START_MARGIN_SECS: u64 = 45;

const OUTCOME: u32 = 0;

/// A bid far below par, on an empty book: nothing to cross, so it rests, and
/// the collateral it locks is what the drain has to give back.
const RESTING_PRICE_BPS: u128 = 3_000;
const RESTING_AMOUNT: u128 = 40_000_000_000;

/// The floor the book applies to `amount * price / FULL_PERCENT`
/// (`MIN_ORDER_NOTIONAL_NACKL`) and the denominator it is measured against.
/// An order under the floor is refused before it can rest — which, at a price
/// this far below par, is easy to walk into by choosing a round amount: the
/// pair above buys 40 tokens for 12 NACKL, and 20 tokens would have bought 6
/// and been thrown out. The assertion is a compile error rather than a run so
/// a later edit to either number cannot discover this on a stand.
const MIN_ORDER_NOTIONAL_NACKL: u128 = 10_000_000_000;
const FULL_PERCENT: u128 = 10_000;
const _: () =
    assert!(RESTING_AMOUNT * RESTING_PRICE_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL_NACKL);
/// And a whole number of lots, or the book refuses it for the other reason.
const _: () = assert!(RESTING_AMOUNT.is_multiple_of(10_000_000));

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_cancelled_event_refunds_every_stake_and_closes_the_market_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "cancelled_event").expect("rent the deployer note");
    let staker = alloc.rent(PnProfile::Trd, "cancelled_event").expect("rent the staker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared =
        prepare_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_CANCELLED).await;

    // ── 1. stake, while the window is open ────────────────────────────────
    let pool_before = dex.get_pmp_details(&prepared.pmp).await.expect("pmp details").total_pool;
    stake(dex, &staker, &prepared.key, OUTCOME).await;
    // The stake reaches the market as a message the note does not wait for, so
    // the note can be idle again while the pool still reads its old value.
    // Waiting on the pool is what closes that window — and a pool that grew by
    // exactly one stake is also the discriminator: a refused `setStake` leaves
    // the note looking idle and unchanged in every other respect.
    let expected_pool = pool_before + STAKE_AMOUNT;
    poll_until(&format!("PMP {} never counted the stake", prepared.pmp), || async {
        dex.get_pmp_details(&prepared.pmp).await.expect("pmp details").total_pool == expected_pool
    })
    .await;

    // ── 2. freeze, and let the creator's refund settle ────────────────────
    let market = freeze_prepared_market(ctx, dex, prepared).await;

    // The freeze hands the creator the mod-G remainder of each clean pool and
    // its own `cancelStake` reverts until that is acknowledged. Nothing else
    // is blocked, which is why only a scenario whose creator touches its own
    // stake has to wait here.
    poll_until(&format!("PMP {} never settled its normalisation refund", market.pmp), || async {
        !invariant::pmp_norm_refund_pending(&r, &market.pmp)
            .await
            .expect("read the normalisation-refund flag")
    })
    .await;

    // ── 3. a resting order, so the cancel has something to drain ──────────
    let coid = nonce as u128 * 10;
    let staker_free_before = pn_balance(&r, &staker.note.address).await;
    let staker_locked_before = pn_locked(&r, &staker.note.address).await;

    place_limit(
        dex,
        &staker,
        &market.key,
        OUTCOME,
        true,
        &RESTING_PRICE_BPS.to_string(),
        RESTING_AMOUNT,
        coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &staker.note.dih_dec, coid, true).await;

    let staker_locked_resting = pn_locked(&r, &staker.note.address).await;
    assert!(
        staker_locked_resting > staker_locked_before,
        "the order rests on the book but the note escrowed nothing for it: {staker_locked_before} \
         -> {staker_locked_resting}"
    );

    // ── 4. the oracles cancel the event ───────────────────────────────────
    // Both halves of the attribution, checked before the vote: the book is
    // still live (the order is on it, asserted just above) and `resultStart`
    // is far enough away that it cannot be what shuts the book down. Without
    // this the scenario would credit the cancel with a drain that had already
    // begun for the ordinary reason.
    assert!(
        now_unix() + RESULT_START_MARGIN_SECS < market.result_start,
        "only {}s left before resultStart, less than the {RESULT_START_MARGIN_SECS}s margin — a \
         drain starting now could not be attributed to the cancel",
        market.result_start.saturating_sub(now_unix())
    );

    cancel_event_and_wait(dex, &market.pmp, &market.oracle).await;

    // ── 5. the drain the cancel triggered ─────────────────────────────────
    wait_order_book_done(dex, &market.pmp).await;
    // The book reports done when it has finished *sending* the cancels. The
    // note's own counter is what says they arrived, and `PrivateNote`
    // refuses `cancelStake` until it reads zero.
    poll_until(&format!("note {} never saw its order cancelled", staker.note.address), || async {
        invariant::pn_open_order_count(&r, &staker.note.address)
            .await
            .expect("read open order count")
            == 0
    })
    .await;

    // Not "the order left the book" — there is no book left to ask. The drain
    // hands the protocol fees to RootPN and destroys the account in the same
    // message that reports completion, so a query here fails outright rather
    // than answering "no such order". Its absence is the stronger statement
    // anyway, and it is what this asserts.
    poll_until(&format!("order book {} outlived its drain", market.order_book), || async {
        r.account_absent(&market.order_book).await.expect("read the order book's account")
    })
    .await;

    assert_eq!(
        pn_locked(&r, &staker.note.address).await,
        staker_locked_before,
        "the cancel drained the book but did not release the escrow behind the resting order"
    );
    assert_eq!(
        pn_balance(&r, &staker.note.address).await,
        staker_free_before,
        "the released escrow did not come back as free collateral"
    );

    // ── 6. the staker takes its stake back ────────────────────────────────
    let staker_holdings = outcome_tokens(dex, &staker).await;
    let staker_position: u128 = staker_holdings.iter().sum();
    assert_eq!(
        staker_position,
        at(&staker_holdings, OUTCOME),
        "the staker holds {staker_holdings:?} across outcomes, but staked only on {OUTCOME} — the \
         refund below would be attributed to a position it never took"
    );
    cancel_stake(dex, &staker, &market.key).await;
    assert_eq!(
        pn_balance(&r, &staker.note.address).await,
        staker_free_before + staker_position,
        "cancelling the stake did not return the {staker_position} the staker had in the pool"
    );

    // ── 7. nothing left over but the creator's own stake ──────────────────
    // The claim, made while it is still checkable: with the staker refunded,
    // everything the market still owes is the creator's own position. Anything
    // above that would be swept to the creator as residual on close, where a
    // balance check could not tell it apart from the refund itself.
    let deployer_position: u128 = outcome_tokens(dex, &deployer).await.iter().sum();
    let unclaimed = invariant::pmp_unclaimed(&r, &market.pmp).await.expect("pmp unclaimed balance");
    assert_eq!(
        unclaimed, deployer_position,
        "the market holds {unclaimed} but owes the creator {deployer_position}; the difference \
         would leave as residual and be indistinguishable from an exact refund"
    );

    // ── 8. the last refund closes the market ──────────────────────────────
    let deployer_free_before = pn_balance(&r, &deployer.note.address).await;
    cancel_stake(dex, &deployer, &market.key).await;

    poll_until(
        &format!("PMP {} did not self-destruct after the last refund", market.pmp),
        || async { r.account_absent(&market.pmp).await.expect("read the market account") },
    )
    .await;
    assert_eq!(
        pn_balance(&r, &deployer.note.address).await,
        deployer_free_before + deployer_position,
        "the creator's refund and the market's residual do not add up to its position alone"
    );

    staker.release_clean(&r).await.expect("release the staker note");
    deployer.release_clean(&r).await.expect("release the deployer note");
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
