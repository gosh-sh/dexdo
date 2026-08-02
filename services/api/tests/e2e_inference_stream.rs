// End-to-end streaming-settlement smoke for the AI Registry inference market
// against a real shellnet, driven through `dodex_chain::Dex` (no DB, no HTTP).
//
// Full deal lifecycle (self-trade — one note is maker, taker AND seller):
//   1. note deploys the per-model InferenceOrderBook;
//   2. a TokenContract is deployed externally (self-rooted, giver-funded);
//   3. the note posts a SELL offer and crosses it with a BUY ⇒ the book funds
//      the TokenContract (handover);
//   4. the giver posts the seller mirror bond (an internal SHELL message —
//      `open()` requires it, and an external call cannot carry currency);
//   5. seller `open()` freezes the probe tick;
//   6. after the probe window (a fixed 180s) of buyer silence, the seller
//      `advance()`s and the probe is accepted — `finalizedOwed` grows and
//      `probeAccepted` flips true;
//   7. buyer `streamStop()` closes the stream and settles.
//
// This proves the whole streaming state machine (open → advance → stop, the
// probe-tick money model in §3.1.2) against a live contract through our
// wrappers. It is SLOW: it sleeps out the real 180s on-chain probe window, so a
// run takes ~5 minutes.
//
//   cargo test -p dodex-api --test e2e_inference_stream -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::deploy_token_contract;
use common::airegistry::wait_inference_book_live;
use common::airegistry::wait_sell_offer_rested;
use common::airegistry::TokenDeal;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfPostSellerBond;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45;
// A limit price must be a positive whole multiple of `PRICE_STEP` (1 SHELL =
// 1e9), so 1 SHELL is the cheapest deal this test can open.
const PRICE_PER_TICK: u128 = 1_000_000_000;
const DEAL_TICKS: u128 = 4;
// Seller mirror bond = `TokenContract._bondAmount()` = 2P, plus a small margin.
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK + PRICE_PER_TICK / 100;
// What the FIRST `advance` has to wait out — which is not the per-deal
// streaming window this used to sleep.
//
// `advance` picks its window by phase: while the probe is unaccepted it uses
// the fixed `PROBE_WINDOW` (180s), and only afterwards do later advances use
// the per-deal `_settleWindow` = clamp(P*600/1e9, 180, 3600), which is 600s at
// this test's 1-SHELL price. The window is measured from `_prepaidTime`, which
// `open()` sets. This test never gets past the probe, so 600s was seven minutes
// of waiting for a gate that had already opened.
const PROBE_WAIT: Duration = Duration::from_secs(180 + 45);

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; sleeps out the 180s probe window (~5 min)"]
async fn inference_stream_open_advance_stop_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    // Test isolation: own note per binary (shared notes leak stream/dispute locks).
    let note = pool.notes[7 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    let signer = || Signer::Keys { keys: keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    // The book ctor enforces `sha256(modelName) == _modelHash`; uniqueness now
    // rides the name (the hash is its preimage), not an arbitrary number.
    let model_name = format!("e2e-stream--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!("[e2e_stream] note={} model_name={model_name} model_hash={model_hash}", note.address);

    let mut failures: Vec<String> = Vec::new();

    // 1-2. Book + TokenContract.
    dex.deploy_inference_order_book(
        &note.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        signer(),
    )
    .await
    .expect("deployInferenceOrderBook accepted");
    let ob = dex
        .get_inference_order_book_address(
            &note.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .expect("getInferenceOrderBookAddress");
    wait_book_live(&dex, &ob).await;

    // postSellOffer verifies token_contract derives from the seller key + this
    // nonce, so the offer must pass the SAME nonce the TokenContract was deployed with.
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let tc = deploy_token_contract(
        dex.context(),
        &note.owner_public_key_hex,
        &note.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: DEAL_TICKS,
        },
        keys.clone(),
    )
    .await
    .expect("deploy TokenContract");
    eprintln!("[e2e_stream] order_book={ob} token_contract={tc}");

    // 3. Offer ↔ buy ⇒ handover funds the TokenContract.
    dex.post_sell_offer(&note.address, ParamsOfPostSellOffer { flags: 0, nonce }, signer())
        .await
        .expect("postSellOffer accepted");
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, POLL_TICKS, POLL_TICK).await {
        failures.push(diag);
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: DEAL_TICKS,
            // >= ticks * (price + 2.5% fee) = 4 * 1.025e9 = 4.1e9.
            escrow: 6_000_000_000,
            flags: 1,
            deadline: 0,
        },
        signer(),
    )
    .await
    .expect("placeInferenceBuy accepted");

    if !wait_funded(&dex, &tc).await {
        failures.push("TokenContract was never funded by the match".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    // 4. Seller mirror bond, shipped from the note: the TC takes the bond from
    //    its `_sellerNote` and refuses every other sender.
    dex.post_seller_bond(
        &note.address,
        ParamsOfPostSellerBond { nonce, amount: SELLER_BOND },
        signer(),
    )
    .await
    .expect("postSellerBond accepted");
    let mut bond_funded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(bond) = dex.token_contract_get_seller_bond(&tc).await
            && bond.bond_funded
        {
            eprintln!("[e2e_stream] seller bond funded: held={}", bond.bond_held);
            bond_funded = true;
            break;
        }
    }
    if !bond_funded {
        failures.push("seller bond never registered (bondFunded stayed false)".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    // 5. Seller opens the stream (freezes the probe tick).
    dex.token_contract_open(&tc, ParamsOfOpen { endpoint_cipher: "00".to_string() }, signer())
        .await
        .expect("TokenContract.open accepted");
    let mut opened = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.opened
        {
            eprintln!("[e2e_stream] opened: frozen={} deposit={}", state.frozen, state.deposit);
            opened = true;
            break;
        }
    }
    if !opened {
        failures.push("stream never opened".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    // 6. Wait out the probe window, then accept the probe.
    eprintln!("[e2e_stream] sleeping {}s for the probe window…", PROBE_WAIT.as_secs());
    tokio::time::sleep(PROBE_WAIT).await;
    dex.token_contract_advance(&tc, signer()).await.expect("TokenContract.advance accepted");
    let mut accepted = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.probe_accepted
        {
            eprintln!(
                "[e2e_stream] probe accepted: finalizedOwed={} prepaid={} frozen={}",
                state.finalized_owed, state.prepaid, state.frozen
            );
            if state.finalized_owed == 0 {
                failures.push("probe accepted but finalizedOwed is 0".to_string());
            }
            accepted = true;
            break;
        }
    }
    if !accepted {
        failures.push("advance did not accept the probe within budget".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    // 7. Buyer stops the stream (clean close, standard split).
    dex.stream_stop(&note.address, ParamsOfStreamDeal { token_contract: tc.clone() }, signer())
        .await
        .expect("streamStop accepted");
    let mut stopped = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && !state.opened
        {
            eprintln!(
                "[e2e_stream] stopped: opened={} finalizedOwed={} ticksFinalized via fees",
                state.opened, state.finalized_owed
            );
            stopped = true;
            break;
        }
    }
    if !stopped {
        failures.push("stream never closed after streamStop".to_string());
    }

    finish(&dex, &note.address, &model_hash, &keys, failures).await;
}

async fn wait_book_live(dex: &Dex, ob: &str) {
    wait_inference_book_live(dex, ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
}

async fn wait_funded(dex: &Dex, tc: &str) -> bool {
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(tc).await
            && state.funded
        {
            eprintln!("[e2e_stream] TokenContract funded: deposit={}", state.deposit);
            return true;
        }
    }
    false
}

async fn finish(dex: &Dex, note: &str, model_hash: &str, keys: &KeyPair, failures: Vec<String>) {
    use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
    let _ = dex
        .cancel_all_inference_orders(
            note,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
            Signer::Keys { keys: keys.clone() },
        )
        .await;
    assert!(failures.is_empty(), "e2e_stream failures: {failures:#?}");
}
