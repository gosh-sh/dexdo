//! A market maker's whole working day, in the order a maker actually does it.
//!
//! The suite has covered each of these calls in isolation or not at all. What
//! it has never covered is the sequence: quote both sides in one batch, get
//! taken on part of it, withdraw one quote by name, withdraw the rest
//! wholesale, unwind the inventory back into collateral, and settle when the
//! market closes. Each step leaves state the next one depends on — a lock, a
//! client id, a stake record — and the failures that matter are the ones where
//! step three works but leaves step five with nothing to find.
//!
//! ## The phases
//!
//! 1. **A two-sided quote in one batch.** Three bids and three asks with
//!    client ids, placed together, all resting.
//! 2. **Taken on one of them.** A taker crosses part of the best ask, so the
//!    maker is left holding a partially filled order — the state every
//!    subsequent step has to handle rather than the clean one.
//! 3. **One quote withdrawn by name.** `cancelOrderByClient` on a client id
//!    the maker chose: that order leaves the book and its escrow comes back,
//!    and the others do not move.
//! 4. **The rest withdrawn wholesale.** `cancelAllOrders` — never called by
//!    any scenario until now — has to empty the owner index, zero the note's
//!    own counter, and return every remaining lock.
//! 5. **And again with nothing to cancel**, which must complete rather than
//!    hang: the note latches `_pendingBatchActive` on the way in and only
//!    `onBatchComplete` releases it, so a book with nothing to report is the
//!    case where a missing acknowledgement would strand the note for good.
//! 6. **Inventory back into collateral.** A split and a merge of the same
//!    basket count, asserted as an exact round trip: what the split took, the
//!    merge returns, down to the unit. Stated on the *difference* the fresh
//!    split made rather than on everything the note holds, so the trading
//!    above cannot blur it.
//! 7. **Settlement.** The market resolves and the maker claims what its
//!    remaining position is worth.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::order_book::OrderBookOrder;
use dodex_contracts::dex::private_note::ParamsOfCancelAllOrders;
use dodex_contracts::dex::private_note::ParamsOfMergeFullSet;
use dodex_contracts::dex::private_note::ParamsOfPlaceBatch;

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
use crate::common::market::resolve_and_drain;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Long enough for a batch, a fill, two cancellation passes, a split and a
/// merge, with the idle remainder spent waiting for the market to close.
const STAKE_PERIOD_MM: u64 = 420;

const OUTCOME: u32 = 0;
const OTHER_OUTCOME: u32 = 1;
const FULL_PERCENT: u128 = 10_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// The quote: three bids under three asks, none of them crossing the other.
const BID_BPS: [u128; 3] = [5_000, 4_900, 4_800];
const ASK_BPS: [u128; 3] = [6_000, 6_100, 6_200];

/// Each quote's size. Sized off the *worst* bid: every order has to clear the
/// 10 NACKL minimum notional on its own, and the widest quote is the one that
/// nearly does not.
const QUOTE_AMOUNT: u128 = 25_000_000_000;

