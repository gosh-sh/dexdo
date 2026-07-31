//! The coupon a note gets for free, the debt it comes with, and what happens
//! to both across three markets.
//!
//! A note with nothing left can mint itself one coupon: a fixed nominal it may
//! bet with but never withdraw, carrying a debt of 5% of that nominal. Winning
//! with it pays real collateral and adds the whole payout to the debt, and the
//! debt is only ever repaid out of the profit of a later bet made while
//! carrying it. None of that has ever run — `use_coupon` is `false` in every
//! other scenario and `_debt` is zero everywhere — so the entire second
//! economy of this market sits untested next to the first.
//!
//! ## Why three markets
//!
//! `generateCoupon` refuses a note that has anything: every balance must be
//! under the stake minimum, there must be no stake record, and neither
//! `_hasWithdrawn` nor `_hasTransferred` may be latched. That last pair rules
//! out both obvious ways of emptying a funded note — withdrawing and
//! transferring each set the flag that then bars the coupon. What is left is
//! to **lose it**: stake the whole balance on the outcome that does not win,
//! claim nothing, and end with an empty record and a balance under the
//! minimum. That is the first market, and it costs a market to arrive at a
//! precondition rather than to test anything.
//!
//! The second market is where the coupon is spent and won with; the third is
//! where the resulting debt is bet against. Neither can be the same market as
//! the one before it: a coupon needs an empty stake record, which only a claim
//! produces, and a claim only happens on a market that has already resolved.
//!
//! ## What each phase asserts
//!
//! - **The coupon exists and the debt with it.** Nominal and 5% of nominal,
//!   both exact.
//! - **The pool limit holds.** A coupon stake of the whole nominal is far past
//!   the 5%-of-outcome ceiling, so the market refuses it and the bounce puts
//!   the coupon back on the note — the reading is that `_couponsValue`
//!   returns to what it was, which a note that had simply never sent anything
//!   would also show, so the valid stake that follows is what separates them.
//! - **A coupon stake moves no collateral.** `_couponsValue` falls, the
//!   market's coupon pool rises by the same amount, and `_balance` does not
//!   move at all. That last reading is the definition of a coupon bet.
//! - **Winning with it pays real collateral and costs exactly that in debt.**
//!   The claim credits `payoutCoupon` and adds the same figure to `_debt`, so
//!   the two deltas are equal — asserted against each other rather than
//!   against a recomputed coefficient.
//! - **A note carrying debt bets differently.** The next ordinary stake is
//!   routed to the market's debt pool instead of its clean one, with no flag
//!   passed by the caller: the note decides from `_debt` alone.
//! - **And repayment is formula 17.** A winning debt bet returns its
//!   principal plus a profit, and the debt falls by that profit times
//!   `500/9500` — the redistribution share the clean winners give up. Every
//!   term is read off an account: the principal is what was staked, the profit
//!   is the payout minus it, and the repayment is the fall in `_debt`.
//!
//! ## What it does not assert
//!
//! The debt is not cleared, and no withdrawal follows. Repayment is
//! proportional to the *profit* of the debt bet, at `500/9500` of it, so
//! clearing a debt takes a win of roughly nineteen times its size — out of
//! reach of one market seeded the way these are. Saying "the debt was repaid"
//! would need a market shaped to make it happen, which would be a scenario
//! about pool arithmetic rather than about the debt mechanism.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfGenerateCoupon;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::freeze_prepared_market;
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::resolve_and_drain;
use crate::common::market::stake_amount;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Each of the three markets. Short, because none of them trades — they exist
/// to take a stake and resolve.
const STAKE_PERIOD_COUPON: u64 = 200;

/// The outcome each market resolves to, and the one the first market's stake
/// deliberately picks instead.
const WINNING_OUTCOME: u32 = 0;
const LOSING_OUTCOME: u32 = 1;

const _: () = assert!(WINNING_OUTCOME != LOSING_OUTCOME);

/// `NACKL_COUPON_VALUE` — the nominal a coupon carries for this currency.
const COUPON_NOMINAL: u128 = 100_000_000_000;

