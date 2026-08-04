//! What a claim on the wrong outcome pays, which is nothing.
//!
//! Every claim the suite has ever made was a winning one. `proof_money` stakes
//! and claims a single note; `forfeit_close` pays a winner out of a pot that
//! losers and quitters left behind — but the losers there *forfeit*, they never
//! call `claim`. So the branch a losing position actually takes has never run.
//!
//! ## Why nothing is the hard thing to assert
//!
//! A claim that pays zero and a claim that never arrived look identical from
//! the outside: the note's balance does not move either way, and the write path
//! is fire-and-forget, so there is no return value that separates them. What
//! separates them is the **stake record**. `claim` clears it whatever the
//! outcome was; a message that never landed leaves it standing. So the losing
//! claim is read as the pair — balance unmoved AND record gone — and neither
//! half means anything alone.
//!
//! The winner in the same market is the positive control, and it is the same
//! call: if `claim` were broken outright, the winner's payout would say so
//! before the loser's zero could be mistaken for correct behaviour.
//!
//! ## And the money has to come from somewhere
//!
//! A winner is paid out of what the losing side put in. That gives a bound
//! nothing else in the suite checks: the winner cannot gain more than the pot
//! actually holds. Asserted here as the loser's whole stake being at least what
//! the winner took above their own principal — money moved between the two
//! sides, and none was created on the way.
//!
//!   cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
//!     -E 'test(=losing_claim::a_claim_on_the_losing_outcome_pays_nothing_and_clears_its_record_local)'

use ackinacki_kit::tvm_client::abi::Signer;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::freeze_prepared_market;
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::resolve_and_drain;
use crate::common::market::stake_amount;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Long enough for two stakes to be acknowledged one after another — a note
/// takes one operation at a time — and short enough that the market's result
/// window is not the cost of the scenario.
const STAKE_PERIOD_LOSING: u64 = 240;

const WINNING_OUTCOME: u32 = 0;
const LOSING_OUTCOME: u32 = 1;

const _: () = assert!(WINNING_OUTCOME != LOSING_OUTCOME);

/// What each side stakes. A multiple of the 0.01 NACKL lot, well over the
/// 1 NACKL minimum, and the same on both sides so the pot arithmetic below is
/// about the outcome rather than about the sizes.
const STAKE: u128 = 20_000_000_000;

const _: () = assert!(STAKE.is_multiple_of(10_000_000));
const _: () = assert!(STAKE >= 1_000_000_000);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_claim_on_the_losing_outcome_pays_nothing_and_clears_its_record_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "losing_claim").expect("rent the creator note");
    let winner = alloc.rent(PnProfile::Trd, "losing_claim").expect("rent the winning note");
    let loser = alloc.rent(PnProfile::Trd, "losing_claim").expect("rent the losing note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared =
        prepare_ephemeral_market(ctx, dex, &b0, &creator, nonce, STAKE_PERIOD_LOSING).await;

    // ── two sides of the same market ──────────────────────────────────────
    stake_amount(dex, &winner, &prepared.key, WINNING_OUTCOME, STAKE, false).await;
    stake_amount(dex, &loser, &prepared.key, LOSING_OUTCOME, STAKE, false).await;
    for (who, note) in [("winner", &winner), ("loser", &loser)] {
        assert!(
            !dex.get_stakes(&note.note.address).await.expect("stakes").stakes.is_empty(),
            "the {who}'s stake did not register, so this market has only one side and the rest of \
             the scenario would be about nothing"
        );
    }

    let market = freeze_prepared_market(ctx, dex, prepared).await;

    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, WINNING_OUTCOME).await;

    // ── the claim that pays nothing ───────────────────────────────────────
    //
    // Read AFTER the resolve: the stake left `_balance` back when it was
    // placed, so what is being asked here is only what the claim itself moves.
    let loser_before = pn_balance(&r, &loser.note.address).await;
    claim(dex, &loser, &market).await;

    // The record going is what says the claim ran at all. Without it, the
    // unmoved balance below would be equally true of a message the network
    // dropped on the floor.
    poll_until("the losing claim never cleared its stake record", || async {
        dex.get_stakes(&loser.note.address).await.expect("stakes").stakes.is_empty()
    })
    .await;
    assert_eq!(
        pn_balance(&r, &loser.note.address).await,
        loser_before,
        "a stake on outcome {LOSING_OUTCOME} was paid something by a market that resolved to \
         {WINNING_OUTCOME}"
    );

    // ── and the claim that does ───────────────────────────────────────────
    let winner_before = pn_balance(&r, &winner.note.address).await;
    claim(dex, &winner, &market).await;
    poll_until("the winning claim never cleared its stake record", || async {
        dex.get_stakes(&winner.note.address).await.expect("stakes").stakes.is_empty()
    })
    .await;
    let paid = pn_balance(&r, &winner.note.address).await - winner_before;
    assert!(
        paid > STAKE,
        "the winning side was paid {paid} against a stake of {STAKE} — not even its principal \
         back, which would mean `claim` pays nobody and the loser's zero above says nothing"
    );

    // Money moved between the two sides; none was made on the way. The pot the
    // winner draws from is what the losing side left in it, so the profit
    // cannot exceed the whole of the losing stake.
    let profit = paid - STAKE;
    assert!(
        profit <= STAKE,
        "the winner took {profit} above its own stake out of a losing side worth {STAKE}"
    );

    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    winner.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    loser.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

async fn claim(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    dex.claim(
        &note.note.address,
        market.key.clone(),
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("claim");
    wait_not_busy(dex, &note.note.address, "claim").await;
}

async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}
