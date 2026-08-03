//! Misc small helpers: time, account-active wait, NACKL balance read,
//! GraphQL event-entry destructuring, and the polling primitives every
//! chain-driving scenario waits on.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::tvm_client::ClientContext;
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

/// Re-send an external message the replay guard threw out.
///
/// Every scenario addresses the same root singletons — `RootOracle` for
/// `deployOracle`, `RootPN` for `deployPrivateNote` — and an external message
/// to one of them carries a `timestamp` header the contract compares against
/// the last stamp it accepted. Two lanes need not collide inside the same
/// millisecond for that to bite: arriving out of order is enough, and whichever
/// carries the older stamp is thrown out with exit code 52.
///
/// That rejection happens BEFORE `tvm.accept()`, so nothing was applied. A
/// re-send is therefore not a second go at a half-finished operation — it is
/// the first go at one that never started, and the fresh timestamp it carries
/// is the whole of what it was missing.
///
/// Anything else is a real failure and panics with what the chain said.
pub async fn send_past_replay_guard<F, Fut, T>(what: &str, mut send: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, dodex_sdk::errors::AppError>>,
{
    /// `52` — "Replay protection exception" in the TVM Solidity runtime.
    const REPLAY_REJECTED: &str = "52";
    const SEND_ATTEMPTS: u32 = 5;

    for attempt in 1..=SEND_ATTEMPTS {
        match send().await {
            Ok(value) => return value,
            Err(err)
                if err.error_code.as_deref() == Some(REPLAY_REJECTED)
                    && attempt < SEND_ATTEMPTS =>
            {
                // Back off by an amount no other lane is likely to pick, so two
                // that collided do not collide again in step: the attempt
                // number separates lanes that are at different attempts, the
                // clock's own tail separates lanes that are at the same one.
                let jitter = now_nanos_tail(300);
                eprintln!(
                    "{what}: replay protection rejected the send on attempt \
                     {attempt}/{SEND_ATTEMPTS}; re-sending in {}ms with a fresh timestamp",
                    200 * u64::from(attempt) + jitter
                );
                tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt) + jitter)).await;
            }
            Err(err) => panic!("{what}: {err:?}"),
        }
    }
    unreachable!("the last attempt either returns or panics")
}

/// Low bits of the wall clock, for spreading retries that would otherwise line
/// up. Not randomness and not relied on as such — just something two processes
/// are unlikely to share.
fn now_nanos_tail(modulus: u64) -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
        % modulus
}

/// Top up a singleton's native gas, and make sure the credit actually landed.
///
/// The kit's `top_up_native_with_giver_if_below` sends ONE giver message, waits
/// three seconds, prints whatever balance it then sees, and returns `Ok`
/// regardless of what that balance was. So a giver message the network dropped
/// is indistinguishable from a successful top-up, and the caller walks into a
/// deploy with no gas behind it. From the outside that is an `exit_code 52` in
/// the compute phase, naming nothing.
///
/// Dropping one is ordinary here rather than exceptional: the block manager
/// takes a few requests a second and every scenario lane reaches for the same
/// giver, so the more of them run side by side the likelier it gets. Hence a
/// re-send until the balance genuinely clears the floor — and a panic that says
/// what happened if it never does, instead of leaving the next call to fail
/// with an exit code.
pub async fn ensure_native_gas<T>(
    context: Arc<ClientContext>,
    contract: &T,
    min_native: u64,
    top_up: u64,
    label: &str,
) where
    T: AccountAccessor + AddressAccessor,
{
    const CREDIT_ATTEMPTS: u32 = 4;
    for attempt in 1..=CREDIT_ATTEMPTS {
        top_up_native_with_giver_if_below(context.clone(), contract, min_native, top_up, label)
            .await
            .unwrap_or_else(|e| panic!("{label}: top up native gas: {e:?}"));
        for _ in 0..POLL_ATTEMPTS {
            if native_balance(contract).await >= u128::from(min_native) {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        eprintln!(
            "{label}: giver credit {attempt}/{CREDIT_ATTEMPTS} never landed (balance {} is still \
             below {min_native}); re-sending",
            native_balance(contract).await
        );
    }
    panic!(
        "{label}: native gas never reached {min_native} after {CREDIT_ATTEMPTS} giver credits. \
         Whatever runs next needs that gas, so it would fail in the compute phase with an exit \
         code and nothing else to say"
    );
}

/// Native balance of an account, or 0 if it cannot be read right now. A read
/// that fails is not evidence of an empty account — it is just not evidence of
/// a funded one, which is all the caller above is asking about.
async fn native_balance<T: AccountAccessor>(contract: &T) -> u128 {
    if contract.fetch_account().await.is_err() {
        return 0;
    }
    contract
        .account()
        .lock()
        .await
        .balance
        .as_ref()
        .and_then(|v| v.to_string().parse::<u128>().ok())
        .unwrap_or(0)
}

pub fn pn_nackl(details: &dodex_sdk::PrivateNoteDetails) -> u128 {
    details.balance.get(&TOKEN_TYPE_NACKL.to_string()).copied().unwrap_or_default()
}

/// Pull `eventName` from an `OracleEventList._events` map entry. The getter
/// returns ABI-camelCase fields, so we look up `eventName` (not snake_case).
pub fn event_entry_name(entry: &serde_json::Value) -> Option<&str> {
    entry.get("eventName").and_then(|v| v.as_str())
}
