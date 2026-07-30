//! Two live orders that do not cross, and what cancelling them gives back.
//!
//! The suite's only order-book coverage is `proof_money`'s single trade, where
//! a buy meets a resting sell at the same price and both leave the book. That
//! exercises matching; it says nothing about the far more common case of an
//! order that simply rests. Nothing anywhere asserts that a bid below an ask
//! is *left alone*, and nothing asserts what a cancel returns.
//!
//! Both gaps hide the same class of defect. An engine that matched too
//! eagerly, and one that leaked escrow on cancel, would each sail through
//! every test there is today.
//!
//! ## What it asserts
//!
//! A seller rests an ask, a buyer rests a bid below it, and then both cancel:
//!
//! - **Neither order fills.** `OrderBook.Order.amount` is the *remaining*
//!   size (`initialAmount` holds the original), so reading the owner index and
//!   finding the full amount still there is a direct statement that no part of
//!   either order was matched — not an inference from the absence of a trade
//!   event.
//! - **Both rest, on their own side.** Each order is present in its own
//!   owner's index with the price, side and outcome it was placed with, and
//!   each note reports exactly one open order.
//! - **Placing moves escrow without changing totals.** A buy debits
//!   `_balance[tt]` and credits `_lockedInOrders[tt]` by the same amount, so
//!   their sum is invariant while the locked side must actually grow; a sell
//!   debits the outcome tokens out of the note's stake record. Both are
//!   asserted as movements, never against a re-derivation of the contract's
//!   own fee and cost arithmetic.
//! - **Cancelling gives back exactly what placing took.** Every one of those
//!   readings returns to its pre-placement value, to the unit.
//!
//! ## Why the two orders belong to different notes
//!
//! A single note could hold both sides, and the pool has the notes to spare
//! either way — but then a self-trade guard, rather than the prices, could be
//! what keeps them apart, and the scenario would claim to prove something it
//! never tested. Two owners leave the price relation as the only reason they
//! do not meet.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`. Takes no
//! manifest and no preflight: like `usdc_release`, every assertion is a delta
//! against a baseline read moments earlier, so it neither needs a pristine
//! stand nor consumes one.

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
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::wait_not_busy;

/// Distance to `resultStart`. The staking window is a tenth of it and is
/// waited out by the fixture; the rest is this scenario's budget for a split,
/// two placements and two cancels, each of which is an acknowledged note
/// operation.
const STAKE_PERIOD_ORDERS: u64 = 240;

/// The outcome both orders are placed on. They can only fail to cross if they
/// are on the same book.
const OUTCOME: u32 = 0;

/// Bid strictly below ask, both multiples of the 10 bps tick. At
/// [`ORDER_AMOUNT`] the bid is worth 18 NACKL and the ask 24, each clearing
/// the 10 NACKL minimum notional.
const BID_BPS: &str = "6000";
const ASK_BPS: &str = "8000";

/// A multiple of the 0.01 NACKL lot size, and well inside the ~50 outcome
/// tokens a [`SPLIT_COLLATERAL`] split mints.
const ORDER_AMOUNT: u128 = 30_000_000_000;

