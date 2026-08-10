//! Which resting order a taker eats first, and how much of it is left.
//!
//! `proof_money` crosses one ask with one bid at the same price. That says
//! nothing about *choice*: with more than one resting order, a book has to
//! decide which to consume, and two independent disciplines decide it —
//! levels are walked best-first, and orders inside a level are consumed in
//! the order they arrived. Neither is exercised anywhere, and both fail
//! quietly: a book that walked levels worst-first, or served the newest
//! order in a level, would fill every order eventually and look correct in
//! any test that only counts tokens.
//!
//! ## The arrangement
//!
//! One maker rests three asks on the same outcome:
//!
//! | order | size | price | placed |
//! |-------|------|-------|--------|
//! | A1    | 20   | 0.60  | first  |
//! | A2    | 20   | 0.60  | second |
//! | A3    | 20   | 0.70  | third  |
//!
//! and a taker then buys 30 at 0.70 — enough to clear one order and part of
//! another, and priced to reach either level. Exactly one outcome is correct:
//!
//! - **A1 is gone.** The 0.60 level is the better one for a buyer, and A1 is
//!   the older of the two orders sitting on it.
//! - **A2 has 10 left.** `Order.amount` is the remaining size, so this single
//!   reading carries both the partial fill and the ordering: a book serving
//!   the newest order first would have left this one whole and emptied A2's
//!   twin instead.
//! - **A3 is untouched at 20.** The taker's price reaches 0.70, so nothing but
//!   level priority keeps A3 whole while 0.60 still had liquidity.
//!
//! The three asks come from one note deliberately. The level FIFO holds
//! orders in arrival order regardless of owner, so one maker exercises it
//! exactly as two would, at two notes instead of four — and the pool is sized
//! per scenario.
//!
//! ## The walk, which the first taker deliberately is not
//!
//! Note what the arrangement above proves and does not: the taker's 30 fits
//! inside the 0.60 level, so the assertion is that it **stopped** there. That
//! is level *priority*. Walking — one order consuming more than one level — is
//! a different claim, and a second taker makes it: 25 more at 0.70, which
//! clears what is left of 0.60 and continues into 0.70.
//!
//! That order also carries the price-improvement claim, because it is priced
//! at 0.70 throughout while 10 of its 25 fill at 0.60:
//!
//! - a book clearing each portion at **its own maker's** price collects
//!   `10×0.60 + 15×0.70 = 16.5` and refunds the rest of what was locked;
//! - a book clearing the whole order at the **taker's** limit collects
//!   `25×0.70 = 17.5`.
//!
//! One NACKL apart, against a fee of at most a few thousandths — so a
//! two-sided bound on what the taker was charged separates them cleanly while
//! leaving the fee formula unrestated. Restating it would check the
//! implementation against itself.
//!
//! Last, the same trade is read as conservation on the collateral leg: what
//! the taker paid equals what the maker received plus what the book kept.
//! That is the reading that catches a floor taken twice or a rounding
//! remainder with no owner — neither shows up in a token count, and both are
//! invisible from either side alone.
//!
//! ## Trading with yourself
//!
//! Nothing stops one note from taking its own resting order — the book
//! matches on price and arrival, not on owner. What stops it from being worth
//! doing is the fee: the taker side pays it in full, the maker side gets three
//! quarters of it back as a rebate, and the quarter that stays with the book
//! is a strict loss. A third phase makes that trade and reads the loss against
//! the book's own protocol-fee counter. They have to be the same number: a
//! rebate that ever exceeded the fee would make wash-trading pay, and the
//! ledger would show the note ending richer than it started.
//!
//! ## The remainders a floor leaves behind
//!
//! A buy locks what its whole size costs, once. Its fills are charged one at a
//! time, each floored to a whole unit of collateral, so a size delivered in
//! several fills costs slightly less than the same size in one — and the
//! difference sits in the order's lock with nobody to claim it. The fourth
//! phase arranges exactly that: a resting buy consumed by three market sells
//! whose sizes make every fill lose a fraction, and a lock that ends one unit
//! above the sum of the fills.
//!
//! What it reads afterwards is that the note's escrow is **empty**, not one
//! unit short of it. A book that decremented only what the fills consumed
//! would leave that unit locked forever, invisible to every reading except
//! this one.
//!
//! The same phase carries the fee split, because two accounts see it from
//! opposite sides: the buyer's collateral is short by the fills' cost minus
//! the rebates it collected, the seller's is up by the same cost minus the
//! fees it paid, and the book keeps the difference. Each total is recovered
//! from its own account, so asserting that they add up is a statement about
//! where the money went rather than a re-derivation of the rate it went at.
//!
//! ## What this does not cover
//!
//! IOC into an empty side is a separate unit.
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
use crate::common::market::FLAG_MARKET;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_LADDER: u64 = 300;
const OUTCOME: u32 = 0;

