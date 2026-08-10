//! Every order that must not be placed, and which of the two layers refuses
//! it.
//!
//! An order passes two independent gates. The note validates what it can
//! decide alone — sizes, prices, whether the sender can afford it — and never
//! sends what it rejects. The book validates what only it knows about the
//! order's shape, and rejects by sending the order back. The two look
//! identical from outside if you only ask whether anything rested, and the
//! difference matters: an order the note stopped cost a message, while one
//! the book stopped consumed a queue slot and had to be unwound.
//!
//! `_opNonce` tells them apart. The note advances it exactly where it
//! dispatches a batch to a book and nowhere else, so an unmoved nonce is the
//! note refusing on its own and a moved one is the book refusing afterwards.
//! Neither refusal is legible as an error — both `require`s sit after
//! `tvm.accept()`, and a send does not wait for a transaction — so every case
//! here is read as the absence of its effects, against that one reading of
//! who answered.
//!
//! ## What the note will not send
//!
//! Nothing, in every case: no order rests, no collateral moves, and the nonce
//! stays where it was.
//!
//! - an amount of zero;
//! - a price of zero, and a price that is not a whole number of ticks;
//! - the two size gates, each approached from both sides by a value that can
//!   only fail *that* one: an order worth exactly the minimum notional is
//!   accepted and one a lot cheaper is not, while one unit above that same
//!   size — still worth more than the minimum, no longer a whole lot — is
//!   refused by the lot gate alone. The minimum notional applies to sells as
//!   much as to buys, so the accepted control is placed on both sides;
//! - a sell of outcome tokens the note does not hold, and a sell of an
//!   outcome that does not exist.
//!
//! ## What the book will not queue
//!
//! Each of these passes the note — it checks no flag against another — and
//! is refused by the book, which returns the order. So the nonce moves, and
//! what has to come back is the escrow and the client id.
//!
//! - `POST_ONLY` with any of `MARKET`, `IOC`, `FOK`, or a minimum fill size:
//!   an order that only ever rests cannot also be one that never does, and a
//!   taker-side minimum on a maker-only order has nothing to measure;
//! - `IOC` together with `FOK`, and `MARKET` together with either;
//! - a minimum fill larger than the order itself.
//!
//! ## And two cancellations that do nothing
//!
//! An order id that belongs to somebody else, and one that never existed.
//! Both are silent no-ops in the book rather than reverts — it checks the
//! owner and returns — so what says they were no-ops is that the order named
//! is still resting afterwards and nothing was refunded.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfCancelOrder;

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
use crate::common::market::place_order_full;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::market::FLAG_IOC;
use crate::common::market::FLAG_MARKET;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_REFUSALS: u64 = 360;
const OUTCOME: u32 = 0;
const FULL_PERCENT: u128 = 10_000;

/// The quantisation every size and price below is measured against:
/// `LOT_SIZE_NACKL`, `TICK_SIZE` and `MIN_ORDER_NOTIONAL_NACKL`.
const LOT: u128 = 10_000_000;
const TICK: u128 = 10;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// `FLAG_POST_ONLY` and `FLAG_FOK`, which no scenario had reason to name
/// until the combinations became the subject.
const FLAG_POST_ONLY: u8 = 0x02;
const FLAG_FOK: u8 = 0x08;

/// Two prices that do not cross, so the valid orders this scenario leaves
/// resting stay resting rather than trading with each other.
const BUY_BPS: u128 = 5_000;
const SELL_BPS: u128 = 6_000;

const _: () = assert!(BUY_BPS < SELL_BPS, "the controls must not trade with one another");
const _: () = assert!(BUY_BPS.is_multiple_of(TICK) && SELL_BPS.is_multiple_of(TICK));

/// The size whose value at `BUY_BPS` is exactly the minimum notional — the
/// boundary, from the side that is allowed.
const AT_MIN_NOTIONAL: u128 = MIN_ORDER_NOTIONAL * FULL_PERCENT / BUY_BPS;

/// One lot below it: still a whole number of lots, and now worth less than
/// the minimum. Refused by the notional gate and by nothing else.
const UNDER_MIN_NOTIONAL: u128 = AT_MIN_NOTIONAL - LOT;

/// One *unit* above it: worth more than the minimum, and no longer a whole
/// number of lots. Refused by the lot gate and by nothing else.
const OFF_LOT: u128 = AT_MIN_NOTIONAL + 1;

