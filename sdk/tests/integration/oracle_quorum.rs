//! A market that answers to three oracles instead of one, and what it takes
//! to make it do anything.
//!
//! Every market the suite has built names a single oracle, which makes its
//! quorum one — and a threshold of one is not a threshold. Nothing has ever
//! observed a vote that did *not* execute, which is the half of the mechanism
//! that matters: an oracle acting alone must not be able to set a market's
//! timings or decide its outcome.
//!
//! Three oracles publish the same event. An event's id is the hash of its
//! contents and nothing about the list it was added to, so the same event
//! published three times has one id, and a market can name all three against
//! it. The quorum is `ceil(3 × 66%) = 2`.
//!
//! ## What the votes have to do
//!
//! - **A stranger's signature does nothing.** The submission is authenticated
//!   against the market's own oracle set before the message is even accepted,
//!   so a keypair that is not in it cannot move the market — read as the
//!   market staying unapproved.
//! - **One vote is not enough**, and **the same vote twice is still one
//!   vote**: an oracle repeating itself is discarded rather than counted, so
//!   two submissions from one oracle leave the market exactly as unapproved
//!   as one did.
//! - **A vote can be changed while the market is still short of quorum**, and
//!   changing it has to *move* the vote rather than add one. That is the
//!   reading the whole phase is built around: the first oracle votes one
//!   deadline, changes to another, and the second oracle then votes the
//!   second — so the market executes, and the deadline it executes with says
//!   which of the two counts were kept. A book that only ever incremented
//!   would have reached quorum on the abandoned value instead.
//! - **And the outcome is decided the same way.** One oracle's resolve leaves
//!   the market unresolved; the second one settles it.
//!
//! ## What it does not cover
//!
//! The cancellation vote — quorum and the duplicate-vote rule on
//! `submitCancelEvent` — needs a market that is cancelled rather than
//! resolved, and a market is one or the other. `cancelled_event` covers the
//! single-oracle path; the multi-oracle one wants a second market and belongs
//! with it rather than here.
//!
//! The two oracle-configuration defects (a pubkey shared by two lists, a
//! `trustAddr` collision) are deliberately absent: both are candidate
//! contract defects, and a characterization test written before the fix
//! decides what the fixed behaviour should be is a test that has to be
//! rewritten with it.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::keys::gen_keys;
use crate::common::locks;
use crate::common::market::oracle_signer;
use crate::common::misc::now_unix;
use crate::common::misc::poll_until;
use crate::common::misc::wait_until;
use crate::common::pmp::deploy_pmp_with_oracles;
use crate::common::pmp::prepare_oracle_quorum;

/// Three oracles, and the quorum that follows from them: `ceil(3 × 6600 /
/// 10000) = 2`. Mirrors `THRESHOLD` rather than restating a fraction.
const ORACLES: usize = 3;
const THRESHOLD_BPS: u128 = 6_600;
const FULL_PERCENT: u128 = 10_000;
const QUORUM: usize = (ORACLES as u128 * THRESHOLD_BPS).div_ceil(FULL_PERCENT) as usize;

// The arrangement only says anything if a single oracle is short of quorum and
// two are enough — with any other threshold this scenario asserts something
// else than it claims to.
const _: () = assert!(QUORUM == 2);
const _: () = assert!(QUORUM > 1 && QUORUM < ORACLES);

/// The deadline the first oracle proposes, and the one it changes to. Far
/// enough apart that the executed value cannot be mistaken for the abandoned
/// one, and the later of the two is what the market ends up living by.
const FIRST_PERIOD: u64 = 200;
const SECOND_PERIOD: u64 = 320;

const _: () = assert!(SECOND_PERIOD > FIRST_PERIOD + 60);

