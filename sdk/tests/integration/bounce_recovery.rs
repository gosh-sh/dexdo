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
//! What separates them is `_lastHash`. `setStake` writes it on its way out,
//! naming the stake the operation is about, and nothing clears it — the bounce
//! handler itself reads it to find the record it is restoring. So a
//! `_lastHash` that changed is proof the note accepted the operation, and the
//! second phase both tests its own branch and proves the first really did
//! unlock the note.
//!
//! The `_stakes` record is emphatically *not* that proof, though it looks like
//! it should be. A `setStake` that bounces on a note holding no confirmed
//! position leaves `stake.amount` all zero, and `onBounce` deletes the record
//! outright — so "the record is there" is what a correct recovery does **not**
//! produce here. An earlier version of this scenario asserted exactly that and
//! failed against a contract that was behaving correctly.
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
    // Read before sending: the discriminator is that this changes, not that it
    // holds any particular value. A note comes out of the pool sweep-clean,
    // but `_lastHash` is not one of the fields the sweep requires to be empty,
    // so a recycled note can arrive already carrying one.
    let last_hash_before =
        invariant::pn_last_stake_hash(&r, &addr).await.expect("read the last-stake trace");

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

    // The discriminator. `setStake` names the stake it is about in
    // `_lastHash` on its way out and nothing clears it, so a change here is
    // proof the note accepted the operation; one refused by a `_busy` the
    // first bounce failed to clear leaves it exactly as it was — and the
    // balance reading below would then be equally true of a note that never
    // did anything at all.
    //
    // Not the `_stakes` record: a bounced first stake leaves `stake.amount`
    // all zero and `onBounce` deletes the record, so its absence is what a
    // correct recovery looks like. See the module header.
    let last_hash_after =
        invariant::pn_last_stake_hash(&r, &addr).await.expect("read the last-stake trace");
    assert_ne!(
        last_hash_after, last_hash_before,
        "the note never accepted the stake, so the first bounce left it locked and the balance \
         below is unchanged for the wrong reason"
    );
    assert!(
        dex.get_stakes(&addr).await.expect("pn stakes").stakes.is_empty(),
        "the bounced stake left a record behind; with nothing confirmed on it, `onBounce` should \
         have deleted it rather than leaving an empty one for the owner to trip over"
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