/// The two ask levels. `BEST_BPS` is the better price for a buyer, so a
/// correct walk empties it before touching `WORSE_BPS`.
const BEST_BPS: u128 = 6_000;
const WORSE_BPS: u128 = 7_000;
const FULL_PERCENT: u128 = 10_000;

const _: () = assert!(BEST_BPS < WORSE_BPS, "the best level must be the cheaper one for a buyer");

/// Each ask. At `BEST_BPS` this is worth 12 NACKL, clearing the 10 NACKL
/// minimum notional with room to spare.
const ASK_AMOUNT: u128 = 20_000_000_000;

/// The taker's size: one whole ask plus half of the next. Any size that
/// divided evenly into the asks would leave nothing partially filled, and the
/// ordering claim would rest on absence alone.
const TAKE_AMOUNT: u128 = 30_000_000_000;

/// What the taker's fill leaves on A2.
const A2_REMAINING: u128 = ASK_AMOUNT + ASK_AMOUNT - TAKE_AMOUNT;

/// The second taker, which starts where the first stopped: it clears what is
/// left of the 0.60 level and continues into 0.70. Two price levels in one
/// order — the walk itself, which the first taker deliberately does not do.
const TAKE2_AMOUNT: u128 = 25_000_000_000;

/// What that leaves on A3.
const A3_REMAINING: u128 = A2_REMAINING + ASK_AMOUNT - TAKE2_AMOUNT;

/// What the second taker owes if each portion cleared at *its own maker's*
/// price — the whole claim of the phase, since the order is priced at 0.70 and
/// a book that charged its own limit price would collect
/// `TAKE2_AMOUNT × 0.70` instead.
const TAKE2_COST: u128 = A2_REMAINING * BEST_BPS / FULL_PERCENT
    + (TAKE2_AMOUNT - A2_REMAINING) * WORSE_BPS / FULL_PERCENT;

/// What the same size would cost with no price improvement at all.
const TAKE2_COST_UNIMPROVED: u128 = TAKE2_AMOUNT * WORSE_BPS / FULL_PERCENT;

/// An upper bound on the taker fee — deliberately a bound and not the
/// contract's formula (0.045%), which restating here would check the
/// implementation against itself. A tenth of a percent is more than double the
/// real rate and still an order of magnitude below the improvement being
/// asserted, so the two cannot be confused for one another.
const FEE_CAP_PERMILLE: u128 = 1;

// The second taker has to cross the level boundary and stop inside A3: sized
// short it never leaves 0.60 and proves nothing new, sized long it empties the
// book and "A3 is gone" says nothing about where it stopped.
const _: () = assert!(TAKE2_AMOUNT > A2_REMAINING);
const _: () = assert!(TAKE2_AMOUNT < A2_REMAINING + ASK_AMOUNT);
// And the improvement has to dominate the fee, or the bound below cannot tell
// "charged at the makers' prices" from "charged at the limit price".
const _: () = assert!(TAKE2_COST_UNIMPROVED - TAKE2_COST > TAKE2_COST * FEE_CAP_PERMILLE / 1000);

/// Collateral the maker splits. Three asks need 60 outcome tokens, the
/// self-match another 20, and the floor phase's buy is paid for in collateral
/// rather than tokens; a split of this yields roughly 100 of each.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

/// The 10 NACKL floor under any order's value, mirroring
/// `MIN_ORDER_NOTIONAL_NACKL`.
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// The 0.01 NACKL lot every limit order's size is quantised to.
const LOT: u128 = 10_000_000;

