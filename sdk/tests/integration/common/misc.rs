//! Misc small helpers: time, account-active wait, NACKL balance read,
//! GraphQL event-entry destructuring, and the polling primitives every
//! chain-driving scenario waits on.

use std::future::Future;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::traits::AccountAccessor;
use dodex_sdk::Dex;

use crate::common::context::TOKEN_TYPE_NACKL;

/// Slack added when waiting out a contract deadline. The gates are compared
/// against block timestamps while a test compares against the host clock,
/// and a message that arrives a second early is simply rejected — leaving the
/// following barrier to time out on a condition that never had a chance to
/// become true.
const CHAIN_CLOCK_SLACK_SECS: u64 = 5;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 30 × 2 s = 60 s, the suite's standard ceiling for one acknowledged
/// operation.
const POLL_ATTEMPTS: usize = 30;

/// How long [`wait_not_busy`] looks for a note to *become* busy before giving
/// up on ever seeing it. Deliberately short: not seeing the marker proves
/// nothing either way — the operation may have completed between two polls, or
/// been rejected outright and never existed — so there is nothing to be gained
/// by waiting longer, and every second spent here comes out of a deadline the
/// scenario has to meet. What distinguishes the two cases is each phase's
/// effect assertion, not this loop.
const BUSY_APPEAR_ATTEMPTS: usize = 5;

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("time").as_secs()
}

/// Sleep until `deadline` has passed on the chain's side of the clock. See
/// [`CHAIN_CLOCK_SLACK_SECS`] for why the host clock alone is not enough.
pub async fn wait_until(deadline_unix: u64) {
    let target = deadline_unix + CHAIN_CLOCK_SLACK_SECS;
    let now = now_unix();
    if now < target {
        tokio::time::sleep(Duration::from_secs(target - now)).await;
    }
}

/// Poll `probe` until it answers `true`, or panic after [`POLL_ATTEMPTS`]
/// tries. `what` is phrased as the failure, so the panic reads as a statement
/// about what never happened.
pub async fn poll_until<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..POLL_ATTEMPTS {
        if probe().await {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("{what} within {}s", poll_budget_secs());
}

pub fn poll_budget_secs() -> u64 {
    POLL_ATTEMPTS as u64 * POLL_INTERVAL.as_secs()
}

/// One poll interval — for a caller that drives its own loop because it has to
/// re-send between attempts rather than only re-read.
pub const fn poll_interval() -> Duration {
    POLL_INTERVAL
}

/// The same budget as [`poll_until`], for those same self-driven loops.
pub const fn poll_attempts() -> usize {
    POLL_ATTEMPTS
}

/// Settle an operation sent to `pn_address`: wait for the note's `_busy`
/// marker to appear and then clear.
///
/// Two phases, because the marker is only set once the chain picks the
/// external message up: a caller that checks straight away sees the
/// pre-message `None` and concludes the operation is finished before it has
/// even started.
///
/// **Returning is not evidence the operation succeeded**, and no caller may
/// read it that way. Never seeing the marker is ambiguous by construction —
/// the operation may have completed between two polls, or it may have been
/// rejected by a `require` before `tvm.accept()` and never have existed, and
/// those two are the same observation from here. Distinguishing them is the
/// job of each phase's effect assertion; this function only keeps the scenario
/// from sending a note its next operation while it is still busy with the
/// last. Hence the short [`BUSY_APPEAR_ATTEMPTS`] budget: waiting longer buys
/// no certainty, and the time comes out of a deadline the caller has to meet.
pub async fn wait_not_busy(dex: &Dex, pn_address: &str, op: &str) {
    let mut saw_busy = false;
    for _ in 0..BUSY_APPEAR_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let d = dex.get_private_note_details(pn_address).await.expect("pn details");
        if d.busy_address.is_some() {
            saw_busy = true;
            break;
        }
    }
    if !saw_busy {
        return;
    }
    poll_until(&format!("note {pn_address} stayed busy after {op}"), || async {
        dex.get_private_note_details(pn_address).await.expect("pn details").busy_address.is_none()
    })
    .await;
}

pub async fn wait_active<T: AccountAccessor>(contract: &T, label: &str) {
    contract
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(30),
            attempts_timeout: Some(2_000),
        })
        .await
        .unwrap_or_else(|e| panic!("wait {label} active: {e:?}"));
}

pub fn pn_nackl(details: &dodex_sdk::PrivateNoteDetails) -> u128 {
    details.balance.get(&TOKEN_TYPE_NACKL.to_string()).copied().unwrap_or_default()
}

/// Pull `eventName` from an `OracleEventList._events` map entry. The getter
/// returns ABI-camelCase fields, so we look up `eventName` (not snake_case).
pub fn event_entry_name(entry: &serde_json::Value) -> Option<&str> {
    entry.get("eventName").and_then(|v| v.as_str())
}
