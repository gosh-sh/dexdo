// End-to-end CLOB-match smoke for the AI Registry inference market against a
// real shellnet, driven through `dodex_chain::Dex` (no DB, no HTTP).
//
// Flow (self-trade — one note is both maker and taker, like the reference
// `test_inference_events`):
//   1. note deploys the per-model InferenceOrderBook (internal message);
//   2. a standalone TokenContract is deployed externally + giver-funded (the
//      seller's per-deal escrow the offer points at);
//   3. the note posts a SELL offer backed by that TokenContract;
//   4. the note places a crossing BUY ⇒ the book matches and forwards the
//      SHELL to TokenContract.fundFromOrderBook;
//   5. assert the TokenContract is now funded and bound to the buyer note.
//
// This exercises the seller path + the matching engine + the TokenContract
// settlement decoders against a live contract. The TokenContract is deployed by
// an external message, so it is self-rooted (its `dapp_id` == its own account
// id) and funded with ECC SHELL via flag 16 — native value does not cross a
// dApp boundary on Acki Nacki (see `common::airegistry::deploy_token_contract`).
// The streaming settlement (open/advance/stop) and probe model need timed
// windows (600-1200s at these prices) and are covered separately.
//
//   cargo test -p dodex-api --test e2e_inference_match -- --ignored --nocapture
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
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

// A limit price must be a positive whole multiple of `PRICE_STEP` (1 SHELL =
// 1e9); the book rejects sub-SHELL dust with ERR_BAD_PARAM before assigning an
// order id, so a too-small price reads as "the order never rested".
const PRICE_PER_TICK: u128 = 1_000_000_000;
const OFFER_TICKS: u128 = 2;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires a reachable shellnet endpoint + seed_notes.json"]
async fn inference_offer_matches_buy_and_funds_token_contract() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    // Test isolation: own note per binary (shared notes leak stream/dispute locks).
    let note = pool.notes[6 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    let signer = || Signer::Keys { keys: keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    // The book ctor enforces `sha256(modelName) == _modelHash`; uniqueness now
    // rides the name (the hash is its preimage), not an arbitrary number.
    let model_name = format!("e2e-model--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!("[e2e_match] note={} model_name={model_name} model_hash={model_hash}", note.address);

    let mut failures: Vec<String> = Vec::new();

    // 1. Note deploys the per-model book.
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
    eprintln!("[e2e_match] order_book={ob}");
    wait_book_live(&dex, &ob).await;

    // 2. Deploy the seller's TokenContract externally (giver-funded). Self-trade
    //    ⇒ seller pubkey/note are this note; the root model is the canonical one
    //    the live SuperRoot derives, since the address must match what the note
    //    and the book recompute. postSellOffer addresses the TC by
    //    (seller key, nonce), so the offer must pass the SAME nonce it was
    //    deployed with.
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let tc = deploy_token_contract(
        dex.context(),
        &note.owner_public_key_hex,
        &note.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: OFFER_TICKS,
        },
        keys.clone(),
    )
    .await
    .expect("deploy TokenContract");
    eprintln!("[e2e_match] token_contract={tc}");

    // 3. Seller posts a SELL offer backed by the TokenContract.
    dex.post_sell_offer(&note.address, ParamsOfPostSellOffer { flags: 0, nonce }, signer())
        .await
        .expect("postSellOffer accepted");

    // Wait until the offer rests in the book.
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, POLL_TICKS, POLL_TICK).await {
        failures.push(diag);
    }

    // 4. Crossing BUY (taker): same price ⇒ matches the resting sell.
    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: OFFER_TICKS,
            // >= ticks * (price + 2.5% fee) = 2 * 1.025e9.
            escrow: 3_000_000_000,
            flags: 1,
            deadline: 0,
        },
        signer(),
    )
    .await
    .expect("placeInferenceBuy accepted");

    // 5. The match forwards SHELL to TokenContract.fundFromOrderBook.
    let mut funded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        match dex.token_contract_get_state(&tc).await {
            Ok(state) if state.funded => {
                eprintln!("[e2e_match] TokenContract funded: deposit={}", state.deposit);
                if state.deposit == 0 {
                    failures.push("funded TokenContract has zero deposit".to_string());
                }
                funded = true;
                break;
            }
            Ok(_) => eprintln!("[e2e_match] TokenContract not funded yet (retry)"),
            Err(err) => eprintln!("[e2e_match] getState errored (retry): {err:?}"),
        }
    }
    if !funded {
        failures.push("TokenContract was never funded by the match".to_string());
    } else if let Ok(parties) = dex.token_contract_get_parties(&tc).await {
        eprintln!(
            "[e2e_match] parties: buyer={} sellerNote={}",
            parties.buyer, parties.seller_note
        );
        if parties.buyer != note.address {
            failures.push(format!(
                "funded buyer should be the note: want {}, got {}",
                note.address, parties.buyer
            ));
        }
    }

    // 6. Cleanup: drop any of the note's resting orders on this book.
    let _ = dex
        .cancel_all_inference_orders(
            &note.address,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.clone() },
            signer(),
        )
        .await;

    assert!(failures.is_empty(), "e2e_match failures: {failures:#?}");
}

async fn wait_book_live(dex: &Dex, ob: &str) {
    wait_inference_book_live(dex, ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    eprintln!("[e2e_match] book live");
}