/// Collateral the seller splits to get something to sell.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn non_crossing_orders_rest_and_cancel_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "resting_orders").expect("rent the deployer note");
    let seller = alloc.rent(PnProfile::Trd, "resting_orders").expect("rent the seller note");
    let buyer = alloc.rent(PnProfile::Trd, "resting_orders").expect("rent the buyer note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_ORDERS).await;

    // The seller needs something to sell. A split mints a full set out of
    // collateral, which is the only way a note comes to hold outcome tokens
    // before any trade has happened.
    split_full_set(dex, &seller, &market.key, SPLIT_COLLATERAL).await;

    let seller_tokens_before = outcome_tokens(dex, &seller).await;
    assert!(
        at(&seller_tokens_before, OUTCOME) >= ORDER_AMOUNT,
        "the split left the seller {} outcome-{OUTCOME} tokens, fewer than the {ORDER_AMOUNT} \
         it is about to offer",
        at(&seller_tokens_before, OUTCOME)
    );
    let buyer_free_before = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_before = pn_locked(&r, &buyer.note.address).await;

    let sell_coid = nonce as u128 * 10;
    let buy_coid = sell_coid + 1;

    place_limit(dex, &seller, &market.key, OUTCOME, false, ASK_BPS, ORDER_AMOUNT, sell_coid).await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, sell_coid, true).await;

    place_limit(dex, &buyer, &market.key, OUTCOME, true, BID_BPS, ORDER_AMOUNT, buy_coid).await;
    wait_owner_order(dex, &market.order_book, &buyer.note.dih_dec, buy_coid, true).await;

    // The book holding both orders is not yet the notes having finished with
    // them: the owner index is written by the book, while the escrow readings
    // below belong to the notes. Waiting on each note's own operation is what
    // separates the two, and it is deliberately not a wait on anything this
    // then asserts.
    wait_not_busy(dex, &seller.note.address, "place sell").await;
    wait_not_busy(dex, &buyer.note.address, "place buy").await;

    // Neither order was touched. `amount` is the remaining size, so this is
    // the whole non-crossing claim in one reading.
    let sell = owned_order(dex, &market.order_book, &seller.note.dih_dec, sell_coid).await;
    let buy = owned_order(dex, &market.order_book, &buyer.note.dih_dec, buy_coid).await;
    assert_eq!(sell.amount, ORDER_AMOUNT, "the ask was partially filled by a bid below it");
    assert_eq!(buy.amount, ORDER_AMOUNT, "the bid was partially filled by an ask above it");
    assert!(!sell.is_buy && buy.is_buy, "the two orders are not on opposite sides");
    assert_eq!(sell.price, ASK_BPS, "the ask rests at a price it was not placed with");
    assert_eq!(buy.price, BID_BPS, "the bid rests at a price it was not placed with");
    assert_eq!((sell.outcome_id, buy.outcome_id), (OUTCOME, OUTCOME));

    assert_eq!(open_orders(&r, &seller.note.address).await, 1, "the seller has one resting ask");
    assert_eq!(open_orders(&r, &buyer.note.address).await, 1, "the buyer has one resting bid");

    // The buy moved collateral from free to locked and nothing else: the sum
    // is what the contract preserves, and the locked side growing is what
    // proves anything happened at all.
    let buyer_free_resting = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_resting = pn_locked(&r, &buyer.note.address).await;
    assert!(
        buyer_locked_resting > buyer_locked_before,
        "the bid locked nothing: {buyer_locked_before} -> {buyer_locked_resting}"
    );
    assert_eq!(
        buyer_free_resting + buyer_locked_resting,
        buyer_free_before + buyer_locked_before,
        "resting the bid changed the buyer's total, not just where it sits"
    );

    // The sell locked outcome tokens out of the note's stake record.
    let seller_tokens_resting = outcome_tokens(dex, &seller).await;
    assert_eq!(
        at(&seller_tokens_resting, OUTCOME) + ORDER_AMOUNT,
        at(&seller_tokens_before, OUTCOME),
        "resting the ask did not take exactly {ORDER_AMOUNT} outcome-{OUTCOME} tokens"
    );

    cancel_by_client(dex, &seller, &market.key, sell_coid).await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, sell_coid, false).await;
    cancel_by_client(dex, &buyer, &market.key, buy_coid).await;
    wait_owner_order(dex, &market.order_book, &buyer.note.dih_dec, buy_coid, false).await;
    wait_not_busy(dex, &seller.note.address, "cancel sell").await;
    wait_not_busy(dex, &buyer.note.address, "cancel buy").await;

    assert_eq!(open_orders(&r, &seller.note.address).await, 0, "the seller's ask is still open");
    assert_eq!(open_orders(&r, &buyer.note.address).await, 0, "the buyer's bid is still open");

    // The point of the cancel half: exactly what was taken comes back.
    assert_eq!(
        pn_balance(&r, &buyer.note.address).await,
        buyer_free_before,
        "cancelling the bid did not restore the buyer's free collateral"
    );
    assert_eq!(
        pn_locked(&r, &buyer.note.address).await,
        buyer_locked_before,
        "cancelling the bid left collateral locked"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &seller).await, OUTCOME),
        at(&seller_tokens_before, OUTCOME),
        "cancelling the ask did not return the seller's outcome tokens"
    );

    // Both notes still hold a market's stake record and outcome tokens, which
    // `release_clean` reads as dirty — correctly, since another scenario would
    // inherit them.
    seller.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    buyer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// One order out of an owner's index, by the client id it was placed with.
async fn owned_order(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> dodex_sdk::OwnedOrder {
    let owned = dex
        .get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner");
    owned.orders.into_iter().find(|o| o.client_order_id == client_order_id).unwrap_or_else(|| {
        panic!("order {client_order_id} is not in {deposit_identifier_hash}'s index")
    })
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
