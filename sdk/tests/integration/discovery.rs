//! PN discovery and aggregated-balance tests.

use std::collections::HashMap;

use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use dodex_contracts::dex::private_note::ParamsOfWithdrawTokens;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;

use crate::common::context::create_context;
use crate::common::context::create_dex;
use crate::common::context::CURRENCY_ID_NACKL;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::ECC_SHELL_DEPOSIT;
use crate::common::context::GIVER_ADDRESS;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::context::VAULT_DEPOSIT;
use crate::common::keys::gen_keys;
use crate::common::misc::wait_active;
use crate::common::pn::deploy_pn_with_keys;
use crate::common::pn::ensure_root_pn_funded;
use crate::common::voucher::make_voucher_proof;

#[tokio::test]
#[ignore = "requires a live network and a funded giver: mints its own notes from vouchers instead of renting from the stand pool"]
async fn test_aggregated_balance_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Deploy two PNs with different deposits
    let deposit_1: u64 = 100_000_000_000; // Nominal::N100 NACKL
    let deposit_2: u64 = 1_000_000_000_000; // Nominal::N1000 NACKL

    let keys1 = gen_keys(context.clone());
    let keys2 = gen_keys(context.clone());

    let zk1 =
        make_voucher_proof(context.clone(), &keys1.public, TOKEN_TYPE_NACKL, deposit_1, false)
            .await;
    let zk2 =
        make_voucher_proof(context.clone(), &keys2.public, TOKEN_TYPE_NACKL, deposit_2, false)
            .await;
    let dih1 = proof::hex_u256_to_dec(&zk1.deposit_identifier_hash_hex);
    let dih2 = proof::hex_u256_to_dec(&zk2.deposit_identifier_hash_hex);

    dex.deploy_private_note(
        ParamsOfDeployPrivateNote {
            zkproof: zk1.proof,
            deposit_identifier_hash: dih1.clone(),
            final_layer_historical_hash_root: proof::hex_u256_to_dec(
                &zk1.final_layer_historical_hash_root_hex,
            ),
            voucher_nominal_fr: proof::hex_u256_to_dec(&zk1.voucher_nominal_fr_hex),
            token_type_fr: proof::hex_u256_to_dec(&zk1.token_type_fr_hex),
            ephemeral_pubkey: proof::pubkey_to_dec(&keys1.public),
            value: zk1.voucher_value,
            token_type: zk1.voucher_token_type,
            layer_number: zk1.layer_number,
        },
        Signer::Keys { keys: keys1 },
    )
    .await
    .expect("deploy pn1");

    dex.deploy_private_note(
        ParamsOfDeployPrivateNote {
            zkproof: zk2.proof,
            deposit_identifier_hash: dih2.clone(),
            final_layer_historical_hash_root: proof::hex_u256_to_dec(
                &zk2.final_layer_historical_hash_root_hex,
            ),
            voucher_nominal_fr: proof::hex_u256_to_dec(&zk2.voucher_nominal_fr_hex),
            token_type_fr: proof::hex_u256_to_dec(&zk2.token_type_fr_hex),
            ephemeral_pubkey: proof::pubkey_to_dec(&keys2.public),
            value: zk2.voucher_value,
            token_type: zk2.voucher_token_type,
            layer_number: zk2.layer_number,
        },
        Signer::Keys { keys: keys2 },
    )
    .await
    .expect("deploy pn2");

    // Wait for both to deploy
    let addr1 = dex
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih1.clone(),
        })
        .await
        .expect("addr1");
    let addr2 = dex
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih2.clone(),
        })
        .await
        .expect("addr2");

    let pn1 = PrivateNote::new(context.clone(), dex_contract_params(&addr1));
    let pn2 = PrivateNote::new(context.clone(), dex_contract_params(&addr2));
    wait_active(&pn1, "PN1").await;
    wait_active(&pn2, "PN2").await;

    // Get aggregated balance
    let agg = dex.get_aggregated_balance(&[dih1, dih2]).await.expect("aggregated_balance");

    assert_eq!(agg.notes.len(), 2);
    let total_nackl = agg.totals.get(&TOKEN_TYPE_NACKL.to_string()).copied().unwrap_or_default();
    assert_eq!(total_nackl, (deposit_1 + deposit_2) as u128);
    eprintln!("Aggregated: {:?}", agg.totals);
}