const _: () = assert!(BID_BPS[0] < ASK_BPS[0], "the maker must not cross itself");
const _: () = assert!(QUOTE_AMOUNT * BID_BPS[2] / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
const _: () = assert!(QUOTE_AMOUNT * ASK_BPS[0] / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// What the taker lifts off the best ask — less than it holds, so the maker is
/// left with a partially filled order rather than a clean book, and still
/// enough to be a valid order of its own.
const TAKE_AMOUNT: u128 = 20_000_000_000;

const _: () = assert!(TAKE_AMOUNT < QUOTE_AMOUNT, "the fill has to leave a remainder");
const _: () = assert!(TAKE_AMOUNT * ASK_BPS[0] / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

/// What the best ask keeps afterwards.
const ASK_REMAINING: u128 = QUOTE_AMOUNT - TAKE_AMOUNT;

/// Collateral the maker splits for inventory, and again for the round trip.
const SPLIT_COLLATERAL: u128 = 300_000_000_000;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_maker_quotes_gets_taken_unwinds_and_settles_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "mm_cycle").expect("rent the deployer note");
    let maker = alloc.rent(PnProfile::Trd, "mm_cycle").expect("rent the maker note");
    let taker = alloc.rent(PnProfile::Trd, "mm_cycle").expect("rent the taker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market = deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_MM).await;

    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;
    let inventory = outcome_tokens(dex, &maker).await;
    assert!(
        at(&inventory, OUTCOME) >= 3 * QUOTE_AMOUNT,
        "the split left the maker {} outcome-{OUTCOME} tokens, short of the {} its asks need",
        at(&inventory, OUTCOME),
        3 * QUOTE_AMOUNT
    );

    let base = nonce as u128 * 10;
    let bids: Vec<u128> = (0..3).map(|i| base + 1 + i as u128).collect();
    let asks: Vec<u128> = (0..3).map(|i| base + 4 + i as u128).collect();

    // ── the quote ─────────────────────────────────────────────────────────
    let mut quote = Vec::new();
    for (i, cid) in bids.iter().enumerate() {
        quote.push(order_at(true, BID_BPS[i], *cid));
    }
    for (i, cid) in asks.iter().enumerate() {
        quote.push(order_at(false, ASK_BPS[i], *cid));
    }
    dex.place_batch(
        &maker.note.address,
        ParamsOfPlaceBatch {
            event_id: market.key.event_id.clone(),
            oracle_list_hash: market.key.oracle_list_hash.clone(),
            token_type: market.key.token_type,
            orders: quote,
            cancel_ids: Vec::new(),
        },
        Signer::Keys { keys: maker.note.keys.clone() },
    )
    .await
    .expect("place_batch");
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, asks[2], true).await;
    wait_not_busy(dex, &maker.note.address, "a two-sided quote").await;

    let resting = owner_orders(dex, &market.order_book, &maker.note.dih_dec).await;
    assert_eq!(
        resting.len(),
        bids.len() + asks.len(),
        "the batch left {} of its {} orders on the book",
        resting.len(),
        bids.len() + asks.len()
    );
    assert_eq!(
        open_orders(&r, &maker.note.address).await as usize,
        bids.len() + asks.len(),
        "the note's own count disagrees with the book about how many orders it has"
    );

    // ── taken on the best ask ─────────────────────────────────────────────
    let take_coid = base + 20;
    place_limit(
        dex,
        &taker,
        &market.key,
        OUTCOME,
        true,
        &ASK_BPS[0].to_string(),
        TAKE_AMOUNT,
        take_coid,
    )
    .await;
    poll_until("the taker never reached the best ask", || async {
        order_size(dex, &market.order_book, &maker.note.dih_dec, asks[0]).await
            != Some(QUOTE_AMOUNT)
    })
    .await;
    wait_not_busy(dex, &taker.note.address, "a taker lifting the best ask").await;
    wait_not_busy(dex, &maker.note.address, "the best ask filling").await;

    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, asks[0]).await,
        Some(ASK_REMAINING),
        "the partially lifted ask does not hold what the fill left it"
    );

    // ── one quote withdrawn by name ───────────────────────────────────────
    //
    // The maker names a client id of its own choosing; the book knows the
    // order by a different id entirely, and the translation between them is
    // the note's. An order cancelled by the wrong translation would take
    // someone else's quote off the book — hence the reading that the *others*
    // are still there.
    let named = bids[1];
    let locked_before_named = pn_locked(&r, &maker.note.address).await;
    cancel_by_client(dex, &maker, &market.key, named).await;
    poll_until("the named quote never left the book", || async {
        order_size(dex, &market.order_book, &maker.note.dih_dec, named).await.is_none()
    })
    .await;

    let after_named = owner_orders(dex, &market.order_book, &maker.note.dih_dec).await;
    assert!(!after_named.contains_key(&named), "the named quote is still resting");
    for cid in bids.iter().chain(asks.iter()).filter(|c| **c != named) {
        assert!(
            after_named.contains_key(cid),
            "cancelling {named} by name took quote {cid} with it"
        );
    }
    assert!(
        pn_locked(&r, &maker.note.address).await < locked_before_named,
        "the cancelled quote's collateral is still escrowed"
    );

    // ── the rest withdrawn wholesale ──────────────────────────────────────
    cancel_all(dex, &maker, &market).await;
    poll_until("the book still holds quotes after a full cancellation", || async {
        owner_orders(dex, &market.order_book, &maker.note.dih_dec).await.is_empty()
    })
    .await;
    wait_not_busy(dex, &maker.note.address, "cancelling every quote").await;

    assert_eq!(
        open_orders(&r, &maker.note.address).await,
        0,
        "the note still counts open orders after cancelling all of them"
    );
    assert_eq!(
        pn_locked(&r, &maker.note.address).await,
        0,
        "collateral is still escrowed against orders that no longer exist"
    );

    // ── and again, with nothing to cancel ─────────────────────────────────
    //
    // The note latches `_pendingBatchActive` before it asks and only the
    // book's acknowledgement releases it. A book that says nothing when it
    // has nothing to cancel would leave the note permanently mid-batch, and
    // the next operation is what would discover it — so the next operation is
    // the assertion.
    cancel_all(dex, &maker, &market).await;
    wait_not_busy(dex, &maker.note.address, "cancelling nothing").await;
    assert!(
        !note_busy(dex, &maker.note.address).await,
        "the note is still busy after asking a book with nothing to cancel"
    );

    // ── inventory back into collateral ────────────────────────────────────
    //
    // A split takes collateral and mints a basket; a merge burns the basket
    // and returns the collateral. Read as a round trip on the *difference* a
    // fresh split makes — everything the note held before it has been traded
    // against and is not a whole number of baskets any more.
    let basket = invariant::pmp_split_merge_q(&r, &market.pmp).await.expect("read the basket");
    let baskets = SPLIT_COLLATERAL / basket;
    assert!(baskets > 0, "{SPLIT_COLLATERAL} does not cover one basket of {basket}");
    let round_trip = baskets * basket;

    let free_before_round = pn_balance(&r, &maker.note.address).await;
    let tokens_before_round = outcome_tokens(dex, &maker).await;
    split_full_set(dex, &maker, &market.key, round_trip).await;
    let tokens_after_split = outcome_tokens(dex, &maker).await;

    let minted: Vec<u128> = (0..tokens_after_split.len())
        .map(|k| at(&tokens_after_split, k as u32) - at(&tokens_before_round, k as u32))
        .collect();
    assert!(
        minted.iter().any(|m| *m > 0),
        "the split minted nothing, so the merge below has nothing to give back"
    );

    dex.merge_full_set(
        &maker.note.address,
        ParamsOfMergeFullSet {
            event_id: market.key.event_id.clone(),
            oracle_list_hash: market.key.oracle_list_hash.clone(),
            token_type: market.key.token_type,
            amount: minted.clone(),
        },
        Signer::Keys { keys: maker.note.keys.clone() },
    )
    .await
    .expect("merge_full_set");
    wait_not_busy(dex, &maker.note.address, "merging the basket back").await;

    assert_eq!(
        pn_balance(&r, &maker.note.address).await,
        free_before_round,
        "splitting {round_trip} and merging the same basket back did not return what it cost"
    );
    let tokens_after_merge = outcome_tokens(dex, &maker).await;
    for outcome in [OUTCOME, OTHER_OUTCOME] {
        assert_eq!(
            at(&tokens_after_merge, outcome),
            at(&tokens_before_round, outcome),
            "the round trip left the maker holding a different amount of outcome {outcome} than \
             it started with"
        );
    }

    // ── settlement ────────────────────────────────────────────────────────
    let claim_before = pn_balance(&r, &maker.note.address).await;
    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, OUTCOME).await;
    wait_not_busy(dex, &maker.note.address, "the drain reaching the maker").await;

    dex.claim(
        &maker.note.address,
        market.key.clone(),
        Signer::Keys { keys: maker.note.keys.clone() },
    )
    .await
    .expect("claim");
    wait_not_busy(dex, &maker.note.address, "claiming the maker's position").await;

    assert!(
        pn_balance(&r, &maker.note.address).await > claim_before,
        "the maker held winning outcome tokens and was paid nothing for them"
    );
    assert!(
        dex.get_stakes(&maker.note.address).await.expect("stakes").stakes.is_empty(),
        "the claim left the maker's stake record behind"
    );

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// One quote of the ladder, with the fields this scenario never varies pinned.
fn order_at(is_buy: bool, price_bps: u128, client_order_id: u128) -> OrderBookOrder {
    OrderBookOrder {
        outcome_id: OUTCOME,
        is_buy,
        flags: 0,
        price: price_bps.to_string(),
        amount: QUOTE_AMOUNT,
        min_amount: 0,
        epoch_id: 0,
        client_order_id,
    }
}

async fn cancel_all(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    dex.cancel_all_orders(
        &note.note.address,
        ParamsOfCancelAllOrders {
            event_id: market.key.event_id.clone(),
            oracle_list_hash: market.key.oracle_list_hash.clone(),
            token_type: market.key.token_type,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("cancel_all_orders");
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

async fn order_size(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> Option<u128> {
    owner_orders(dex, ob_addr, deposit_identifier_hash).await.get(&client_order_id).copied()
}

async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn pn_locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn open_orders(r: &chain_reader::ChainReader, pn_address: &str) -> u32 {
    invariant::pn_open_order_count(r, pn_address).await.expect("read open order count")
}

async fn note_busy(dex: &dodex_sdk::Dex, pn_address: &str) -> bool {
    dex.get_private_note_details(pn_address).await.expect("pn details").busy_address.is_some()
}
