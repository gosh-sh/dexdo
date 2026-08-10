//! Two orders that would trade, kept apart — and what a book does with a
//! batch it will not take.
//!
//! ## Segments
//!
//! A price level is not addressed by price alone. `_levels` is keyed by
//! outcome, side, **epoch** and price, so two orders can name prices that
//! cross and still never meet, because they are looking at different books.
//! Every scenario in the suite has placed everything in epoch `0` on one
//! outcome, which means the separation has never been exercised — and it is
//! the kind that fails silently in the direction that matters: a book that
//! ignored the epoch would match orders their owners deliberately kept apart,
//! and both sides would see a fill they never asked for.
//!
//! Two phases, one arrangement each:
//!
//! - a sell in epoch 1 and a buy in epoch 2, priced to cross. Neither may
//!   fill. Then the same buy in **epoch 1** does fill — which is the control
//!   the phase needs, since "nothing happened" is also what a buy priced too
//!   low, or a note out of collateral, would produce.
//! - a sell of one outcome and a buy of the other, at those same crossing
//!   prices. Neither may fill either. The control is the phase above: the
//!   prices are the ones that just traded.
//!
//! ## Batches
//!
//! `placeBatch` takes a list of placements and a list of cancellations
//! together, and the rules it enforces on them are all `require`s after
//! `tvm.accept()` — invisible to the sender, whose send does not wait for a
//! transaction. So every refusal here is read the same way as elsewhere in
//! the suite: nothing rested, nothing was locked, and `_opNonce` — which the
//! note advances only where it dispatches to the book — has not moved.
//!
//! What the phase covers:
//!
//! - **cancels run before places.** A batch that cancels a resting bid and
//!   places an ask at that same price is the discriminator: run the other way
//!   round, the ask would eat the bid on its way in. This is the reason the
//!   ordering exists, and asserting the ask *rested* is the only way to see
//!   it from outside.
//! - **the two limits are independent.** Ten placements and ten
//!   cancellations in one call is legal; eleven of either alone is not.
//! - **a client id may not repeat inside a batch**, which the note enforces
//!   with the same reservation it uses across batches.
//! - **an empty batch is refused** rather than acknowledged as a no-op.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::order_book::OrderBookOrder;
use dodex_contracts::dex::private_note::ParamsOfPlaceBatch;

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
use crate::common::market::place_limit_in_epoch;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_SEGMENTS: u64 = 360;

/// The two outcomes this market carries.
const OUTCOME_A: u32 = 0;
const OUTCOME_B: u32 = 1;

/// The crossing pair every phase uses: an ask below a bid, so at equal
/// coordinates they trade. Whether they do is what each phase is about.
const ASK_BPS: u128 = 6_000;
const BID_BPS: u128 = 7_000;
const FULL_PERCENT: u128 = 10_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

const _: () = assert!(ASK_BPS < BID_BPS, "the ask has to be affordable to the bid");

/// Each order in the isolation phases. Worth 12 NACKL at the ask's price and
/// 14 at the bid's, both clear of the minimum notional.
const SEGMENT_AMOUNT: u128 = 20_000_000_000;

