//! Bringing a prediction market up, step by step, and the fixture that runs
//! every step for a scenario that only wants the finished thing.
//!
//! A tradable market is not one call. The oracle publishes an event, the
//! deployer note deploys the PMP against it, the oracle votes the timings in
//! (nothing may stake until it has), the staking window opens and closes, and
//! only the freeze that follows deploys the order book. `proof_money` walks
//! that sequence taking a conservation snapshot between every pair of steps;
//! a scenario testing order types or shutdown wants the end state and nothing
//! in between.
//!
//! Both read the same steps from here. The alternative — a fixture that
//! re-implements the sequence next to the scenario that measures it — is two
//! copies of the same market semantics free to drift, and the one that drifts
//! is the one nobody runs against a chain every pipeline.
//!
//! [`deploy_ephemeral_market`] deliberately does **not** stake on the
//! scenario's behalf. `deploy_pmp_with_deployer` already seeds both pools with
//! the deployer's initial stakes, so the market is well-formed without any,
//! and a scenario that wants stakes in specific outcomes wants to place them
//! itself — with its own amounts, from its own notes, and usually with its own
//! assertion about what the pool became. [`stake`] is right here for that.

use std::sync::Arc;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_e2e_harness::locks::ChainLockGuard;
use dodex_sdk::dex_contract_params;
use dodex_sdk::Dex;

use crate::common::allocator::LeasedPn;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::misc::now_unix;
use crate::common::misc::poll_attempts;
use crate::common::misc::poll_budget_secs;
use crate::common::misc::poll_interval;
use crate::common::misc::poll_until;
use crate::common::misc::wait_active;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;
use crate::common::pmp::deploy_pmp_with_deployer;
use crate::common::pmp::prepare_oracle_event;
use crate::common::pmp::OracleEventCtx;

/// A market that exists, is approved, is frozen, and has an order book.
#[allow(dead_code)]
pub struct EphemeralMarket {
    pub pmp: String,
    pub order_book: String,
    /// Identifies the market to every note-side call (`setStake`,
    /// `splitFullSet`, `placeOrder`, `claim`). Read off the contract rather
    /// than reassembled by the caller, so it cannot disagree with it.
    pub key: ParamsOfStakeKey,
    /// The oracle that published the event and votes on it — a scenario that
    /// resolves the market needs its keys.
    pub oracle: OracleEventCtx,
    /// The deadline after which the market stops accepting pool changes and
    /// starts accepting a resolve. Everything a scenario does with this market
    /// has to land before it.
    pub result_start: u64,
}

/// The oracle's own keys — `submitSetTimings` and `submitResolve` are votes,
/// authenticated against the oracle set the market was deployed with.
pub fn oracle_signer(ev: &OracleEventCtx) -> Signer {
    Signer::Keys { keys: ev.oracle_keys.clone() }
}

/// Vote the timings in and wait for the market to count the vote.
///
/// `acceptStake` rejects everything until this has happened, and the approval
/// is also what makes `stakeEnd` and `resultStart` readable — the contract
/// derives the staking window from `resultStart`, and a caller that recomputed
/// it would be describing a different market than the chain does.
pub async fn set_timings_and_approve(
    dex: &Dex,
    pmp_addr: &str,
    ev: &OracleEventCtx,
    result_start: u64,
) {
    dex.submit_set_timings(pmp_addr, ParamsOfSubmitSetTimings { result_start }, oracle_signer(ev))
        .await
        .expect("submit_set_timings");
    wait_pmp_approved(dex, pmp_addr).await;
}

pub async fn wait_pmp_approved(dex: &Dex, pmp_addr: &str) {
    poll_until(&format!("PMP {pmp_addr} did not become approved"), || async {
        dex.get_pmp_details(pmp_addr).await.expect("pmp details").approved
    })
    .await;
}

