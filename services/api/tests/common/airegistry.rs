// Deal-contract helpers for the AI Registry e2e suites.
//
// Contracts 4.0.36 moved the deal on chain. A `TokenContract` is deployed by the
// seller's `PrivateNote` (`deployDeal`) as an internal message and lands in the
// note's DEX dApp, so it is addressed like any other DEX contract and its
// settlement events reach the indexer's existing capture stream.
//
// The external route this file used to drive is not merely deprecated, it is
// refused: the constructor requires `msg.sender` to be the canonical note for
// the deal's `depositIdentifierHash`, and an external message has no sender. So
// the giver-funded create-then-deploy dance is gone, and with it the flag-16
// send that existed only because a self-rooted account could not be reached
// with native value.
//
// What survives is the address derivation. The deal's three statics
// (`_sellerPubkey`, `_rootModelAddress`, `_nonce`) and its code are unchanged,
// so the address is still computable offline, before anything exists — which is
// what lets a test watch the address across the deploy that fills it.

#![allow(dead_code)]

use std::sync::Arc;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::dapp::SystemDapp;
use ackinacki_kit::contracts::event::query_events;
use ackinacki_kit::contracts::traits::AccountAccessor;
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
use dodex_chain::dex_contract_params;
use dodex_chain::self_rooted_contract_params;
use dodex_contracts::airegistry::inference_order_book::ResultOfGetStats;
use dodex_contracts::airegistry::super_root::ParamsOfGetRootModelAddress;
use dodex_contracts::airegistry::super_root::SuperRoot;
use dodex_contracts::airegistry::token_contract::TokenContract;
use dodex_contracts::dex::private_note::ParamsOfDeployDeal;
use dodex_contracts::dex::private_note::PrivateNote;
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

/// The ECC[2] reserve a deal deploy carries, in SHELL units (1 SHELL = 1e9).
///
/// `deployDeal`'s docstring budgets **0.300 + 0.015·T** for a plain run —
/// deploy, offer, match, open, probe, T claims, close, withdraw — and every
/// entrypoint burns its measured charge from this one pot. A suite that also
/// disputes or seller-stops pays a second terminal charge, so that is added
/// here rather than left for each test to remember.
///
/// Do not reach for the 0.240 that still appears in the contracts PR body: it
/// credited `fundDeal` with topping the reserve up (it does not — the ECC used
/// to convert to native on arrival and never reached it) and undercounts by
/// 0.070. The older 0.215 + 0.013·T and 0.210 + 0.015·T sized a mechanism in
/// which every entry drew on a native deposit instead.
const RESERVE_BASE: u128 = 300_000_000;
const RESERVE_PER_TICK: u128 = 15_000_000;
const RESERVE_SECOND_TERMINAL: u128 = 45_000_000;

/// Gas reserve to send with a deal of `max_ticks` ticks. See [`RESERVE_BASE`].
pub fn deal_gas_reserve(max_ticks: u128) -> u128 {
    RESERVE_BASE + RESERVE_PER_TICK * max_ticks + RESERVE_SECOND_TERMINAL
}

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
        Ok(o) if o.offer_posted => "offerPosted=true".to_string(),
        // Either `postFromNote` never took effect — its canonical-note guard
        // (`_sellerNote` vs the PN code hash pinned in the TC) rejects a note
        // built from different PrivateNote code — or the book refused the terms
        // and `onSellClosed` already cleared the latch.
        Ok(_) => "offerPosted=false".to_string(),
        // The note addressed a TC that is not the one deployed here.
        Err(err) => format!("getOffer unreadable: {err:?}"),
    };
    Err(format!("sell offer never rested in the book ({latch})"))
}

/// Immutable deal config passed to the `TokenContract` constructor.
///
/// These are also the terms the deal posts to the book: `postSellOffer` on the
/// note carries only `(flags, nonce, ttl)`, so an offer always rests at this
/// `price_per_tick` for this `max_ticks`. They are constructor arguments and
/// not part of the address — which is why the constructor authenticates its
/// sender, so nobody else can occupy the address with terms of their own.
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