/// The debt minted with it: 5% of the nominal, which the contract computes as
/// `_couponsValue * 5 / 100`.
const COUPON_DEBT: u128 = COUPON_NOMINAL * 5 / 100;

/// `MIN_VALUE` for NACKL — the stake minimum, and so the ceiling every balance
/// has to be under before a coupon may be minted.
const MIN_STAKE: u128 = 1_000_000_000;

/// The 0.01 NACKL lot a stake is quantised to. Staking a balance down to its
/// lot remainder leaves less than one lot behind, which is what makes the
/// remainder small enough for the coupon gate without needing it to be zero.
const LOT: u128 = 10_000_000;

const _: () = assert!(LOT < MIN_STAKE, "a lot remainder has to fit under the stake minimum");

/// What the note bets with its coupon. Comfortably over the stake minimum and
/// comfortably under the market's coupon ceiling, which is 5% of the outcome's
/// total — the deployer alone seeds each outcome with about 100 NACKL, so the
/// ceiling is around 5 NACKL and this is well inside it.
const COUPON_STAKE: u128 = 2_000_000_000;

const _: () = assert!(COUPON_STAKE >= MIN_STAKE);
const _: () = assert!(COUPON_STAKE.is_multiple_of(LOT));
// And the refused one: the whole nominal against a ceiling of a twentieth of
// the outcome, which no market this scenario builds comes close to.
const _: () = assert!(COUPON_NOMINAL > 19 * COUPON_STAKE);

/// What the note bets while carrying the debt. Paid out of the coupon's
/// winnings, so it has to be small enough that those cover it.
const DEBT_STAKE: u128 = 1_000_000_000;

const _: () = assert!(DEBT_STAKE >= MIN_STAKE);
const _: () = assert!(DEBT_STAKE.is_multiple_of(LOT));

