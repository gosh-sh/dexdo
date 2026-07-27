//! Two market setups, side by side: `parallel_setup_a` and `parallel_setup_b`
//! run as two independent test processes against the same live stand and
//! prove the harness's core concurrency claim — that two scenarios sharing
//! one small note pool and one ledger genuinely do not collide.
//!
//! Co-existence (both processes merely finishing without erroring) would be
//! a much weaker claim than independence, so each side publishes what it
//! actually did — the note it leased, the nonce it drew, the oracle address
//! and PMP address it derived — through the ledger's mailbox, and asserts
//! every one of those differs from its peer's. Same-note reuse, a nonce
//! collision, or two markets landing on the same address would all be silent
//! if this only checked "both sides passed"; they show up here as an
//! `assert_ne!` failure instead.
//!
//! # What it needs
//!
//! A live stand, addressed like `proof_money`'s scenario:
//!
//! - `E2E_NETWORK_ENDPOINT` — the stand's URL, scheme included.
//! - `E2E_SEED_NOTES` — the pre-baked `PrivateNote` pool; its directory also
//!   holds the shared ledger and the `b0.lock` sidecar.
//! - `E2E_RUN_ID` — the ledger generation this run belongs to.
//!
//! `#[ignore]`, and meant to run as a pair: start both `parallel_setup_a` and
//! `parallel_setup_b` together (`--run-ignored only`, both names selected) —
//! run alone, either side blocks on the `ready` rendezvous until its timeout
//! and fails, since its peer never shows up.
//!
//! Each side takes `ChainLockGuard::shared`, not the conservation scenario's
//! exclusive hold: this is exactly the case shared exists for, two
//! chain-mutating scenarios running at the same time, neither one asserting
//! anything about global state that the other's transactions could disturb.

use std::time::Duration;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::allocator::TaintReason;
use crate::common::context;
use crate::common::ledger::Ledger;
use crate::common::locks::ChainLockGuard;
use crate::common::pmp;

/// One side of the pair. `me`/`peer` are the two mailbox names the ledger
/// keys its rendezvous and result entries under ("a"/"b"); `parallel_setup_a`
/// and `parallel_setup_b` each call this with the pair swapped.
async fn one_side(me: &str, peer: &str) {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let dir = allocator::seed_dir().expect("seed notes directory");
    let led = Ledger::open(&dir, &run_id);

    // A market setup mutates the chain but asserts nothing about global
    // state, so it takes the lock shared: this and the peer side are meant
    // to hold it at the same time. Acquired once, held for the whole
    // scenario, and passed by reference into every helper below, none of
    // which locks anything itself.
    let guard = ChainLockGuard::shared(&dir).expect("acquire b0.lock shared");

    // The `ready` barrier and the `result/*` exchange are deliberately
    // different key namespaces. `rendezvous` and `put_entry` both write
    // through the same mailbox map; reusing one key for both purposes would
    // let the result write clobber the ready mark a slower peer is still
    // polling for, or vice versa. A timed-out barrier is a hard failure —
    // it means the peer process never started.
    led.rendezvous("ready", me, peer, Duration::from_secs(120))
        .expect("peer did not reach the ready barrier in time");

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let lease = alloc.rent(PnProfile::Dep, me).expect("rent a deployer note");
    // Drawn from the same ledger generation's monotonic counter the peer
    // draws from, so a distinct nonce here is itself a positive check: it
    // means the two `rent`/`next_nonce` calls landed as separate ledger
    // transactions rather than one process observing the other's state.
    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");

    let ctx = context::create_context();
    let dex = context::create_dex();

    // The oracle and event names `prepare_oracle_event` derives are a
    // function of `nonce`, not of wall-clock time — the entire reason this
    // scenario can assert the two oracle addresses differ instead of
    // racing to see which side's clock-based name collides first.
    let ev = pmp::prepare_oracle_event(&ctx, &dex, &guard, nonce).await;
    let pmp_addr = pmp::deploy_pmp_with_deployer(&ctx, &dex, &lease, &ev, &guard).await;

    // Write-only, at exactly this key — never overwrites the `ready` mark,
    // and never collides with the peer's own `result/<peer>` key.
    led.put_entry(
        &format!("result/{me}"),
        &format!("{}|{}|{}|{}", lease.note.address, ev.oracle_address, pmp_addr, nonce),
    )
    .expect("publish this side's result");

    // Read-only wait for the peer's key specifically — never touches
    // `result/<me>`, so there is no danger of a slow peer reading back its
    // own write reflected through this side.
    let peer_result = led
        .wait_entry(&format!("result/{peer}"), Duration::from_secs(600))
        .expect("peer never published its result");
    let fields: Vec<&str> = peer_result.test.split('|').collect();
    assert_eq!(fields.len(), 4, "peer result malformed: `{}`", peer_result.test);

    assert_ne!(lease.note.address, fields[0], "the two sides leased the same note");
    assert_ne!(ev.oracle_address, fields[1], "the two sides derived the same oracle address");
    assert_ne!(pmp_addr, fields[2], "the two sides derived the same PMP address");
    assert_ne!(nonce.to_string(), fields[3], "the two sides were handed the same nonce");

    // `release_clean` is not an option here: `deploy_pmp_with_deployer`
    // leaves the deployer note's `_stakes` populated with the two initial
    // stakes it seeded, and the sweep behind `release_clean` would see that
    // as dirty state and refuse the release. This scenario's whole point is
    // to leave a real market behind, so quarantining under an explicit
    // reason is the honest tail — not a clean return this note never
    // qualifies for.
    lease.taint(TaintReason::DirtyState {
        fields: vec!["_stakes (parallel-setup market creation)".to_string()],
    });
}

#[tokio::test]
#[ignore = "requires a live chain and a peer process: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, \
            E2E_RUN_ID — run together with parallel_setup_b via --run-ignored only"]
async fn parallel_setup_a() {
    one_side("a", "b").await;
}

#[tokio::test]
#[ignore = "requires a live chain and a peer process: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, \
            E2E_RUN_ID — run together with parallel_setup_a via --run-ignored only"]
async fn parallel_setup_b() {
    one_side("b", "a").await;
}
