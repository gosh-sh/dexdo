//! The four ways a market is refused by the oracle it named, and where the
//! fee ends up in each.
//!
//! Every other scenario deploys a market the same way: name an oracle, name
//! the event it published, attach the fee that event asks for. The market
//! comes up. What has never run is any of the branches taken when one of those
//! three does not match — and they are not error paths in the ordinary sense.
//! A refused market is **created first**: the note debits its initial stakes,
//! attaches `Σ oracleFee + NETWORK_FEE_AMOUNT` as physical SHELL, and the PMP
//! exists on chain long enough to ask its oracles for confirmation. Only then
//! does the refusal come back, and everything the note paid has to find its way
//! home.
//!
//! ## The four refusals
//!
//! Three of them are decided by `confirmEvent`, which the PMP's own
//! constructor sends. It answers `rejectEvent` when the event id is one the
//! list never published, when the event's deadline has already passed, and
//! when the attached fee is below what the event asks. The fourth is decided a
//! step later: the list confirms, the PMP's `approveEvent` finds the creator
//! supplied a different number of initial stakes than the event has outcomes,
//! and it cancels itself.
//!
//! The two halves settle the fee differently, and that difference is the
//! reading this scenario is built around. On the three `confirmEvent`
//! rejections the oracle **never sees the fee** — the list hands it straight
//! back to the PMP, which forwards it to the creator, because no oracle
//! rendered any service. On the mismatch the list had already confirmed and
//! already been paid, so the oracle **keeps the fee** and only the count is
//! corrected. So:
//!
//! | refusal                  | fee ends at | creator is out of pocket by |
//! |--------------------------|-------------|-----------------------------|
//! | event never published    | the creator | the network fee             |
//! | fee below the event's    | the creator | the network fee             |
//! | deadline already passed  | the creator | the network fee             |
//! | stakes ≠ outcomes        | the oracle  | fee + network fee           |
//!
//! ## Why three oracles for one event
//!
//! An event's id is `tvm.hash(name, deadline, describe, outcomes)` — nothing
//! about the list it was published to — so the same event published by three
//! oracles has one id, and three markets can name it, one oracle each. That
//! matters twice over. A market's address is `f(eventId, oracleListHash,
//! tokenType)`, so naming a different oracle is the only way to make a second
//! market for the same event without landing on the address the first one
//! self-destructed from; and the three lists end up holding the same event
//! with three different confirmation counts, which is the whole of what this
//! scenario has to say about counts:
//!
//! - the **refuser** was asked three times — once about this event, once about
//!   an id it never held and once about a second event of its own that had
//!   outlived its deadline — and counted none of them: count 0, never
//!   incremented;
//! - the **canceller** confirmed it and then cancelled: count 0, incremented
//!   and released;
//! - the **control** confirmed it and kept it: count 1.
//!
//! The first two readings are identical and reached by opposite routes, which
//! is exactly why the fee reading above is needed: the oracle fee arriving at
//! the canceller's account is the durable trace that its confirmation really
//! happened, and the refuser's untouched account is the trace that its never
//! did. Without them, "count is 0" would be equally true of a `confirmEvent`
//! that was never delivered.
//!
//! ## What a barrier can mean here
//!
//! A refused market leaves the creator exactly as it found it, which is also
//! true of a `deployPMP` that the note refused outright and never sent. The
//! discriminator in every phase is the **loss**: the creator is down a network
//! fee it can only have lost by paying for a market that was built. A note
//! that never dispatched one is out of pocket by nothing.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::Dex;

use crate::common::allocator;
use crate::common::allocator::LeasedPn;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::NETWORK_FEE_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::misc::event_entry_name;
use crate::common::misc::now_unix;
use crate::common::misc::poll_until;
use crate::common::misc::wait_active;
use crate::common::misc::wait_until;
use crate::common::pmp::prepare_oracle_quorum;