const _: () = assert!(SEGMENT_AMOUNT * ASK_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// The segments the first phase keeps apart. Both non-zero: `0` is what every
/// other scenario places into, and a phase that used it would be asserting
/// against the default rather than against a named segment.
const EPOCH_ONE: u64 = 1;
const EPOCH_TWO: u64 = 2;

const _: () = assert!(EPOCH_ONE != EPOCH_TWO);

/// The segment the batch phase works in, and the reason it has one: the
/// phases above leave orders resting, and one of them is a bid on this outcome
/// at a price any ask the batch places would cross. A phase that shares a
/// segment with an earlier phase's leftovers is not testing what it says it
/// is — it is racing them.
const EPOCH_BATCH: u64 = 3;

const _: () = assert!(EPOCH_BATCH != EPOCH_ONE && EPOCH_BATCH != EPOCH_TWO);
const _: () = assert!(EPOCH_BATCH != 0, "epoch 0 is where every other scenario places");

/// The pair the cancels-first claim rests on: a bid and an ask at the same
/// price, so an ask placed before the cancellation would trade with the bid
/// instead of resting.
const BATCH_CROSS_BPS: u128 = 5_000;

/// The ladders, priced under that pair so ten bids arriving later cannot eat
/// the ask the batch left resting.
const BATCH_BID_BPS: u128 = 4_000;
const BATCH_AMOUNT: u128 = 30_000_000_000;

const _: () = assert!(BATCH_BID_BPS < BATCH_CROSS_BPS, "the ladder must not reach the ask");
const _: () = assert!(BATCH_AMOUNT * BATCH_BID_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
const _: () = assert!(BATCH_AMOUNT * BATCH_CROSS_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// Collateral the maker splits. The asks it writes come out of this: one in
/// the epoch phase, one on the other outcome, and the batch phase's own.
const SPLIT_COLLATERAL: u128 = 300_000_000_000;

/// `MAX_BATCH_SIZE` — the most either list of a batch may carry.
const MAX_BATCH: usize = 10;

/// One past it, which is what the refusals send.
const OVER_BATCH: usize = MAX_BATCH + 1;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn segments_keep_crossing_orders_apart_and_a_batch_is_all_or_nothing_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "book_segments").expect("rent the deployer note");
    let maker = alloc.rent(PnProfile::Trd, "book_segments").expect("rent the maker note");
    let taker = alloc.rent(PnProfile::Trd, "book_segments").expect("rent the taker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_SEGMENTS).await;

    // Both outcomes, because the second phase sells the one the first does
    // not. A split mints the whole set, so one call covers both.
    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;
    let maker_tokens = outcome_tokens(dex, &maker).await;
    for (outcome, needed) in
        [(OUTCOME_A, SEGMENT_AMOUNT + BATCH_AMOUNT), (OUTCOME_B, SEGMENT_AMOUNT)]
    {
        assert!(
            at(&maker_tokens, outcome) >= needed,
            "the split left the maker {} outcome-{outcome} tokens, short of the {needed} its              asks on that outcome need",
            at(&maker_tokens, outcome)
        );
    }

    let base = nonce as u128 * 10;

    // ── two segments of the same book ─────────────────────────────────────
    let epoch_ask = base + 1;
    let epoch_bid = base + 2;

    place_limit_in_epoch(
        dex,
        &maker,
        &market.key,
        OUTCOME_A,
        false,
        &ASK_BPS.to_string(),
        SEGMENT_AMOUNT,
        EPOCH_ONE,
        epoch_ask,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, epoch_ask, true).await;

    let taker_locked_before = pn_locked(&r, &taker.note.address).await;
    place_limit_in_epoch(
        dex,
        &taker,
        &market.key,
        OUTCOME_A,
        true,
        &BID_BPS.to_string(),
        SEGMENT_AMOUNT,
        EPOCH_TWO,
        epoch_bid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &taker.note.dih_dec, epoch_bid, true).await;
    wait_not_busy(dex, &taker.note.address, "a bid in the other segment").await;

    // The claim: the bid rested rather than filling, and the ask it could
    // have afforded is untouched.
    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, epoch_ask).await,
        Some(SEGMENT_AMOUNT),
        "the ask lost size to a bid placed in another segment of the book"
    );
    assert_eq!(
        order_size(dex, &market.order_book, &taker.note.dih_dec, epoch_bid).await,
        Some(SEGMENT_AMOUNT),
        "the bid did not rest whole"
    );
    assert!(
        pn_locked(&r, &taker.note.address).await > taker_locked_before,
        "the bid holds no collateral, so it is not really resting"
    );

    // The control. Same two prices, same two notes, same outcome — only the
    // segment changes, and now they trade. Without this, every reading above
    // is equally true of a bid that was simply too poor to fill.
    let control_bid = base + 3;
    place_limit_in_epoch(
        dex,
        &taker,
        &market.key,
        OUTCOME_A,
        true,
        &BID_BPS.to_string(),
        SEGMENT_AMOUNT,
        EPOCH_ONE,
        control_bid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, epoch_ask, false).await;
    wait_not_busy(dex, &taker.note.address, "a bid in the ask's own segment").await;
    assert_eq!(
        order_size(dex, &market.order_book, &taker.note.dih_dec, epoch_bid).await,
        Some(SEGMENT_AMOUNT),
        "the fill in one segment consumed the order resting in the other"
    );

    // ── two outcomes of the same market ───────────────────────────────────
    //
    // Same prices, same epoch, different outcome. The prices are the ones
    // that just traded a paragraph above, which is what makes this phase's
    // silence meaningful without a control of its own.
    let outcome_ask = base + 4;
    let outcome_bid = base + 5;

    place_limit(
        dex,
        &maker,
        &market.key,
        OUTCOME_B,
        false,
        &ASK_BPS.to_string(),
        SEGMENT_AMOUNT,
        outcome_ask,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, outcome_ask, true).await;

    place_limit(
        dex,
        &taker,
        &market.key,
        OUTCOME_A,
        true,
        &BID_BPS.to_string(),
        SEGMENT_AMOUNT,
        outcome_bid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &taker.note.dih_dec, outcome_bid, true).await;
    wait_not_busy(dex, &taker.note.address, "a bid on the other outcome").await;

    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, outcome_ask).await,
        Some(SEGMENT_AMOUNT),
        "an ask on outcome {OUTCOME_B} was filled by a bid on outcome {OUTCOME_A}"
    );
    assert_eq!(
        order_size(dex, &market.order_book, &taker.note.dih_dec, outcome_bid).await,
        Some(SEGMENT_AMOUNT),
        "the bid on outcome {OUTCOME_A} did not rest whole"
    );

    // ── what a batch does, and what it refuses ────────────────────────────
    //
    // In a segment of its own, because the phases above left orders resting
    // and one of them is a bid this phase's ask would cross. Nothing else has
    // ever been placed in `EPOCH_BATCH`, so everything here rests or trades
    // only against the rest of this phase.
    let batch_base = base + 100;
    let resting_before = owner_orders(dex, &market.order_book, &maker.note.dih_dec).await.len();

    // Cancels first, and the ordering is the whole claim. The batch cancels a
    // resting bid and places an ask at that same price: run places-first, the
    // ask would find the bid and trade with its own owner.
    let victim_bid = batch_base + 1;
    let widow_ask = batch_base + 2;
    place_limit_in_epoch(
        dex,
        &maker,
        &market.key,
        OUTCOME_A,
        true,
        &BATCH_CROSS_BPS.to_string(),
        BATCH_AMOUNT,
        EPOCH_BATCH,
        victim_bid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, victim_bid, true).await;
    let victim_id = order_id(dex, &market.order_book, &maker.note.dih_dec, victim_bid)
        .await
        .expect("the bid the batch is about to cancel");

    send_batch(
        dex,
        &maker,
        &market,
        vec![order_at(OUTCOME_A, false, BATCH_CROSS_BPS, BATCH_AMOUNT, widow_ask)],
        vec![victim_id],
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, widow_ask, true).await;
    wait_not_busy(dex, &maker.note.address, "a batch that cancels and places").await;

    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, victim_bid).await,
        None,
        "the batch did not cancel the bid it was told to"
    );
    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, widow_ask).await,
        Some(BATCH_AMOUNT),
        "the ask placed alongside that cancellation is gone or short — placed before the cancel, \
         it would have traded with the bid instead of resting"
    );

    // Ten and ten in one call: the limits are counted per list, not together.
    let ladder: Vec<u128> = (0..MAX_BATCH as u128).map(|i| batch_base + 10 + i).collect();
    send_batch(
        dex,
        &maker,
        &market,
        ladder
            .iter()
            .map(|cid| order_at(OUTCOME_A, true, BATCH_BID_BPS, BATCH_AMOUNT, *cid))
            .collect(),
        Vec::new(),
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, ladder[MAX_BATCH - 1], true)
        .await;
    wait_not_busy(dex, &maker.note.address, "a batch of ten placements").await;

    let ladder_ids: Vec<u128> = {
        let mut ids = Vec::new();
        for cid in &ladder {
            ids.push(
                order_id(dex, &market.order_book, &maker.note.dih_dec, *cid)
                    .await
                    .unwrap_or_else(|| panic!("order {cid} of the ten-placement batch is missing")),
            );
        }
        ids
    };

    let second_ladder: Vec<u128> = (0..MAX_BATCH as u128).map(|i| batch_base + 30 + i).collect();
    send_batch(
        dex,
        &maker,
        &market,
        second_ladder
            .iter()
            .map(|cid| order_at(OUTCOME_A, true, BATCH_BID_BPS, BATCH_AMOUNT, *cid))
            .collect(),
        ladder_ids,
    )
    .await;
    wait_owner_order(
        dex,
        &market.order_book,
        &maker.note.dih_dec,
        second_ladder[MAX_BATCH - 1],
        true,
    )
    .await;
    wait_not_busy(dex, &maker.note.address, "a batch of ten and ten").await;

    for cid in &ladder {
        assert_eq!(
            order_size(dex, &market.order_book, &maker.note.dih_dec, *cid).await,
            None,
            "order {cid} survived the cancellation half of a ten-and-ten batch"
        );
    }
    for cid in &second_ladder {
        assert_eq!(
            order_size(dex, &market.order_book, &maker.note.dih_dec, *cid).await,
            Some(BATCH_AMOUNT),
            "order {cid} of the placement half of a ten-and-ten batch is not resting"
        );
    }

    // ── and the batches it will not take ──────────────────────────────────
    //
    // Every one of these is a `require` the note reaches after accepting the
    // message, so none of them answers the sender. What they leave is an
    // unmoved nonce and a book that looks exactly as it did.
    let refused_nonce = op_nonce(&r, &maker.note.address).await;
    let refused_orders = owner_orders(dex, &market.order_book, &maker.note.dih_dec).await;
    let refused_cids = batch_base + 60;

    let over_limit: Vec<OrderBookOrder> = (0..OVER_BATCH as u128)
        .map(|i| order_at(OUTCOME_A, true, BATCH_BID_BPS, BATCH_AMOUNT, refused_cids + i))
        .collect();
    let over_cancels: Vec<u128> = (1..=OVER_BATCH as u128).collect();

    for (label, orders, cancels) in [
        ("an empty batch", Vec::new(), Vec::new()),
        ("eleven placements", over_limit.clone(), Vec::new()),
        ("eleven cancellations", Vec::new(), over_cancels),
        (
            "a batch repeating a client id",
            vec![
                order_at(OUTCOME_A, true, BATCH_BID_BPS, BATCH_AMOUNT, refused_cids),
                order_at(OUTCOME_A, true, BATCH_BID_BPS, BATCH_AMOUNT, refused_cids),
            ],
            Vec::new(),
        ),
    ] {
        send_batch(dex, &maker, &market, orders, cancels).await;
        wait_not_busy(dex, &maker.note.address, label).await;
        assert_eq!(
            op_nonce(&r, &maker.note.address).await,
            refused_nonce,
            "{label} reached the book: the note advanced its nonce for it"
        );
    }

    let after_refusals = owner_orders(dex, &market.order_book, &maker.note.dih_dec).await;
    assert_eq!(
        after_refusals, refused_orders,
        "the refused batches changed what is resting on the book"
    );
    assert!(
        after_refusals.len() > resting_before,
        "the batch phase left nothing resting, so the reading above compares two empty books"
    );

    // The client ids the refused batches carried are free again — the note
    // reserves them before it validates, and a valid order proves it gave
    // them back.
    place_limit_in_epoch(
        dex,
        &maker,
        &market.key,
        OUTCOME_A,
        true,
        &BATCH_BID_BPS.to_string(),
        BATCH_AMOUNT,
        EPOCH_BATCH,
        refused_cids,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, refused_cids, true).await;
    assert!(
        op_nonce(&r, &maker.note.address).await > refused_nonce,
        "a valid placement left the nonce where the refused batches did — the path itself is not \
         working and the readings above say nothing"
    );

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// One entry of a batch, with the fields this scenario never varies pinned.
fn order_at(
    outcome_id: u32,
    is_buy: bool,
    price_bps: u128,
    amount: u128,
    client_order_id: u128,
) -> OrderBookOrder {
    OrderBookOrder {
        outcome_id,
        is_buy,
        flags: 0,
        price: price_bps.to_string(),
        amount,
        min_amount: 0,
        epoch_id: EPOCH_BATCH,
        client_order_id,
    }
}

