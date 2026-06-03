//! Cross-token-type test: each `PrivateNote` is single-token
//! (NACKL/SHELL/USDC), and a holder needs three independent PNs to hold three
//! currencies.

use std::collections::HashMap;

use ackinacki_kit::contracts::dex::private_note::PrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use ackinacki_kit::contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use ackinacki_kit::contracts::dex::root_pn::RootPn;
use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::tvm_client::abi::Signer;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;

use crate::common::context::create_context;
use crate::common::context::create_dex;
use crate::common::context::VAULT_DEPOSIT;
use crate::common::keys::gen_keys;
use crate::common::misc::wait_active;
use crate::common::pn::ensure_root_pn_funded;
use crate::common::voucher::make_voucher_proof;

#[tokio::test]
async fn test_one_pn_per_token_type() {
    let context = create_context();
    let dex = create_dex();
    ensure_root_pn_funded(&context).await;

    // Top up RootPN with SHELL and USDC too
    let mut ecc = HashMap::new();
    ecc.insert(1u32, VAULT_DEPOSIT * 3); // NACKL
    ecc.insert(2u32, VAULT_DEPOSIT * 3); // SHELL
    ecc.insert(3u32, 100_000_000u64 * 3); // USDC (6 decimals, N100 = 100_000_000)
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        50_000_000_000,
        ecc,
        1,
    )
    .await
    .expect("giver top up RootPN multi-token");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Deploy PN with NACKL
    let keys_nackl = gen_keys(context.clone());
    let (pn_nackl_addr, _, _) = {
        let zk =
            make_voucher_proof(context.clone(), &keys_nackl.public, 1, VAULT_DEPOSIT, false).await;
        let dih = proof::hex_u256_to_dec(&zk.deposit_identifier_hash_hex);
        let epk = proof::pubkey_to_dec(&keys_nackl.public);
        dex.deploy_private_note(
            ParamsOfDeployPrivateNote {
                zkproof: zk.proof,
                deposit_identifier_hash: dih.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&zk.token_type_fr_hex),
                ephemeral_pubkey: epk,
                value: zk.voucher_value,
                token_type: zk.voucher_token_type,
                layer_number: zk.layer_number,
            },
            Signer::Keys { keys: keys_nackl.clone() },
        )
        .await
        .expect("deploy NACKL PN");
        let addr = dex
            .get_private_note_address(ParamsOfGetPrivateNoteAddress {
                deposit_identifier_hash: dih.clone(),
            })
            .await
            .expect("nackl addr");
        let pn = PrivateNote::new(context.clone(), dex_contract_params(&addr));
        wait_active(&pn, "NACKL PN").await;
        (addr, dih, keys_nackl)
    };

    // Deploy PN with SHELL
    let keys_shell = gen_keys(context.clone());
    let (pn_shell_addr, _, _) = {
        let zk =
            make_voucher_proof(context.clone(), &keys_shell.public, 2, VAULT_DEPOSIT, false).await;
        let dih = proof::hex_u256_to_dec(&zk.deposit_identifier_hash_hex);
        let epk = proof::pubkey_to_dec(&keys_shell.public);
        dex.deploy_private_note(
            ParamsOfDeployPrivateNote {
                zkproof: zk.proof,
                deposit_identifier_hash: dih.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&zk.token_type_fr_hex),
                ephemeral_pubkey: epk,
                value: zk.voucher_value,
                token_type: zk.voucher_token_type,
                layer_number: zk.layer_number,
            },
            Signer::Keys { keys: keys_shell.clone() },
        )
        .await
        .expect("deploy SHELL PN");
        let addr = dex
            .get_private_note_address(ParamsOfGetPrivateNoteAddress {
                deposit_identifier_hash: dih.clone(),
            })
            .await
            .expect("shell addr");
        let pn = PrivateNote::new(context.clone(), dex_contract_params(&addr));
        wait_active(&pn, "SHELL PN").await;
        (addr, dih, keys_shell)
    };

    // Deploy PN with USDC
    let keys_usdc = gen_keys(context.clone());
    let (pn_usdc_addr, _, _) = {
        let zk =
            make_voucher_proof(context.clone(), &keys_usdc.public, 3, 100_000_000, false).await;
        let dih = proof::hex_u256_to_dec(&zk.deposit_identifier_hash_hex);
        let epk = proof::pubkey_to_dec(&keys_usdc.public);
        dex.deploy_private_note(
            ParamsOfDeployPrivateNote {
                zkproof: zk.proof,
                deposit_identifier_hash: dih.clone(),
                final_layer_historical_hash_root: proof::hex_u256_to_dec(
                    &zk.final_layer_historical_hash_root_hex,
                ),
                voucher_nominal_fr: proof::hex_u256_to_dec(&zk.voucher_nominal_fr_hex),
                token_type_fr: proof::hex_u256_to_dec(&zk.token_type_fr_hex),
                ephemeral_pubkey: epk,
                value: zk.voucher_value,
                token_type: zk.voucher_token_type,
                layer_number: zk.layer_number,
            },
            Signer::Keys { keys: keys_usdc.clone() },
        )
        .await
        .expect("deploy USDC PN");
        let addr = dex
            .get_private_note_address(ParamsOfGetPrivateNoteAddress {
                deposit_identifier_hash: dih.clone(),
            })
            .await
            .expect("usdc addr");
        let pn = PrivateNote::new(context.clone(), dex_contract_params(&addr));
        wait_active(&pn, "USDC PN").await;
        (addr, dih, keys_usdc)
    };

    // Verify each PN has only its token type
    let d_nackl = dex.get_private_note_details(&pn_nackl_addr).await.expect("nackl details");
    let d_shell = dex.get_private_note_details(&pn_shell_addr).await.expect("shell details");
    let d_usdc = dex.get_private_note_details(&pn_usdc_addr).await.expect("usdc details");

    assert_eq!(d_nackl.balance.get("1").copied().unwrap_or(0), VAULT_DEPOSIT as u128);
    assert_eq!(d_nackl.balance.get("2").copied().unwrap_or(0), 0);
    assert_eq!(d_nackl.balance.get("3").copied().unwrap_or(0), 0);

    assert_eq!(d_shell.balance.get("2").copied().unwrap_or(0), VAULT_DEPOSIT as u128);
    assert_eq!(d_shell.balance.get("1").copied().unwrap_or(0), 0);

    assert_eq!(d_usdc.balance.get("3").copied().unwrap_or(0), 100_000_000u128);
    assert_eq!(d_usdc.balance.get("1").copied().unwrap_or(0), 0);

    eprintln!(
        "3 PNs deployed: NACKL={}, SHELL={}, USDC={}",
        d_nackl.balance.get("1").unwrap_or(&0),
        d_shell.balance.get("2").unwrap_or(&0),
        d_usdc.balance.get("3").unwrap_or(&0),
    );
    eprintln!("One PN = one token type. Contract enforces msg.currencies.keys().length == 1");
}
