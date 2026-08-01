//! One note in two markets at once, and the two places that arrangement can
//! go wrong.
//!
//! Every scenario so far has put a note in exactly one market, which hides a
//! whole class of collision: the note keeps per-order bookkeeping, and each
//! order book numbers its own orders from one. Two markets therefore hand the
//! same note an order with the same id as a matter of course — the first order
//! on any book is order 1 — and everything the note remembers about an order
//! has to be keyed by the book as well as the id, or the second market
//! overwrites the first's record and one of the two locks is lost.
//!
//! The other collision is the reverse: state that is *supposed* to be shared
//! being scoped too tightly, or state that is supposed to be per-market being
//! shared. A claim is gated on the note having no open orders — but on **that
//! market's** orders, not on any order anywhere. Scoped to the note, a maker
//! quoting one market could never settle another.
//!
//! ## What it does
//!
//! Two markets, one note staking in both — which is already a reading: the
//! stake records are keyed by market and there have to be two of them.
//!
//! Then a resting buy on each book, both of them the first order their book
//! has seen, so both are order 1. Both have to rest, the note's escrow has to
//! hold the sum of the two locks, and cancelling one has to return exactly
//! that one's lock and leave the other order untouched. A flat key would
//! have made the second placement overwrite the first, and the cancellation
//! would then have released the wrong figure — or nothing.
//!
//! Last, the first market resolves while the order on the second is still
//! resting, and the note claims. That claim is what says the gate is
//! per-market: with the two confused, a note with a live quote anywhere could
//! never be paid out anywhere.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::cancel_by_client;
use crate::common::market::freeze_prepared_market;
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::resolve_and_drain;
use crate::common::market::stake_amount;
use crate::common::market::wait_owner_order;
use crate::common::market::EphemeralMarket;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// The market that resolves inside the test, and the one that has to outlive
/// it with an order still resting on its book.
const SHORT_PERIOD: u64 = 260;
const LONG_PERIOD: u64 = 560;

const _: () = assert!(LONG_PERIOD > SHORT_PERIOD + 120, "the second market has to outlast the first");

const OUTCOME: u32 = 0;
const FULL_PERCENT: u128 = 10_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// What the note stakes in each market.
const STAKE: u128 = 20_000_000_000;

/// The bid it rests on each book. Deliberately different prices, so the two
/// locks differ and "the escrow holds both" is not satisfied by holding one of
/// them twice.
const BID_A_BPS: u128 = 6_000;
const BID_B_BPS: u128 = 5_000;
const BID_AMOUNT: u128 = 25_000_000_000;

const LOCK_A: u128 = BID_AMOUNT * BID_A_BPS / FULL_PERCENT;
const LOCK_B: u128 = BID_AMOUNT * BID_B_BPS / FULL_PERCENT;

const _: () = assert!(LOCK_A != LOCK_B, "two equal locks would not tell one from two");
const _: () = assert!(LOCK_A >= MIN_ORDER_NOTIONAL && LOCK_B >= MIN_ORDER_NOTIONAL);
const _: () = assert!(STAKE.is_multiple_of(10_000_000) && BID_AMOUNT.is_multiple_of(10_000_000));