/// Where the seller's deal for `(note key, nonce)` will land.
///
/// Derived offline from the deal's three statics plus the vendored
/// `TokenContract` code — the same inputs `DexLib.buildTokenContractStateInit`
/// uses inside the note, which is what makes the two derivations agree. The
/// note is never told this address, so an agreement between them is the only
/// evidence that a deploy went where it was meant to.
///
/// Constructor arguments do not enter a StateInit and so do not enter the
/// address. They are supplied anyway because `encode_message` will not encode a
/// deploy without them; only `.address` of the result is used, and the message
/// it signs is thrown away.
pub async fn canonical_deal_address(
    ctx: Arc<ClientContext>,
    seller_pubkey_hex: &str,
    seller_note_addr: &str,
    nonce: u64,
    deal: &TokenDeal,
    deploy_keys: KeyPair,
) -> anyhow::Result<String> {
    let pubkey = format!("0x{}", seller_pubkey_hex.trim_start_matches("0x"));
    let root_model_addr = canonical_root_model_address(ctx.clone(), &pubkey).await?;
    let deposit_hash = PrivateNote::new(ctx.clone(), dex_contract_params(seller_note_addr))
        .get_deposit_identifier_hash()
        .await
        .map_err(|e| anyhow!("read _depositIdentifierHash of {seller_note_addr}: {e:?}"))?
        .deposit_identifier_hash;

    let deploy_set = DeploySet {
        tvc: Some(base64::engine::general_purpose::STANDARD.encode(TOKEN_CONTRACT_TVC)),
        code: None,
        state_init: None,
        workchain_id: Some(0),
        initial_data: Some(json!({
            // ABI >= 2.4 requires the contract pubkey in initial_data. The deal
            // pins the seller key as both its `pubkey` and its `_sellerPubkey`
            // static, exactly as `buildTokenContractStateInit` does.
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
            "modelHash": model_hash_dec(&deal.model_name),
            "pricePerTick": deal.price_per_tick.to_string(),
            "maxTicks": deal.max_ticks.to_string(),
            "sellerNote": seller_note_addr,
            "depositIdentifierHash": deposit_hash,
        })),
    };

    let encoded = encode_message(
        ctx,
        ParamsOfEncodeMessage {
            abi: Abi::Json(TOKEN_CONTRACT_ABI.to_string()),
            address: None,
            deploy_set: Some(deploy_set),
            call_set: Some(ctor),
            signer: Signer::Keys { keys: deploy_keys },
            processing_try_index: None,
            signature_id: None,
        },
    )
    .await
    .map_err(|e| anyhow!("derive TokenContract address: {e:?}"))?;

    Ok(encoded.address)
}

/// Deploy the seller's deal from its own note and wait for it to come alive.
///
/// No giver anywhere: the note pays the reserve out of its own SHELL. The
/// deploy is `bounce: false` — there is no account at the target yet — so a
/// refusal surfaces only as an address that never goes Active, which is why the
/// timeout below carries a diagnosis rather than just a timeout.
pub async fn deploy_deal_from_note(
    dex: &dodex_chain::Dex,
    seller_note_addr: &str,
    seller_pubkey_hex: &str,
    nonce: u64,
    deal: TokenDeal,
    deploy_keys: KeyPair,
) -> anyhow::Result<String> {
    let ctx = dex.context();
    let address = canonical_deal_address(
        ctx.clone(),
        seller_pubkey_hex,
        seller_note_addr,
        nonce,
        &deal,
        deploy_keys.clone(),
    )
    .await?;

    dex.deploy_deal(
        seller_note_addr,
        ParamsOfDeployDeal {
            nonce,
            model_name: deal.model_name.clone(),
            // The ctor verifies `sha256(modelName) == modelHash`, so bind the
            // hash to the name's preimage or the deploy reverts.
            model_hash: model_hash_dec(&deal.model_name),
            price_per_tick: deal.price_per_tick,
            max_ticks: deal.max_ticks,
            gas_reserve: deal_gas_reserve(deal.max_ticks),
        },
        Signer::Keys { keys: deploy_keys },
    )
    .await
    .map_err(|e| anyhow!("deployDeal from {seller_note_addr}: {e:?}"))?;

    let tc = TokenContract::new(ctx, dex_contract_params(address.clone()));
    if let Err(err) = tc
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(45),
            attempts_timeout: Some(2_000),
        })
        .await
    {
        return Err(anyhow!(
            "the deal never came up at {address}: {err:?}; {}",
            diagnose_dead_deal(dex, &address).await
        ));
    }
    Ok(address)
}

/// Name the reason a deal never came up, from the one observable a
/// `bounce: false` deploy leaves behind: the account's code hash.
async fn diagnose_dead_deal(dex: &dodex_chain::Dex, address: &str) -> String {
    let account = match dex.dex_account_shell(address).await {
        Ok(account) => account,
        Err(err) => return format!("account unreadable: {err:?}"),
    };
    let acc_type = &account.acc_type;
    match account.code_hash.as_deref() {
        // Same failure the book has, one contract along. RootPN.onCodeUpgrade
        // calls tvm.resetStorage() and restores only 6 codes; `_tokenContractCode`
        // is set separately, so every RootPN upgrade wipes it and every note
        // minted afterwards bakes an empty deal code.
        Some(hash) if hash == EMPTY_CELL_HASH => format!(
            "the account exists (acc_type={acc_type}, {} native, {} shell) but was deployed with              an EMPTY code cell, so no constructor ever ran. The note's baked              `_tokenContractCode` is empty — RootPN was never given setTokenContractCode, or an              updateCode wiped it and it was not re-run",
            account.native, account.shell
        ),
        Some(hash) => format!(
            "the account runs code {hash} (acc_type={acc_type}) — something is deployed there, it              just is not answering"
        ),
        None => format!(
            "nothing was created at the address (acc_type={acc_type}): the note never sent the              deploy. `deployDeal` is owner-gated and refuses a note that has withdrawn, and the              deploy itself is bounce:false, so a refusal leaves no other trace"
        ),
    }
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
