//! An oracle's own housekeeping: the lists it owns, the events it publishes
//! and retracts, and who is allowed to do any of it.
//!
//! Every scenario in the suite uses an oracle the same way — one oracle, its
//! list number zero, one event added and never touched again. That is the
//! narrowest possible path through a contract that is built to hold several
//! lists per oracle and several events per list, with an owner check on each
//! of them. What has never run is the rest of it, and the parts that have
//! never run are exactly the ones where a missing owner check would not
//! announce itself: a stranger who could add or retract an event on somebody
//! else's list moves nothing and breaks nothing until a market is deployed
//! against what they wrote.
//!
//! No market is built here at all. Everything is oracle-side, which makes
//! this the one scenario on the stand with nothing to wait for.
//!
//! ## What it does
//!
//! - **Lists beyond the first.** An oracle deploys list one and list two,
//!   each at the address the oracle itself computes for that index. They are
//!   different addresses, and the event published into one is unknown to the
//!   other — the isolation that makes several lists per oracle worth having.
//! - **An event retracted.** `deleteEvent` on a list that owns it removes it;
//!   the same call from a stranger's key does not.
//! - **And a list nobody owns.** Deploying a list from a key that is not the
//!   oracle's leaves no account at the address that key was aiming at.
//!
//! `setDescription` is deliberately absent: the SDK carries no wrapper for
//! it, and adding one to the public surface for the sake of a single
//! assertion belongs with whoever needs the field rather than here.
//!
//! Every refusal is read as the absence of its effect, for the reason the
//! rest of the suite gives: these guards are `require`s reached after
//! `tvm.accept()`, or before it at the signature check, and neither reports
//! back to a send that does not wait for a transaction. Each refused call is
//! therefore paired with the same call from the key that is allowed to make
//! it.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use std::collections::HashMap;

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::oracle::ParamsOfDeployEventList;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::oracle_event_list::ParamsOfDeleteEvent;

use crate::common::allocator;
use crate::common::chain_reader;
use crate::common::context::ORACLE_FEE;
use crate::common::keys::gen_keys;
use crate::common::locks;
use crate::common::misc::event_entry_name;
use crate::common::misc::now_unix;
use crate::common::misc::poll_until;
use crate::common::misc::wait_until;
use crate::common::pmp::prepare_oracle_event;

/// The two lists this scenario adds beyond the one every oracle is born with.
const SECOND_LIST: u128 = 1;
const THIRD_LIST: u128 = 2;

const _: () = assert!(SECOND_LIST != 0 && THIRD_LIST != 0 && SECOND_LIST != THIRD_LIST);

/// How long the event this scenario retracts stays valid.
///
/// A list only lets go of an event once nothing has confirmed it *and* its
/// deadline has passed — and `addEvent` refuses a deadline that is already
/// past. So the two rules bracket this figure: far enough ahead to be
/// accepted, near enough to be waited out inside a test.
const EVENT_LIFETIME: u64 = 30;