/// The id every book gives its first order. Both markets are fresh, and
/// nothing else places into them, so this is what makes the two records
/// collide if they are not keyed by book.
const FIRST_ORDER_ID: u128 = 1;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn one_note_in_two_markets_keeps_them_apart_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let dep_a = alloc.rent(PnProfile::Dep, "multi_market").expect("rent the first creator");
    let dep_b = alloc.rent(PnProfile::Dep, "multi_market").expect("rent the second creator");
    let trader = alloc.rent(PnProfile::Trd, "multi_market").expect("rent the trading note");

    // Both markets are brought to their open staking window before either is
    // frozen, so the note can stake in both and the two windows overlap
    // instead of running end to end.
    let nonce_a = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prep_a = prepare_ephemeral_market(ctx, dex, &b0, &dep_a, nonce_a, SHORT_PERIOD).await;
    let nonce_b = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prep_b = prepare_ephemeral_market(ctx, dex, &b0, &dep_b, nonce_b, LONG_PERIOD).await;

    stake_amount(dex, &trader, &prep_a.key, OUTCOME, STAKE, false).await;
    stake_amount(dex, &trader, &prep_b.key, OUTCOME, STAKE, false).await;

    // The first reading, and the cheapest: two markets, two records. Keyed by
    // anything less than the market, the second stake would have landed on
    // top of the first.
    let stakes = dex.get_stakes(&trader.note.address).await.expect("stakes").stakes;
    assert_eq!(
        stakes.len(),
        2,
        "the note holds {} stake record(s) after staking in two markets",
        stakes.len()
    );

    let market_a = freeze_prepared_market(ctx, dex, prep_a).await;
    let market_b = freeze_prepared_market(ctx, dex, prep_b).await;

    // ── the same order id on two books ────────────────────────────────────
    let base = nonce_a as u128 * 10;
    let (cid_a, cid_b) = (base + 1, base + 2);
    let locked_before = pn_locked(&r, &trader.note.address).await;

    rest_bid(dex, &trader, &market_a, BID_A_BPS, cid_a).await;
    rest_bid(dex, &trader, &market_b, BID_B_BPS, cid_b).await;

    let id_a = order_id(dex, &market_a.order_book, &trader.note.dih_dec, cid_a).await;
    let id_b = order_id(dex, &market_b.order_book, &trader.note.dih_dec, cid_b).await;
    assert_eq!(
        (id_a, id_b),
        (Some(FIRST_ORDER_ID), Some(FIRST_ORDER_ID)),
        "the two books did not both number their first order {FIRST_ORDER_ID} ({id_a:?} and \
         {id_b:?}), so this arrangement is not the collision it is meant to be"
    );

    // Both locks are held at once. A record keyed by id alone would have kept
    // one of the two.
    assert_eq!(
        pn_locked(&r, &trader.note.address).await,
        locked_before + LOCK_A + LOCK_B,
        "the note is not escrowing both bids at once"
    );
    assert_eq!(
        open_orders(&r, &trader.note.address).await,
        2,
        "the note does not count both of its orders"
    );

    // ── and cancelling one of them ────────────────────────────────────────
    //
    // This is where a shared key would show: the cancellation names the note's
    // own client id, the note translates it into a book and an order id, and
    // the amount it releases comes from the record under that pair.
    cancel_by_client(dex, &trader, &market_a.key, cid_a).await;
    poll_until("the cancelled bid never left the first book", || async {
        order_id(dex, &market_a.order_book, &trader.note.dih_dec, cid_a).await.is_none()
    })
    .await;

    assert_eq!(
        pn_locked(&r, &trader.note.address).await,
        locked_before + LOCK_B,
        "cancelling the first market's bid released something other than exactly its own lock"
    );
    assert_eq!(
        order_size(dex, &market_b.order_book, &trader.note.dih_dec, cid_b).await,
        Some(BID_AMOUNT),
        "cancelling an order on one book disturbed the order sharing its id on the other"
    );

    // ── settling one market with a live quote in the other ────────────────
    wait_until(market_a.result_start).await;
    resolve_and_drain(dex, &market_a.pmp, &market_a.oracle, OUTCOME).await;
    wait_not_busy(dex, &trader.note.address, "the first market's drain").await;

    // Still resting, and still counted: the drain of one market must not have
    // reached into the other.
    assert_eq!(
        order_size(dex, &market_b.order_book, &trader.note.dih_dec, cid_b).await,
        Some(BID_AMOUNT),
        "one market's shutdown cancelled an order belonging to another"
    );
    assert_eq!(
        open_orders(&r, &trader.note.address).await,
        1,
        "the note's open-order count is wrong with one market drained and one still quoting"
    );

    let before_claim = pn_balance(&r, &trader.note.address).await;
    dex.claim(
        &trader.note.address,
        market_a.key.clone(),
        Signer::Keys { keys: trader.note.keys.clone() },
    )
    .await
    .expect("claim");
    wait_not_busy(dex, &trader.note.address, "claiming the settled market").await;

    // The claim went through with an order still open elsewhere, which is the
    // whole point: the gate counts this market's orders, not the note's.
    assert!(
        pn_balance(&r, &trader.note.address).await > before_claim,
        "the note was not paid for its winning stake while it still had a quote in another market"
    );
    assert_eq!(
        dex.get_stakes(&trader.note.address).await.expect("stakes").stakes.len(),
        1,
        "the claim on one market did not leave exactly the other market's stake behind"
    );
    assert_eq!(
        order_size(dex, &market_b.order_book, &trader.note.dih_dec, cid_b).await,
        Some(BID_AMOUNT),
        "claiming one market took the other's order with it"
    );

    trader.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    dep_a.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    dep_b.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Rest a bid on one market's book and wait for it to be there.
async fn rest_bid(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &EphemeralMarket,
    price_bps: u128,
    client_order_id: u128,
) {
    crate::common::market::place_limit(
        dex,
        note,
        &market.key,
        OUTCOME,
        true,
        &price_bps.to_string(),
        BID_AMOUNT,
        client_order_id,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, client_order_id, true).await;
    wait_not_busy(dex, &note.note.address, "resting a bid").await;
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

async fn order_size(
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
        .map(|o| o.amount)
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
