// External-deploy helper for AI Registry contracts that are NOT deployed from
// a note via an internal message. Right now that is just `TokenContract` (the
// seller's per-deal escrow): the e2e CLOB-match flow needs one on-chain before
// a sell offer can reference it.
//
// The kit has no high-level "deploy from .tvc" entry point, so we drive the
// SDK primitives directly: derive the deterministic address from the
// DeploySet, create + fund it via the default giver, then deploy + wait Active.
// The book itself is still deployed by the note (`deployInferenceOrderBook`),
// so it needs no external deploy.
//
// Acki Nacki specifics that make this work:
//   * a freshly externally-deployed contract is the root of its own dApp
//     (`dapp_id == account_id`), so it is addressed via
//     `self_rooted_contract_params`, not the System dApp;
//   * native value does not cross a dApp boundary, so the giver credit must be
//     sent as ECC SHELL with flag 16 (which lands it as the new account's
//     native gas), not as native `value`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::contracts::event::query_events;
use ackinacki_kit::contracts::giver::send_currency_with_flag_from_default_giver;
use ackinacki_kit::contracts::giver::v3::GiverV3;
use ackinacki_kit::contracts::giver::v3::ParamsOfSendCurrencyWithBody;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::SendMessage;
use ackinacki_kit::tvm_client::abi::encode_message;
use ackinacki_kit::tvm_client::abi::encode_message_body;
use ackinacki_kit::tvm_client::abi::Abi;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::DeploySet;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessage;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessageBody;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use base64::Engine as _;
use dodex_chain::self_rooted_contract_params;
use dodex_contracts::airegistry::super_root::ParamsOfGetRootModelAddress;
use dodex_contracts::airegistry::super_root::SuperRoot;
use dodex_contracts::airegistry::token_contract::TokenContract;
use serde_json::json;

use super::e2e_setup::model_hash_dec;

const TOKEN_CONTRACT_TVC: &[u8] =
    include_bytes!("../../../../contracts/airegistry/TokenContract.tvc");
const TOKEN_CONTRACT_ABI: &str =
    include_str!("../../../../contracts/airegistry/TokenContract.abi.json");

/// ECC currency id for SHELL.
const SHELL_CURRENCY_ID: u32 = 2;
/// SHELL sent (as ECC, flag 16) to create + gas the fresh account. The ctor
/// self-mints its MIN_BALANCE via `gosh.mintshellq`, so the giver only needs to
/// cover deploy compute + the ctor's internal register-callback forward — keep
/// it modest so a run does not drain the shared shellnet giver.
const CREATION_SHELL: u64 = 200_000_000_000;

/// Canonical shellnet `SuperRoot` — the `SUPER_ROOT_ADDR` baked into the 4.0.x
/// `InferenceOrderBook` (`contracts/airegistry/InferenceOrderBook.sol`). A
/// SuperRoot-code / RootModel rotation re-pins this; keep it in sync with the
/// contract constant.
const SUPER_ROOT_ADDR: &str = "0:312d7665b9e262a2e5b2e77953912abf69c89f652562cc11ca97778a74c329cf";

/// Immutable deal config passed to the `TokenContract` constructor.
pub struct TokenDeal {
    pub model_name: String,
    pub tick_size: u128,
    pub price_per_tick: u128,
    pub max_ticks: u128,
}

/// Deterministic RootModel address for a seller pubkey (`0x…` hex), via the
/// canonical SuperRoot getter. This is the value the book pins as the deal
/// contract's `_rootModelAddress` when it recomputes
/// `_tokenContractAddr(sellerPubkey, nonce)`, so the TC must be deployed here.
async fn canonical_root_model_address(
    ctx: Arc<ClientContext>,
    seller_pubkey: &str,
) -> anyhow::Result<String> {
    SuperRoot::new(ctx, self_rooted_contract_params(SUPER_ROOT_ADDR))
        .get_root_model_address(ParamsOfGetRootModelAddress {
            owner_pubkey: seller_pubkey.to_string(),
        })
        .await
        .map(|r| r.address)
        .map_err(|e| anyhow!("getRootModelAddress({seller_pubkey}): {e:?}"))
}