#[tokio::test]
#[ignore = "requires a live network and a funded giver: mints its own notes from vouchers instead of renting from the stand pool"]
async fn test_discover_my_notes_via_dex() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Deploy a PN with known keys
    let keys = gen_keys(context.clone());
    let zk =
        make_voucher_proof(context.clone(), &keys.public, TOKEN_TYPE_NACKL, VAULT_DEPOSIT, false)
            .await;
    let dih_dec = proof::hex_u256_to_dec(&zk.deposit_identifier_hash_hex);
    let epk_dec = proof::pubkey_to_dec(&keys.public);

    dex.deploy_private_note(
        ParamsOfDeployPrivateNote {
            zkproof: zk.proof,
            deposit_identifier_hash: dih_dec.clone(),
            final_layer_historical_hash_root: proof::hex_u256_to_dec(
                &zk.final_layer_historical_hash_root_hex,
            ),
            voucher_nominal_fr: proof::hex_u256_to_dec(&zk.voucher_nominal_fr_hex),
            token_type_fr: proof::hex_u256_to_dec(&zk.token_type_fr_hex),
            ephemeral_pubkey: epk_dec.clone(),
            value: zk.voucher_value,
            token_type: zk.voucher_token_type,
            layer_number: zk.layer_number,
        },
        Signer::Keys { keys: keys.clone() },
    )
    .await
    .expect("deploy pn");

    let pn_address = dex
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih_dec.clone(),
        })
        .await
        .expect("addr");
    let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
    wait_active(&pn, "PN").await;

    // Discover: our key should find this PN
    let mut my_keys = std::collections::HashSet::new();
    my_keys.insert(epk_dec.clone());

    let discovered = dex.discover_my_notes(&my_keys).await.expect("discover");
    assert!(
        discovered.iter().any(|n| n.note_address == pn_address),
        "should discover our PN; found: {:?}",
        discovered,
    );

    // Random key should find nothing new
    let other_keys = gen_keys(context.clone());
    let mut other_set = std::collections::HashSet::new();
    other_set.insert(proof::pubkey_to_dec(&other_keys.public));

    let other_discovered = dex.discover_my_notes(&other_set).await.expect("discover other");
    assert!(
        !other_discovered.iter().any(|n| n.note_address == pn_address),
        "random key should not find our PN",
    );
}

#[tokio::test]
#[ignore = "requires a live network and a funded giver: mints its own notes from vouchers instead of renting from the stand pool"]
async fn test_discover_10_notes_some_withdrawn() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Top up RootPN with extra NACKL for 10 notes × 100 NACKL
    let mut ecc = HashMap::new();
    ecc.insert(CURRENCY_ID_NACKL, VAULT_DEPOSIT * 12);
    ecc.insert(CURRENCY_ID_SHELL, 50_000_000_000u64);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        100_000_000_000,
        ecc,
        1,
    )
    .await
    .expect("giver top up RootPN for 10-PN discovery");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Generate 10 keypairs (simulating derivation from seed)
    let all_keys: Vec<KeyPair> = (0..10).map(|_| gen_keys(context.clone())).collect();
    let pubkey_set: std::collections::HashSet<String> =
        all_keys.iter().map(|k| proof::pubkey_to_dec(&k.public)).collect();

    // Deploy 10 PNs
    let mut notes: Vec<(String, String, usize)> = Vec::new(); // (address, dih, index)
    for (i, keys) in all_keys.iter().enumerate() {
        let (addr, dih) = deploy_pn_with_keys(&context, &dex, keys, VAULT_DEPOSIT).await;
        eprintln!("PN[{i}] deployed: {addr}");
        notes.push((addr, dih, i));
    }

    // Withdraw from notes 2, 5, 8 (make them empty)
    let withdraw_indices = [2, 5, 8];
    for &i in &withdraw_indices {
        let (ref addr, _, idx) = notes[i];

        // Fund PN with ECC shell for gas (needed for withdraw internal msg)
        let ecc_zk = make_voucher_proof(
            context.clone(),
            &all_keys[i].public,
            CURRENCY_ID_SHELL,
            ECC_SHELL_DEPOSIT,
            true,
        )
        .await;
        let (_, ref dih, _) = notes[i];
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
                recipient_ephemeral_pubkey: proof::pubkey_to_dec(&all_keys[i].public),
            },
            Signer::Keys { keys: all_keys[i].clone() },
        )
        .await
        .expect("send_ecc_shell for withdraw");

        // Fund native gas
        send_currency_with_flag_from_default_giver(
            context.clone(),
            addr,
            5_000_000_000,
            HashMap::new(),
            1,
        )
        .await
        .expect("giver fund PN native gas for withdraw");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        dex.withdraw_tokens(
            addr,
            ParamsOfWithdrawTokens {
                dest_wallet_addr: GIVER_ADDRESS.to_string(),
                dapp_id: SystemDapp::System.dapp_id().to_string(),
            },
            Signer::Keys { keys: all_keys[i].clone() },
        )
        .await
        .expect("withdraw");
        eprintln!("PN[{idx}] withdrawn");
    }
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Discover all notes
    let discovered = dex.discover_my_notes(&pubkey_set).await.expect("discover");
    eprintln!("Discovered {} notes", discovered.len());
    assert_eq!(discovered.len(), 10, "should find all 10 notes");

    // Verify token_type
    let withdrawn_addrs: std::collections::HashSet<String> =
        withdraw_indices.iter().map(|&i| notes[i].0.clone()).collect();

    let mut with_balance = 0;
    let mut empty = 0;
    for note in &discovered {
        eprintln!(
            "  {} token_type={:?} initial_balance={} epk={}...",
            note.note_address,
            note.token_type,
            note.initial_balance,
            &note.ephemeral_pubkey[..8],
        );

        if withdrawn_addrs.contains(&note.note_address) {
            assert!(
                note.token_type.is_none(),
                "withdrawn PN {} should have token_type=None",
                note.note_address,
            );
            empty += 1;
        } else {
            assert_eq!(
                note.token_type,
                Some(TOKEN_TYPE_NACKL),
                "funded PN {} should have token_type=Some(1)",
                note.note_address,
            );
            with_balance += 1;
        }
    }
    assert_eq!(with_balance, 7, "7 notes with balance");
    assert_eq!(empty, 3, "3 withdrawn notes");
}