/// The oracles this scenario needs, and what each one is for: the one that
/// refuses three times, the one that confirms and then cancels, and the one
/// that carries the market which actually comes up.
const ORACLES: usize = 3;

/// How long the event published for the lapsed-deadline phase stays valid.
///
/// Bracketed from both sides: `addEvent` refuses a deadline that has already
/// passed, and `confirmEvent` rejects one that has. So it has to be far enough
/// ahead to be accepted and near enough to be waited out — and the wait itself
/// costs nothing here, because it is published first and used last, with two
/// other phases running out its lifetime.
const STALE_LIFETIME: u64 = 30;

/// What the events this scenario publishes are worth to their oracle, minus
/// one — the smallest possible shortfall, so nothing but the comparison itself
/// can be what refuses the market.
const SHORT_FEE: u128 = ORACLE_FEE - 1;

const _: () = assert!(SHORT_FEE < ORACLE_FEE, "the short fee has to be short");

/// How many outcomes the fixture's events carry, and how many initial stakes
/// the mismatch phase supplies instead.
const OUTCOMES: usize = 2;
const MISMATCHED_STAKES: usize = 3;

const _: () = assert!(MISMATCHED_STAKES != OUTCOMES, "a mismatch has to mismatch");

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_its_oracle_refuses_unwinds_and_leaves_no_confirmation_behind_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "event_rejects").expect("rent the creator note");
    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");

    let oracles = prepare_oracle_quorum(ctx, dex, &b0, nonce, ORACLES).await;
    let refuser = &oracles[0];
    let canceller = &oracles[1];
    let control = &oracles[2];
    let event_id = refuser.event_id.clone();

    // Published now, deployed against last: its whole lifetime elapses while
    // the two phases below run, so waiting it out costs this scenario nothing.
    let stale_name = format!("Stale{nonce:08x}");
    let stale_deadline = now_unix() + STALE_LIFETIME;
    let stale_id =
        publish_event(dex, &refuser.el_address, &refuser.oracle_keys, &stale_name, stale_deadline)
            .await;
    assert_ne!(
        stale_id, event_id,
        "the short-lived event was published under the id the fixture's event already has, so the \
         phase below would be testing the wrong one"
    );

    let refuser_shell_before = account_shell(&r, &refuser.oracle_address).await;

    // ── 1. an event the list never published ──────────────────────────────
    //
    // The oracle is real and its list is there to answer; only the id is one
    // nothing was ever added under. `deployPMP` cannot tell — the event lives
    // on a contract it does not read — so the market is built and the list is
    // the first thing in the chain to know better.
    let unknown_id = format!("0x{:064x}", 0x00E4_0000_u64 + nonce);
    let before = snapshot(&r, dex, &creator).await;
    let pmp = deploy_market(
        ctx,
        dex,
        &creator,
        &unknown_id,
        std::slice::from_ref(&refuser.oracle_name),
        ORACLE_FEE,
        &[DEPLOYER_SEED_AMOUNT; OUTCOMES],
    )
    .await;
    expect_unwound(
        &r,
        dex,
        &creator,
        &pmp,
        "an event its oracle never published",
        &before,
        NETWORK_FEE_AMOUNT,
    )
    .await;

    // ── 2. a fee one unit short ───────────────────────────────────────────
    //
    // Everything the phase above got wrong is right here — a real event, a
    // live oracle, a deadline two billion seconds out. The only thing the list
    // can be refusing is the fee.
    let before = snapshot(&r, dex, &creator).await;
    let pmp = deploy_market(
        ctx,
        dex,
        &creator,
        &event_id,
        std::slice::from_ref(&refuser.oracle_name),
        SHORT_FEE,
        &[DEPLOYER_SEED_AMOUNT; OUTCOMES],
    )
    .await;
    expect_unwound(
        &r,
        dex,
        &creator,
        &pmp,
        "a fee below what the event asks",
        &before,
        NETWORK_FEE_AMOUNT,
    )
    .await;

    // ── 3. a deadline that has already passed ─────────────────────────────
    wait_until(stale_deadline).await;
    let before = snapshot(&r, dex, &creator).await;
    let pmp = deploy_market(
        ctx,
        dex,
        &creator,
        &stale_id,
        std::slice::from_ref(&refuser.oracle_name),
        ORACLE_FEE,
        &[DEPLOYER_SEED_AMOUNT; OUTCOMES],
    )
    .await;
    expect_unwound(
        &r,
        dex,
        &creator,
        &pmp,
        "an event whose deadline had already passed",
        &before,
        NETWORK_FEE_AMOUNT,
    )
    .await;

    // Three refusals, and the oracle behind them was never paid for any of
    // them — which is the other half of every "the creator got the fee back"
    // above, and the reading the mismatch phase below is measured against.
    assert_eq!(
        account_shell(&r, &refuser.oracle_address).await,
        refuser_shell_before,
        "the oracle that refused three markets was paid for one of them: the fee is supposed to \
         travel back to the creator on every reject path, since no oracle serviced the event"
    );

    // ── 4. more initial stakes than the event has outcomes ────────────────
    //
    // This one is refused a step later and by the other contract. The list
    // knows nothing wrong with it: the event is real, the deadline is far off,
    // the fee is right — so it confirms, takes the fee and counts it. The
    // market itself is what finds the mismatch, and it has to undo a
    // confirmation that already happened rather than one that never did.
    let canceller_shell_before = account_shell(&r, &canceller.oracle_address).await;
    let before = snapshot(&r, dex, &creator).await;
    let pmp = deploy_market(
        ctx,
        dex,
        &creator,
        &event_id,
        std::slice::from_ref(&canceller.oracle_name),
        ORACLE_FEE,
        &[DEPLOYER_SEED_AMOUNT; MISMATCHED_STAKES],
    )
    .await;
    expect_unwound(
        &r,
        dex,
        &creator,
        &pmp,
        "more initial stakes than the event has outcomes",
        &before,
        ORACLE_FEE + NETWORK_FEE_AMOUNT,
    )
    .await;
    assert_eq!(
        account_shell(&r, &canceller.oracle_address).await,
        canceller_shell_before + ORACLE_FEE,
        "the oracle that confirmed the mismatched market was not paid, so its confirmation never \
         happened and the count reading below would say nothing about a release"
    );

    // ── the control: the same event, named right ──────────────────────────
    //
    // Everything above is a market that failed to come up, and every reading
    // taken from it is a reading of something that did not happen. This is the
    // one that does: the same event, the same fee, the same stakes as the
    // first three phases — a different oracle only because a market's address
    // is fixed by the event and the oracle it names, and the refuser's is
    // spoken for.
    let control_shell_before = account_shell(&r, &control.oracle_address).await;
    let before = snapshot(&r, dex, &creator).await;
    let pmp = deploy_market(
        ctx,
        dex,
        &creator,
        &event_id,
        std::slice::from_ref(&control.oracle_name),
        ORACLE_FEE,
        &[DEPLOYER_SEED_AMOUNT; OUTCOMES],
    )
    .await;
    wait_active(
        &Pmp::new(ctx.clone(), dex_contract_params(&pmp)),
        "the market its oracle had no reason to refuse",
    )
    .await;
    poll_until(&format!("the market {pmp} never reached its oracle's confirmation"), || async {
        let d = dex.get_pmp_details(&pmp).await.expect("pmp details");
        d.number_of_oracle_events > 0 && d.approved_oracle_events >= d.number_of_oracle_events
    })
    .await;
    assert_eq!(
        account_shell(&r, &control.oracle_address).await,
        control_shell_before + ORACLE_FEE,
        "the oracle of the market that came up was not paid its fee"
    );
    assert_eq!(
        account_shell(&r, &creator.note.address).await,
        before.shell - ORACLE_FEE - NETWORK_FEE_AMOUNT,
        "the creator of a market that came up is out of pocket by something other than the fee it \
         paid its oracle and the network fee its market is still holding"
    );

    // ── and the three counts the event is left with ───────────────────────
    //
    // One event id, three lists, three different histories. The refuser turned
    // down every market that named it and counted none of them; the canceller
    // counted one and gave it back; the control counted one and kept it. The
    // last of the three is what says a count moves at all.
    assert_eq!(
        event_count(dex, &refuser.el_address, &event_id).await,
        Some(0),
        "the list that rejected every market naming it is holding a confirmation for one of them"
    );
    assert_eq!(
        event_count(dex, &refuser.el_address, &stale_id).await,
        Some(0),
        "the lapsed event is holding a confirmation, so it can never be retracted"
    );
    assert_eq!(
        event_count(dex, &canceller.el_address, &event_id).await,
        Some(0),
        "the list that confirmed the mismatched market never got its confirmation back when the \
         market cancelled itself"
    );
    assert_eq!(
        event_count(dex, &control.el_address, &event_id).await,
        Some(1),
        "the list carrying the market that came up is not holding its confirmation, so every zero \
         above is a reading of a counter that never moves"
    );

    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// What the creator looks like before a market is deployed from it.
