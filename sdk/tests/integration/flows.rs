//! Multi-step user flows: recovery from seed, repeated gas top-up,
//! change-owner-then-stake.

use std::collections::HashMap;

use ackinacki_kit::contracts::dex::pmp::ParamsOfSubmitSetTimings;
use ackinacki_kit::contracts::dex::pmp::Pmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfChangeOwner;
use ackinacki_kit::contracts::dex::private_note::ParamsOfDeployPmp;
use ackinacki_kit::contracts::dex::private_note::ParamsOfSetStake;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPmpAddress;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_sdk::proof;

use crate::common::context::create_context;
use crate::common::context::create_dex;
use crate::common::context::CURRENCY_ID_NACKL;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::ECC_SHELL_DEPOSIT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::PMP_DEPOSIT;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::STAKE_PERIOD_LONG;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::context::VAULT_DEPOSIT;
use crate::common::keys::gen_keys;
use crate::common::misc::now_unix;
use crate::common::misc::pn_nackl;
use crate::common::misc::wait_active;
use crate::common::pmp::deploy_oracle_with_event;
use crate::common::pmp::setup_pmp;
use crate::common::pn::deploy_funded_pn;
use crate::common::pn::deploy_pn_with_keys;
use crate::common::pn::ensure_root_pn_funded;
use crate::common::voucher::make_voucher_proof;

#[tokio::test]
async fn test_recovery_and_operate_via_dex() {
    let context = create_context();
    let dex = create_dex();

    // Phase A: "old device" — deploy a PN with known keys (big enough for PMP)
    ensure_root_pn_funded(&context).await;
    let original_keys = gen_keys(context.clone());
    let (pn_address, _, _) = {
        let (addr, dih) = deploy_pn_with_keys(&context, &dex, &original_keys, PMP_DEPOSIT).await;

        // Fund with ECC shell + native gas
        let ecc_zk = make_voucher_proof(
            context.clone(),
            &original_keys.public,
            CURRENCY_ID_SHELL,
            ECC_SHELL_DEPOSIT,
            true,
        )
        .await;
        dex.send_ecc_shell(
            ParamsOfSendEccShellToPrivateNote {
                proof: ecc_zk.proof,
                nullifier_hash: proof::hex_u256_to_dec(&ecc_zk.deposit_identifier_hash_hex),
                deposit_identifier_hash: dih.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &ecc_zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&ecc_zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&ecc_zk.token_type_fr_hex),
                value: ecc_zk.voucher_value,
                layer_number: ecc_zk.layer_number,
                recipient_ephemeral_pubkey: proof::pubkey_to_dec(&original_keys.public),
            },
            Signer::Keys { keys: original_keys.clone() },
        )
        .await
        .expect("send_ecc_shell");
        send_currency_with_flag_from_default_giver(
            context.clone(),
            &addr,
            20_000_000_000,
            HashMap::new(),
            1,
        )
        .await
        .expect("giver fund recovered PN native gas");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        (addr, dih, original_keys.clone())
    };
    eprintln!("old device: PN deployed at {pn_address}");

    // Phase B: "new device" — only knows the pubkey, discovers the note
    let mut my_pubkeys = std::collections::HashSet::new();
    my_pubkeys.insert(proof::pubkey_to_dec(&original_keys.public));

    let discovered = dex.discover_my_notes(&my_pubkeys).await.expect("discover");
    let found = discovered.iter().find(|n| n.note_address == pn_address);
    assert!(found.is_some(), "should discover PN from pubkey");
    let found = found.unwrap();
    assert_eq!(found.token_type, Some(TOKEN_TYPE_NACKL));
    eprintln!("new device: found PN {} with token_type={:?}", found.note_address, found.token_type);

    // Phase C: operate on the recovered note — set up a PMP and stake
    // Need extra shell ECC on RootPN for the PN to deploy PMP
    let mut extra_ecc = HashMap::new();
    extra_ecc.insert(CURRENCY_ID_SHELL, ECC_SHELL_DEPOSIT * 2);
    extra_ecc.insert(CURRENCY_ID_NACKL, PMP_DEPOSIT * 2);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        50_000_000_000,
        extra_ecc,
        1,
    )
    .await
    .expect("giver top up RootPN ECC for recovery flow");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let (_, _, oracle_keys, event_id, oracle_name) =
        deploy_oracle_with_event(&context, &dex, "Recovery-").await;

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
        Signer::Keys { keys: original_keys.clone() },
    )
    .await
    .expect("deploy_pmp on recovered note");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let root_pn = RootPn::new_default(context.clone());
    let pmp_address = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: event_id.clone(),
            names: vec![oracle_name],
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .expect("pmp_address")
        .pmp_address;

    let pmp = Pmp::new(context.clone(), &pmp_address);
    wait_active(&pmp, "PMP").await;

    for _ in 0..30 {
        let d = dex.get_pmp_details(&pmp_address).await.expect("pmp");
        if d.approved_oracle_events >= d.number_of_oracle_events && d.number_of_oracle_events > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let details = dex.get_pmp_details(&pmp_address).await.expect("pmp");

    let result_start = now_unix() + STAKE_PERIOD_LONG;
    dex.submit_set_timings(
        &pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: oracle_keys },
    )
    .await
    .expect("set_timings");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    dex.set_stake(
        &pn_address,
        ParamsOfSetStake {
            event_id,
            oracle_list_hash: details.oracle_list_hash,
            token_type: TOKEN_TYPE_NACKL,
            outcome: 0,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: original_keys },
    )
    .await
    .expect("set_stake on recovered note");
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    eprintln!("recovery → operate: stake placed on recovered note");
}

