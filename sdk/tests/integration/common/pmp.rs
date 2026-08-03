//! Oracle deploy + event publish + PMP setup helpers.

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddEvent;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;
use dodex_sdk::Dex;

use crate::common::allocator::LeasedPn;
use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::PMP_DEPOSIT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::keys::gen_keys;
use crate::common::locks::ChainLockGuard;
use crate::common::misc::ensure_native_gas;
use crate::common::misc::event_entry_name;
use crate::common::misc::now_unix;
use crate::common::misc::send_past_replay_guard;
use crate::common::misc::wait_active;
use crate::common::pn::deploy_funded_pn;
use crate::common::pn::ensure_root_pn_funded;

/// Deploy oracle + event, return (oracle_address, el_address, oracle_keys,
/// event_id, oracle_name).
pub async fn deploy_oracle_with_event(
    context: &Arc<ClientContext>,
    dex: &Dex,
    prefix: &str,
) -> (String, String, KeyPair, String, String) {
    use dodex_contracts::dex::oracle::Oracle;
    use dodex_contracts::dex::oracle_event_list::OracleEventList;
    use dodex_contracts::dex::root_oracle::RootOracle;

    let oracle_keys = gen_keys(context.clone());
    let ephemeral_keys = gen_keys(context.clone());
    let run_id = now_unix();
    let oracle_name = format!("{prefix}{run_id:x}");

    // Top up RootOracle
    let root_oracle =
        RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    wait_active(&root_oracle, "RootOracle").await;
    ensure_native_gas(context.clone(), &root_oracle, 120_000_000_000, 50_000_000_000, "RootOracle")
        .await;

    send_past_replay_guard("deploy_oracle", || {
        dex.deploy_oracle(
            ParamsOfDeployOracle {
                oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
                oracle_name: oracle_name.clone(),
            },
            Signer::Keys { keys: ephemeral_keys.clone() },
        )
    })
    .await;

    let oracle_address =
        dex.get_oracle_address(oracle_name.clone()).await.expect("get_oracle_address");
    let oracle_contract = Oracle::new(context.clone(), dex_contract_params(&oracle_address));
    wait_active(&oracle_contract, "Oracle").await;

    let el_address = dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
        .expect("get_event_list_address");
    let el_contract = OracleEventList::new(context.clone(), dex_contract_params(&el_address));
    wait_active(&el_contract, "EventList").await;

    let event_name = format!("Match {run_id:x}");
    let mut outcomes = HashMap::new();
    outcomes.insert(0_u32, "Team A".to_string());
    outcomes.insert(1_u32, "Team B".to_string());

    dex.add_event(
        &el_address,
        ParamsOfAddEvent {
            event_name: event_name.clone(),
            oracle_fee: ORACLE_FEE,
            deadline: 2_000_000_000,
            describe: "Who wins?".to_string(),
            outcome_names: outcomes,
            trust_addr: None,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .expect("add_event");

    let mut event_id = String::new();
    for _ in 0..15 {
        let events = dex.get_events(&el_address).await.expect("get_events");
        if let Some((id, _)) =
            events.events.iter().find(|(_, e)| event_entry_name(e) == Some(&event_name))
        {
            event_id = id.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    assert!(!event_id.is_empty(), "event must appear");

    (oracle_address, el_address, oracle_keys, event_id, oracle_name)
}

/// Oracle + published event, everything phase 2 of the two-phase PMP setup
/// (`deploy_pmp_with_deployer`) needs to deploy the PMP.
pub struct OracleEventCtx {
    pub oracle_address: String,
    pub el_address: String,
    pub oracle_keys: KeyPair,
    pub event_id: String,
    pub oracle_name: String,
}

/// Phase 1 of the two-phase PMP setup: deploy an oracle and publish its
/// event. Splitting this from PMP deployment lets a conservation scenario
/// take its before-snapshot in between the two phases, so the PMP's own
/// deployment fee falls inside the measured window while the event-publish
/// fee does not.
///
/// The oracle and event names are derived from `nonce` (the allocator
/// ledger's monotonic counter, see `Allocator::next_nonce`), not from
/// `now_unix()`: two scenarios started within the same wall-clock second
/// would otherwise derive the identical oracle name, and therefore the
/// identical oracle address, and the resulting failure would look like an
/// unrelated chain error rather than a name collision.
///
/// `_guard` proves the caller already holds `ChainLockGuard` for the
/// duration of this call; this function never calls `flock` itself — a
/// nested acquisition on the same lock file would self-deadlock or
/// silently downgrade an exclusive hold to a shared one.
///
/// Still tops up RootOracle's native gas from the giver: that pays for
/// transaction execution, not currency inside the tracked balance set, so
/// it stays outside a conservation scenario's Σ-scope.
pub async fn prepare_oracle_event(
    context: &Arc<ClientContext>,
    dex: &Dex,
    guard: &ChainLockGuard,
    nonce: u64,
) -> OracleEventCtx {
    prepare_oracle_member(context, dex, guard, nonce, 0).await
}

/// Prepare `count` oracles that all carry the **same** event.
///
/// An event's id is `tvm.hash(name, deadline, describe, outcomes)` — nothing
/// about the list it was added to — so the same event published by several
/// oracles has one id, and a market can name all of them against it. That is
/// the only way to build a market with a quorum to reach: `deployPMP` takes a
/// vector of oracle names for a single event id.
#[allow(dead_code)]
pub async fn prepare_oracle_quorum(
    context: &Arc<ClientContext>,
    dex: &Dex,
    guard: &ChainLockGuard,
    nonce: u64,
    count: usize,
) -> Vec<OracleEventCtx> {
    let mut out = Vec::with_capacity(count);
    for member in 0..count {
        out.push(prepare_oracle_member(context, dex, guard, nonce, member).await);
    }
    let first = &out[0].event_id;
    for (i, o) in out.iter().enumerate() {
        assert_eq!(
            &o.event_id, first,
            "oracle {i} published event id {}, not the {first} the others did — a market cannot \
             name oracles that disagree about which event they are confirming",
            o.event_id
        );
    }
    out
}

async fn prepare_oracle_member(
    context: &Arc<ClientContext>,
    dex: &Dex,
    _guard: &ChainLockGuard,
    nonce: u64,
    member: usize,
) -> OracleEventCtx {
    use dodex_contracts::dex::oracle::Oracle;
    use dodex_contracts::dex::oracle_event_list::OracleEventList;
    use dodex_contracts::dex::root_oracle::RootOracle;

    let oracle_keys = gen_keys(context.clone());
    let ephemeral_keys = gen_keys(context.clone());
    let oracle_name = format!("Fnd{nonce:08x}{member}");

    // Top up RootOracle
    let root_oracle =
        RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    wait_active(&root_oracle, "RootOracle").await;
    ensure_native_gas(context.clone(), &root_oracle, 120_000_000_000, 50_000_000_000, "RootOracle")
        .await;

    send_past_replay_guard("deploy_oracle", || {
        dex.deploy_oracle(
            ParamsOfDeployOracle {
                oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
                oracle_name: oracle_name.clone(),
            },
            Signer::Keys { keys: ephemeral_keys.clone() },
        )
    })
    .await;

    let oracle_address =
        dex.get_oracle_address(oracle_name.clone()).await.expect("get_oracle_address");
    let oracle_contract = Oracle::new(context.clone(), dex_contract_params(&oracle_address));
    wait_active(&oracle_contract, "Oracle").await;

    let el_address = dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
        .expect("get_event_list_address");
    let el_contract = OracleEventList::new(context.clone(), dex_contract_params(&el_address));
    wait_active(&el_contract, "EventList").await;

    let event_name = format!("FndEvt{nonce:08x}");
    let mut outcomes = HashMap::new();
    outcomes.insert(0_u32, "Team A".to_string());
    outcomes.insert(1_u32, "Team B".to_string());

    dex.add_event(
        &el_address,
        ParamsOfAddEvent {
            event_name: event_name.clone(),
            oracle_fee: ORACLE_FEE,
            deadline: 2_000_000_000,
            describe: "Who wins?".to_string(),
            outcome_names: outcomes,
            trust_addr: None,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .expect("add_event");

    let mut event_id = String::new();
    for _ in 0..15 {
        let events = dex.get_events(&el_address).await.expect("get_events");
        if let Some((id, _)) =
            events.events.iter().find(|(_, e)| event_entry_name(e) == Some(&event_name))
        {
            event_id = id.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    assert!(!event_id.is_empty(), "event must appear");

    OracleEventCtx { oracle_address, el_address, oracle_keys, event_id, oracle_name }
}

/// Phase 2 of the two-phase PMP setup: deploy the PMP from an
/// already-leased, already-funded `PrivateNote` (`deployer`) and wait for
/// oracle quorum. Callers doing a conservation scenario take their
/// before-snapshot between `prepare_oracle_event` and this call — see that
/// function's docs for why.
///
/// Deliberately does not call `ensure_root_pn_funded` or `deploy_funded_pn`:
/// `deployer` is a note the allocator already leased and funded, and a
/// giver top-up of RootPN here would be currency entering the tracked
/// balance set from outside the scenario — an undeclared external delta a
/// conservation assertion has no way to account for. A separate preflight
/// check asserts RootPN funding is already present on a from-scratch stand,
/// where it comes from the zerostate.
///
/// Waits for `approved_oracle_events` to reach `number_of_oracle_events`,
/// not for the `approved` flag: `approved` is only set by a later
/// `setTimings` call, a separate step the scenario performs after this
/// function returns — waiting on it here would hang forever.
///
/// Unlike `setup_pmp`'s equivalent wait, which falls through silently if
/// quorum never lands, this panics on exhaustion, naming the PMP address and
/// the last observed counters. A silent fallthrough here would hand a
/// conservation scenario a PMP address as if quorum had been reached, and
/// the scenario would go on to stake into it; the resulting failure would
/// surface much later with no obvious link back to the real cause.
/// `setup_pmp` keeps its silent fallthrough — it is the shared entry point
/// of the older `pmp`/`oracle`/`flows`/`history`/`pn_basic` modules — so the
/// two tails deliberately differ on this one point.
///
/// Returns only the PMP address, not `setup_pmp`'s full detail set: no
/// caller of this two-phase split reads `oracle_list_hash`, so the extra
/// `get_pmp_details` round trip it would cost buys nothing.
///
/// `_guard` proves the caller already holds `ChainLockGuard`; see
/// `prepare_oracle_event` for why this function never calls `flock` itself.
/// Deploy a market that names several oracles for one event.
///
/// The vector's order is part of the market's identity — `oracleListHash` is
/// computed from the names — so the same slice has to be handed to the deploy
/// and to the address computation, which is why this takes one and does both.
#[allow(dead_code)]
pub async fn deploy_pmp_with_oracles(
    context: &Arc<ClientContext>,
    dex: &Dex,
    deployer: &LeasedPn,
    oracles: &[OracleEventCtx],
    _guard: &ChainLockGuard,
) -> String {
    assert!(!oracles.is_empty(), "a market needs at least one oracle");
    let names: Vec<String> = oracles.iter().map(|o| o.oracle_name.clone()).collect();
    let event_id = oracles[0].event_id.clone();

    dex.deploy_pmp(
        &deployer.note.address,
        ParamsOfDeployPmp {
            event_id: event_id.clone(),
            oracle_fee: vec![ORACLE_FEE; names.len()],
            token_type: TOKEN_TYPE_NACKL,
            names: names.clone(),
            index: vec![0; names.len()],
            initial_stakes: vec![DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
        },
        Signer::Keys { keys: deployer.note.keys.clone() },
    )
    .await
    .expect("deploy_pmp");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id,
            names,
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .expect("get_pmp_address")
        .pmp_address;

    let pmp_contract = Pmp::new(context.clone(), dex_contract_params(&pmp_address));
    wait_active(&pmp_contract, "PMP").await;

    // Every named oracle has to confirm before the market will take a vote on
    // anything, so this waits for all of them rather than for a majority: the
    // quorum this scenario is about is the one among *confirmed* oracles.
    let mut last = (0_u128, 0_u128);
    for _ in 0..40 {
        let d = dex.get_pmp_details(&pmp_address).await.expect("pmp details");
        last = (d.approved_oracle_events, d.number_of_oracle_events);
        if d.number_of_oracle_events == oracles.len() as u128
            && d.approved_oracle_events >= d.number_of_oracle_events
        {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return pmp_address;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    panic!(
        "PMP {pmp_address} never had all {} of its oracles confirm: approved={}, declared={}",
        oracles.len(),
        last.0,
        last.1
    );
}

pub async fn deploy_pmp_with_deployer(
    context: &Arc<ClientContext>,
    dex: &Dex,
    deployer: &LeasedPn,
    ev: &OracleEventCtx,
    guard: &ChainLockGuard,
) -> String {
    deploy_pmp_in_currency(
        context,
        dex,
        deployer,
        ev,
        guard,
        TOKEN_TYPE_NACKL,
        DEPLOYER_SEED_AMOUNT,
    )
    .await
}

/// [`deploy_pmp_with_deployer`] for a market denominated in something other
/// than NACKL.
///
/// Both extra parameters travel together on purpose: a market's currency
/// decides what its creator's initial stakes are *denominated in*, and the
/// figure NACKL uses is a hundred thousand times too large for a token with
/// six decimals. Passing one without the other is the mistake this signature
/// makes impossible.
#[allow(dead_code)]
pub async fn deploy_pmp_in_currency(
    context: &Arc<ClientContext>,
    dex: &Dex,
    deployer: &LeasedPn,
    ev: &OracleEventCtx,
    _guard: &ChainLockGuard,
    token_type: u32,
    seed_per_outcome: u128,
) -> String {
    dex.deploy_pmp(
        &deployer.note.address,
        ParamsOfDeployPmp {
            event_id: ev.event_id.clone(),
            oracle_fee: vec![ORACLE_FEE],
            token_type,
            names: vec![ev.oracle_name.clone()],
            index: vec![0],
            initial_stakes: vec![seed_per_outcome, seed_per_outcome],
        },
        Signer::Keys { keys: deployer.note.keys.clone() },
    )
    .await
    .expect("deploy_pmp");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: ev.event_id.clone(),
            names: vec![ev.oracle_name.clone()],
            token_type,
        })
        .await
        .expect("get_pmp_address")
        .pmp_address;

    let pmp_contract = Pmp::new(context.clone(), dex_contract_params(&pmp_address));
    wait_active(&pmp_contract, "PMP").await;

    let mut reached_quorum = false;
    let mut last_counters = (0_u128, 0_u128);
    for _ in 0..30 {
        let d = dex.get_pmp_details(&pmp_address).await.expect("pmp details");
        last_counters = (d.approved_oracle_events, d.number_of_oracle_events);
        if d.approved_oracle_events >= d.number_of_oracle_events && d.number_of_oracle_events > 0 {
            reached_quorum = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let (approved_oracle_events, number_of_oracle_events) = last_counters;
    assert!(
        reached_quorum,
        "PMP {pmp_address} did not reach oracle quorum within 90s: \
         approved_oracle_events={approved_oracle_events}, number_of_oracle_events={number_of_oracle_events}"
    );
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    pmp_address
}

pub struct PmpSetup {
    pub pn_address: String,
    pub pn_keys: KeyPair,
    pub pmp_address: String,
    pub oracle_keys: KeyPair,
    pub oracle_list_hash: String,
    pub event_id: String,
    pub oracle_name: String,
    pub oracle_address: String,
}

/// Full PMP setup: oracle + event + PN + deploy PMP + wait approval.
pub async fn setup_pmp(context: &Arc<ClientContext>, dex: &Dex) -> PmpSetup {
    ensure_root_pn_funded(context).await;
    let (oracle_address, _, oracle_keys, event_id, oracle_name) =
        deploy_oracle_with_event(context, dex, "BeeDex-").await;

    let (pn_address, _, pn_keys) = deploy_funded_pn(context, dex, PMP_DEPOSIT).await;

    dex.deploy_pmp(
        &pn_address,
        ParamsOfDeployPmp {
            event_id: event_id.clone(),
            oracle_fee: vec![ORACLE_FEE],
            token_type: TOKEN_TYPE_NACKL,
            names: vec![oracle_name.clone()],
            index: vec![0],
            initial_stakes: vec![DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
        },
        Signer::Keys { keys: pn_keys.clone() },
    )
    .await
    .expect("deploy_pmp");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: event_id.clone(),
            names: vec![oracle_name.clone()],
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .expect("get_pmp_address")
        .pmp_address;

    let pmp_contract = Pmp::new(context.clone(), dex_contract_params(&pmp_address));
    wait_active(&pmp_contract, "PMP").await;

    // Wait for approval
    for _ in 0..30 {
        let d = dex.get_pmp_details(&pmp_address).await.expect("pmp details");
        if d.approved_oracle_events >= d.number_of_oracle_events && d.number_of_oracle_events > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let details = dex.get_pmp_details(&pmp_address).await.expect("pmp approved");
    let oracle_list_hash = details.oracle_list_hash;

    PmpSetup {
        pn_address,
        pn_keys,
        pmp_address,
        oracle_keys,
        oracle_list_hash,
        event_id,
        oracle_name,
        oracle_address,
    }
}
