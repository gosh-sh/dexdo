//! An outcome token traded above par — 1.5 collateral for something that can
//! never redeem for more than 1.
//!
//! Prices are basis points against `FULL_PERCENT = 10000`, and the contracts
//! put **no upper bound** on them: `OrderBook` checks the tick multiple and
//! the minimum notional, and nothing else. Every price this suite has used so
//! far was below par, where a buy's collateral cost (`amount * price /
//! 10000`) is smaller than the token count it buys. Above par that inequality
//! flips, and any place in the code that had quietly assumed otherwise —
//! sizing a lock from the token count, or capping a price at one — would let
//! a buyer take tokens without paying for them.
//!
//! Nothing about this is exotic from the book's side. It is a price like any
//! other, which is exactly why it is worth a test: the arithmetic that breaks
//! here is the arithmetic nothing else exercises.
//!
//! ## What it asserts
//!
//! One ask at 1.5, one buy that crosses it:
//!
//! - the buyer receives exactly the tokens it bought, and the ask leaves the
//!   book — the trade really happened;
//! - the buyer's collateral falls by **at least the notional it offered**
//!   (`amount * 15000 / 10000`, which above par exceeds the token count).
//!   This is the claim: a cap at par anywhere would make the buyer pay no
//!   more than the token count, and the assertion would fail. It is a bound
//!   rather than an equality because the exact figure includes the
//!   contract's fee, and restating fee arithmetic here would check the
//!   implementation against itself;
//! - nothing is left in the buyer's `_lockedInOrders`, so the unusual price
//!   did not strand escrow;
//! - the seller's free collateral grows by **more than the token count** —
//!   the other side of the same claim, read on the account that received it.
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
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_ABOVE_PAR: u64 = 180;
const OUTCOME: u32 = 0;

/// 150% — half again what an outcome token can ever redeem for. A multiple of
/// the 10 bps tick, like any other price.
const ABOVE_PAR_BPS: u128 = 15_000;

/// Basis-point denominator, mirroring the contracts' `FULL_PERCENT`.
const FULL_PERCENT: u128 = 10_000;

/// The size traded, a multiple of the 0.01 NACKL lot.
const TRADE_AMOUNT: u128 = 20_000_000_000;

/// What the buyer offered for it: above par this exceeds `TRADE_AMOUNT`,
/// which is the whole point of the scenario.
const NOTIONAL: u128 = TRADE_AMOUNT * ABOVE_PAR_BPS / FULL_PERCENT;

/// Collateral the seller splits to get something to sell.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

// The premise, enforced by the compiler: below par this scenario would assert
// nothing that the ordinary trading scenarios do not already cover.
const _: () = assert!(ABOVE_PAR_BPS > FULL_PERCENT);
const _: () = assert!(NOTIONAL > TRADE_AMOUNT);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_trade_above_par_costs_more_than_the_tokens_it_buys_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "price_above_par").expect("rent the deployer note");
    let seller = alloc.rent(PnProfile::Trd, "price_above_par").expect("rent the seller note");
    let buyer = alloc.rent(PnProfile::Trd, "price_above_par").expect("rent the buyer note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_ABOVE_PAR).await;

    split_full_set(dex, &seller, &market.key, SPLIT_COLLATERAL).await;

    let price = ABOVE_PAR_BPS.to_string();
    let ask_coid = nonce as u128 * 10;
    let buy_coid = ask_coid + 1;

    let buyer_free_before = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_before = pn_locked(&r, &buyer.note.address).await;
    let buyer_tokens_before = outcome_tokens(dex, &buyer).await;
    let seller_free_before = pn_balance(&r, &seller.note.address).await;

    place_limit(dex, &seller, &market.key, OUTCOME, false, &price, TRADE_AMOUNT, ask_coid).await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, true).await;

    place_limit(dex, &buyer, &market.key, OUTCOME, true, &price, TRADE_AMOUNT, buy_coid).await;
    // The ask leaving the book is the fill; without it every reading below
    // would be that of a trade that never happened.
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, false).await;
    wait_not_busy(dex, &buyer.note.address, "buy above par").await;
    wait_not_busy(dex, &seller.note.address, "ask filled").await;

    assert_eq!(
        at(&outcome_tokens(dex, &buyer).await, OUTCOME),
        at(&buyer_tokens_before, OUTCOME) + TRADE_AMOUNT,
        "the buyer did not receive the {TRADE_AMOUNT} tokens it bought"
    );

    // The claim. Above par the notional exceeds the token count, so a cap at
    // par anywhere in the pricing would show up as the buyer paying no more
    // than `TRADE_AMOUNT`.
    let buyer_free_after = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_after = pn_locked(&r, &buyer.note.address).await;
    let paid = (buyer_free_before + buyer_locked_before)
        .checked_sub(buyer_free_after + buyer_locked_after)
        .unwrap_or_else(|| {
            panic!(
                "the buyer's collateral grew across a purchase: {} -> {}",
                buyer_free_before + buyer_locked_before,
                buyer_free_after + buyer_locked_after
            )
        });
    assert!(
        paid >= NOTIONAL,
        "the buyer paid {paid} for {TRADE_AMOUNT} tokens offered at {ABOVE_PAR_BPS} bps, less \
         than the {NOTIONAL} it offered — a price above par was not charged in full"
    );

    assert_eq!(
        buyer_locked_after, buyer_locked_before,
        "a fully filled buy above par left collateral locked"
    );

    // The same claim from the receiving side.
    let seller_free_after = pn_balance(&r, &seller.note.address).await;
    assert!(
        seller_free_after > seller_free_before + TRADE_AMOUNT,
        "the seller received {} for {TRADE_AMOUNT} tokens sold above par, no more than the \
         token count itself",
        seller_free_after.saturating_sub(seller_free_before)
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