/// The outcome the resolve votes name.
const OUTCOME: u32 = 0;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_with_three_oracles_moves_only_on_two_of_them_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "oracle_quorum").expect("rent the creator note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let oracles = prepare_oracle_quorum(ctx, dex, &b0, nonce, ORACLES).await;
    let pmp = deploy_pmp_with_oracles(ctx, dex, &creator, &oracles, &b0).await;

    let details = dex.get_pmp_details(&pmp).await.expect("pmp details");
    assert_eq!(
        details.number_of_oracle_events, ORACLES as u128,
        "the market answers to {} oracles, not the {ORACLES} it was deployed against",
        details.number_of_oracle_events
    );
    assert!(!details.approved, "a freshly confirmed market must not have timings yet");

    // ── a stranger ────────────────────────────────────────────────────────
    //
    // The submission is authenticated before the message is accepted, so this
    // leaves nothing behind at all — including, in particular, a vote.
    let first_deadline = now_unix() + FIRST_PERIOD;
    let stranger = gen_keys(ctx.clone());
    let _ = dex
        .submit_set_timings(
            &pmp,
            ParamsOfSubmitSetTimings { result_start: first_deadline },
            Signer::Keys { keys: stranger },
        )
        .await;
    assert!(!approved(dex, &pmp).await, "a market took timings from a keypair it does not know");

    // ── one oracle, twice ─────────────────────────────────────────────────
    //
    // Both submissions name the same deadline. The second is discarded on the
    // way in — the market compares it with what that oracle already said —
    // so the count stays at one and the market stays unapproved. Were repeats
    // counted, these two alone would have made quorum.
    for attempt in 1..=2 {
        vote_timings(dex, &oracles[0], &pmp, first_deadline).await;
        assert!(
            !approved(dex, &pmp).await,
            "the market approved on submission {attempt} from one oracle, short of the {QUORUM} \
             its quorum needs"
        );
    }

    // ── the same oracle, a different answer ───────────────────────────────
    //
    // Still one vote, now on a different deadline. If a change added a vote
    // rather than moving it, this oracle would be holding two.
    let second_deadline = now_unix() + SECOND_PERIOD;
    vote_timings(dex, &oracles[0], &pmp, second_deadline).await;
    assert!(
        !approved(dex, &pmp).await,
        "changing a vote made a quorum out of one oracle"
    );

    // ── and a second oracle joins the changed answer ──────────────────────
    //
    // This is where it executes, and the deadline it executes with is the
    // whole claim: had the first oracle's abandoned vote still been counted,
    // the market would have had two on the *first* deadline by now and
    // settled on that instead.
    vote_timings(dex, &oracles[1], &pmp, second_deadline).await;
    poll_until("the market never approved on a quorum of two", || async {
        approved(dex, &pmp).await
    })
    .await;

    let settled = dex.get_pmp_details(&pmp).await.expect("pmp details");
    assert_eq!(
        settled.result_start, second_deadline,
        "the market approved with a deadline of {}, not the {second_deadline} the two agreeing \
         oracles named — the vote that was changed away from was still being counted",
        settled.result_start
    );

    // ── the outcome, decided the same way ─────────────────────────────────
    wait_until(settled.result_start).await;

    dex.submit_resolve(&pmp, ParamsOfSubmitResolve { outcome_id: OUTCOME }, oracle_signer(&oracles[0]))
        .await
        .expect("submit_resolve");
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert_eq!(
        resolved(dex, &pmp).await,
        None,
        "one oracle resolved the market on its own"
    );

    dex.submit_resolve(&pmp, ParamsOfSubmitResolve { outcome_id: OUTCOME }, oracle_signer(&oracles[1]))
        .await
        .expect("submit_resolve");
    poll_until("the market never resolved on a quorum of two", || async {
        resolved(dex, &pmp).await == Some(OUTCOME)
    })
    .await;

    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// One oracle's timing vote, sent and given a moment to land. Fire-and-forget
/// on purpose: a vote that is discarded, or one that reaches quorum, look the
/// same from the send, and each phase reads the difference itself.
async fn vote_timings(
    dex: &dodex_sdk::Dex,
    oracle: &crate::common::pmp::OracleEventCtx,
    pmp: &str,
    result_start: u64,
) {
    let _ = dex
        .submit_set_timings(
            pmp,
            ParamsOfSubmitSetTimings { result_start },
            oracle_signer(oracle),
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
}

async fn approved(dex: &dodex_sdk::Dex, pmp: &str) -> bool {
    dex.get_pmp_details(pmp).await.expect("pmp details").approved
}

async fn resolved(dex: &dodex_sdk::Dex, pmp: &str) -> Option<u32> {
    dex.get_pmp_details(pmp).await.expect("pmp details").resolved_outcome
}
