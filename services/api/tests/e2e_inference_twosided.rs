// A streaming inference deal between two different notes, run until the money
// for it is gone.
//
// Every other inference test in this suite is a **self-trade**: one note is
// maker, taker and seller at once. That is enough to drive the state machine
// and not enough to say anything about the two parties being distinct — the
// deal's buyer and seller are the same account, so a contract that confused
// the two, paid the wrong side, or authorised one where it meant the other
// would look exactly the same from outside. Here they are two notes, and every
// call is made by whichever of them the contract requires:
//
//   * the **seller** deploys the book, owns the `TokenContract` (it derives
//     from the seller's key + nonce), posts the offer, posts the mirror bond,
//     `open()`s and `advance()`s — the last two are `onlyOwnerPubkey(seller)`;
//   * the **buyer** places the crossing buy, becomes the deal's `_buyer` when
//     the book hands the escrow over, and is the only account `stop()` accepts.
//
// ## Until the money is gone
//
// `maxTicks` is not a counter the stream checks — `advance` never looks at it.
// It bounds **funding**: `paid <= maxTicks * (P + fee)`, with a floor of two
// units. So a deal runs out of ticks because it runs out of deposit, and this
// test funds exactly two of them, which is the smallest deal the contract
// accepts and the smallest that has both phases:
//
//   unit = P + fee(P) = 1e9 + 2.5% = 1_025_000_000, paid = 2 * unit
//   open()            frozen = P,          deposit = 1.05e9
//   advance() #1      probe accepted, 1 tick finalized, fee taken,
//                     prepaid = P (deposit exactly covers P + fee),
//                     buffer not refilled (needs P + 2*fee)
//   advance() #2      the prepaid tick finalized, fee taken, deposit = 0
//
// Two ticks finalized, nothing left. That is the assertion: a deal pays out
// exactly what it was funded for, tick by tick, and stops.
//
// What is deliberately NOT asserted is the refusal of a third `advance`. It
// would be refused for having no prepaid tick — but only once its window had
// also elapsed, and until then the window guard refuses it first. Asserting the
// interesting refusal would mean waiting a second 600s window to tell it apart
// from the boring one, and a refusal read at the wrong moment is a statement
// about the clock rather than about the deal.
//
// ## Timing
//
// The two advances wait different windows, and the difference is the whole
// reason this test is affordable. The probe advance waits the fixed
// `PROBE_WINDOW` (180s); every later one waits the per-deal `_settleWindow` =
// clamp(P*600/1e9, 180, 3600), which is 600s at the minimum 1-SHELL price the
// book will take. So the run is ~13 minutes, almost all of it the second wait.
//
//   cargo test -p dodex-api --test e2e_inference_twosided -- --ignored --nocapture
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
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfPostSellerBond;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

/// The cheapest tick the book will price: a limit price has to be a positive
/// whole multiple of `PRICE_STEP`, which is one SHELL.
const PRICE_PER_TICK: u128 = 1_000_000_000;

/// Platform fee, buyer-side and by-fact — 2.5%, charged per finalized tick.
const FEE_BPS: u128 = 250;
const BPS: u128 = 10_000;

/// What one tick costs the buyer, which is the unit both funding bounds are
/// written in.
const UNIT: u128 = PRICE_PER_TICK + PRICE_PER_TICK * FEE_BPS / BPS;

/// The smallest deal the contract accepts: funding must cover at least two
/// units (a probe tick and at least one streaming tick) and at most
/// `maxTicks` of them, so at two ticks the two bounds meet and the deal is
/// funded exactly.
const DEAL_TICKS: u128 = 2;

const _: () = assert!(DEAL_TICKS == 2, "the arithmetic in the header is written for exactly two");

/// Seller mirror bond — `TokenContract._bondAmount()` = 2P, plus a margin.
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK + PRICE_PER_TICK / 100;

/// What the buy is willing to pay per tick. The book forwards what the fill
/// costs, not what the buyer offered, so this only has to be enough.
const BUY_ESCROW: u128 = 6_000_000_000;

const _: () = assert!(BUY_ESCROW >= DEAL_TICKS * UNIT);

/// The fixed window the probe advance waits, measured from `open()`.
const PROBE_WAIT: Duration = Duration::from_secs(180 + 45);