/// Stake [`STAKE_AMOUNT`] on `outcome` and wait for the market to acknowledge
/// it. A note refuses a second operation while the first is in flight, so the
/// acknowledgement is a precondition of whatever the scenario does next, not
/// only of a barrier.
pub async fn stake(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey, outcome: u32) {
    dex.set_stake(
        &note.note.address,
        ParamsOfSetStake {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            outcome,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("set_stake");
    wait_not_busy(dex, &note.note.address, "set_stake").await;
}

/// Freeze the market now that the staking window has closed. Unsigned: the
/// entry point checks the deadline and the market's state, never a caller
/// identity.
///
/// Re-sent while the market is still unfrozen, because the deadline is checked
/// against a block timestamp: a message that arrives a moment early is
/// rejected before the contract accepts it, and an external message that is
/// rejected leaves no trace at the sender — sending is fire-and-forget, so
/// nothing here can observe the rejection and a single attempt would leave the
/// scenario waiting for an order book nobody is going to deploy. The state is
/// re-read before every attempt so a market that is already frozen is never
/// sent to at all.
pub async fn pmp_freeze_now(ctx: &Arc<ClientContext>, dex: &Dex, pmp_addr: &str) {
    let contract = Pmp::new(Arc::clone(ctx), dex_contract_params(pmp_addr));
    // The postcondition is "this market is frozen", not "this call froze it".
    // Two things reach that state without us: `setTimings` freezes immediately
    // when the window it sets has already closed, and any `splitFullSet` /
    // `mergeFullSet` / resolve after `stakeEnd` freezes on the way through. A
    // send that lands after either rejects with `ERR_ALREADY_FROZEN`, and the
    // read above cannot reliably prevent that — it answers from committed
    // state, so a market frozen a moment ago still reads as unfrozen.
    //
    // Hence: a send failure is not fatal here, the state check is the
    // authority, and the last error is carried into the timeout message so a
    // market that genuinely never freezes still says why.
    let mut last_err = None;
    for _ in 0..poll_attempts() {
        if dex.get_pmp_details(pmp_addr).await.expect("pmp details").frozen {
            return;
        }
        if let Err(err) = contract.freeze_now(Signer::None).await {
            last_err = Some(err);
        }
        tokio::time::sleep(poll_interval()).await;
    }
    panic!(
        "PMP {pmp_addr} did not freeze within {}s{}",
        poll_budget_secs(),
        match last_err {
            Some(err) => format!("; last freezeNow error: {err:?}"),
            None => "; every freezeNow was accepted".to_string(),
        }
    );
}

/// Wait for the order book the freeze deploys, and return its address. The
/// address is derived deterministically and the getter answers with it long
/// before the account exists, so the wait is on the account being active, not
/// on the getter answering.
pub async fn wait_order_book(ctx: &Arc<ClientContext>, dex: &Dex, pmp_addr: &str) -> String {
    let ob_addr = dex.get_order_book_address(pmp_addr).await.expect("get_order_book_address");
    let contract = OrderBook::new(Arc::clone(ctx), dex_contract_params(&ob_addr));
    wait_active(&contract, "OrderBook").await;
    ob_addr
}

/// One market, from nothing to a live order book, for a scenario whose subject
/// is what happens *after* that.
///
/// `stake_period` is the distance from now to `resultStart`, and it buys two
/// budgets out of one number: the contract makes the staking window a tenth of
/// it — which this waits out in full, since the freeze is refused until it
/// closes — and the remaining nine tenths are what the caller has to do its
/// work in before the market stops accepting pool changes. A market meant for
/// a couple of orders wants it small; one meant for a long sequence wants it
/// large enough that `result_start` is not the thing that fails.
///
/// `_guard` proves the caller already holds `ChainLockGuard`; see
/// `prepare_oracle_event` for why nothing here calls `flock` itself.
///
/// **This composition has never run against a chain.** Every step it calls is
/// exercised by `proof_money` on every pipeline run, and the order matches
/// that scenario's own bring-up line for line — but nothing yet calls this
/// function, so the first scenario that does should expect to be the one
/// debugging it, and should not read a green pipeline as evidence about this.
#[allow(dead_code)]
pub async fn deploy_ephemeral_market(
    ctx: &Arc<ClientContext>,
    dex: &Dex,
    guard: &ChainLockGuard,
    deployer: &LeasedPn,
    nonce: u64,
    stake_period: u64,
) -> EphemeralMarket {
    let oracle = prepare_oracle_event(ctx, dex, guard, nonce).await;
    let pmp = deploy_pmp_with_deployer(ctx, dex, deployer, &oracle, guard).await;

    set_timings_and_approve(dex, &pmp, &oracle, now_unix() + stake_period).await;

    // Both deadlines come from the contract, never from arithmetic repeated
    // here: `stakeEnd` is an integer division the chain performed on the
    // timings it actually recorded, and a second copy of it would drift.
    let details = dex.get_pmp_details(&pmp).await.expect("pmp details after approval");
    let key = ParamsOfStakeKey {
        event_id: details.event_id.clone(),
        oracle_list_hash: details.oracle_list_hash.clone(),
        token_type: TOKEN_TYPE_NACKL,
    };

    wait_until(details.stake_end).await;
    pmp_freeze_now(ctx, dex, &pmp).await;
    let order_book = wait_order_book(ctx, dex, &pmp).await;

    EphemeralMarket { pmp, order_book, key, oracle, result_start: details.result_start }
}