/// The tick every price is quantised to.
const TICK: u128 = 10;

/// The self-match. Priced under `WORSE_BPS` so A3 stays out of reach and the
/// note's own ask is the only thing its buy can find.
const SELF_BPS: u128 = 6_000;
const SELF_AMOUNT: u128 = 20_000_000_000;

const _: () = assert!(SELF_BPS < WORSE_BPS);
const _: () = assert!(SELF_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(SELF_AMOUNT * SELF_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// The floor phase's resting buy. The price is a tick multiple that is *not* a
/// multiple of the 1000 that would make every fill's cost land on a whole
/// unit — without that, no arrangement of fills loses anything to the floor
/// and the phase asserts an empty set.
const RESIDUAL_BPS: u128 = 6_010;
const RESIDUAL_AMOUNT: u128 = 20_000_000_000;

/// The three market sells that consume it. Market orders are not lot-quantised
/// — that is what allows sizes whose cost has a fractional part at all — and
/// they add up to the buy exactly, so the third one closes it.
const FILL_A: u128 = 6_666_666_666;
const FILL_B: u128 = 6_666_666_666;
const FILL_C: u128 = RESIDUAL_AMOUNT - FILL_A - FILL_B;

/// What each of them costs, floored per fill the way the book floors it.
const COST_A: u128 = FILL_A * RESIDUAL_BPS / FULL_PERCENT;
const COST_B: u128 = FILL_B * RESIDUAL_BPS / FULL_PERCENT;
const COST_C: u128 = FILL_C * RESIDUAL_BPS / FULL_PERCENT;
const SUM_FILL_COST: u128 = COST_A + COST_B + COST_C;

/// What the same size costs charged in one go — the figure the order's lock is
/// sized from.
const WHOLE_ORDER_COST: u128 = RESIDUAL_AMOUNT * RESIDUAL_BPS / FULL_PERCENT;

/// The gap between the two: collateral the buyer locked, the fills never
/// consumed, and nobody would ever ask for.
const FLOOR_RESIDUAL: u128 = WHOLE_ORDER_COST - SUM_FILL_COST;

/// How many fills that phase makes. A floored three-quarters can fall short of
/// the exact one by less than a unit each time, and no more.
const RESIDUAL_FILLS: u128 = 3;

// The point of the arrangement: charged fill by fill, the order costs strictly
// less than its own lock. Without this the phase would assert that an empty
// escrow is empty.
const _: () = assert!(FLOOR_RESIDUAL > 0);
const _: () = assert!(RESIDUAL_BPS.is_multiple_of(TICK));
const _: () = assert!(RESIDUAL_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(WHOLE_ORDER_COST >= MIN_ORDER_NOTIONAL);
const _: () = assert!(FILL_A + FILL_B + FILL_C == RESIDUAL_AMOUNT);
// And it must not cross A3, which is still resting from the phases above.
const _: () = assert!(RESIDUAL_BPS < WORSE_BPS);

// The taker has to clear one whole ask and stop inside the next. Sized into an
// even division, "A2 has 10 left" becomes "A2 is gone" and the ordering claim
// would rest on absence alone — which a book serving newest-first satisfies
// too. Enforced by the compiler: editing either constant that far breaks the
// build rather than a run.
const _: () = assert!(TAKE_AMOUNT > ASK_AMOUNT);
const _: () = assert!(TAKE_AMOUNT < 2 * ASK_AMOUNT);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_taker_walks_levels_best_first_and_a_level_in_arrival_order_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "matching_ladder").expect("rent the deployer note");
    let maker = alloc.rent(PnProfile::Trd, "matching_ladder").expect("rent the maker note");
    let taker = alloc.rent(PnProfile::Trd, "matching_ladder").expect("rent the taker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_LADDER).await;

    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;
    let maker_tokens = outcome_tokens(dex, &maker).await;
    assert!(
        at(&maker_tokens, OUTCOME) >= 3 * ASK_AMOUNT,
        "the split left the maker {} outcome-{OUTCOME} tokens, not enough for three asks of \
         {ASK_AMOUNT}",
        at(&maker_tokens, OUTCOME)
    );

    let base = nonce as u128 * 10;
    let (a1, a2, a3) = (base + 1, base + 2, base + 3);

    // Placed one at a time, each confirmed on the book before the next is
    // sent. The arrival order inside the 0.60 level is the entire point of
    // A1 and A2, and two placements in flight at once would leave it to
    // chance which arrived first.
    for (coid, price) in [(a1, BEST_BPS), (a2, BEST_BPS), (a3, WORSE_BPS)] {
        place_limit(dex, &maker, &market.key, OUTCOME, false, &price.to_string(), ASK_AMOUNT, coid)
            .await;
        wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, coid, true).await;
    }

    let taker_tokens_before = outcome_tokens(dex, &taker).await;
    let take_coid = base + 4;

    // Priced at the worse level on purpose: the taker can afford either, so
    // only level priority decides what it gets.
    place_limit(
        dex,
        &taker,
        &market.key,
        OUTCOME,
        true,
        &WORSE_BPS.to_string(),
        TAKE_AMOUNT,
        take_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, a1, false).await;
    wait_not_busy(dex, &taker.note.address, "taker buy").await;
    wait_not_busy(dex, &maker.note.address, "asks filled").await;

    let resting = maker_orders(dex, &market.order_book, &maker.note.dih_dec).await;

    // A1 is gone: the better level was served, and the older order on it went
    // first. Its absence is already asserted by the barrier above; what this
    // adds is that it is absent *while* the other two are not.
    assert!(!resting.contains_key(&a1), "A1 is still on the book");

    let a2_left = *resting
        .get(&a2)
        .unwrap_or_else(|| panic!("A2 left the book entirely; the taker should have left part"));
    assert_eq!(
        a2_left, A2_REMAINING,
        "A2 has {a2_left} left, not {A2_REMAINING} — the taker either served the 0.60 level in \
         the wrong order or did not stop where its size ran out"
    );

    let a3_left = *resting
        .get(&a3)
        .unwrap_or_else(|| panic!("A3 was consumed; the 0.60 level still had liquidity"));
    assert_eq!(
        a3_left,
        ASK_AMOUNT,
        "A3 lost {} to a taker that should not have reached the 0.70 level",
        ASK_AMOUNT - a3_left
    );

    // The taker's side: filled outright, nothing rested, and it holds exactly
    // what the two asks gave up.
    let taker_orders = dex
        .get_orders_by_owner(&market.order_book, taker.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        taker_orders.orders.is_empty(),
        "the taker's buy left {} order(s) resting; its whole size was available",
        taker_orders.orders.len()
    );
    assert_eq!(
        invariant::pn_open_order_count(&r, &taker.note.address).await.expect("open orders"),
        0
    );
    assert_eq!(
        at(&outcome_tokens(dex, &taker).await, OUTCOME),
        at(&taker_tokens_before, OUTCOME) + TAKE_AMOUNT,
        "the taker did not receive exactly the {TAKE_AMOUNT} tokens it bought"
    );
    assert_eq!(
        invariant::pn_locked_opt(&r, &taker.note.address, TOKEN_TYPE_NACKL)
            .await
            .expect("read taker escrow")
            .expect("taker is on chain"),
        0,
        "a fully filled buy left collateral locked"
    );

    // ── the walk itself: one order across two price levels ────────────────
    // The first taker stopped inside the better level, which is what proved
    // priority. This one starts there and has to continue into the worse one,
    // so it is the order that actually walks.
    let take2_coid = base + 5;
    let maker_free_before = pn_free(&r, &maker.note.address).await;
    let maker_tokens_before = at(&outcome_tokens(dex, &maker).await, OUTCOME);
    let taker_held_before = pn_free(&r, &taker.note.address).await + pn_locked(&r, &taker).await;
    let taker_tokens_mid = at(&outcome_tokens(dex, &taker).await, OUTCOME);
    let book_fees_before = book_fees(dex, &market.order_book).await;

    place_limit(
        dex,
        &taker,
        &market.key,
        OUTCOME,
        true,
        &WORSE_BPS.to_string(),
        TAKE2_AMOUNT,
        take2_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, a2, false).await;
    wait_not_busy(dex, &taker.note.address, "the walking taker").await;
    wait_not_busy(dex, &maker.note.address, "asks filled across levels").await;

    let resting = maker_orders(dex, &market.order_book, &maker.note.dih_dec).await;
    assert!(!resting.contains_key(&a2), "A2 survived a taker that was sized past it");
    let a3_left = *resting
        .get(&a3)
        .unwrap_or_else(|| panic!("A3 left the book entirely; the taker should have left part"));
    assert_eq!(
        a3_left, A3_REMAINING,
        "A3 has {a3_left} left, not {A3_REMAINING} — the taker did not continue into 0.70 with \
         exactly what 0.60 could not fill"
    );

    assert_eq!(
        at(&outcome_tokens(dex, &taker).await, OUTCOME),
        taker_tokens_mid + TAKE2_AMOUNT,
        "the walking taker did not receive the {TAKE2_AMOUNT} tokens it bought"
    );
    // The maker's position does **not** move here, and that is the claim. A
    // sell takes the outcome tokens out of `stake.amount` when the order is
    // placed — `resting_orders` asserts exactly that — so by fill time they
    // are already gone, and the fill converts them into collateral rather than
    // debiting the stake a second time. A book that debited again would leave
    // the maker short of tokens it had already handed over.
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME),
        maker_tokens_before,
        "the fill moved the maker's stake, which the placement had already debited — the tokens \
         were taken twice"
    );

    // The clearing prices, stated as what the taker was actually charged. The
    // order is priced at 0.70 throughout, so a book that cleared it at its own
    // limit would collect `TAKE2_COST_UNIMPROVED`; one that cleared each
    // portion at its maker's price collects `TAKE2_COST` and refunds the
    // difference. The upper bound is a fee cap rather than the fee, so this
    // pins the prices without restating the contract's fee arithmetic.
    let taker_paid =
        taker_held_before - (pn_free(&r, &taker.note.address).await + pn_locked(&r, &taker).await);
    assert!(
        taker_paid >= TAKE2_COST,
        "the taker paid {taker_paid} for a basket worth {TAKE2_COST} at the makers' own prices — \
         less than the tokens cost"
    );
    assert!(
        taker_paid <= TAKE2_COST + TAKE2_COST * FEE_CAP_PERMILLE / 1000,
        "the taker paid {taker_paid}, above {TAKE2_COST} plus any plausible fee and toward the \
         {TAKE2_COST_UNIMPROVED} its own limit price would have cost: the 0.60 portion was not \
         cleared at 0.60, or the improvement was not refunded"
    );

    // Conservation across the trade, on the collateral leg. The taker's outlay
    // has to land somewhere and nowhere else: the maker's proceeds plus the
    // protocol's share of the fee. This is the reading that catches a floor
    // taken twice or a rounding remainder left with no owner — neither shows
    // up in a token count, and both would be invisible to either side alone.
    let maker_gained = pn_free(&r, &maker.note.address).await - maker_free_before;
    let protocol_gained = book_fees(dex, &market.order_book).await - book_fees_before;
    assert_eq!(
        taker_paid,
        maker_gained + protocol_gained,
        "the taker paid {taker_paid}, the maker received {maker_gained} and the book kept \
         {protocol_gained}: {} went nowhere",
        taker_paid as i128 - (maker_gained + protocol_gained) as i128
    );

    // ── one note on both sides ────────────────────────────────────────────
    //
    // The book has no notion of who owns what it matches, so a note can take
    // its own order. What makes that pointless rather than free is the fee
    // split: the taker leg pays the fee, the maker leg is rebated three
    // quarters of it, and the quarter left with the book is the whole of the
    // trade's result. Both legs land on the same note, so its net is readable
    // as one number — and has to equal what the book kept.
    let self_ask_coid = base + 6;
    let self_buy_coid = base + 7;
    let maker_tokens_at_self = at(&outcome_tokens(dex, &maker).await, OUTCOME);
    assert!(
        maker_tokens_at_self >= SELF_AMOUNT,
        "the maker holds {maker_tokens_at_self} outcome-{OUTCOME} tokens, not the {SELF_AMOUNT} \
         the self-match needs"
    );
    let self_free_before = pn_free(&r, &maker.note.address).await;
    let self_fees_before = book_fees(dex, &market.order_book).await;

    place_limit(
        dex,
        &maker,
        &market.key,
        OUTCOME,
        false,
        &SELF_BPS.to_string(),
        SELF_AMOUNT,
        self_ask_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, self_ask_coid, true).await;

    place_limit(
        dex,
        &maker,
        &market.key,
        OUTCOME,
        true,
        &SELF_BPS.to_string(),
        SELF_AMOUNT,
        self_buy_coid,
    )
    .await;
    // The ask leaving the book is the match; both legs settle on the one note,
    // so waiting for it to go quiet covers the rest.
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, self_ask_coid, false).await;
    wait_not_busy(dex, &maker.note.address, "the self-match").await;

    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME),
        maker_tokens_at_self,
        "a note that sold {SELF_AMOUNT} tokens to itself and bought them back does not hold what \
         it started with"
    );
    assert_eq!(
        pn_locked(&r, &maker).await,
        0,
        "the self-match left collateral locked; both of its orders are gone"
    );

    // The claim. A wash trade costs the note exactly the book's cut — no more,
    // which would mean collateral leaking somewhere unnamed, and no less,
    // which would mean the rebate paying for the privilege of trading with
    // yourself.
    let self_after = pn_free(&r, &maker.note.address).await;
    let self_loss = self_free_before.checked_sub(self_after).unwrap_or_else(|| {
        panic!(
            "the note came out of a trade with itself richer ({self_free_before} -> \
             {self_after}) — the rebate paid for more than the fee cost"
        )
    });
    let self_kept = book_fees(dex, &market.order_book).await - self_fees_before;
    assert!(self_kept > 0, "a self-match at {SELF_BPS} bps cost the book's counter nothing");
    assert_eq!(
        self_loss, self_kept,
        "the note is out {self_loss} on a trade with itself while the book kept {self_kept}: \
         wash-trading is either subsidised or overcharged by {}",
        self_loss as i128 - self_kept as i128
    );

    // ── what a floor leaves in the lock ───────────────────────────────────
    //
    // The buy below is locked for what its whole size costs and charged for
    // each fill separately, floored. Three fills, each losing a fraction, end
    // one unit under the lock — and that unit has to come back rather than sit
    // in escrow forever.
    let residual_coid = base + 8;
    let residual_free_before = pn_free(&r, &maker.note.address).await;
    let residual_tokens_before = at(&outcome_tokens(dex, &maker).await, OUTCOME);
    let seller_free_before = pn_free(&r, &taker.note.address).await;
    let seller_tokens_before = at(&outcome_tokens(dex, &taker).await, OUTCOME);
    let residual_fees_before = book_fees(dex, &market.order_book).await;
    assert!(
        seller_tokens_before >= RESIDUAL_AMOUNT,
        "the seller holds {seller_tokens_before} outcome-{OUTCOME} tokens, not the \
         {RESIDUAL_AMOUNT} this phase sells"
    );

    place_limit(
        dex,
        &maker,
        &market.key,
        OUTCOME,
        true,
        &RESIDUAL_BPS.to_string(),
        RESIDUAL_AMOUNT,
        residual_coid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, residual_coid, true).await;

    let mut left = RESIDUAL_AMOUNT;
    for (i, fill) in [FILL_A, FILL_B, FILL_C].into_iter().enumerate() {
        let before = left;
        place_order_with_flags(
            dex,
            &taker,
            &market.key,
            OUTCOME,
            false,
            "0",
            fill,
            FLAG_MARKET,
            base + 20 + i as u128,
        )
        .await;
        // Wait for the bid to move at all, then say what it moved to. Polling
        // for the expected figure would turn a wrong one into a timeout.
        poll_until(&format!("market sell {} never reached the bid", i + 1), || async {
            maker_orders(dex, &market.order_book, &maker.note.dih_dec)
                .await
                .get(&residual_coid)
                .copied()
                != Some(before)
        })
        .await;
        wait_not_busy(dex, &taker.note.address, "market sell into the resting bid").await;
        left -= fill;

        let resting = maker_orders(dex, &market.order_book, &maker.note.dih_dec).await;
        let seen = resting.get(&residual_coid).copied();
        let want = if left == 0 { None } else { Some(left) };
        assert_eq!(
            seen, want,
            "after selling {fill} into it the bid holds {seen:?}, not {want:?}"
        );
    }
    wait_not_busy(dex, &maker.note.address, "the bid filling out").await;

    // The drain. The lock was sized from `WHOLE_ORDER_COST` and the fills only
    // ever consumed `SUM_FILL_COST`; the difference is returned on the fill
    // that closes the order, so escrow ends empty rather than holding
    // `FLOOR_RESIDUAL` nobody can reach.
    assert_eq!(
        pn_locked(&r, &maker).await,
        0,
        "the closed buy left {FLOOR_RESIDUAL} of floor residual locked instead of returning it"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME),
        residual_tokens_before + RESIDUAL_AMOUNT,
        "the buyer did not receive the {RESIDUAL_AMOUNT} tokens its bid was filled with"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &taker).await, OUTCOME),
        seller_tokens_before - RESIDUAL_AMOUNT,
        "the seller gave up something other than the {RESIDUAL_AMOUNT} tokens it sold"
    );

    // Each side's totals, recovered from its own account: the buyer is short
    // the fills' cost less what it was rebated, the seller is up that cost
    // less what it paid in fees.
    let buyer_out = residual_free_before - pn_free(&r, &maker.note.address).await;
    let total_rebate = SUM_FILL_COST.checked_sub(buyer_out).unwrap_or_else(|| {
        panic!(
            "the buyer is out {buyer_out} on fills worth {SUM_FILL_COST} — more than the tokens \
             cost, with a rebate that is supposed to come back on top"
        )
    });
    let seller_in = pn_free(&r, &taker.note.address).await - seller_free_before;
    let total_fee = SUM_FILL_COST.checked_sub(seller_in).unwrap_or_else(|| {
        panic!(
            "the seller received {seller_in} for fills worth {SUM_FILL_COST} — more than the \
             tokens were sold for, with a fee that is supposed to come off the top"
        )
    });
    let protocol_kept = book_fees(dex, &market.order_book).await - residual_fees_before;

    assert!(total_fee > 0, "three fills worth {SUM_FILL_COST} cost the taker no fee at all");
    assert_eq!(
        protocol_kept,
        total_fee - total_rebate,
        "the taker paid {total_fee} in fees, {total_rebate} came back to the maker and the book \
         kept {protocol_kept}: {} is unaccounted for",
        total_fee as i128 - total_rebate as i128 - protocol_kept as i128
    );

    // And the split itself: three quarters, floored per fill, so the rebate is
    // never rounded up and never short by a whole unit per fill.
    assert!(
        total_rebate * 4 <= total_fee * 3,
        "the maker was rebated {total_rebate} of a {total_fee} fee — more than the three quarters \
         it is owed"
    );
    assert!(
        total_rebate * 4 > total_fee * 3 - 4 * RESIDUAL_FILLS,
        "the maker was rebated {total_rebate} of a {total_fee} fee, short of three quarters by \
         more than the flooring of {RESIDUAL_FILLS} fills can account for"
    );

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// The note's free NACKL — `_balance`, without what its orders hold.
async fn pn_free(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

/// The note's NACKL escrowed against its resting orders.
async fn pn_locked(r: &chain_reader::ChainReader, note: &allocator::LeasedPn) -> u128 {
    invariant::pn_locked_opt(r, &note.note.address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .expect("note is on chain")
}

/// The protocol's share of the fees this book has taken so far. Read off the
/// book rather than off RootPN: the hand-over only happens at shutdown, so
/// until then RootPN knows nothing about a trade that has already been paid
/// for.
async fn book_fees(dex: &dodex_sdk::Dex, ob_addr: &str) -> u128 {
    dex.get_order_book_details(ob_addr).await.expect("order book details").total_protocol_fees
}

/// The maker's live orders as `client_order_id -> remaining size`.
///
/// Keyed by the client id rather than the chain's `order_id` because the
/// scenario knows which order is which by the id it placed them with, and a
/// reading indexed by anything else would have to be matched back up anyway.
async fn maker_orders(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
) -> std::collections::BTreeMap<u128, u128> {
    dex.get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner")
        .orders
        .into_iter()
        .map(|o| (o.client_order_id, o.amount))
        .collect()
}