/// `DEBT_REDISTRIBUTION_PERCENT` and `FULL_PERCENT`: the share of a debt
/// winner's profit that the clean winners give up, and which formula 17 turns
/// back into repayment.
const DEBT_REDISTRIBUTION: u128 = 500;
const FULL_PERCENT: u128 = 10_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_coupon_is_won_with_and_the_debt_it_leaves_is_bet_against_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let note = alloc.rent(PnProfile::Trd, "coupon_debt").expect("rent the coupon note");

    // ── the market that takes everything the note has ─────────────────────
    let dep_a = alloc.rent(PnProfile::Dep, "coupon_debt").expect("rent the first deployer");
    let nonce_a = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared = prepare_ephemeral_market(ctx, dex, &b0, &dep_a, nonce_a, STAKE_PERIOD_COUPON)
        .await;

    let start_balance = pn_balance(&r, &note.note.address).await;
    let sacrifice = start_balance - start_balance % LOT;
    assert!(
        sacrifice >= MIN_STAKE,
        "the note holds {start_balance}, not enough to stake in the first place"
    );
    stake_amount(dex, &note, &prepared.key, LOSING_OUTCOME, sacrifice, false).await;
    assert_eq!(
        pn_balance(&r, &note.note.address).await,
        start_balance - sacrifice,
        "the stake did not take the collateral it was given"
    );

    let market_a = freeze_prepared_market(ctx, dex, prepared).await;
    wait_until(market_a.result_start).await;
    resolve_and_drain(dex, &market_a.pmp, &market_a.oracle, WINNING_OUTCOME).await;
    claim(dex, &note, &market_a).await;

    // What the first market was for: a note poor enough and empty enough to
    // qualify for a coupon. The remainder below a lot is what a stake could
    // not take, and it is an order of magnitude under the stake minimum.
    let leftovers = pn_balance(&r, &note.note.address).await;
    assert!(
        leftovers < MIN_STAKE,
        "the note came out of a lost bet holding {leftovers}, at or above the {MIN_STAKE} \
         minimum — the coupon gate will refuse it"
    );
    assert!(
        dex.get_stakes(&note.note.address).await.expect("stakes").stakes.is_empty(),
        "the claim left a stake record behind, which is its own bar to a coupon"
    );

    // ── the coupon ────────────────────────────────────────────────────────
    dex.generate_coupon(
        &note.note.address,
        ParamsOfGenerateCoupon { token_type: TOKEN_TYPE_NACKL },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("generate_coupon");
    poll_until("the note never minted its coupon", || async {
        coupons(&r, &note.note.address).await > 0
    })
    .await;

    assert_eq!(
        coupons(&r, &note.note.address).await,
        COUPON_NOMINAL,
        "the coupon was minted at a nominal other than this currency's"
    );
    assert_eq!(
        debt(&r, &note.note.address).await,
        COUPON_DEBT,
        "the debt minted with the coupon is not the 5% of nominal it is defined as"
    );
    assert_eq!(
        pn_balance(&r, &note.note.address).await,
        leftovers,
        "minting a coupon moved real collateral"
    );

    // ── the market the coupon is spent in ─────────────────────────────────
    let dep_b = alloc.rent(PnProfile::Dep, "coupon_debt").expect("rent the second deployer");
    let nonce_b = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared_b = prepare_ephemeral_market(ctx, dex, &b0, &dep_b, nonce_b, STAKE_PERIOD_COUPON)
        .await;

    // First the one the market must refuse. A coupon pool may not exceed a
    // twentieth of its outcome's total, and the whole nominal is far past
    // that — so the stake reverts inside the market and the bounce hands the
    // coupon back.
    stake_amount(dex, &note, &prepared_b.key, WINNING_OUTCOME, COUPON_NOMINAL, true).await;
    poll_until("the refused coupon stake never came back", || async {
        coupons(&r, &note.note.address).await == COUPON_NOMINAL
            && !note_busy(dex, &note.note.address).await
    })
    .await;
    assert_eq!(
        debt(&r, &note.note.address).await,
        COUPON_DEBT,
        "a refused coupon stake changed what the note owes"
    );

    // Then the one it takes. This is the reading that makes a coupon bet a
    // coupon bet: the market's coupon pool grows, and the note's collateral
    // does not move at all.
    let pool_before = coupon_pool(&r, &prepared_b.pmp).await;
    let balance_before_coupon = pn_balance(&r, &note.note.address).await;
    stake_amount(dex, &note, &prepared_b.key, WINNING_OUTCOME, COUPON_STAKE, true).await;
    poll_until("the coupon stake never reached the market", || async {
        coupon_pool(&r, &prepared_b.pmp).await > pool_before
    })
    .await;

    assert_eq!(
        coupon_pool(&r, &prepared_b.pmp).await,
        pool_before + COUPON_STAKE,
        "the market's coupon pool did not grow by exactly what was staked into it"
    );
    assert_eq!(
        coupons(&r, &note.note.address).await,
        COUPON_NOMINAL - COUPON_STAKE,
        "the note's coupon did not fall by exactly what it bet"
    );
    assert_eq!(
        pn_balance(&r, &note.note.address).await,
        balance_before_coupon,
        "a coupon stake moved real collateral, which is the one thing it must not do"
    );

    let market_b = freeze_prepared_market(ctx, dex, prepared_b).await;
    wait_until(market_b.result_start).await;
    resolve_and_drain(dex, &market_b.pmp, &market_b.oracle, WINNING_OUTCOME).await;

    let balance_before_win = pn_balance(&r, &note.note.address).await;
    let debt_before_win = debt(&r, &note.note.address).await;
    claim(dex, &note, &market_b).await;

    // Winning with a coupon pays real collateral — and charges the whole
    // payout to the debt. The two deltas are asserted against each other
    // rather than against a recomputed coefficient, which would be this test
    // checking the contract's arithmetic with a copy of it.
    let won = pn_balance(&r, &note.note.address).await - balance_before_win;
    let owed = debt(&r, &note.note.address).await - debt_before_win;
    assert!(won > 0, "a winning coupon paid nothing");
    assert_eq!(
        won, owed,
        "the coupon paid {won} and added {owed} to the debt; a coupon win is supposed to be \
         charged in full"
    );

    // ── and the market the debt is bet against ────────────────────────────
    let dep_c = alloc.rent(PnProfile::Dep, "coupon_debt").expect("rent the third deployer");
    let nonce_c = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared_c = prepare_ephemeral_market(ctx, dex, &b0, &dep_c, nonce_c, STAKE_PERIOD_COUPON)
        .await;

    let free_for_debt = pn_balance(&r, &note.note.address).await;
    assert!(
        free_for_debt >= DEBT_STAKE,
        "the coupon's winnings came to {free_for_debt}, not enough for the {DEBT_STAKE} the debt \
         bet needs"
    );

    let debt_pool_before = debt_pool(&r, &prepared_c.pmp).await;
    stake_amount(dex, &note, &prepared_c.key, WINNING_OUTCOME, DEBT_STAKE, false).await;
    poll_until("the debt stake never reached the market", || async {
        debt_pool(&r, &prepared_c.pmp).await > debt_pool_before
    })
    .await;

    // Nothing in the call said "debt": the note routes the stake there itself,
    // from `_debt` alone. A note without one would have landed in the clean
    // pool, and the market would have paid it as a clean winner.
    assert_eq!(
        debt_pool(&r, &prepared_c.pmp).await,
        debt_pool_before + DEBT_STAKE,
        "an ordinary stake from a note carrying debt did not go to the debt pool"
    );
    assert_eq!(
        pn_balance(&r, &note.note.address).await,
        free_for_debt - DEBT_STAKE,
        "the debt stake did not cost real collateral, which it must"
    );

    let market_c = freeze_prepared_market(ctx, dex, prepared_c).await;
    wait_until(market_c.result_start).await;
    resolve_and_drain(dex, &market_c.pmp, &market_c.oracle, WINNING_OUTCOME).await;

    let balance_before_repay = pn_balance(&r, &note.note.address).await;
    let debt_before_repay = debt(&r, &note.note.address).await;
    claim(dex, &note, &market_c).await;

    // Formula 17, with every term read off an account: the principal is what
    // was staked, the profit is what came back beyond it, and the repayment is
    // the fall in the debt — which has to be the redistribution share of that
    // profit, the part the clean winners gave up.
    let paid_out = pn_balance(&r, &note.note.address).await - balance_before_repay;
    assert!(
        paid_out > DEBT_STAKE,
        "the winning debt bet returned {paid_out} on a {DEBT_STAKE} principal, so there was no \
         profit for a repayment to come out of"
    );
    let profit = paid_out - DEBT_STAKE;
    let expected_repayment = profit * DEBT_REDISTRIBUTION / (FULL_PERCENT - DEBT_REDISTRIBUTION);
    let debt_after = debt(&r, &note.note.address).await;
    assert!(
        expected_repayment < debt_before_repay,
        "the profit of {profit} would repay {expected_repayment} against a debt of \
         {debt_before_repay} — at or past the whole debt, where the contract clamps to zero and \
         the equality below stops being the claim"
    );
    assert_eq!(
        debt_before_repay - debt_after,
        expected_repayment,
        "the debt fell by {} on a profit of {profit}, not the {expected_repayment} the \
         redistribution share of that profit comes to",
        debt_before_repay - debt_after
    );

    note.taint(allocator::TaintReason::DirtyState {
        fields: vec!["_debt".to_string(), "_couponsValue".to_string()],
    });
    for dep in [dep_a, dep_b, dep_c] {
        dep.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    }
}

async fn claim(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    dex.claim(&note.note.address, market.key.clone(), Signer::Keys { keys: note.note.keys.clone() })
        .await
        .expect("claim");
    wait_not_busy(dex, &note.note.address, "claim").await;
}

async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn coupons(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_coupons_value(r, pn_address).await.expect("read the note's coupon")
}

async fn debt(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_debt(r, pn_address).await.expect("read the note's debt")
}

async fn coupon_pool(r: &chain_reader::ChainReader, pmp: &str) -> u128 {
    invariant::pmp_total_coupon_pool(r, pmp).await.expect("read the market's coupon pool")
}

async fn debt_pool(r: &chain_reader::ChainReader, pmp: &str) -> u128 {
    invariant::pmp_total_debt_pool(r, pmp).await.expect("read the market's debt pool")
}

async fn note_busy(dex: &dodex_sdk::Dex, pn_address: &str) -> bool {
    dex.get_private_note_details(pn_address).await.expect("pn details").busy_address.is_some()
}
