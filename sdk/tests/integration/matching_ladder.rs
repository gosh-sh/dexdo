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
//! ## What this does not cover
//!
//! `buyerRefund` on price improvement (the taker locks at 0.70 and pays 0.60,
//! and the difference comes back) is visible here only as "less was spent
//! than was locked", which is weaker than an equality; asserting the amount
//! means restating the contract's fee arithmetic. Conservation across both
//! legs is `proof_money`'s Σ-check, not repeated here. Self-matching and IOC
//! into an empty side are separate units.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::abi_uint;
use crate::common::market::at;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_LADDER: u64 = 300;
const OUTCOME: u32 = 0;

/// The two ask levels. `BEST_BPS` is the better price for a buyer, so a
/// correct walk empties it before touching `WORSE_BPS`.
const BEST_BPS: &str = "6000";
const WORSE_BPS: &str = "7000";

/// Each ask. At `BEST_BPS` this is worth 12 NACKL, clearing the 10 NACKL
/// minimum notional with room to spare.
const ASK_AMOUNT: u128 = 20_000_000_000;

/// The taker's size: one whole ask plus half of the next. Any size that
/// divided evenly into the asks would leave nothing partially filled, and the
/// ordering claim would rest on absence alone.
const TAKE_AMOUNT: u128 = 30_000_000_000;

/// What the taker's fill leaves on A2.
const A2_REMAINING: u128 = ASK_AMOUNT + ASK_AMOUNT - TAKE_AMOUNT;

/// Collateral the maker splits. Three asks need 60 outcome tokens; a split of
/// this yields roughly 100 of each.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

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
        place_limit(dex, &maker, &market.key, OUTCOME, false, price, ASK_AMOUNT, coid).await;
        wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, coid, true).await;
    }

    let taker_tokens_before = outcome_tokens(dex, &taker).await;
    let take_coid = base + 4;

    // Priced at the worse level on purpose: the taker can afford either, so
    // only level priority decides what it gets.
    place_limit(dex, &taker, &market.key, OUTCOME, true, WORSE_BPS, TAKE_AMOUNT, take_coid).await;
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

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
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

/// Kept honest against the ABI encoding trap `market::abi_uint` documents:
/// the prices in this scenario are only ever compared through it.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_best_level_is_the_cheaper_one_for_a_buyer() {
        assert!(abi_uint(BEST_BPS).unwrap() < abi_uint(WORSE_BPS).unwrap());
    }
}
