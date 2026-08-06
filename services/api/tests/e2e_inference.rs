// End-to-end smoke test for the AI Registry inference order book against a
// real shellnet, driven directly through `dodex_chain::Dex` (no DB, no HTTP
// router — there are no inference request handlers yet).
//
// The note is the on-chain participant: it deploys a fresh per-model
// `InferenceOrderBook` (the book code is baked into the note at deploy), then
// places a resting BUY with SHELL escrow and cancels it. Asserts read the
// book back through the `InferenceOrderBook` getter wrappers, so this also
// exercises our airegistry result decoders against a live contract.
//
// Scope: the buy-side CLOB slice only. The seller match / streaming-deal /
// probe flows need `TokenContract` deploys (external deploy + giver funding),
// which the e2e harness does not have yet — those are a separate follow-up.
//
// Marked `#[ignore]`: needs a reachable shellnet endpoint and a seed-note pool
// whose notes hold native gas (+ SHELL for the escrow phase).
//
//   cargo test -p dodex-api --test e2e_inference -- --ignored --nocapture
//
// Endpoint: `E2E_NETWORK_ENDPOINT` or the default shellnet.
// Notes:    `E2E_SEED_NOTES` (seed_notes.json format) or the bundled fixture.
//
// === SECURITY NOTE ===
// The seed-notes file ships plaintext `pn_seckey_hex` for shellnet-only
// throwaway PNs. Safe ONLY because shellnet is a public devnet and the keys
// are not reused elsewhere.

mod common;

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::wait_inference_book_live;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget — book deploy is an internal message.

/// A per-run model name so each run deploys a fresh book — its address derives
/// from `sha256(modelName)`, so a unique name keeps order-count assertions clean.
/// The ctor enforces `sha256(modelName) == _modelHash`, so the hash must be
/// `model_hash_dec(&this)`, never an arbitrary value.
fn unique_model_name() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("e2e--{nanos}")
}

#[tokio::test]
#[ignore = "requires a reachable shellnet endpoint + seed_notes.json"]
async fn inference_order_book_buy_then_cancel_against_shellnet() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load_inference();
    // Test isolation: own note per binary (shared notes leak stream/dispute locks).
    let note = pool.notes[9 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    let signer = || Signer::Keys { keys: keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let model_name = unique_model_name();
    let model_hash = model_hash_dec(&model_name);
    eprintln!(
        "[e2e_inference] note={} model_name={model_name} model_hash={model_hash}",
        note.address
    );

    let mut failures: Vec<String> = Vec::new();

    // 1. Note deploys the per-model InferenceOrderBook (internal message).
    dex.deploy_inference_order_book(
        &note.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        signer(),
    )
    .await
    .expect("deployInferenceOrderBook external call accepted");

    // 2. Derive the deterministic book address from the note.
    let ob = dex
        .get_inference_order_book_address(
            &note.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .expect("getInferenceOrderBookAddress");
    eprintln!("[e2e_inference] order_book={ob}");

    // 3. Wait until the book is live: its getters answer once it is Active.
    let stats = wait_inference_book_live(&dex, &ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    eprintln!(
        "[e2e_inference] book live: order_count={} next_order_id={}",
        stats.order_count, stats.next_order_id
    );
    if stats.order_count != 0 {
        failures.push(format!("fresh book should have order_count=0, got {}", stats.order_count));
    }

    // 4. Place a resting BUY (flags=0 = limit/rest; no offers ⇒ it just rests).
    //    Escrow is physical SHELL held by the note.
    // Two ticks is the book's minimum: a deal serves a probe tick plus at least
    // one stream tick, so `placeBuyOrder` rejects `ticks < 2` outright.
    let ticks: u128 = 2;
    // A limit price must be a positive whole multiple of PRICE_STEP (1 SHELL =
    // 1e9); `placeBuyOrder` rejects sub-SHELL dust with ERR_BAD_PARAM before the
    // order is ever assigned an id, so a too-small price shows up as "the buy
    // never rested" rather than as a placement error. 1 SHELL is the minimum.
    const PRICE_PER_TICK: u128 = 1_000_000_000;
    // The book requires escrow >= ticks * (price + 2.5% platform fee); leave a
    // margin above the exact 2 * 1.025e9 so a fee-constant change does not turn
    // this into a silent non-placement again.
    const BUY_ESCROW: u128 = 3_000_000_000;
    let place = dex
        .place_inference_buy(
            &note.address,
            ParamsOfPlaceInferenceBuy {
                model_hash: model_hash.clone(),
                max_price_per_tick: PRICE_PER_TICK,
                ticks,
                escrow: BUY_ESCROW,
                flags: 0,
                deadline: 0,
            },
            signer(),
        )
        .await;
    let place_ok = place.is_ok();
    if let Err(err) = place {
        failures.push(format!("placeInferenceBuy external call failed: {err:?}"));
    }

    // 5. Poll until the resting buy surfaces, then assert its shape.
    let mut order_id: Option<u128> = None;
    if place_ok {
        for _ in 0..POLL_TICKS {
            tokio::time::sleep(POLL_TICK).await;
            match dex.inference_get_stats(&ob).await {
                Ok(stats) if stats.order_count >= 1 => {
                    // First (and only) order id on a fresh book.
                    order_id = Some(stats.next_order_id.saturating_sub(1).max(1));
                    eprintln!("[e2e_inference] resting order surfaced: id={:?}", order_id);
                    break;
                }
                Ok(_) => eprintln!("[e2e_inference] buy not resting yet (retry)"),
                Err(err) => eprintln!("[e2e_inference] getStats errored (retry): {err:?}"),
            }
        }
        match order_id {
            None => failures.push("resting buy never surfaced in getStats".to_string()),
            Some(id) => match dex.inference_get_order(&ob, id).await {
                Ok(order) => {
                    if !order.is_buy {
                        failures.push(format!("order {id} should be a buy"));
                    }
                    if order.amount != ticks {
                        failures
                            .push(format!("order {id} amount: want {ticks}, got {}", order.amount));
                    }
                }
                Err(err) => failures.push(format!("getOrder({id}) failed: {err:?}")),
            },
        }
    }

    // 6. Cleanup: cancel all of the note's orders on this book, confirm drained.
    if let Err(err) = dex
        .cancel_all_inference_orders(
            &note.address,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.clone() },
            signer(),
        )
        .await
    {
        eprintln!("[e2e_inference] cleanup cancelAll failed (best-effort): {err:?}");
    } else {
        for _ in 0..POLL_TICKS {
            tokio::time::sleep(POLL_TICK).await;
            let Ok(stats) = dex.inference_get_stats(&ob).await else { continue };
            if stats.order_count == 0 {
                eprintln!("[e2e_inference] book drained after cancelAll");
                break;
            }
        }
    }

    assert!(failures.is_empty(), "e2e_inference failures: {failures:#?}");
}
