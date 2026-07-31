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
use anyhow::Context;
use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfCancelOrderByClient;
use dodex_contracts::dex::private_note::ParamsOfPlaceOrder;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfSplitFullSet;
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

/// Split `collateral` into a full set of outcome tokens. Only legal between
/// the freeze and `resultStart`, and only once the deployer note has
/// acknowledged the normalisation refund.
pub async fn split_full_set(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey, collateral: u128) {
    dex.split_full_set(
        &note.note.address,
        ParamsOfSplitFullSet {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            collateral,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("split_full_set");
    wait_not_busy(dex, &note.note.address, "split_full_set").await;
}

/// Place one limit order. Fire-and-forget: sending says nothing about what
/// the book did with it, and resting and filling look identical from here.
/// [`wait_owner_order`] is what tells them apart, and a caller that needs the
/// sending note settled as well waits on that separately — this deliberately
/// does neither, so that a caller cannot mistake "sent" for "rested".
#[allow(clippy::too_many_arguments)]
pub async fn place_limit(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    outcome_id: u32,
    is_buy: bool,
    price_bps: &str,
    amount: u128,
    client_order_id: u128,
) {
    place_order_with_flags(
        dex,
        note,
        key,
        outcome_id,
        is_buy,
        price_bps,
        amount,
        0,
        client_order_id,
    )
    .await;
}

/// `OrderBook`'s market-order flag. A market order carries no usable price —
/// the book substitutes the extreme of its side — and, unlike a limit order,
/// never rests: whatever it cannot fill is returned to the caller instead of
/// being inserted into the book.
pub const FLAG_MARKET: u8 = 0x04;

/// Immediate-or-cancel. Shares `FLAG_MARKET`'s never-rest branch in the book
/// — whatever it cannot fill right now is returned — but keeps a usable
/// price, so it is the flag for "cross what is there at my price or nothing".
pub const FLAG_IOC: u8 = 0x01;

/// [`place_limit`] with the order flags spelled out. Separate because the
/// flags change what `amount` *means*: a market buy denominates it in quote
/// (collateral), a limit order in base (outcome tokens).
#[allow(clippy::too_many_arguments)]
pub async fn place_order_with_flags(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    outcome_id: u32,
    is_buy: bool,
    price_bps: &str,
    amount: u128,
    flags: u8,
    client_order_id: u128,
) {
    place_order_full(
        dex,
        note,
        key,
        outcome_id,
        is_buy,
        price_bps,
        amount,
        flags,
        0,
        0,
        client_order_id,
    )
    .await
    .expect("place_order");
}

/// [`place_limit`] into a named segment of the book. Levels are keyed by
/// `epochId`, so two orders that would cross at their prices never see each
/// other unless they name the same one — which is a claim only a caller that
/// can name a segment other than `0` can make.
#[allow(clippy::too_many_arguments)]
pub async fn place_limit_in_epoch(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    outcome_id: u32,
    is_buy: bool,
    price_bps: &str,
    amount: u128,
    epoch_id: u64,
    client_order_id: u128,
) {
    place_order_full(
        dex,
        note,
        key,
        outcome_id,
        is_buy,
        price_bps,
        amount,
        0,
        0,
        epoch_id,
        client_order_id,
    )
    .await
    .expect("place_order");
}

/// Every parameter of a placement, and no expectation about the outcome.
///
/// The helpers above are this one with the parameters their callers never
/// vary pinned, and with the send's own result asserted. Both differences
/// matter to the negatives: they need `min_amount` and `epoch_id` spelled
/// out, and a refusal after `tvm.accept()` is not reported to the sender at
/// all while one before it may be — so the `expect` above would turn an
/// observation into a panic. Neither the value nor the error is asserted on
/// by those callers; what a refusal costs the note is.
#[allow(clippy::too_many_arguments)]
pub async fn place_order_full(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    outcome_id: u32,
    is_buy: bool,
    price_bps: &str,
    amount: u128,
    flags: u8,
    min_amount: u128,
    epoch_id: u64,
    client_order_id: u128,
) -> Result<dodex_sdk::ResultOfBlockchainWrite, dodex_sdk::errors::AppError> {
    dex.place_order(
        &note.note.address,
        ParamsOfPlaceOrder {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            outcome_id,
            is_buy,
            price: price_bps.to_string(),
            amount,
            flags,
            min_amount,
            epoch_id,
            client_order_id,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
}

/// Cancel one of the note's live orders by the client id it was placed with.
pub async fn cancel_by_client(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    client_order_id: u128,
) {
    dex.cancel_order_by_client(
        &note.note.address,
        ParamsOfCancelOrderByClient {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            client_order_id,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("cancel_order_by_client");
    wait_not_busy(dex, &note.note.address, "cancel_order_by_client").await;
}

/// Wait for a client order id to appear in (or disappear from) the book's
/// owner index.
pub async fn wait_owner_order(
    dex: &Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
    want_present: bool,
) {
    let what = if want_present { "appear in" } else { "disappear from" };
    poll_until(
        &format!("order {client_order_id} of {deposit_identifier_hash} did not {what} the book"),
        || async {
            let owned = dex
                .get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
                .await
                .expect("get_orders_by_owner");
            owned.orders.iter().any(|o| o.client_order_id == client_order_id) == want_present
        },
    )
    .await;
}

/// A note's outcome-token holdings, indexed by outcome.
///
/// Read out of the note's single stake record, which is what makes the
/// one-market-per-note assumption explicit rather than silent: with two
/// records there is no way to tell from here which market's tokens these are,
/// and picking either would make every assertion built on it meaningless.
pub async fn outcome_tokens(dex: &Dex, note: &LeasedPn) -> Vec<u128> {
    let addr = &note.note.address;
    let stakes = dex.get_stakes(addr).await.expect("pn stakes").stakes;
    assert!(
        stakes.len() <= 1,
        "note {addr} holds {} stake records; this scenario assumes one market per note and \
         cannot tell which record is its own",
        stakes.len()
    );
    let Some(record) = stakes.values().next() else { return Vec::new() };
    let amounts = record
        .get("amount")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("stake record of {addr} has no `amount` array: {record}"));
    amounts
        .iter()
        .map(|v| {
            // `uint128` decodes to a decimal string, never a JSON number.
            let raw = v
                .as_str()
                .unwrap_or_else(|| panic!("outcome amount of {addr} is not a string: {v}"));
            raw.parse().unwrap_or_else(|e| panic!("parse outcome amount `{raw}` of {addr}: {e}"))
        })
        .collect()
}

/// One outcome's holding out of [`outcome_tokens`]. An outcome the note has
/// never held is absent from the vector and reads zero.
pub fn at(holdings: &[u128], outcome: u32) -> u128 {
    holdings.get(outcome as usize).copied().unwrap_or(0)
}

/// Vote the event cancelled and wait until the market records it.
///
/// The vote is counted per oracle and only executes on quorum; with the single
/// oracle these markets are deployed with, one vote is the quorum. A repeat
/// vote from the same oracle returns silently rather than reverting, so the
/// send says nothing — `is_cancelled` is what does.
#[allow(dead_code)]
pub async fn cancel_event_and_wait(dex: &Dex, pmp_addr: &str, ev: &OracleEventCtx) {
    dex.submit_cancel_event(pmp_addr, oracle_signer(ev)).await.expect("submit_cancel_event");
    poll_until(&format!("PMP {pmp_addr} did not record the cancel vote"), || async {
        dex.get_pmp_details(pmp_addr).await.expect("pmp details").is_cancelled
    })
    .await;
}

/// Cancel one note's stake and wait for the refund to land in its record.
///
/// The barrier is the note's stake record disappearing, which is also the
/// discriminator: `onStakeCancelled` deletes it and credits `_balance` in the
/// same message, while every way the call can be refused — a market that is
/// not cancelled, a book still draining, an order still open — leaves the
/// record exactly where it was. Waiting on `_busy` instead would prove
/// nothing: the first poll can land before the note has even set it.
#[allow(dead_code)]
pub async fn cancel_stake(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey) {
    let addr = &note.note.address;
    dex.cancel_stake(addr, key.clone(), Signer::Keys { keys: note.note.keys.clone() })
        .await
        .expect("cancel_stake");
    poll_until(&format!("note {addr} still holds its stake record after cancelStake"), || async {
        dex.get_stakes(addr).await.expect("pn stakes").stakes.is_empty()
    })
    .await;
}

/// An ABI-decoded unsigned integer, as a number.
///
/// `tvm_abi`'s detokenizer encodes a `uint256` as `"0x"` + 64 hex digits and
/// every narrower width as a plain decimal string, so a field's *type* — not
/// its value — decides which one comes back. `OrderBook`'s price is a
/// `uint256`, which is why a bare `assert_eq!(order.price, "8000")` compares a
/// decimal literal against `0x…1f40` and fails on a correct book.
///
/// The prefix is the only safe discriminator, and the reason is worth stating:
/// `"8000"` is a well-formed value in both encodings and means two different
/// numbers in them. Guessing by trying hex first and falling back would read
/// every decimal price as a much larger one.
pub fn abi_uint(raw: &str) -> anyhow::Result<u128> {
    match raw.strip_prefix("0x") {
        Some(hex) => u128::from_str_radix(hex, 16)
            .with_context(|| format!("`{raw}` is 0x-prefixed but not a hex integer")),
        None => raw.parse().with_context(|| format!("`{raw}` is not a decimal integer")),
    }
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

pub async fn wait_resolved(dex: &Dex, pmp_addr: &str, outcome_id: u32) {
    poll_until(&format!("PMP {pmp_addr} did not resolve to outcome {outcome_id}"), || async {
        dex.get_pmp_details(pmp_addr).await.expect("pmp details").resolved_outcome
            == Some(outcome_id)
    })
    .await;
}

pub async fn wait_order_book_done(dex: &Dex, pmp_addr: &str) {
    poll_until(&format!("order book of PMP {pmp_addr} did not finish draining"), || async {
        dex.get_pmp_shutdown_state(pmp_addr).await.expect("pmp shutdown state").order_book_done
    })
    .await;
}

/// Resolve the market and wait until its order book has finished draining.
///
/// Resolving is what shuts the book down; the drain then cancels whatever
/// still rests, refunds it to the owning notes, hands the book's protocol
/// fees to RootPN and destroys the book in the same message that reports
/// completion. Both waits are needed and neither implies the other: the
/// market can report a resolved outcome while the book is still refunding.
pub async fn resolve_and_drain(dex: &Dex, pmp_addr: &str, ev: &OracleEventCtx, outcome_id: u32) {
    dex.submit_resolve(pmp_addr, ParamsOfSubmitResolve { outcome_id }, oracle_signer(ev))
        .await
        .expect("submit_resolve");
    wait_resolved(dex, pmp_addr, outcome_id).await;
    wait_order_book_done(dex, pmp_addr).await;
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
#[allow(dead_code)]
pub async fn deploy_ephemeral_market(
    ctx: &Arc<ClientContext>,
    dex: &Dex,
    guard: &ChainLockGuard,
    deployer: &LeasedPn,
    nonce: u64,
    stake_period: u64,
) -> EphemeralMarket {
    let prepared = prepare_ephemeral_market(ctx, dex, guard, deployer, nonce, stake_period).await;
    freeze_prepared_market(ctx, dex, prepared).await
}

/// A market that exists and is approved, with its staking window still open.
///
/// The half of the bring-up a scenario wants when its subject is the staking
/// window itself: stakes are only accepted between the approval and
/// `stake_end`, and [`deploy_ephemeral_market`] passes straight through that
/// window on its way to an order book.
#[allow(dead_code)]
pub struct PreparedMarket {
    pub pmp: String,
    pub key: ParamsOfStakeKey,
    pub oracle: OracleEventCtx,
    /// When the market stops accepting stakes and will accept a freeze.
    pub stake_end: u64,
    pub result_start: u64,
}

/// Bring a market up as far as the open staking window.
#[allow(dead_code)]
pub async fn prepare_ephemeral_market(
    ctx: &Arc<ClientContext>,
    dex: &Dex,
    guard: &ChainLockGuard,
    deployer: &LeasedPn,
    nonce: u64,
    stake_period: u64,
) -> PreparedMarket {
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

    PreparedMarket {
        pmp,
        key,
        oracle,
        stake_end: details.stake_end,
        result_start: details.result_start,
    }
}

/// Wait the staking window out, freeze, and wait for the order book the freeze
/// deploys.
#[allow(dead_code)]
pub async fn freeze_prepared_market(
    ctx: &Arc<ClientContext>,
    dex: &Dex,
    prepared: PreparedMarket,
) -> EphemeralMarket {
    let PreparedMarket { pmp, key, oracle, stake_end, result_start } = prepared;
    wait_until(stake_end).await;
    pmp_freeze_now(ctx, dex, &pmp).await;
    let order_book = wait_order_book(ctx, dex, &pmp).await;

    EphemeralMarket { pmp, order_book, key, oracle, result_start }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uint256_comes_back_as_padded_hex_and_reads_as_its_value() {
        assert_eq!(
            abi_uint("0x0000000000000000000000000000000000000000000000000000001f40").unwrap(),
            8000
        );
    }

    #[test]
    fn a_narrow_width_comes_back_decimal_and_is_not_read_as_hex() {
        // The whole reason the prefix is the discriminator: read as hex this
        // would be 32768, and every decimal price in the suite would compare
        // against a number four times too large.
        assert_eq!(abi_uint("8000").unwrap(), 8000);
    }

    #[test]
    fn a_value_in_neither_encoding_is_an_error() {
        assert!(abi_uint("0xzz").is_err());
        assert!(abi_uint("1f40").is_err());
    }
}
