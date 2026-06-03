//! Oracle deploy + event publish + PMP setup helpers.

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::contracts::dex::oracle::ParamsOfGetEventListAddress;
use ackinacki_kit::contracts::dex::oracle_event_list::ParamsOfAddEvent;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::root_oracle::ParamsOfDeployOracle;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::giver::v3::top_up_native_with_giver_if_below;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;
use dodex_sdk::Dex;

use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::PMP_DEPOSIT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::keys::gen_keys;
use crate::common::misc::event_entry_name;
use crate::common::misc::now_unix;
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
    use ackinacki_kit::contracts::dex::oracle::Oracle;
    use ackinacki_kit::contracts::dex::oracle_event_list::OracleEventList;
    use ackinacki_kit::contracts::dex::root_oracle::RootOracle;

    let oracle_keys = gen_keys(context.clone());
    let ephemeral_keys = gen_keys(context.clone());
    let run_id = now_unix();
    let oracle_name = format!("{prefix}{run_id:x}");

    // Top up RootOracle
    let root_oracle =
        RootOracle::new(context.clone(), dex_contract_params(RootOracle::DEFAULT_ADDRESS));
    wait_active(&root_oracle, "RootOracle").await;
    top_up_native_with_giver_if_below(
        context.clone(),
        &root_oracle,
        120_000_000_000,
        50_000_000_000,
        "RootOracle",
    )
    .await
    .expect("top up RootOracle native gas");

    dex.deploy_oracle(
        ParamsOfDeployOracle {
            oracle_pubkey: proof::pubkey_to_dec(&oracle_keys.public),
            oracle_name: oracle_name.clone(),
        },
        Signer::Keys { keys: ephemeral_keys },
    )
    .await
    .expect("deploy_oracle");

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
    outcomes.insert(1_u32, "Team A".to_string());
    outcomes.insert(2_u32, "Team B".to_string());

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