struct Before {
    /// Free `_balance` in the market's currency — the initial stakes come out
    /// of it and a refusal has to put them back.
    nackl: u128,
    /// The account's own physical SHELL, which is where the oracle and network
    /// fees live. Never a logical balance: fees are attached to messages.
    shell: u128,
    /// How many stake records the note carries. A deploy adds one immediately
    /// and a refusal has to remove it.
    stakes: usize,
}

async fn snapshot(r: &chain_reader::ChainReader, dex: &Dex, note: &LeasedPn) -> Before {
    Before {
        nackl: pn_balance(r, &note.note.address).await,
        shell: account_shell(r, &note.note.address).await,
        stakes: stake_count(dex, &note.note.address).await,
    }
}

/// Wait for a refused market to finish unwinding, then say what it gave back.
///
/// The barrier is the whole settled state rather than any one reading, because
/// the unwind arrives as several independent messages — the stakes refund, the
/// fee transfer, the self-destruct — and any of them can land first. The
/// assertions afterwards are therefore a restatement of the barrier; what
/// makes them worth stating is the message each carries when it is the one
/// that never became true.
///
/// `lost_shell` is the discriminator that keeps this from passing for a
/// `deployPMP` that was refused by the note and never sent: a market that was
/// never built costs its creator nothing, and every caller here expects a
/// non-zero loss.
async fn expect_unwound(
    r: &chain_reader::ChainReader,
    dex: &Dex,
    note: &LeasedPn,
    pmp: &str,
    what: &str,
    before: &Before,
    lost_shell: u128,
) {
    let addr = &note.note.address;
    poll_until(&format!("the market refused for {what} never unwound"), || async {
        r.account_absent(pmp).await.expect("read the refused market's account")
            && dex.get_private_note_details(addr).await.expect("pn details").busy_address.is_none()
            && pn_balance(r, addr).await == before.nackl
            && account_shell(r, addr).await == before.shell - lost_shell
            && stake_count(dex, addr).await == before.stakes
    })
    .await;

    assert!(
        r.account_absent(pmp).await.expect("read the refused market's account"),
        "the market refused for {what} is still on chain"
    );
    assert_eq!(
        pn_balance(r, addr).await,
        before.nackl,
        "the market refused for {what} did not return the creator's initial stakes"
    );
    assert_eq!(
        account_shell(r, addr).await,
        before.shell - lost_shell,
        "the creator of the market refused for {what} is out of pocket by something other than \
         the {lost_shell} it was supposed to lose"
    );
    assert_eq!(
        stake_count(dex, addr).await,
        before.stakes,
        "the creator still carries a stake record for the market refused for {what}, which no \
         longer exists to clear it"
    );
}

