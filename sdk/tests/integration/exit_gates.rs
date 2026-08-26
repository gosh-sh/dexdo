//! What a note may not do while it still has an order on a book, and the
//! sizes a market refuses to split or merge.
//!
//! ## The gate a resting order puts on everything else
//!
//! An order holds two things a note cannot account for while they are out
//! there: collateral in escrow, and outcome tokens the market believes are
//! still staked. So every way a note has of settling up or leaving is barred
//! while any of its orders on that market is alive — claiming, cancelling the
//! stake, forfeiting it, withdrawing, transferring, minting a coupon.
//!
//! That is one guard, `_openOrdersByEvent`, in front of six doors, and the
//! reason it is worth pushing all six is that they are separate `require`s in
//! separate functions that happen to agree today. The failure it guards
//! against is specific and silent: `onOrderCancelled` drops returned outcome
//! tokens on the floor when the stake record has already gone, so a note that
//! got through any of these doors early would lose exactly what the drain was
//! about to hand back.
//!
//! None of the six answers. They are `require`s past `tvm.accept()`, so what
//! each leaves is nothing — and "nothing" is also what an unsent message
//! leaves. So the order is cancelled and one of the six is made again, where
//! it has to work. The one chosen is the forfeit: it is barred by the open
//! order and by nothing else, whereas a transfer or a coupon would still be
//! refused afterwards for wanting an empty stake record, which this note does
//! not have and is not the subject.
//!
//! ## And the two sizes a market will not take
//!
//! A split mints whole baskets, so collateral under one basket buys nothing
//! and is refused rather than swallowed. A merge burns whole baskets, so an
//! amount that is short on any single outcome — even by one unit, even with
//! a fortune in the others — makes no basket at all and is refused too.
//! Both are the same idea from opposite ends, and both are worth having
//! because the honest failure is not a revert but a silent zero.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfGenerateCoupon;
use dodex_contracts::dex::private_note::ParamsOfInitTransfer;
use dodex_contracts::dex::private_note::ParamsOfMergeFullSet;
use dodex_contracts::dex::private_note::ParamsOfSplitFullSet;
use dodex_contracts::dex::private_note::ParamsOfWithdrawTokens;

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
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_GATES: u64 = 360;
const OUTCOME: u32 = 0;
const OTHER_OUTCOME: u32 = 1;
const FULL_PERCENT: u128 = 10_000;
const LOT: u128 = 10_000_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// The sell the note leaves on the book while every exit is tried. A sell,
/// because it holds outcome tokens rather than collateral — those are what
/// `onOrderCancelled` would drop if a claim got in first, which is the
/// failure the gate exists for.
const GATE_BPS: u128 = 6_000;
const GATE_AMOUNT: u128 = 20_000_000_000;

