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
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::contracts::traits::SendMessage;
use ackinacki_kit::tvm_client::abi::encode_message;
use ackinacki_kit::tvm_client::abi::Abi;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::DeploySet;
use ackinacki_kit::tvm_client::abi::ParamsOfEncodeMessage;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::boc::decode_state_init;
use ackinacki_kit::tvm_client::boc::ParamsOfDecodeStateInit;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::ClientContext;
use anyhow::anyhow;
use base64::Engine as _;
use dodex_chain::self_rooted_contract_params;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetStats;
use dodex_contracts::airegistry::super_root::ParamsOfGetRootModelAddress;
use dodex_contracts::airegistry::super_root::SuperRoot;
use dodex_contracts::airegistry::token_contract::TokenContract;
use serde_json::json;

use super::e2e_setup::model_hash_dec;

const TOKEN_CONTRACT_TVC: &[u8] =
    include_bytes!("../../../../contracts/airegistry/TokenContract.tvc");
const TOKEN_CONTRACT_ABI: &str =
    include_str!("../../../../contracts/airegistry/TokenContract.abi.json");
/// The book image this checkout builds against. Only its code hash is used, and
/// that is computed from the artifact at run time rather than written out as a
/// literal, so it cannot drift from what the contracts build produced.
const INFERENCE_ORDER_BOOK_TVC: &[u8] =
    include_bytes!("../../../../contracts/airegistry/InferenceOrderBook.tvc");

/// TVM hash of the empty cell — `sha256(0x00, 0x00)`. A deploy applies its
/// StateInit before the constructor runs, so a StateInit whose code cell is
/// empty still creates and funds the account; it just reports this hash and has
/// nothing to execute, then or ever.
const EMPTY_CELL_HASH: &str = "96a296d224f285c67bee93c30f8a309157f0daa35dc5b87e410b78630a09cfc7";

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
const SUPER_ROOT_ADDR: &str = "0:0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c";

/// Wait for a freshly deployed book to answer its getters; on timeout, say WHY
/// it is dead instead of just reporting a timeout.
///
/// `note.deployInferenceOrderBook` sends `new InferenceOrderBook` with
/// `bounce:false`, and the book's ctor guards on the deployer being a canonical
/// note, so a refused deploy leaves nothing behind to observe — `getStats` just
/// keeps failing. The account's own code hash is what separates the causes.
pub async fn wait_inference_book_live(
    dex: &dodex_chain::Dex,
    order_book: &str,
    ticks: u32,
    tick: std::time::Duration,
) -> Result<ResultOfGetStats, String> {
    let mut last_err = "never polled".to_string();
    for _ in 0..ticks {
        tokio::time::sleep(tick).await;
        match dex.inference_get_stats(order_book).await {
            Ok(stats) => return Ok(stats),
            Err(err) => last_err = format!("{err:?}"),
        }
    }
    Err(format!(
        "InferenceOrderBook {order_book} did not become live within budget: {}; \
         last getStats error: {last_err}",
        diagnose_dead_book(dex, order_book).await
    ))
}

/// Name the reason a book never came up, from the one observable that survives
/// a `bounce:false` deploy: the account's code hash.
async fn diagnose_dead_book(dex: &dodex_chain::Dex, order_book: &str) -> String {
    let account = match dex.inference_book_account(order_book).await {
        Ok(account) => account,
        Err(err) => return format!("account unreadable: {err:?}"),
    };
    let acc_type = &account.acc_type;
    let balance = account.balance.as_deref().unwrap_or("?");
    let expected = expected_book_code_hash();
    match account.code_hash.as_deref() {
        // RootPN.onCodeUpgrade calls tvm.resetStorage() and restores only 6
        // codes; `_inferenceOrderBookCode` is deliberately left out to keep the
        // upgrade message under the gateway's body limit. Every RootPN upgrade
        // therefore wipes it, and every note minted afterwards bakes an empty
        // book code that deploys a codeless account at a well-formed address.
        Some(hash) if hash == EMPTY_CELL_HASH => format!(
            "the account exists (acc_type={acc_type}, balance={balance}) but was deployed with an \
             EMPTY code cell, so no constructor ever ran. The note's baked \
             `_inferenceOrderBookCode` is empty — a RootPN upgrade wiped it and was not followed \
             by RootPN.setInferenceOrderBookCode"
        ),
        Some(hash) if expected.as_deref().is_some_and(|want| want != hash) => format!(
            "the book runs code {hash}, but this checkout builds \
             {} — the note bakes a different InferenceOrderBook build",
            expected.as_deref().unwrap_or("?")
        ),
        Some(_) => format!(
            "the book carries the expected code (acc_type={acc_type}, balance={balance}) yet its \
             getters do not answer — the network, not the deploy, is the suspect"
        ),
        None => format!("the account carries no state at all (acc_type={acc_type})"),
    }
}