/// The slack added before the retraction is attempted, for the same reason
/// `wait_until` adds it everywhere else: the deadline is compared against a
/// block timestamp, not the host clock.
const DEADLINE_SLACK: u64 = 10;

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn an_oracle_owns_its_lists_and_only_its_owner_may_write_to_them_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");

    // The fixture already builds an oracle with its first list and one event
    // in it; this scenario is about everything that comes after.
    let ev = prepare_oracle_event(ctx, dex, &b0, nonce).await;
    let stranger = Signer::Keys { keys: gen_keys(ctx.clone()) };

    // ── lists beyond the first ────────────────────────────────────────────
    let second = list_address(dex, &ev.oracle_address, SECOND_LIST).await;
    let third = list_address(dex, &ev.oracle_address, THIRD_LIST).await;
    assert_ne!(
        second, third,
        "the oracle computes one address for two different list indices, so nothing below \
         distinguishes them"
    );
    assert_ne!(second, ev.el_address, "list {SECOND_LIST} shares an address with list zero");

    // A stranger aiming at the same index first. Nothing may appear there —
    // and because the address is deterministic, "nothing appeared" is a
    // reading about that exact account rather than about the call.
    deploy_list(dex, &ev.oracle_address, SECOND_LIST, "a stranger's list", &stranger).await;
    assert!(
        r.account_absent(&second).await.expect("read the second list's account"),
        "a key that is not the oracle's deployed a list at {second}"
    );

    // The same call from the oracle's own key.
    deploy_list(
        dex,
        &ev.oracle_address,
        SECOND_LIST,
        "the second list",
        &Signer::Keys { keys: ev.oracle_keys.clone() },
    )
    .await;
    poll_until("the oracle's own second list never came up", || async {
        !r.account_absent(&second).await.expect("read the second list's account")
    })
    .await;

    deploy_list(
        dex,
        &ev.oracle_address,
        THIRD_LIST,
        "the third list",
        &Signer::Keys { keys: ev.oracle_keys.clone() },
    )
    .await;
    poll_until("the oracle's own third list never came up", || async {
        !r.account_absent(&third).await.expect("read the third list's account")
    })
    .await;

    // ── an event in one list is unknown to the other ──────────────────────
    let event_name = format!("AdmEvt{nonce:08x}");
    let deadline = now_unix() + EVENT_LIFETIME;
    add_event(dex, &second, &event_name, deadline, &Signer::Keys { keys: ev.oracle_keys.clone() })
        .await;
    let event_id = poll_for_event(dex, &second, &event_name).await;

    assert!(
        find_event(dex, &third, &event_name).await.is_none(),
        "an event added to one of the oracle's lists appeared in another"
    );
    assert!(
        find_event(dex, &ev.el_address, &event_name).await.is_none(),
        "an event added to a later list appeared in the first one"
    );

    // ── and one a stranger may not add ────────────────────────────────────
    //
    // On a deadline of its own rather than the one above: that one is minutes
    // from lapsing by design, and a list refuses an event whose deadline has
    // already passed no matter who offers it. Sharing it would leave a refusal
    // that says nothing about ownership.
    let refused_name = format!("AdmRef{nonce:08x}");
    let refused_deadline = now_unix() + EVENT_LIFETIME;
    add_event(dex, &second, &refused_name, refused_deadline, &stranger).await;
    assert!(
        find_event(dex, &second, &refused_name).await.is_none(),
        "a key that is not the oracle's published an event on its list"
    );

    // ── retracting one ────────────────────────────────────────────────────
    //
    // A list lets go of an event only once nothing has confirmed it and its
    // deadline has passed. Nothing has confirmed this one; the deadline is
    // waited out here, because until it is, `deleteEvent` is a silent no-op
    // for the owner too and the stranger's attempt below would prove nothing.
    wait_until(deadline + DEADLINE_SLACK).await;
    delete_event(dex, &second, &event_id, &stranger).await;
    assert!(
        find_event(dex, &second, &event_name).await.is_some(),
        "a stranger retracted an event from the oracle's list"
    );

    delete_event(dex, &second, &event_id, &Signer::Keys { keys: ev.oracle_keys.clone() }).await;
    poll_until("the oracle could not retract its own event", || async {
        find_event(dex, &second, &event_name).await.is_none()
    })
    .await;
}

/// The address the oracle computes for one of its lists — deterministic, so
/// it can be read before anything is deployed there.
async fn list_address(dex: &dodex_sdk::Dex, oracle: &str, index: u128) -> String {
    dex.get_event_list_address(oracle, ParamsOfGetEventListAddress { index })
        .await
        .expect("get_event_list_address")
}

/// Ask an oracle to deploy one of its lists. Fire-and-forget: a request it
/// refuses does not answer, and whether it was refused is the caller's own
/// reading of the address.
async fn deploy_list(
    dex: &dodex_sdk::Dex,
    oracle: &str,
    index: u128,
    description: &str,
    signer: &Signer,
) {
    let _ = dex
        .deploy_event_list(
            oracle,
            ParamsOfDeployEventList { index, description: description.to_string() },
            signer.clone(),
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
}

async fn add_event(dex: &dodex_sdk::Dex, list: &str, name: &str, deadline: u64, signer: &Signer) {
    let mut outcomes = HashMap::new();
    outcomes.insert(0_u32, "Team A".to_string());
    outcomes.insert(1_u32, "Team B".to_string());
    let _ = dex
        .add_event(
            list,
            ParamsOfAddEvent {
                event_name: name.to_string(),
                oracle_fee: ORACLE_FEE,
                deadline,
                describe: "Who wins?".to_string(),
                outcome_names: outcomes,
                trust_addr: None,
            },
            signer.clone(),
        )
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
}

async fn delete_event(dex: &dodex_sdk::Dex, list: &str, event_id: &str, signer: &Signer) {
    let _ = dex
        .delete_event(list, ParamsOfDeleteEvent { event_id: event_id.to_string() }, signer.clone())
        .await;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
}

/// The id of an event in a list, by the name it was published under.
async fn find_event(dex: &dodex_sdk::Dex, list: &str, name: &str) -> Option<String> {
    dex.get_events(list)
        .await
        .expect("get_events")
        .events
        .into_iter()
        .find(|(_, e)| event_entry_name(e) == Some(name))
        .map(|(id, _)| id)
}

async fn poll_for_event(dex: &dodex_sdk::Dex, list: &str, name: &str) -> String {
    poll_until(&format!("event {name} never appeared in {list}"), || async {
        find_event(dex, list, name).await.is_some()
    })
    .await;
    find_event(dex, list, name).await.expect("the event the poll above waited for")
}