/// Ask a note to deploy a market and return the address it will land on.
///
/// Deliberately does not wait for the market to come up: three of this
/// scenario's five callers are deploying one that never will, and what each of
/// them waits for is its own.
async fn deploy_market(
    ctx: &Arc<ClientContext>,
    dex: &Dex,
    creator: &LeasedPn,
    event_id: &str,
    names: &[String],
    fee: u128,
    initial_stakes: &[u128],
) -> String {
    dex.deploy_pmp(
        &creator.note.address,
        ParamsOfDeployPmp {
            event_id: event_id.to_string(),
            oracle_fee: vec![fee; names.len()],
            token_type: TOKEN_TYPE_NACKL,
            names: names.to_vec(),
            index: vec![0; names.len()],
            initial_stakes: initial_stakes.to_vec(),
        },
        Signer::Keys { keys: creator.note.keys.clone() },
    )
    .await
    .expect("deploy_pmp");

    RootPn::new(ctx.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS))
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: event_id.to_string(),
            names: names.to_vec(),
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .expect("get_pmp_address")
        .pmp_address
}

/// Publish one more event on a list that already carries the fixture's, and
/// return the id it landed under.
///
/// The fixture's own events are all fixed at a deadline two billion seconds
/// out, which is the one thing this scenario needs an event *not* to have.
async fn publish_event(
    dex: &Dex,
    list: &str,
    oracle_keys: &KeyPair,
    name: &str,
    deadline: u64,
) -> String {
    let mut outcomes = HashMap::new();
    outcomes.insert(0_u32, "Team A".to_string());
    outcomes.insert(1_u32, "Team B".to_string());
    dex.add_event(
        list,
        ParamsOfAddEvent {
            event_name: name.to_string(),
            oracle_fee: ORACLE_FEE,
            deadline,
            describe: "Who wins?".to_string(),
            outcome_names: outcomes,
            trust_addr: None,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .expect("add_event");

    poll_until(&format!("event {name} never appeared in {list}"), || async {
        find_event(dex, list, name).await.is_some()
    })
    .await;
    find_event(dex, list, name).await.expect("the event the poll above waited for")
}

/// The id of an event in a list, by the name it was published under.
async fn find_event(dex: &Dex, list: &str, name: &str) -> Option<String> {
    dex.get_events(list)
        .await
        .expect("get_events")
        .events
        .into_iter()
        .find(|(_, e)| event_entry_name(e) == Some(name))
        .map(|(id, _)| id)
}

/// How many markets a list is currently holding a confirmation for, read
/// straight off `EventInfo.count`. `None` means the list does not carry the
/// event at all, which is a different statement from a count of zero and must
/// never be collapsed into one.
async fn event_count(dex: &Dex, list: &str, event_id: &str) -> Option<u128> {
    dex.get_events(list)
        .await
        .expect("get_events")
        .events
        .get(event_id)
        .and_then(|e| e.get("count"))
        .and_then(|c| c.as_str())
        .map(|c| c.parse::<u128>().expect("a confirmation count that is not a number"))
}

/// The account's own physical SHELL — `currencies[2]` on the account itself,
/// not the custodied `_balance` a note keeps for its owner. Oracle and network
/// fees travel this way and never touch a logical balance.
async fn account_shell(r: &chain_reader::ChainReader, addr: &str) -> u128 {
    r.account_ecc(addr)
        .await
        .unwrap_or_else(|e| panic!("read physical ECC of {addr}: {e:?}"))
        .ecc
        .get(&CURRENCY_ID_SHELL)
        .copied()
        .unwrap_or(0)
}

/// The note's free NACKL — `_balance`, without what its resting orders hold.
async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn stake_count(dex: &Dex, pn_address: &str) -> usize {
    dex.get_stakes(pn_address).await.expect("pn stakes").stakes.len()
}
