//! A note that sent money to something that was not there gets it back, and
//! can still be used afterwards.
//!
//! Every operation a note performs on another contract is fire-and-forget: it
//! debits itself, sets `_busy` to the counterparty and sends. If that
//! counterparty does not exist the message bounces, and `onBounce` is the only
//! thing standing between the owner and a note that is both poorer and
//! permanently locked — `_busy` gates every subsequent operation, so a note
//! whose bounce was mishandled is not merely out of pocket, it is bricked.
//!
//! Nothing exercised that path. Every scenario so far addressed live
//! counterparties, which is the case where `onBounce` never runs.
//!
//! ## Two ops, two branches
//!
//! `onBounce` dispatches on which operation was in flight, and the two cheap
//! ones need no market at all:
//!
//! 1. **`initTransfer` to a note that does not exist.** The transfer branch
//!    restores `_balance` and clears `_busy`, and deliberately does not touch
//!    `_lockedInOrders` — a transfer moves owned tokens, not order collateral.
//! 2. **`setStake` against a market that does not exist.** A different branch:
//!    it restores the balance out of the stake's `candidateAmount` and clears
//!    `_busyOpNonce` as well.
//!
//! ## Why the second op is also the proof of the first
//!
//! Asserting "the note still works" after a bounce is where this kind of test
//! usually says nothing. `setStake` refuses outright when `_busy` is set, and
//! a refusal before `tvm.accept()` leaves no trace — so a stuck note and a
//! healthy one that simply restored its balance look identical if you only
//! read the balance.
//!
//! What separates them is that a `setStake` which *was* accepted leaves a
//! `_stakes` entry behind even when it bounces (the branch zeroes
//! `candidateAmount` and writes the record back), while one refused by a stuck
//! `_busy` leaves `_stakes` empty. So the second phase both tests its own
//! branch and proves the first phase really did unlock the note.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`. Needs no
//! market, no deployer and no preflight — the counterparties it addresses are
//! chosen precisely because nothing is there.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfInitTransfer;
use dodex_contracts::dex::private_note::ParamsOfSetStake;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::misc::wait_not_busy;

/// A deposit hash no note in any pool holds. The seed file numbers its notes
/// from zero, so anything this size derives an address that has never been
/// deployed — which is the whole point.
const ABSENT_NOTE_DIH: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639900";

/// An event id no oracle ever published, so the market address derived from it
/// holds no account either.
const ABSENT_EVENT_ID: &str = "999999999999999999999999999999999999999";
const ABSENT_ORACLE_LIST_HASH: &str = "888888888888888888888888888888888888888";

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_bounced_operation_gives_the_money_back_and_unlocks_the_note_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let _b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let note = alloc.rent(PnProfile::Trd, "bounce_recovery").expect("rent a note");
    let addr = note.note.address.clone();

    let balance_before = pn_balance(&r, &addr).await;
    assert!(
        balance_before >= STAKE_AMOUNT,
        "the note holds {balance_before}, too little to send {STAKE_AMOUNT} anywhere"
    );
    assert!(
        dex.get_stakes(&addr).await.expect("pn stakes").stakes.is_empty(),
        "the note already holds a stake record, so the discriminator this scenario relies on \
         would be true before it started"
    );

    // ── 1. transfer to a note that is not there ───────────────────────────
    dex.init_transfer(
        &addr,
        ParamsOfInitTransfer {
            dest_deposit_hash: ABSENT_NOTE_DIH.to_string(),
            token_type: TOKEN_TYPE_NACKL,
            amount: STAKE_AMOUNT,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("init_transfer");

    // The barrier is the recovery itself: `_busy` set and then cleared again.
    // Waiting on the balance would be waiting on what the assertion checks.
    wait_not_busy(dex, &addr, "the bounced transfer").await;

    // The discriminator for this phase. A transfer the note *accepted* latches
    // `_hasTransferred` on its way out, and the latch survives the bounce;
    // one refused by a `require` before `tvm.accept()` leaves nothing behind
    // at all — and the balance reading below would then be exactly as true.
    assert!(
        invariant::pn_has_transferred(&r, &addr).await.expect("read the transfer latch"),
        "the note never accepted the transfer, so nothing bounced and the balance below is \
         unchanged for the wrong reason"
    );
    assert_eq!(
        pn_balance(&r, &addr).await,
        balance_before,
        "the bounced transfer did not return the note's collateral"
    );

    // ── 2. stake into a market that is not there ──────────────────────────
    dex.set_stake(
        &addr,
        ParamsOfSetStake {
            event_id: ABSENT_EVENT_ID.to_string(),
            oracle_list_hash: ABSENT_ORACLE_LIST_HASH.to_string(),
            token_type: TOKEN_TYPE_NACKL,
            outcome: 0,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("set_stake");

    wait_not_busy(dex, &addr, "the bounced stake").await;

    // The discriminator. A `setStake` that reached the note leaves this
    // record behind even though it bounced; one refused by a `_busy` the
    // first bounce failed to clear would have left `_stakes` untouched —
    // and the balance reading below would then be equally true of a note
    // that never did anything at all.
    let stakes = dex.get_stakes(&addr).await.expect("pn stakes").stakes;
    assert!(
        !stakes.is_empty(),
        "the note has no stake record, so the second operation never took effect — the first \
         bounce left it locked"
    );
    assert_eq!(
        pn_balance(&r, &addr).await,
        balance_before,
        "the bounced stake did not return the note's collateral"
    );

    // `initTransfer` latches `_hasTransferred`, which the pool sweep counts as
    // dirty for good — correctly, since a note that has moved value out is not
    // interchangeable with a fresh one. So this note is spent, whatever the
    // outcome above.
    note.taint(allocator::TaintReason::HasTransferred);
}

/// The note's free NACKL — `_balance`, without what its resting orders hold.
async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}
