//! Basic PrivateNote operations: deploy, change owner, transfer, withdraw,
//! full lifecycle (deploy → stake → claim → withdraw → dead-note rejects
//! further stakes).

use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitResolve;
use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::private_note::ParamsOfChangeOwner;
use ackinacki_kit::contracts::dex::private_note::ParamsOfInitTransfer;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::private_note::ParamsOfStakeKey;
use ackinacki_kit::contracts::dex::private_note::ParamsOfWithdrawTokens;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_sdk::proof;

use crate::common::context::create_context;
use crate::common::context::create_dex;
use crate::common::context::GIVER_ADDRESS;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::STAKE_OUTCOME;
use crate::common::context::STAKE_PERIOD;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::context::VAULT_DEPOSIT;
use crate::common::keys::gen_keys;
use crate::common::misc::now_unix;
use crate::common::misc::pn_nackl;
use crate::common::pmp::setup_pmp;
use crate::common::pn::deploy_funded_pn;
use crate::common::pn::deploy_pn;
use crate::common::pn::ensure_root_pn_funded;

#[tokio::test]
async fn test_deploy_private_note_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Deploy without gas — PN exists and has balance, no shell needed
    let (pn_address, _, _) = deploy_pn(&context, &dex, VAULT_DEPOSIT).await;

    let details = dex.get_private_note_details(&pn_address).await.expect("get_details");
    assert_eq!(pn_nackl(&details), VAULT_DEPOSIT as u128);
    assert!(details.busy_address.is_none());
}

#[tokio::test]
async fn test_change_owner_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    let (pn_address, _, old_keys) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;

    let new_keys = gen_keys(context.clone());
    let new_pubkey_dec = proof::pubkey_to_dec(&new_keys.public);

    dex.change_owner(
        &pn_address,
        ParamsOfChangeOwner { new_pubkey: new_pubkey_dec.clone() },
        Signer::Keys { keys: old_keys.clone() },
    )
    .await
    .expect("change_owner");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let details = dex.get_private_note_details(&pn_address).await.expect("details");
    assert_eq!(
        proof::parse_u256(&details.ephemeral_pubkey),
        proof::parse_u256(&new_pubkey_dec),
        "ephemeral_pubkey must match new key",
    );
    assert!(details.busy_address.is_none());

    // Change back
    let orig_pubkey_dec = proof::pubkey_to_dec(&old_keys.public);
    dex.change_owner(
        &pn_address,
        ParamsOfChangeOwner { new_pubkey: orig_pubkey_dec },
        Signer::Keys { keys: new_keys },
    )
    .await
    .expect("change_owner back");
}

#[tokio::test]
async fn test_transfer_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    let (pn1_addr, _, pn1_keys) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;
    let (pn2_addr, pn2_dih, _) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;

    let bal1_before = pn_nackl(&dex.get_private_note_details(&pn1_addr).await.expect("pn1"));
    let bal2_before = pn_nackl(&dex.get_private_note_details(&pn2_addr).await.expect("pn2"));

    let transfer_amount: u128 = 10_000_000;
    dex.init_transfer(
        &pn1_addr,
        ParamsOfInitTransfer {
            dest_deposit_hash: pn2_dih,
            token_type: TOKEN_TYPE_NACKL,
            amount: transfer_amount,
        },
        Signer::Keys { keys: pn1_keys },
    )
    .await
    .expect("init_transfer");
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    let bal1_after = pn_nackl(&dex.get_private_note_details(&pn1_addr).await.expect("pn1 after"));
    let bal2_after = pn_nackl(&dex.get_private_note_details(&pn2_addr).await.expect("pn2 after"));
    assert_eq!(bal1_after, bal1_before - transfer_amount);
    assert_eq!(bal2_after, bal2_before + transfer_amount);
}

#[tokio::test]
async fn test_withdraw_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    let (pn_address, _, keys) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;

    let bal = pn_nackl(&dex.get_private_note_details(&pn_address).await.expect("pn"));
    assert!(bal > 0);

    dex.withdraw_tokens(
        &pn_address,
        ParamsOfWithdrawTokens {
            dest_wallet_addr: GIVER_ADDRESS.to_string(),
            dapp_id: SystemDapp::System.dapp_id().to_string(),
        },
        Signer::Keys { keys },
    )
    .await
    .expect("withdraw_tokens");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let after = dex.get_private_note_details(&pn_address).await.expect("pn after");
    assert_eq!(pn_nackl(&after), 0);
}

#[tokio::test]
async fn test_pn_full_lifecycle_via_dex() {
    let context = create_context();
    let dex = create_dex();
    let s = setup_pmp(&context, &dex).await;

    let result_start = now_unix() + STAKE_PERIOD;
    dex.submit_set_timings(
        &s.pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: s.oracle_keys.clone() },
    )
    .await
    .expect("set_timings");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Stake
    dex.set_stake(
        &s.pn_address,
        ParamsOfSetStake {
            event_id: s.event_id.clone(),
            oracle_list_hash: s.oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
            outcome: STAKE_OUTCOME,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("set_stake");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Wait + resolve
    let wait = result_start.saturating_sub(now_unix()) + 5;
    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;

    dex.submit_resolve(
        &s.pmp_address,
        ParamsOfSubmitResolve { outcome_id: STAKE_OUTCOME },
        Signer::Keys { keys: s.oracle_keys },
    )
    .await
    .expect("resolve");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Claim
    dex.claim(
        &s.pn_address,
        ParamsOfStakeKey {
            event_id: s.event_id.clone(),
            oracle_list_hash: s.oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
        },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("claim");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let bal = pn_nackl(&dex.get_private_note_details(&s.pn_address).await.expect("pn"));
    assert!(bal > 0, "PN should have balance after claim");

    // Withdraw — kills the note
    dex.withdraw_tokens(
        &s.pn_address,
        ParamsOfWithdrawTokens {
            dest_wallet_addr: GIVER_ADDRESS.to_string(),
            dapp_id: SystemDapp::System.dapp_id().to_string(),
        },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("withdraw");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let details = dex.get_private_note_details(&s.pn_address).await.expect("pn after withdraw");
    assert_eq!(pn_nackl(&details), 0, "balance should be 0 after withdraw");
    assert!(details.has_withdrawn, "has_withdrawn should be true");

    // Try to stake again — should fail (note is dead)
    let result = dex
        .set_stake(
            &s.pn_address,
            ParamsOfSetStake {
                event_id: s.event_id,
                oracle_list_hash: s.oracle_list_hash,
                token_type: TOKEN_TYPE_NACKL,
                outcome: 0,
                amount: STAKE_AMOUNT,
                use_coupon: false,
            },
            Signer::Keys { keys: s.pn_keys },
        )
        .await;
    assert!(result.is_err(), "dead note should reject set_stake");
    eprintln!("dead note correctly rejected: {}", result.unwrap_err());
}