/// The per-deal window every later advance waits: clamp(P*600/1e9, 180, 3600),
/// which at this price is 600s. Nothing cheaper is available — the price floor
/// and the window slope meet at one SHELL.
const STREAM_WAIT: Duration = Duration::from_secs(600 + 45);

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn signer_of(note: &TestPn) -> Signer {
    Signer::Keys {
        keys: KeyPair {
            public: note.owner_public_key_hex.clone(),
            secret: note.owner_secret_key_hex.clone(),
        },
    }
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; sleeps out a 180s and a 600s window (~13 min)"]
async fn a_deal_between_two_notes_pays_out_every_tick_it_was_funded_for() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    // Two notes of this suite's own, and never the ones another binary uses: a
    // deal leaves its parties bound to a TokenContract, so a note shared with
    // another inference test would carry that binding into it.
    let seller = pool.notes[10 % pool.notes.len()].clone();
    let buyer = pool.notes[11 % pool.notes.len()].clone();
    assert_ne!(
        seller.address, buyer.address,
        "the pool handed out one note for both sides, so this would be another self-trade and \
         everything below would hold for a contract that cannot tell the parties apart"
    );

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    // The book ctor enforces `sha256(modelName) == _modelHash`, so uniqueness
    // rides the name and the hash is its preimage.
    let model_name = format!("e2e-twosided--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!(
        "[e2e_twosided] seller={} buyer={} model_name={model_name}",
        seller.address, buyer.address
    );

    let mut failures: Vec<String> = Vec::new();

    // ── the seller's book and the seller's deal ───────────────────────────
    dex.deploy_inference_order_book(
        &seller.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        signer_of(&seller),
    )
    .await
    .expect("deployInferenceOrderBook accepted");
    let ob = dex
        .get_inference_order_book_address(
            &seller.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .expect("getInferenceOrderBookAddress");
    wait_inference_book_live(&dex, &ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));

    // `postSellOffer` checks the TokenContract derives from the seller's key
    // and this nonce, so the offer has to carry the same nonce the contract was
    // deployed under.
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let tc = deploy_token_contract(
        dex.context(),
        &seller.owner_public_key_hex,
        &seller.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: DEAL_TICKS,
        },
        KeyPair {
            public: seller.owner_public_key_hex.clone(),
            secret: seller.owner_secret_key_hex.clone(),
        },
    )
    .await
    .expect("deploy TokenContract");
    eprintln!("[e2e_twosided] order_book={ob} token_contract={tc}");

    dex.post_sell_offer(
        &seller.address,
        ParamsOfPostSellOffer { flags: 0, nonce },
        signer_of(&seller),
    )
    .await
    .expect("postSellOffer accepted");
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, POLL_TICKS, POLL_TICK).await {
        failures.push(diag);
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // ── the other note's buy, which is what makes this a deal ─────────────
    dex.place_inference_buy(
        &buyer.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: DEAL_TICKS,
            escrow: BUY_ESCROW,
            flags: 1,
            deadline: 0,
        },
        signer_of(&buyer),
    )
    .await
    .expect("placeInferenceBuy accepted");

    let funded = poll_state(&dex, &tc, "funded", |s| s.funded).await;
    if !funded {
        failures.push("the match never handed the escrow to the TokenContract".to_string());
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // The deal now names two different accounts, which is the thing every other
    // inference test in this suite cannot say.
    match dex.token_contract_get_parties(&tc).await {
        Ok(parties) => {
            if parties.buyer != buyer.address {
                failures.push(format!(
                    "the deal's buyer is {}, not the note that placed the buy ({})",
                    parties.buyer, buyer.address
                ));
            }
            if parties.seller_note != seller.address {
                failures.push(format!(
                    "the deal's seller note is {}, not the note that posted the offer ({})",
                    parties.seller_note, seller.address
                ));
            }
        }
        Err(err) => failures.push(format!("getParties unreadable: {err:?}")),
    }
    if !failures.is_empty() {
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // ── the seller's bond, and the stream opened ──────────────────────────
    dex.post_seller_bond(
        &seller.address,
        ParamsOfPostSellerBond { nonce, amount: SELLER_BOND },
        signer_of(&seller),
    )
    .await
    .expect("postSellerBond accepted");
    let mut bonded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(bond) = dex.token_contract_get_seller_bond(&tc).await
            && bond.bond_funded
        {
            bonded = true;
            break;
        }
    }
    if !bonded {
        failures.push("the seller's mirror bond never registered".to_string());
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    dex.token_contract_open(
        &tc,
        ParamsOfOpen { endpoint_cipher: "00".to_string() },
        signer_of(&seller),
    )
    .await
    .expect("TokenContract.open accepted");
    if !poll_state(&dex, &tc, "opened", |s| s.opened).await {
        failures.push("the stream never opened".to_string());
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // ── tick one: the probe ───────────────────────────────────────────────
    eprintln!("[e2e_twosided] sleeping {}s for the probe window…", PROBE_WAIT.as_secs());
    tokio::time::sleep(PROBE_WAIT).await;
    dex.token_contract_advance(&tc, signer_of(&seller)).await.expect("advance accepted");
    if !poll_state(&dex, &tc, "probe accepted", |s| s.probe_accepted).await {
        failures.push("the probe was never accepted".to_string());
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }
    let after_probe = dex.token_contract_get_state(&tc).await.expect("state after the probe");
    eprintln!(
        "[e2e_twosided] probe: finalizedOwed={} prepaid={} frozen={} deposit={}",
        after_probe.finalized_owed, after_probe.prepaid, after_probe.frozen, after_probe.deposit
    );
    if after_probe.finalized_owed != PRICE_PER_TICK {
        failures.push(format!(
            "the probe tick finalized {} rather than one tick of {PRICE_PER_TICK}",
            after_probe.finalized_owed
        ));
    }
    if after_probe.prepaid != PRICE_PER_TICK {
        failures.push(format!(
            "the deposit did not prepay the second tick (prepaid={}); the deal was funded for \
             exactly two and the first left enough for the second",
            after_probe.prepaid
        ));
    }

    // ── tick two: the last one the funding bought ─────────────────────────
    eprintln!("[e2e_twosided] sleeping {}s for the streaming window…", STREAM_WAIT.as_secs());
    tokio::time::sleep(STREAM_WAIT).await;
    dex.token_contract_advance(&tc, signer_of(&seller)).await.expect("second advance accepted");
    let drained = poll_state(&dex, &tc, "second tick finalized", |s| {
        s.finalized_owed >= DEAL_TICKS * PRICE_PER_TICK
    })
    .await;
    let after_stream =
        dex.token_contract_get_state(&tc).await.expect("state after the second tick");
    eprintln!(
        "[e2e_twosided] streamed: finalizedOwed={} prepaid={} frozen={} deposit={}",
        after_stream.finalized_owed,
        after_stream.prepaid,
        after_stream.frozen,
        after_stream.deposit
    );
    if !drained {
        failures.push(format!(
            "the second tick never finalized: finalizedOwed={} after both advances",
            after_stream.finalized_owed
        ));
    } else {
        if after_stream.finalized_owed != DEAL_TICKS * PRICE_PER_TICK {
            failures.push(format!(
                "the deal paid {} for a funding of exactly {DEAL_TICKS} ticks",
                after_stream.finalized_owed
            ));
        }
        // Nothing left to serve: the funding bought two ticks and both are
        // spent, so neither the live tick nor the buffer nor the deposit can
        // carry a third.
        if after_stream.prepaid != 0 || after_stream.frozen != 0 || after_stream.deposit != 0 {
            failures.push(format!(
                "a deal funded for {DEAL_TICKS} ticks still holds escrow after both: prepaid={} \
                 frozen={} deposit={}",
                after_stream.prepaid, after_stream.frozen, after_stream.deposit
            ));
        }
    }

    // ── and the buyer, not the seller, is who may close it ────────────────
    dex.stream_stop(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: tc.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("streamStop accepted");
    if !poll_state(&dex, &tc, "closed", |s| !s.opened).await {
        failures.push("the buyer's stop never closed the stream".to_string());
    }

    finish(&dex, &seller, &buyer, &model_hash, failures).await;
}

/// Poll the deal's state until `probe` holds, naming what was waited for.
async fn poll_state<F>(dex: &Dex, tc: &str, what: &str, probe: F) -> bool
where
    F: Fn(&dodex_contracts::airegistry::token_contract::ResultOfGetState) -> bool,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(tc).await
            && probe(&state)
        {
            return true;
        }
    }
    eprintln!("[e2e_twosided] never reached: {what}");
    false
}

/// Take both notes' inference orders off the book whatever happened, then
/// report. A buy left resting outlives the test and the next run of it.
async fn finish(
    dex: &Dex,
    seller: &TestPn,
    buyer: &TestPn,
    model_hash: &str,
    failures: Vec<String>,
) {
    for note in [seller, buyer] {
        let _ = dex
            .cancel_all_inference_orders(
                &note.address,
                ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
                signer_of(note),
            )
            .await;
    }
    assert!(failures.is_empty(), "e2e_twosided failures: {failures:#?}");
}