// Each boundary has to isolate one gate, or a refusal says nothing about
// which one bit. The compiler checks that here rather than the chain later.
const _: () = assert!(AT_MIN_NOTIONAL.is_multiple_of(LOT));
const _: () = assert!(AT_MIN_NOTIONAL * BUY_BPS / FULL_PERCENT == MIN_ORDER_NOTIONAL);
const _: () = assert!(AT_MIN_NOTIONAL * SELL_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
const _: () = assert!(UNDER_MIN_NOTIONAL.is_multiple_of(LOT));
const _: () = assert!(UNDER_MIN_NOTIONAL * BUY_BPS / FULL_PERCENT < MIN_ORDER_NOTIONAL);
const _: () = assert!(!OFF_LOT.is_multiple_of(LOT));
const _: () = assert!(OFF_LOT * BUY_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// The size the flag combinations are sent with — valid in every respect the
/// note inspects, so that only the flags can be what stops them.
const GOOD_AMOUNT: u128 = AT_MIN_NOTIONAL;

/// A price that is not a whole number of ticks.
const OFF_TICK_BPS: u128 = BUY_BPS + 1;

const _: () = assert!(!OFF_TICK_BPS.is_multiple_of(TICK));

/// An outcome this market does not have. It carries two.
const NO_SUCH_OUTCOME: u32 = 7;

/// Collateral the note splits, so it has tokens to write sells against.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn orders_that_must_not_be_placed_are_refused_by_one_layer_or_the_other_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "order_refusals").expect("rent the deployer note");
    let note = alloc.rent(PnProfile::Trd, "order_refusals").expect("rent the trading note");
    let bystander = alloc.rent(PnProfile::Trd, "order_refusals").expect("rent the bystander note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_REFUSALS).await;

    split_full_set(dex, &note, &market.key, SPLIT_COLLATERAL).await;
    let held = at(&outcome_tokens(dex, &note).await, OUTCOME);
    assert!(
        held >= GOOD_AMOUNT,
        "the split left {held} outcome-{OUTCOME} tokens, not enough to write a sell against"
    );

    let base = nonce as u128 * 10;
    let mut cid = base + 1;
    let mut next_cid = || {
        cid += 1;
        cid
    };

    // ── what the note will not send ───────────────────────────────────────
    let quiet_nonce = op_nonce(&r, &note.note.address).await;
    let quiet_free = free(&r, &note.note.address).await;
    let quiet_locked = locked(&r, &note.note.address).await;

    for (label, is_buy, price, amount, outcome) in [
        ("an order for nothing at all", true, BUY_BPS, 0, OUTCOME),
        ("a size that is not whole lots", true, BUY_BPS, OFF_LOT, OUTCOME),
        ("a price of zero", true, 0, AT_MIN_NOTIONAL, OUTCOME),
        ("a price off the tick", true, OFF_TICK_BPS, AT_MIN_NOTIONAL, OUTCOME),
        ("an order under the minimum notional", true, BUY_BPS, UNDER_MIN_NOTIONAL, OUTCOME),
        ("a sell of an outcome that does not exist", false, SELL_BPS, AT_MIN_NOTIONAL, NO_SUCH_OUTCOME),
        ("a sell of more tokens than the note holds", false, SELL_BPS, held + LOT, OUTCOME),
    ] {
        let _ = place_order_full(
            dex,
            &note,
            &market.key,
            outcome,
            is_buy,
            &price.to_string(),
            amount,
            0,
            0,
            0,
            next_cid(),
        )
        .await;
        wait_not_busy(dex, &note.note.address, label).await;
        assert_eq!(
            op_nonce(&r, &note.note.address).await,
            quiet_nonce,
            "the note dispatched {label} to the book instead of refusing it itself"
        );
    }

    assert!(
        owner_orders(dex, &market.order_book, &note.note.dih_dec).await.is_empty(),
        "one of the orders the note should have refused is resting"
    );
    assert_eq!(
        (free(&r, &note.note.address).await, locked(&r, &note.note.address).await),
        (quiet_free, quiet_locked),
        "an order the note refused moved collateral anyway"
    );

    // The boundary from the side that is allowed. Same two gates, the
    // smallest values that clear them — so the two refusals above are those
    // gates biting rather than the note refusing everything.
    let sell_cid = next_cid();
    place_limit(
        dex,
        &note,
        &market.key,
        OUTCOME,
        false,
        &SELL_BPS.to_string(),
        AT_MIN_NOTIONAL,
        sell_cid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, sell_cid, true).await;

    let min_cid = next_cid();
    place_limit(dex, &note, &market.key, OUTCOME, true, &BUY_BPS.to_string(), AT_MIN_NOTIONAL, min_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, min_cid, true).await;
    wait_not_busy(dex, &note.note.address, "a buy worth exactly the minimum").await;

    // ── what the book will not queue ──────────────────────────────────────
    //
    // None of these is a combination the note looks at, so every one of them
    // is dispatched and comes back. The nonce moving is what says so.
    let queued_nonce = op_nonce(&r, &note.note.address).await;
    let queued_locked = locked(&r, &note.note.address).await;
    let mut sent = 0u64;

    for (label, flags, min_amount) in [
        ("post-only that is also a market order", FLAG_POST_ONLY | FLAG_MARKET, 0),
        ("post-only that is also immediate-or-cancel", FLAG_POST_ONLY | FLAG_IOC, 0),
        ("post-only that is also fill-or-kill", FLAG_POST_ONLY | FLAG_FOK, 0),
        ("post-only carrying a minimum fill", FLAG_POST_ONLY, LOT),
        ("immediate-or-cancel that is also fill-or-kill", FLAG_IOC | FLAG_FOK, 0),
        ("a market order that is also immediate-or-cancel", FLAG_MARKET | FLAG_IOC, 0),
        ("a market order that is also fill-or-kill", FLAG_MARKET | FLAG_FOK, 0),
        ("a minimum fill larger than the order", 0, GOOD_AMOUNT + LOT),
    ] {
        let _ = place_order_full(
            dex,
            &note,
            &market.key,
            OUTCOME,
            true,
            &BUY_BPS.to_string(),
            GOOD_AMOUNT,
            flags,
            min_amount,
            0,
            next_cid(),
        )
        .await;
        wait_not_busy(dex, &note.note.address, label).await;
        sent += 1;
        assert_eq!(
            op_nonce(&r, &note.note.address).await,
            queued_nonce + sent,
            "{label} never reached the book — the note refused a combination it does not inspect"
        );
    }

    // And every one of them was handed back: nothing extra rests, and the
    // escrow is exactly where it was before the eight were sent.
    let resting = owner_orders(dex, &market.order_book, &note.note.dih_dec).await;
    assert_eq!(
        resting.len(),
        2,
        "after eight orders the book had to refuse, {} are resting instead of the two valid ones",
        resting.len()
    );
    assert!(
        resting.contains_key(&min_cid) && resting.contains_key(&sell_cid),
        "the orders that survived are not the two valid ones"
    );
    assert_eq!(
        locked(&r, &note.note.address).await,
        queued_locked,
        "an order the book refused kept its collateral escrowed"
    );

    // ── and two cancellations that do nothing ─────────────────────────────
    //
    // The book checks the owner and returns without a word, so neither of
    // these can be read as an error. What says they did nothing is the order
    // that is still there.
    let victim_id = order_id(dex, &market.order_book, &note.note.dih_dec, min_cid)
        .await
        .expect("the valid buy's own order id");

    cancel_raw(dex, &bystander, &market, victim_id).await;
    assert_eq!(
        order_id(dex, &market.order_book, &note.note.dih_dec, min_cid).await,
        Some(victim_id),
        "a note cancelled an order belonging to somebody else"
    );

    let nonexistent = victim_id + 1_000_000;
    let before_ghost = locked(&r, &note.note.address).await;
    cancel_raw(dex, &note, &market, nonexistent).await;
    assert_eq!(
        locked(&r, &note.note.address).await,
        before_ghost,
        "cancelling an order id that never existed refunded something"
    );
    assert_eq!(
        order_id(dex, &market.order_book, &note.note.dih_dec, min_cid).await,
        Some(victim_id),
        "cancelling a nonexistent id took a real order with it"
    );

    note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    bystander.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Cancel by the book's own order id rather than by a client id, which is the
/// only way to name an order the caller does not own — or one that was never
/// there.
async fn cancel_raw(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
    order_id: u128,
) {
    let _ = dex
        .cancel_order(
            &note.note.address,
            ParamsOfCancelOrder {
                event_id: market.key.event_id.clone(),
                oracle_list_hash: market.key.oracle_list_hash.clone(),
                token_type: market.key.token_type,
                order_id,
            },
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
    wait_not_busy(dex, &note.note.address, "a cancellation that should do nothing").await;
}

async fn owner_orders(
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

async fn order_id(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> Option<u128> {
    dex.get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner")
        .orders
        .into_iter()
        .find(|o| o.client_order_id == client_order_id)
        .map(|o| o.order_id)
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

async fn op_nonce(r: &chain_reader::ChainReader, pn_address: &str) -> u64 {
    invariant::pn_op_nonce(r, pn_address).await.expect("read the note's op nonce")
}
