//! A market that resolves to the outcome nobody tests, and three stakes that
//! are walked away from instead of claimed.
//!
//! Two things go untested together here, and they go together because the
//! second is the cheapest way to reach the first.
//!
//! **Every resolve in the suite names outcome 0.** That is the index every
//! array starts at, so a payout that read the wrong element, or a coefficient
//! computed against the wrong pool, would agree with itself throughout and
//! look correct. This market resolves to outcome **1** and pays a staker who
//! backed it.
//!
//! **And nothing has ever been forfeited.** `deleteStake` hands a stake back
//! to the market and takes nothing in return: the record on the note is
//! deleted, the collateral stays in the pot, and the winners' share of it
//! grows. It is the one exit from a market that is not a refund, and the only
//! path to `_tryClose` other than the last claim — so the market's own end is
//! reachable through it.
//!
//! ## The arrangement
//!
//! - one staker backs outcome 0 and **forfeits before the resolve**. Its
//!   record disappears, its collateral does not come back, and — the reading
//!   that says forfeiting is not cancelling — the market's unclaimed balance
//!   does not shrink either. What it gave up stays for the winners.
//! - one staker backs outcome 1 and **claims after the resolve**, and is paid.
//!   That is the outcome-1 claim, and the phase above is what makes it
//!   non-trivial: with a forfeited loser in the pot there is a profit to
//!   distribute rather than a bare principal to return.
//! - the creator, whose initial stakes sit on both outcomes, **forfeits after
//!   the resolve while holding a winning position** — the branch a claim
//!   would otherwise always take — and that forfeit is the last unclaimed
//!   mass, so the market closes on it. The account goes, and what it still
//!   owed goes to the creator, read before the closing call because afterwards
//!   there is nothing left to read it from.
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
use crate::common::market::prepare_ephemeral_market;
use crate::common::market::resolve_and_drain;
use crate::common::market::stake_amount;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

const STAKE_PERIOD_FORFEIT: u64 = 240;

/// The outcome this market resolves to — deliberately not the zero every
/// other scenario names.
const WINNING_OUTCOME: u32 = 1;
const LOSING_OUTCOME: u32 = 0;

const _: () = assert!(WINNING_OUTCOME != 0, "outcome 0 is the one already covered everywhere");

/// What each staker puts in. A multiple of the 0.01 NACKL lot and well over
/// the 1 NACKL stake minimum.
const STAKE: u128 = 20_000_000_000;

const _: () = assert!(STAKE.is_multiple_of(10_000_000));
const _: () = assert!(STAKE >= 1_000_000_000);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_resolves_away_from_zero_and_closes_on_a_forfeit_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "forfeit_close").expect("rent the creator note");
    let quitter = alloc.rent(PnProfile::Trd, "forfeit_close").expect("rent the quitting note");
    let winner = alloc.rent(PnProfile::Trd, "forfeit_close").expect("rent the winning note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared =
        prepare_ephemeral_market(ctx, dex, &b0, &creator, nonce, STAKE_PERIOD_FORFEIT).await;

    stake_amount(dex, &quitter, &prepared.key, LOSING_OUTCOME, STAKE, false).await;
    stake_amount(dex, &winner, &prepared.key, WINNING_OUTCOME, STAKE, false).await;
    for (who, note) in [("quitter", &quitter), ("winner", &winner)] {
        assert!(
            !dex.get_stakes(&note.note.address).await.expect("stakes").stakes.is_empty(),
            "the {who}'s stake did not register"
        );
    }

    let market = freeze_prepared_market(ctx, dex, prepared).await;

    // ── walking away before the market has an answer ──────────────────────
    //
    // A forfeit is not a cancellation. The record goes, and nothing comes
    // back — which is only readable as the pair of them: a note whose stake
    // vanished and whose balance did not move, against a market that is still
    // holding exactly as much as it was.
    let quitter_balance = pn_balance(&r, &quitter.note.address).await;
    let pot_before_forfeit = unclaimed(&r, &market.pmp).await;

    delete_stake(dex, &quitter, &market).await;
    poll_until("the forfeited stake never left the note", || async {
        dex.get_stakes(&quitter.note.address).await.expect("stakes").stakes.is_empty()
    })
    .await;

    assert_eq!(
        pn_balance(&r, &quitter.note.address).await,
        quitter_balance,
        "the forfeited stake came back; that is a cancellation, not a forfeit"
    );
    assert_eq!(
        unclaimed(&r, &market.pmp).await,
        pot_before_forfeit,
        "the market's unclaimed balance moved on a forfeit — what was given up is supposed to \
         stay in the pot for the winners"
    );

    // ── the resolve, away from zero ───────────────────────────────────────
    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, WINNING_OUTCOME).await;

    let winner_before = pn_balance(&r, &winner.note.address).await;
    claim(dex, &winner, &market).await;
    let paid = pn_balance(&r, &winner.note.address).await - winner_before;
    assert!(
        paid > STAKE,
        "a stake of {STAKE} on the winning outcome {WINNING_OUTCOME} paid {paid} — not even its \
         principal back, let alone a share of what the losing side and the forfeit left behind"
    );
    assert!(
        dex.get_stakes(&winner.note.address).await.expect("stakes").stakes.is_empty(),
        "the claim left the winner's stake record behind"
    );

    // ── and walking away from a winning position ──────────────────────────
    //
    // The creator's initial stakes sit on both outcomes, so it is holding a
    // winner and could claim. Forfeiting instead takes the branch a claim
    // would always take otherwise — and, with everyone else already out, this
    // is the last unclaimed mass, so it is also what closes the market.
    //
    // What the market still owes is read *now*: the closing call is what
    // sweeps it to the creator, and afterwards there is no account left to
    // ask.
    let residual = unclaimed(&r, &market.pmp).await;
    let creator_before = pn_balance(&r, &creator.note.address).await;
    assert!(
        residual > 0,
        "the market owes nothing going into its last forfeit, so the sweep below moves nothing \
         and asserts nothing"
    );

    delete_stake(dex, &creator, &market).await;
    poll_until("the market never closed on its last forfeit", || async {
        r.account_absent(&market.pmp).await.expect("read the market account")
    })
    .await;

    assert!(
        dex.get_stakes(&creator.note.address).await.expect("stakes").stakes.is_empty(),
        "the creator's stake record survived the forfeit that closed the market"
    );
    poll_until("the closing sweep never reached the creator", || async {
        pn_balance(&r, &creator.note.address).await == creator_before + residual
    })
    .await;

    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    quitter.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    winner.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Hand a stake back to the market and take nothing for it. Fire-and-forget:
/// the note accepts the message before it validates, so what the call did is
/// each phase's own reading.
async fn delete_stake(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    let _ = dex
        .delete_stake(
            &note.note.address,
            market.key.clone(),
            Signer::Keys { keys: note.note.keys.clone() },
        )
        .await;
    wait_not_busy(dex, &note.note.address, "delete_stake").await;
}

async fn claim(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    market: &crate::common::market::EphemeralMarket,
) {
    dex.claim(&note.note.address, market.key.clone(), Signer::Keys { keys: note.note.keys.clone() })
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

/// What the market still owes anyone — the figure its closing sweep hands to
/// the creator.
async fn unclaimed(r: &chain_reader::ChainReader, pmp: &str) -> u128 {
    invariant::pmp_unclaimed(r, pmp).await.expect("read the market's unclaimed balance")
}