/// Code hash of the vendored `InferenceOrderBook.tvc`, or `None` if the
/// artifact cannot be parsed (in which case the caller reports what it saw and
/// skips the comparison rather than accusing the wrong side).
fn expected_book_code_hash() -> Option<String> {
    let ctx = Arc::new(ClientContext::new(Default::default()).ok()?);
    decode_state_init(
        ctx,
        ParamsOfDecodeStateInit {
            state_init: base64::engine::general_purpose::STANDARD.encode(INFERENCE_ORDER_BOOK_TVC),
            boc_cache: None,
        },
    )
    .ok()?
    .code_hash
}

/// Wait for a TC's sell offer to rest in the book; on timeout, say WHICH hop
/// dropped it instead of just reporting a timeout.
///
/// `note.postSellOffer` → `TokenContract.postFromNote` →
/// `InferenceOrderBook.placeSellOffer` → `TokenContract.onSellClosed` is
/// `bounce:false` end to end, so a refused offer surfaces nowhere: the book
/// simply never grows an order and every hop looks like it succeeded. The TC's
/// own `_offerPosted` latch is the only observable that separates them.
pub async fn wait_sell_offer_rested(
    dex: &dodex_chain::Dex,
    order_book: &str,
    token_contract: &str,
    ticks: u32,
    tick: std::time::Duration,
) -> Result<(), String> {
    for _ in 0..ticks {
        tokio::time::sleep(tick).await;
        if let Ok(stats) = dex.inference_get_stats(order_book).await
            && stats.order_count >= 1
        {
            return Ok(());
        }
    }
    let latch = match dex.token_contract_get_offer(token_contract).await {
        // The TC accepted `postFromNote` and forwarded to the book, but no order
        // rests: the book never answered, or it refused and `onSellClosed` has
        // not landed yet.
        Ok(o) if o.offer_posted => format!("offerPosted=true closing={}", o.closing),
        // Either `postFromNote` never took effect — its canonical-note guard
        // (`_sellerNote` vs the PN code hash pinned in the TC) rejects a note
        // built from different PrivateNote code — or the book refused the terms
        // and `onSellClosed` already cleared the latch.
        Ok(o) => format!("offerPosted=false closing={}", o.closing),
        // The note addressed a TC that is not the one deployed here.
        Err(err) => format!("getOffer unreadable: {err:?}"),
    };
    Err(format!("sell offer never rested in the book ({latch})"))
}

/// Immutable deal config passed to the `TokenContract` constructor.
///
/// These are also the terms the TC posts to the book: `postSellOffer` on the
/// note carries only `(flags, nonce)`, so an offer always rests at this
/// `price_per_tick` for this `max_ticks`.
pub struct TokenDeal {
    pub model_name: String,
    pub price_per_tick: u128,
    pub max_ticks: u128,
}

