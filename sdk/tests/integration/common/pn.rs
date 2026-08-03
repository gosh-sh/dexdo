//! PrivateNote deploy / gas-funding helpers.

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::contracts::giver::v3::send_currency_with_flag_from_default_giver;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfDeployPrivateNote;
use dodex_contracts::dex::root_pn::ParamsOfGetPrivateNoteAddress;
use dodex_contracts::dex::root_pn::ParamsOfSendEccShellToPrivateNote;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::proof;
use dodex_sdk::Dex;

use crate::common::context::CURRENCY_ID_NACKL;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::ECC_SHELL_DEPOSIT;
use crate::common::context::PMP_DEPOSIT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::keys::gen_keys;
use crate::common::misc::ensure_native_gas;
use crate::common::misc::wait_active;
use crate::common::voucher::make_voucher_proof;

/// Deploy a PN via Dex (deposit only, no gas). Returns (address, dih_dec,
/// keys).
pub async fn deploy_pn(
    context: &Arc<ClientContext>,
    dex: &Dex,
    deposit: u64,
) -> (String, String, KeyPair) {
    let keys = gen_keys(context.clone());
    let signer = Signer::Keys { keys: keys.clone() };

    let zk =
        make_voucher_proof(context.clone(), &keys.public, TOKEN_TYPE_NACKL, deposit, false).await;
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
            ephemeral_pubkey: epk_dec,
            value: zk.voucher_value,
            token_type: zk.voucher_token_type,
            layer_number: zk.layer_number,
        },
        signer,
    )
    .await
    .expect("deploy_private_note");

    let pn_address = dex
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih_dec.clone(),
        })
        .await
        .expect("get_private_note_address");

    let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
    wait_active(&pn, "PrivateNote").await;

    (pn_address, dih_dec, keys)
}

/// Deploy a PN with a specific keypair (not random). Returns (address,
/// dih_dec).
pub async fn deploy_pn_with_keys(
    context: &Arc<ClientContext>,
    dex: &Dex,
    keys: &KeyPair,
    deposit: u64,
) -> (String, String) {
    let signer = Signer::Keys { keys: keys.clone() };

    let zk =
        make_voucher_proof(context.clone(), &keys.public, TOKEN_TYPE_NACKL, deposit, false).await;
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
            ephemeral_pubkey: epk_dec,
            value: zk.voucher_value,
            token_type: zk.voucher_token_type,
            layer_number: zk.layer_number,
        },
        signer,
    )
    .await
    .expect("deploy_pn_with_keys");

    let pn_address = dex
        .get_private_note_address(ParamsOfGetPrivateNoteAddress {
            deposit_identifier_hash: dih_dec.clone(),
        })
        .await
        .expect("get_pn_address");

    let pn = PrivateNote::new(context.clone(), dex_contract_params(&pn_address));
    wait_active(&pn, "PN").await;

    (pn_address, dih_dec)
}

/// Fund PN with Shell gas (ECC shell) + native gas. Call before write
/// operations.
pub async fn fund_pn_gas(
    context: &Arc<ClientContext>,
    dex: &Dex,
    pn_address: &str,
    dih_dec: &str,
    keys: &KeyPair,
    shell_amount: u64,
) {
    let ecc_zk =
        make_voucher_proof(context.clone(), &keys.public, CURRENCY_ID_SHELL, shell_amount, true)
            .await;
    dex.send_ecc_shell(
        ParamsOfSendEccShellToPrivateNote {
            proof: ecc_zk.proof,
            nullifier_hash: proof::hex_u256_to_dec(&ecc_zk.deposit_identifier_hash_hex),
            deposit_identifier_hash: dih_dec.to_string(),
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
    .expect("send_ecc_shell");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    send_currency_with_flag_from_default_giver(
        context.clone(),
        pn_address,
        20_000_000_000,
        HashMap::new(),
        1,
    )
    .await
    .expect("giver fund PN native gas");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
}

/// Deploy a PN + fund gas. **Halo2 voucher proofs MUST be minted sequentially**
/// — the proof commits to `final_layer_historical_hash_root` at generation
/// time, and the contract validates it against the *current* root at submit.
/// If we mint two vouchers in parallel, the second one's root drifts during
/// the first one's submit + wait_active (~10-15 sec, 2-3 blocks) and the
/// contract rejects with `ERR_INVALID_ZKPROOF` (137).
pub async fn deploy_funded_pn(
    context: &Arc<ClientContext>,
    dex: &Dex,
    deposit: u64,
) -> (String, String, KeyPair) {
    let (pn_address, dih_dec, keys) = deploy_pn(context, dex, deposit).await;
    fund_pn_gas(context, dex, &pn_address, &dih_dec, &keys, ECC_SHELL_DEPOSIT).await;
    (pn_address, dih_dec, keys)
}

/// Top up RootPN with NACKL + Shell ECC for test operations.
pub async fn ensure_root_pn_funded(context: &Arc<ClientContext>) {
    let root_pn = RootPn::new(context.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    wait_active(&root_pn, "RootPN").await;
    ensure_native_gas(context.clone(), &root_pn, 120_000_000_000, 50_000_000_000, "RootPN").await;
    let mut ecc = HashMap::new();
    ecc.insert(CURRENCY_ID_NACKL, PMP_DEPOSIT * 2);
    ecc.insert(CURRENCY_ID_SHELL, ECC_SHELL_DEPOSIT * 2);
    send_currency_with_flag_from_default_giver(
        context.clone(),
        RootPn::DEFAULT_ADDRESS,
        50_000_000_000,
        ecc,
        1,
    )
    .await
    .expect("giver top up RootPN ECC");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
}
