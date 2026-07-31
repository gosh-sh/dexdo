//! A market order takes the liquidity it finds and leaves nothing behind.
//!
//! Every order this suite has ever placed was a limit order. A market order
//! is a different instrument in two ways that the book implements as one
//! branch each, and nothing exercises either:
//!
//! - **It never rests.** `OrderBook` inserts an unfilled remainder into the
//!   book for a limit order and returns it to the caller for a market one.
//!   The failure mode is silent and permanent: a market order that rested
//!   would sit on the book holding escrow that its owner believes it got back.
//! - **A market buy is denominated in quote.** `amount` is collateral, not
//!   outcome tokens, and the unfilled part comes back unscaled by any price.
//!
//! ## What it asserts
//!
//! A seller rests an ask; a buyer sends a market buy carrying more collateral
//! than that ask can absorb, so there is necessarily a remainder:
//!
//! - the ask is gone from the seller's index — the market order really did
//!   meet liquidity, which is what stops the rest of this from passing
//!   against an empty book;
//! - the buyer's index is **empty** and its `_openOrderCount` is zero: the
//!   remainder did not rest;
//! - the buyer holds exactly the ask's size in outcome tokens — the fill was
//!   the whole ask and nothing more;
//! - the buyer has **nothing** left in `_lockedInOrders`. This is the escrow
//!   half of "never rests", and it is stated as an equality against the
//!   pre-order reading rather than against a re-derivation of what the fill
//!   cost: the contract's fee arithmetic is its own business, but a market
//!   order finishing with collateral still locked is a defect under any
//!   arithmetic.
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

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_buy_fills_and_never_rests_local() {
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