/// Deploy a standalone `TokenContract` and return its address.
///
/// `seller_pubkey_hex` is the seller note's owner pubkey (hex, with or without
/// `0x`); it becomes the `_sellerPubkey` static so the note's key can sign the
/// owner ops (`open`/`advance`/…). `_rootModelAddress` is derived from the
/// canonical SuperRoot (not a placeholder): the book's `placeSellOffer` recomputes
/// the TC address from `(sellerPubkey, nonce)` pinned to that RootModel, so the
/// deployed TC must sit at the matching address or the offer reverts.
pub async fn deploy_token_contract(
    ctx: Arc<ClientContext>,
    seller_pubkey_hex: &str,
    seller_note_addr: &str,
    nonce: u64,
    deal: TokenDeal,
    deploy_keys: KeyPair,
) -> anyhow::Result<String> {
    let pubkey = format!("0x{}", seller_pubkey_hex.trim_start_matches("0x"));
    let abi = Abi::Json(TOKEN_CONTRACT_ABI.to_string());
    let root_model_addr = canonical_root_model_address(ctx.clone(), &pubkey).await?;

    let deploy_set = DeploySet {
        tvc: Some(base64::engine::general_purpose::STANDARD.encode(TOKEN_CONTRACT_TVC)),
        code: None,
        state_init: None,
        workchain_id: Some(0),
        initial_data: Some(json!({
            // ABI >= 2.4 requires the contract pubkey in initial_data; it matches
            // the deploy signer (the note key) so the deploy message verifies.
            "_pubkey": pubkey,
            "_sellerPubkey": pubkey,
            "_rootModelAddress": root_model_addr,
            "_nonce": nonce.to_string(),
        })),
        initial_pubkey: None,
    };

    let ctor = CallSet {
        function_name: "constructor".to_string(),
        header: None,
        input: Some(json!({
            "modelName": deal.model_name,
            // The ctor verifies `sha256(modelName) == modelHash`, so bind the hash
            // to the name's preimage — otherwise the deploy reverts and the TC is
            // never funded.
            "modelHash": model_hash_dec(&deal.model_name),
            "tickSize": deal.tick_size.to_string(),
            "pricePerTick": deal.price_per_tick.to_string(),
            "maxTicks": deal.max_ticks.to_string(),
            "sellerNote": seller_note_addr,
        })),
    };

    // 1. Derive the deterministic deploy address (address: None) so we can fund
    //    it before the constructor runs.
    let encoded = encode_message(
        ctx.clone(),
        ParamsOfEncodeMessage {
            abi,
            address: None,
            deploy_set: Some(deploy_set.clone()),
            call_set: Some(ctor.clone()),
            signer: Signer::Keys { keys: deploy_keys.clone() },
            processing_try_index: None,
            signature_id: None,
        },
    )
    .await
    .map_err(|e| anyhow!("encode TokenContract deploy: {e:?}"))?;
    let address = encoded.address;

    // 2-3. Create + fund the address under its own dApp (self-rooted). Native
    //    value would not cross the dApp boundary, so the giver credit goes as
    //    ECC SHELL with flag 16 (lands as native gas). Shellnet's giver can drop
    //    a message under load (BM ~3 RPS), so re-send until the account exists.
    let tc = TokenContract::new(ctx.clone(), self_rooted_contract_params(address.clone()));
    let mut landed = false;
    for attempt in 1..=3 {
        let mut ecc = HashMap::new();
        ecc.insert(SHELL_CURRENCY_ID, CREATION_SHELL);
        send_currency_with_flag_from_default_giver(ctx.clone(), &address, 0, ecc, 16)
            .await
            .map_err(|e| anyhow!("giver fund TokenContract {address}: {e:?}"))?;
        match tc
            .wait_account(ParamsOfWaitAccount {
                status: AccountStatus::Uninit,
                attempts: Some(30),
                attempts_timeout: Some(2_000),
            })
            .await
        {
            Ok(_) => {
                landed = true;
                break;
            }
            Err(e) => {
                eprintln!(
                    "[deploy_token_contract] giver credit attempt {attempt} not landed: {e:?}"
                )
            }
        }
    }
    if !landed {
        return Err(anyhow!("giver credit never landed on {address} after retries"));
    }

    // 4. Deploy the constructor (same precomputed address + deploy_set).
    tc.send_message(Some(ctor), Some(deploy_set), Signer::Keys { keys: deploy_keys })
        .await
        .map_err(|e| anyhow!("deploy TokenContract {address}: {e:?}"))?;

    // 5. Wait until it is Active before a sell offer references it.
    tc.wait_account(ParamsOfWaitAccount {
        status: AccountStatus::Active,
        attempts: Some(40),
        attempts_timeout: Some(2_000),
    })
    .await
    .map_err(|e| anyhow!("wait TokenContract {address} active: {e:?}"))?;

    Ok(address)
}