const _: () = assert!(GATE_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(GATE_AMOUNT * GATE_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// Collateral the note splits, so it has tokens to sell and a basket to merge.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

/// Where a transfer would have gone, and how much. Neither matters: the call
/// is refused before either is looked at.
const TRANSFER_AMOUNT: u128 = 1_000_000_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_resting_order_bars_every_exit_and_a_market_refuses_impossible_baskets_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "exit_gates").expect("rent the deployer note");
    let note = alloc.rent(PnProfile::Trd, "exit_gates").expect("rent the note under test");
    let sink = alloc.rent(PnProfile::Trd, "exit_gates").expect("rent the transfer destination");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market = deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_GATES).await;

    split_full_set(dex, &note, &market.key, SPLIT_COLLATERAL).await;
    let minted = outcome_tokens(dex, &note).await;
    assert!(
        at(&minted, OUTCOME) >= GATE_AMOUNT,
        "the split left {} outcome-{OUTCOME} tokens, not enough for the sell every exit is \
         tested against",
        at(&minted, OUTCOME)
    );

    // ── the two sizes a market will not take ──────────────────────────────
    //
    // Done first, while the note is otherwise idle, so a refusal here cannot
    // be confused with the open-order gate below.
    let basket = invariant::pmp_split_merge_q(&r, &market.pmp).await.expect("read the basket");
    assert!(basket > 1, "a basket of {basket} leaves nothing that can be under it");

    let before_small_split = free(&r, &note.note.address).await;
    let tokens_before_small_split = outcome_tokens(dex, &note).await;
    try_split(dex, &note, &market, basket - 1).await;
    assert_eq!(
        free(&r, &note.note.address).await,
        before_small_split,
        "collateral under a whole basket was taken for a split that could mint nothing"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &note).await, OUTCOME),
        at(&tokens_before_small_split, OUTCOME),
        "a split of less than one basket minted something"
    );

    // A merge that is one unit short on a single outcome. Everything else
    // about it is generous — the other outcome is offered in full — and it
    // still makes no whole basket, so nothing may burn.
    let held_other = at(&minted, OTHER_OUTCOME);
    assert!(held_other > 0, "the split minted no outcome-{OTHER_OUTCOME} tokens to be short of");
    let before_short_merge = free(&r, &note.note.address).await;
    try_merge(dex, &note, &market, vec![at(&minted, OUTCOME), 0]).await;
    assert_eq!(
        free(&r, &note.note.address).await,
        before_short_merge,
        "a merge with nothing offered on one outcome returned collateral anyway"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &note).await, OUTCOME),
        at(&minted, OUTCOME),
        "a merge that could burn no whole basket burned tokens"
    );

    // ── the gate a resting order puts on everything else ──────────────────
    let gate_cid = nonce as u128 * 10 + 1;
    place_limit(dex, &note, &market.key, OUTCOME, false, &GATE_BPS.to_string(), GATE_AMOUNT, gate_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, gate_cid, true).await;
    wait_not_busy(dex, &note.note.address, "the order every exit is tried against").await;
    assert_eq!(
        open_orders(&r, &note.note.address).await,
        1,
        "the note does not believe it has the order the gate is supposed to see"
    );

    let barred_free = free(&r, &note.note.address).await;
    let barred_locked = locked(&r, &note.note.address).await;
    let barred_tokens = outcome_tokens(dex, &note).await;

    try_every_exit(dex, &note, &sink, &market).await;

    // Not one of the six may have gone through, and each would have left a
    // different mark: a payout, a refund, a deleted record, a drained
    // balance, a latched flag, a coupon.
    assert_eq!(
        (free(&r, &note.note.address).await, locked(&r, &note.note.address).await),
        (barred_free, barred_locked),
        "one of the exits barred by a resting order moved collateral"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &note).await, OUTCOME),
        at(&barred_tokens, OUTCOME),
        "one of the barred exits moved outcome tokens"
    );
    assert_eq!(
        stake_count(dex, &note).await,
        1,
        "one of the barred exits consumed the stake record"
    );
    assert_eq!(
        coupons(&r, &note.note.address).await,
        0,
        "a note with an order open minted itself a coupon"
    );
    assert_eq!(
        open_orders(&r, &note.note.address).await,
        1,
        "the order the gate rests on is gone, so nothing above was actually gated"
    );

    // ── and the same six with the order gone ──────────────────────────────
    //
    // The control the phase needs: "nothing happened" is equally true of six
    // messages that never arrived. With the order cancelled, the first of the
    // six that is still meaningful has to work.
    cancel_by_client(dex, &note, &market.key, gate_cid).await;
    poll_until("the gating order never left the book", || async {
        open_orders(&r, &note.note.address).await == 0
    })
    .await;

    // Forfeiting is the control, not transferring: `initTransfer` also
    // requires an empty stake record, and this note has one from the split,
    // so it would be refused for a second reason and prove nothing about the
    // first. A forfeit is barred by the open order alone.
    let stakes_before = stake_count(dex, &note).await;
    assert_eq!(stakes_before, 1, "the note has no stake record left to forfeit");

    let _ = dex
        .delete_stake(
            &note.note.address,
            market.key.clone(),
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
    wait_not_busy(dex, &note.note.address, "a forfeit with no orders in the way").await;
    poll_until("the forfeit never went through once the order was gone", || async {
        stake_count(dex, &note).await == 0
    })
    .await;

    note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    sink.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Try all six ways out of a market, in the order a user might reach for
/// them. Every one is expected to do nothing; what they leave is asserted by
/// the caller, together, because each would leave a different trace and the
/// point is that none of them did.
async fn try_every_exit(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    sink: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    let keys = Signer::Keys { keys: note.note.keys.clone() };

    let _ = dex.claim(&note.note.address, market.key.clone(), keys.clone()).await;
    wait_not_busy(dex, &note.note.address, "claim with an order open").await;

    let _ = dex.cancel_stake(&note.note.address, market.key.clone(), keys.clone()).await;
    wait_not_busy(dex, &note.note.address, "cancelStake with an order open").await;

    let _ = dex.delete_stake(&note.note.address, market.key.clone(), keys.clone()).await;
    wait_not_busy(dex, &note.note.address, "deleteStake with an order open").await;

    let _ = dex
        .withdraw_tokens(
            &note.note.address,
            ParamsOfWithdrawTokens {
                dest_wallet_addr: sink.note.address.clone(),
                dapp_id: SystemDapp::Dex.dapp_id().to_string(),
            },
            keys.clone(),
        )
        .await;
    wait_not_busy(dex, &note.note.address, "withdraw with an order open").await;

    let _ = dex
        .init_transfer(
            &note.note.address,
            ParamsOfInitTransfer {
                dest_deposit_hash: sink.note.dih_dec.clone(),
                token_type: TOKEN_TYPE_NACKL,
                amount: TRANSFER_AMOUNT,
                // Record only — the gate under test is the refusal, not the coin.
                ecc_amount: 0,
            },
            keys.clone(),
        )
        .await;
    wait_not_busy(dex, &note.note.address, "transfer with an order open").await;

    let _ = dex
        .generate_coupon(
            &note.note.address,
            ParamsOfGenerateCoupon { token_type: TOKEN_TYPE_NACKL },
            keys,
        )
        .await;
    wait_not_busy(dex, &note.note.address, "generateCoupon with an order open").await;
}

async fn try_split(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
    collateral: u128,
) {
    let _ = dex
        .split_full_set(
            &note.note.address,
            ParamsOfSplitFullSet {
                event_id: market.key.event_id.clone(),
                oracle_list_hash: market.key.oracle_list_hash.clone(),
                token_type: market.key.token_type,
                collateral,
            },
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
    wait_not_busy(dex, &note.note.address, "a split of less than one basket").await;
}

async fn try_merge(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
    amount: Vec<u128>,
) {
    let _ = dex
        .merge_full_set(
            &note.note.address,
            ParamsOfMergeFullSet {
                event_id: market.key.event_id.clone(),
                oracle_list_hash: market.key.oracle_list_hash.clone(),
                token_type: market.key.token_type,
                amount,
            },
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
    wait_not_busy(dex, &note.note.address, "a merge that makes no whole basket").await;
}

async fn stake_count(dex: &dodex_sdk::Dex, note: &allocator::LeasedPn) -> usize {
    dex.get_stakes(&note.note.address).await.expect("stakes").stakes.len()
}

async fn free(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn open_orders(r: &chain_reader::ChainReader, pn_address: &str) -> u32 {
    invariant::pn_open_order_count(r, pn_address).await.expect("read open order count")
}

async fn coupons(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_coupons_value(r, pn_address).await.expect("read the note's coupon")
}
