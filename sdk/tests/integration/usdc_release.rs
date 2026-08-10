//! The release half of USDC custody, against a from-scratch local stand.
//!
//! Every other scenario in this suite moves NACKL. USDC (token type 3) is
//! carried by `RootPN` through the same generic paths — `privateNoteDeployed`
//! books `_deployedValues[tokenType]` for any id, and `withdrawTokens` gates
//! on `currencies[tt]` and `_deployedValues[tt]` without naming a type — but
//! "the code does not special-case it" is an argument, not a test. What has
//! never run is a withdraw: the stand bakes USDC into a note's balance, and
//! until something takes it out again, the release path is asserted by
//! nobody.
//!
//! The asymmetry matters because the two halves fail differently. Custody
//! failing is loud — the note never deploys, or the pool never balances, and
//! the preflight says so before any scenario starts. Release failing is
//! quiet: the money leaves the note's books and simply does not arrive, and
//! nothing in a suite that only ever deposits would notice.
//!
//! ## What it asserts
//!
//! One withdraw, five equalities, all exact:
//!
//! - the note's `_balance[3]` is gone, and its physical SHELL pool with it —
//!   `withdrawTokens` attaches the whole pool to the same message, so a
//!   withdrawn note that kept any could still fund an order;
//! - `RootPN._deployedValues[3]` and RootPN's physical `currencies[3]` each
//!   fall by exactly the withdrawn amount — bookkeeping and backing move
//!   together or the custody model is broken;
//! - the destination gains exactly that amount.
//!
//! ## Why the destination is another leased note
//!
//! It has to be an account in RootPN's own dApp: `walletAddr.transfer` in
//! `RootPN.withdrawTokens` carries no `dest_dapp_id`, so the transfer stays
//! where the sender is, and an address that exists in some other dApp is not
//! the account that would receive it. Every `PrivateNote` qualifies, exists
//! in the zerostate, and — once leased — is the one kind of account no
//! concurrent process may touch, which is what lets the assertion be an
//! equality rather than a lower bound. `PrivateNote.receive()` takes a plain
//! transfer and does nothing else with it.
//!
//! The sink's `_balance` is untouched by this: it gains physical currency,
//! not custodied bookkeeping. `RootPN._deployedValues[3]` and the sum of
//! every note's `_balance[3]` therefore both end at zero, so the invariant
//! the preflight checks still holds after the scenario.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`. Unlike
//! `proof_money` it takes no manifest and no preflight, so it neither needs
//! nor consumes a pristine stand — see the comment on that in the body.

use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfWithdrawTokens;
use dodex_contracts::dex::root_pn::RootPn;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::allocator::TaintReason;
use crate::common::chain_reader;
use crate::common::invariant;
use crate::common::locks;

/// USDC as a *token type* — the key of a note's `_balance` map and of
/// `RootPN._deployedValues`, which on this stand is also the ECC currency id
/// the physical view reads.
const TOKEN_TYPE_USDC: u32 = dodex_sdk::proof::TokenType::Usdc as u32;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn usdc_release_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");

    // Shared, not exclusive. Every equality below is about currency 3, which
    // exists on this stand only because the fixture baked it into one note —
    // no other scenario can move it, and none of them reads or writes the two
    // accounts this one leases. A conservation scenario needs the whole chain
    // still; this one does not.
    let _b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();

    // No preflight, deliberately. It asserts how the stand was *generated*,
    // which stops holding the moment any scenario mints its first movement —
    // so a stand can afford exactly one preflight-taking scenario, and
    // `proof_money` is it. Every assertion here is a delta against a baseline
    // read moments earlier instead, which is what lets this run on a stand
    // that has already served the others rather than needing one of its own.
    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let source = alloc.rent(PnProfile::Usdc, "usdc_release").expect("rent the USDC note");
    let sink = alloc.rent(PnProfile::Trd, "usdc_release").expect("rent the destination note");

    let root_pn = RootPn::DEFAULT_ADDRESS;
    let before = invariant::release_snapshot(
        &r,
        &source.note.address,
        root_pn,
        &sink.note.address,
        TOKEN_TYPE_USDC,
    )
    .await
    .expect("read the release baseline");

    // A pool baked without USDC would make every equality below trivially
    // true — nothing moves, and nothing moving is exactly what they assert.
    assert!(
        before.note_custodied > 0,
        "the leased note holds no USDC; this scenario needs a pool baked with a PN-USDC group"
    );
    assert!(
        before.note_shell > 0,
        "the leased note holds no physical SHELL, so the pool-draining half of the withdraw \
         would assert nothing"
    );

    r.dex
        .withdraw_tokens(
            &source.note.address,
            ParamsOfWithdrawTokens {
                dest_wallet_addr: sink.note.address.clone(),
                // Drives no logic in either contract; RootPN only surfaces it
                // in the `TokensWithdrawn` event.
                dapp_id: SystemDapp::Dex.dapp_id().to_string(),
            },
            Signer::Keys { keys: source.note.keys.clone() },
        )
        .await
        .expect("withdraw_tokens");

    // The barrier waits on the destination's credit — the last effect in the
    // chain — so the equalities that follow read a state every earlier step
    // has already reached. It also fails outright, rather than timing out, if
    // RootPN hands the money back: see `release_barrier_verdict`.
    let after = invariant::await_release(
        &r,
        &source.note.address,
        root_pn,
        &sink.note.address,
        TOKEN_TYPE_USDC,
        &before,
    )
    .await
    .expect("the withdraw never reached the destination");

    let violations = invariant::release_violations(&before, &after);
    assert!(violations.is_empty(), "USDC release moved wrongly:\n  {}", violations.join("\n  "));

    // The source can never serve another scenario: `_hasWithdrawn` latches on
    // for good, and every DEX operation on a note refuses once it is set.
    source.taint(TaintReason::HasWithdrawn);
    // The sink's storage is untouched — it gained physical currency, which no
    // DEX operation reads — so it goes back to the pool the normal way.
    sink.release_clean(&r).await.expect("release the destination note");
}