/// Post the seller probe commission to a `TokenContract` from the default giver.
///
/// `TokenContract.fundProbeCommission` only accepts an INTERNAL message that
/// carries ECC SHELL (an external signed call cannot carry currency, and
/// `open()` requires the commission already funded). Rather than stand up a
/// multisig just to send it, encode the call body and have the giver deliver
/// SHELL + that body in one internal message. The method has no sender guard.
pub async fn fund_probe_commission_via_giver(
    ctx: Arc<ClientContext>,
    token_contract_addr: &str,
    shell_amount: u64,
) -> anyhow::Result<()> {
    let body = encode_message_body(
        ctx.clone(),
        ParamsOfEncodeMessageBody {
            abi: Abi::Json(TOKEN_CONTRACT_ABI.to_string()),
            call_set: CallSet {
                function_name: "fundProbeCommission".to_string(),
                header: None,
                input: None,
            },
            is_internal: true,
            signer: Signer::None,
            processing_try_index: None,
            address: Some(token_contract_addr.to_string()),
            signature_id: None,
        },
    )
    .await
    .map_err(|e| anyhow!("encode fundProbeCommission body: {e:?}"))?
    .body;

    let mut ecc = HashMap::new();
    ecc.insert(SHELL_CURRENCY_ID, shell_amount);
    GiverV3::new_default(ctx)
        .send_currency_with_body(
            ParamsOfSendCurrencyWithBody {
                dest: token_contract_addr.to_string(),
                value: 1_000_000_000,
                ecc,
                flag: 1,
                body,
            },
            Signer::None,
        )
        .await
        .map_err(|e| anyhow!("giver fundProbeCommission → {token_contract_addr}: {e:?}"))?;
    Ok(())
}

/// Fetch the routing ids of the external events an `InferenceOrderBook` emitted.
/// Each ext-out event is routed to `makeAddrExtern(id)`, so the `dst` alone
/// identifies the event type (ids match `airegistry/modifiers/modifiers.sol`).
///
/// The body is intentionally NOT decoded here: typed payload decode of these
/// ext-out event bodies through the kit path (`decode_message_body`, the same
/// `is_internal:false, allow_partial:true` the dex events use) currently returns
/// tvm error 304 ("body does not match the specified ABI") against shellnet —
/// for every IOB event, including single-cell ones, so it is not the multi-cell
/// off-by-32 case. Tracked separately; the routing id still proves emission.
pub async fn fetch_inference_event_ids(
    ctx: Arc<ClientContext>,
    order_book_addr: &str,
) -> anyhow::Result<Vec<u128>> {
    let events = query_events(ctx, order_book_addr, SystemDapp::System.dapp_id(), Some(100))
        .await
        .map_err(|e| anyhow!("query inference events {order_book_addr}: {e:?}"))?;
    Ok(events
        .iter()
        .filter_map(|e| u128::from_str_radix(&e.dst.replace(':', ""), 16).ok())
        .collect())
}