/// Deterministic RootModel address for a seller pubkey (`0x…` hex), via the
/// canonical SuperRoot getter. This is the value the book pins as the deal
/// contract's `_rootModelAddress` when it recomputes
/// `_tokenContractAddr(sellerPubkey, nonce)`, so the TC must be deployed here.
pub async fn canonical_root_model_address(
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

/// A `TokenContract` deploy worked out but not yet sent: the address it will
/// land on, and the two halves of the message that puts it there.
///
/// The address is derivable before anything is on chain, which is the whole
/// point — a cross-dApp deploy only activates on an address that ALREADY holds
/// SHELL, so whoever is paying has to know where to send it first.
pub struct TokenContractDeploy {
    pub address: String,
    deploy_set: DeploySet,
    ctor: CallSet,
    keys: KeyPair,
}

/// Work out where a `TokenContract` for `(seller pubkey, nonce)` will land and
/// what deploys it, without sending anything.
///
/// `seller_pubkey_hex` is the seller note's owner pubkey (hex, with or without
/// `0x`); it becomes the `_sellerPubkey` static so the note's key can sign the
/// owner ops (`open`/`advance`/…). `_rootModelAddress` is derived from the
/// canonical SuperRoot (not a placeholder): the book's `placeSellOffer` recomputes
/// the TC address from `(sellerPubkey, nonce)` pinned to that RootModel, so the
/// deployed TC must sit at the matching address or the offer reverts.
pub async fn plan_token_contract(
    ctx: Arc<ClientContext>,
    seller_pubkey_hex: &str,
    seller_note_addr: &str,
    nonce: u64,
    deal: TokenDeal,
    deploy_keys: KeyPair,
) -> anyhow::Result<TokenContractDeploy> {
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
            "pricePerTick": deal.price_per_tick.to_string(),
            "maxTicks": deal.max_ticks.to_string(),
            "sellerNote": seller_note_addr,
        })),
    };

    // Derive the deterministic deploy address (address: None) so whoever pays
    // for the account can fund it before the constructor runs.
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

    Ok(TokenContractDeploy { address: encoded.address, deploy_set, ctor, keys: deploy_keys })
}

impl TokenContractDeploy {
    /// Send the constructor to an address that is already funded, and wait for
    /// the account to come alive.
    ///
    /// Nothing here pays for the account: the caller has already put SHELL on
    /// the address by whatever route it chose. If none arrived, the deploy
    /// message has nothing to activate and this reports the wait that failed.
    pub async fn send(self, ctx: Arc<ClientContext>) -> anyhow::Result<String> {
        let tc = TokenContract::new(ctx, self_rooted_contract_params(self.address.clone()));
        tc.send_message(Some(self.ctor), Some(self.deploy_set), Signer::Keys { keys: self.keys })
            .await
            .map_err(|e| anyhow!("deploy TokenContract {}: {e:?}", self.address))?;
        tc.wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(40),
            attempts_timeout: Some(2_000),
        })
        .await
        .map_err(|e| anyhow!("wait TokenContract {} active: {e:?}", self.address))?;
        Ok(self.address)
    }
}

/// Deploy a standalone `TokenContract` off the shared giver and return its
/// address.
///
/// This is the ordinary route for a test that just needs a deal contract to
/// exist. The note-funded route the contracts are actually designed around —
/// `PrivateNote.fundDeployShell` — is exercised on its own in
/// `e2e_inference_funding`.
pub async fn deploy_token_contract(
    ctx: Arc<ClientContext>,
    seller_pubkey_hex: &str,
    seller_note_addr: &str,
    nonce: u64,
    deal: TokenDeal,
    deploy_keys: KeyPair,
) -> anyhow::Result<String> {
    let plan = plan_token_contract(
        ctx.clone(),
        seller_pubkey_hex,
        seller_note_addr,
        nonce,
        deal,
        deploy_keys,
    )
    .await?;

    // Create + fund the address under its own dApp (self-rooted). Native value
    // would not cross the dApp boundary, so the giver credit goes as ECC SHELL
    // with flag 16 (lands as native gas). Shellnet's giver can drop a message
    // under load (BM ~3 RPS), so re-send until the account exists.
    let tc = TokenContract::new(ctx.clone(), self_rooted_contract_params(plan.address.clone()));
    let mut landed = false;
    for attempt in 1..=3 {
        let mut ecc = HashMap::new();
        ecc.insert(SHELL_CURRENCY_ID, CREATION_SHELL);
        send_currency_with_flag_from_default_giver(ctx.clone(), &plan.address, 0, ecc, 16)
            .await
            .map_err(|e| anyhow!("giver fund TokenContract {}: {e:?}", plan.address))?;
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
        return Err(anyhow!("giver credit never landed on {} after retries", plan.address));
    }

    plan.send(ctx).await
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
    let events = query_events(ctx, order_book_addr, SystemDapp::Dex.dapp_id(), Some(100))
        .await
        .map_err(|e| anyhow!("query inference events {order_book_addr}: {e:?}"))?;
    Ok(events
        .iter()
        .filter_map(|e| u128::from_str_radix(&e.dst.replace(':', ""), 16).ok())
        .collect())
}