/// Send a batch and say nothing about what became of it. Whether the note took
/// it is each phase's own assertion — a batch it refuses does not report back.
async fn send_batch(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
    orders: Vec<OrderBookOrder>,
    cancel_ids: Vec<u128>,
) {
    let _ = dex
        .place_batch(
            &note.note.address,
            ParamsOfPlaceBatch {
                event_id: market.key.event_id.clone(),
                oracle_list_hash: market.key.oracle_list_hash.clone(),
                token_type: market.key.token_type,
                orders,
                cancel_ids,
            },
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
}

/// A note's live orders as `client_order_id -> remaining size`.
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

/// What is left of one of them, or `None` once it has left the book.
async fn order_size(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> Option<u128> {
    owner_orders(dex, ob_addr, deposit_identifier_hash).await.get(&client_order_id).copied()
}

/// The book's own id for one of them, which is what a cancellation names.
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

/// The note's NACKL held against its resting orders — `_lockedInOrders`.
async fn pn_locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

/// The note's order-operation counter — bumped once per batch it dispatches
/// to a book and never otherwise, which is what separates "the note refused"
/// from "the book refused".
async fn op_nonce(r: &chain_reader::ChainReader, pn_address: &str) -> u64 {
    invariant::pn_op_nonce(r, pn_address).await.expect("read the note's op nonce")
}
