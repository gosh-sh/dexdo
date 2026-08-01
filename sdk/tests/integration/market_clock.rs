//! Everything a market stops accepting once its clock has moved on.
//!
//! A market is a sequence of windows — stakes, then trading, then results —
//! and each of them closes. Every scenario in the suite has stayed politely
//! inside them, so what has never run is the other side of any deadline: a
//! stake that arrives late, an order sent after the book has closed, a claim
//! made before there is anything to claim, a second claim after the first.
//!
//! The reason that gap is worth closing is not that late calls should fail
//! loudly — they cannot fail loudly at all. Every one of these guards is a
//! `require` reached after `tvm.accept()`, so a note that sends one gets no
//! answer and a caller that waits for one waits forever. What a late call
//! leaves is the absence of its effect, and the absence of an effect is also
//! what a message that never arrived leaves. The two are told apart the same
//! way the rest of the suite tells them apart: an operation of the same kind,
//! sent at a moment when it *is* allowed, has to work.
//!
//! ## The order of the windows
//!
//! One market, walked from one end to the other, with a refused call at each
//! boundary and a permitted one either side of it:
//!
//! 1. **A stake inside the window** is taken.
//! 2. **A stake after `stakeEnd`** is not. The staking window is a tenth of a
//!    market's life, so this needs nothing but patience.
//! 3. **A stake after the freeze** is not either — a different guard, reached
//!    on a market that has already snapshotted its pools.
//! 4. **An order inside the trading window** rests.
//! 5. **An order after `resultStart`** does not. The book is closed for
//!    business from that moment, before anything has resolved.
//! 6. **A claim with the book drained and the market still unresolved** pays
//!    nothing. The two conditions on a claim are separate and only one is
//!    met. Getting there is itself part of the shape: the drain is wired into
//!    the market's own balance check rather than being a call anyone makes,
//!    so the first touch after `resultStart` starts it — and that touch
//!    cannot come from the note, which is still holding the order the drain
//!    is about to cancel and whose claim gate refuses while it does.
//! 7. **The resolve**, and a claim that is finally paid.
//! 8. **A second claim** pays nothing: the note deleted its own record on the
//!    first, so there is no longer anything to present.
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
use crate::common::market::freeze_prepared_market;
use crate::common::market::place_limit;
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::resolve_and_drain;
use crate::common::market::stake_amount;
use crate::common::market::wait_owner_order;
use crate::common::misc::now_unix;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Short, because most of this scenario is spent waiting for windows to
/// close rather than for anything to happen inside them. The staking window
/// is a tenth of this, which is the deadline the early phases race.
const STAKE_PERIOD_CLOCK: u64 = 300;

const OUTCOME: u32 = 0;
const FULL_PERCENT: u128 = 10_000;
const LOT: u128 = 10_000_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// What the note stakes, and what it tries to stake again once it is too
/// late. The same figure both times: the only thing that differs between the
/// call that works and the one that does not is when it is sent.
const STAKE: u128 = 20_000_000_000;

const _: () = assert!(STAKE.is_multiple_of(LOT));

/// The order the note rests while the book is open, and tries to place again
/// once it has closed — same size, same price, same reasoning.
const ORDER_BPS: u128 = 5_000;
const ORDER_AMOUNT: u128 = 25_000_000_000;