#[tokio::test]
async fn test_repeated_gas_topup_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    let (pn_address, dih, keys) = deploy_funded_pn(&context, &dex, VAULT_DEPOSIT).await;

    // Need more Shell on RootPN before another top-up.
    let mut ecc = HashMap::new();
    ecc.insert(CURRENCY_ID_SHELL, ECC_SHELL_DEPOSIT * 2);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        2_000_000_000,
        ecc,
        1,
    )
    .await
    .expect("giver top up RootPN SHELL for repeat-topup");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // First top-up already done by deploy_funded_pn. Do a second one.
    let ecc_zk = make_voucher_proof(
        context.clone(),
        &keys.public,
        CURRENCY_ID_SHELL,
        ECC_SHELL_DEPOSIT,
        true,
    )
    .await;

    dex.send_ecc_shell(
        ParamsOfSendEccShellToPrivateNote {
            proof: ecc_zk.proof,
            nullifier_hash: proof::hex_u256_to_dec(&ecc_zk.deposit_identifier_hash_hex),
            deposit_identifier_hash: dih,
            final_layer_historical_hash_root: proof::hex_u256_to_dec(
                &ecc_zk.final_layer_historical_hash_root_hex,
            ),
            voucher_nominal_fr: proof::hex_u256_to_dec(&ecc_zk.voucher_nominal_fr_hex),
            token_type_fr: proof::hex_u256_to_dec(&ecc_zk.token_type_fr_hex),
            value: ecc_zk.voucher_value,
            layer_number: ecc_zk.layer_number,
            recipient_ephemeral_pubkey: proof::pubkey_to_dec(&keys.public),
        },
        Signer::Keys { keys: keys.clone() },
    )
    .await
    .expect("second send_ecc_shell");

    // Verify PN is still operational
    let details = dex.get_private_note_details(&pn_address).await.expect("details");
    assert_eq!(pn_nackl(&details), VAULT_DEPOSIT as u128);
    assert!(details.busy_address.is_none());
    eprintln!("repeated gas top-up OK");
}

#[tokio::test]
async fn test_change_owner_then_stake_via_dex() {
    let context = create_context();
    let dex = create_dex();
    let s = setup_pmp(&context, &dex).await;

    // Change owner to new keys
    let new_keys = gen_keys(context.clone());
    let new_pubkey_dec = proof::pubkey_to_dec(&new_keys.public);

    dex.change_owner(
        &s.pn_address,
        ParamsOfChangeOwner { new_pubkey: new_pubkey_dec },
        Signer::Keys { keys: s.pn_keys.clone() },
    )
    .await
    .expect("change_owner");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Old keys should NOT work for stake
    let result_start = now_unix() + STAKE_PERIOD_LONG;
    dex.submit_set_timings(
        &s.pmp_address,
        ParamsOfSubmitSetTimings { result_start },
        Signer::Keys { keys: s.oracle_keys.clone() },
    )
    .await
    .expect("set_timings");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let old_result = dex
        .set_stake(
            &s.pn_address,
            ParamsOfSetStake {
                event_id: s.event_id.clone(),
                oracle_list_hash: s.oracle_list_hash.clone(),
                token_type: TOKEN_TYPE_NACKL,
                outcome: 0,
                amount: STAKE_AMOUNT,
                use_coupon: false,
            },
            Signer::Keys { keys: s.pn_keys },
        )
        .await;
    assert!(old_result.is_err(), "old keys should be rejected after change_owner");
    eprintln!("old keys rejected as expected");

    // New keys SHOULD work
    dex.set_stake(
        &s.pn_address,
        ParamsOfSetStake {
            event_id: s.event_id,
            oracle_list_hash: s.oracle_list_hash,
            token_type: TOKEN_TYPE_NACKL,
            outcome: 0,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: new_keys },
    )
    .await
    .expect("stake with new keys");
    eprintln!("new keys accepted for stake");
}