const _: () = assert!(ORDER_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(ORDER_AMOUNT * ORDER_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_stops_accepting_things_as_its_windows_close_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "market_clock").expect("rent the creator note");
    let note = alloc.rent(PnProfile::Trd, "market_clock").expect("rent the staking note");
    let latecomer = alloc.rent(PnProfile::Trd, "market_clock").expect("rent the late note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared =
        prepare_ephemeral_market(ctx, dex, &b0, &creator, nonce, STAKE_PERIOD_CLOCK).await;

    // ── inside the staking window ─────────────────────────────────────────
    assert!(
        now_unix() < prepared.stake_end,
        "the staking window closed at {} before anything could be staked into it",
        prepared.stake_end
    );
    stake_amount(dex, &note, &prepared.key, OUTCOME, STAKE, false).await;
    assert_eq!(
        stake_count(dex, &note).await,
        1,
        "a stake sent inside the window was not taken, so nothing below can mean what it says"
    );

    // ── and after it ──────────────────────────────────────────────────────
    //
    // The same call from a note that did nothing wrong except be late. It has
    // its own record to prove the difference: the one above exists, this one
    // never appears.
    wait_until(prepared.stake_end).await;
    let late_balance = free(&r, &latecomer.note.address).await;
    stake_amount(dex, &latecomer, &prepared.key, OUTCOME, STAKE, false).await;
    assert_eq!(
        stake_count(dex, &latecomer).await,
        0,
        "a stake sent after the window closed was taken anyway"
    );
    assert_eq!(
        free(&r, &latecomer.note.address).await,
        late_balance,
        "a stake the market refused still cost the note its collateral"
    );

    // ── and after the freeze ──────────────────────────────────────────────
    //
    // A different guard on the same call: the market has snapshotted its
    // pools by now, and a stake would change what the snapshot said.
    let market = freeze_prepared_market(ctx, dex, prepared).await;
    stake_amount(dex, &latecomer, &market.key, OUTCOME, STAKE, false).await;
    assert_eq!(
        stake_count(dex, &latecomer).await,
        0,
        "a frozen market took a stake"
    );

    // ── inside the trading window ─────────────────────────────────────────
    let base = nonce as u128 * 10;
    let (early_cid, late_cid) = (base + 1, base + 2);

    place_limit(dex, &note, &market.key, OUTCOME, true, &ORDER_BPS.to_string(), ORDER_AMOUNT, early_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, early_cid, true).await;
    wait_not_busy(dex, &note.note.address, "an order inside the trading window").await;

    // ── and after the book closes ─────────────────────────────────────────
    //
    // `resultStart` shuts the book before anything has resolved, so this is
    // the deadline on trading rather than on the market. Same order, same
    // note, later.
    wait_until(market.result_start).await;
    let locked_before_late = locked(&r, &note.note.address).await;
    place_limit(dex, &note, &market.key, OUTCOME, true, &ORDER_BPS.to_string(), ORDER_AMOUNT, late_cid)
        .await;
    wait_not_busy(dex, &note.note.address, "an order after the book closed").await;

    assert!(
        order_absent(dex, &market.order_book, &note.note.dih_dec, late_cid).await,
        "the book took an order after its own deadline"
    );
    assert_eq!(
        locked(&r, &note.note.address).await,
        locked_before_late,
        "an order the closed book refused still escrowed collateral"
    );

    // ── a claim with the book done and no answer yet ─────────────────────
    //
    // The drain is not a call anyone makes: it is wired into the PMP's own
    // balance check, so the first thing to touch the market after
    // `resultStart` starts it. That cannot be this note — it is still holding
    // the order the drain is about to cancel, and its own claim gate refuses
    // while any of its orders are open. The creator does it instead, with a
    // claim of its own that is refused for the same reason as everything else
    // here and triggers the drain on its way through.
    claim(dex, &creator, &market).await;
    crate::common::market::wait_order_book_done(dex, &market.pmp).await;
    wait_not_busy(dex, &note.note.address, "the drain reaching the note").await;

    assert!(
        order_absent(dex, &market.order_book, &note.note.dih_dec, early_cid).await,
        "the drain left the note's order on a book that has finished shutting down"
    );

    // Now the note is past one of a claim's two conditions and short of the
    // other: the book is done, the market has not resolved. It pays nothing,
    // and — the reading that matters more — it does not consume the record,
    // which the real claim below needs to still be there.
    let before_early_claim = free(&r, &note.note.address).await;
    claim(dex, &note, &market).await;
    assert_eq!(
        free(&r, &note.note.address).await,
        before_early_claim,
        "a claim made before the market resolved paid something"
    );
    assert_eq!(
        stake_count(dex, &note).await,
        1,
        "a claim that paid nothing consumed the stake record anyway, so the real claim has \
         nothing left to present"
    );

    // ── the resolve, and the claim that works ─────────────────────────────
    resolve_and_drain(dex, &market.pmp, &market.oracle, OUTCOME).await;
    wait_not_busy(dex, &note.note.address, "the drain reaching the note").await;

    let before_real_claim = free(&r, &note.note.address).await;
    claim(dex, &note, &market).await;
    let paid = free(&r, &note.note.address).await - before_real_claim;
    assert!(paid > 0, "the winning stake was paid nothing once the market had resolved");
    assert_eq!(
        stake_count(dex, &note).await,
        0,
        "the paid claim left the record behind"
    );

    // ── and the second one ────────────────────────────────────────────────
    //
    // Refused by the note rather than by the market: it deleted its own record
    // on the way out, and has nothing left to present.
    let after_paid = free(&r, &note.note.address).await;
    claim(dex, &note, &market).await;
    assert_eq!(
        free(&r, &note.note.address).await,
        after_paid,
        "claiming a second time paid a second time"
    );

    note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    latecomer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

async fn claim(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    let _ = dex
        .claim(&note.note.address, market.key.clone(), Signer::Keys { keys: note.note.keys.clone() })
        .await;
    wait_not_busy(dex, &note.note.address, "claim").await;
}

async fn stake_count(dex: &dodex_sdk::Dex, note: &allocator::LeasedPn) -> usize {
    dex.get_stakes(&note.note.address).await.expect("stakes").stakes.len()
}

async fn order_absent(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> bool {
    !dex.get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner")
        .orders
        .iter()
        .any(|o| o.client_order_id == client_order_id)
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
