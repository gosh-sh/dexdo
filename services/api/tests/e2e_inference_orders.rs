// What an inference book keeps, what it gives back one order at a time, and
// what it never takes at all.
//
// Everything else in this suite drives a book towards a deal: an offer, a
// crossing buy, a TokenContract, a stream. None of it says anything about the
// book as an order book — that two resting orders are two, that cancelling one
// leaves the other, or that a buy the book refuses costs its note nothing.
// This one never opens a deal, so it needs no TokenContract, no bond and none
// of the streaming windows that make the rest of the inference suite slow.
//
// ## Cancelling one of two
//
// Every inference test so far cancels with `cancelAllInferenceOrders`, which
// cannot tell "the right one went" from "everything went". So this rests two
// buys at different prices and cancels the better one by id. Three readings
// follow, and each would survive the wrong one going if it were alone:
//
//   * the book's order count drops by exactly one;
//   * the cancelled id reads back with amount 0 — the book's own marker for an
//     emptied slot — while the other still holds its size;
//   * the best bid falls from the cancelled price to the surviving one.
//
// ## A deadline already past
//
// `placeBuyOrder` requires `deadline == 0 || deadline > block.timestamp`, and
// that `require` sits BEFORE `tvm.accept()`. The note does not duplicate the
// check: it forwards with `bounce: true` precisely so a placement the book
// refuses bounces the SHELL escrow straight back rather than stranding it.
//
// So the refusal is read the way every refusal in this repo is read — by what
// did not happen — with the book's own id counter as the discriminator.
// `next_order_id` advances only once a placement is past validation and into
// the queue, so a refused buy leaves it where it was. That is the same shape
// as `_opNonce` on a note: a counter that moves exactly where the thing under
// test succeeded, and nowhere else.
//
// The control is the pair of buys above: they advance the counter and the
// count, which is what makes their standing still afterwards mean something.
//
//   cargo test -p dodex-api --test e2e_inference_orders -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

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
use dodex_contracts::dex::private_note::ParamsOfCancelInferenceOrder;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

/// One SHELL — the book's `PRICE_STEP`, and so the smallest price it prices.
const SHELL: u128 = 1_000_000_000;

/// Platform fee, buyer-side: 2.5%. `_unit(P) = P + fee(P)` is what a tick costs
/// and what the escrow floor is written in.
const FEE_BPS: u128 = 250;
const BPS: u128 = 10_000;

const fn unit(price: u128) -> u128 {
    price + price * FEE_BPS / BPS
}

/// A buy has to be for at least two ticks: a deal serves a probe tick and at
/// least one streaming tick, so the book rejects a one-tick order up front.
const TICKS: u128 = 2;

/// The two resting bids. Different prices, so which of them is gone after the
/// cancel is visible in the best bid and not only in the order count.
const BETTER_PRICE: u128 = 2 * SHELL;
const WORSE_PRICE: u128 = SHELL;

const _: () = assert!(BETTER_PRICE > WORSE_PRICE);
const _: () = assert!(BETTER_PRICE.is_multiple_of(SHELL) && WORSE_PRICE.is_multiple_of(SHELL));

/// Escrow for each: the book requires it to cover `ticks * unit(price)` in
/// full, and there is no ceiling at placement — the upper bound belongs to
/// funding a deal, which this test never reaches.
const BETTER_ESCROW: u128 = 5 * SHELL;
const WORSE_ESCROW: u128 = 3 * SHELL;

const _: () = assert!(BETTER_ESCROW >= TICKS * unit(BETTER_PRICE));
const _: () = assert!(WORSE_ESCROW >= TICKS * unit(WORSE_PRICE));

/// How far in the past the refused buy's deadline sits. Only has to be past;
/// the margin is for the chain's clock, which is what the contract compares
/// against rather than this host's.
const STALE_BY: u64 = 120;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_book_cancels_the_order_it_was_asked_to_and_never_takes_an_expired_one() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    // Own note per binary, like every other inference test here.
    let note = pool.notes[12 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    let signer = || Signer::Keys { keys: keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    // A book of this run's own, so its id counter starts where a fresh book
    // starts and every id below is the one this test caused.
    let model_name = format!("e2e-orders--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!("[e2e_orders] note={} model_name={model_name}", note.address);

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
    let fresh = wait_inference_book_live(&dex, &ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fresh.order_count, 0, "a book this test just deployed already holds orders");

    let mut failures: Vec<String> = Vec::new();

    // ── two bids, and the ids the book gave them ──────────────────────────
    //
    // The id a placement receives is the counter's value before it, so each is
    // read off the stats taken just before the call rather than guessed.
    let better_id = fresh.next_order_id;
    place_buy(&dex, &note.address, &model_hash, BETTER_PRICE, BETTER_ESCROW, 0, signer()).await;
    if !wait_count(&dex, &ob, 1).await {
        failures.push("the first bid never rested".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    let after_first = dex.inference_get_stats(&ob).await.expect("stats after the first bid");
    let worse_id = after_first.next_order_id;
    assert_ne!(better_id, worse_id, "the book handed the same id to two placements");
    place_buy(&dex, &note.address, &model_hash, WORSE_PRICE, WORSE_ESCROW, 0, signer()).await;
    if !wait_count(&dex, &ob, 2).await {
        failures.push("the second bid never rested".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // Both are this note's, both are buys, both are for what was asked.
    for (id, price) in [(better_id, BETTER_PRICE), (worse_id, WORSE_PRICE)] {
        match dex.inference_get_order(&ob, id).await {
            Ok(order) => {
                if order.note != note.address {
                    failures.push(format!("order {id} belongs to {}, not this note", order.note));
                }
                if !order.is_buy || order.amount != TICKS {
                    failures.push(format!(
                        "order {id} at {price} is not the {TICKS}-tick buy that was placed: \
                         isBuy={} amount={}",
                        order.is_buy, order.amount
                    ));
                }
            }
            Err(err) => failures.push(format!("order {id} unreadable: {err:?}")),
        }
    }
    match dex.inference_get_best_bid_ask(&ob).await {
        Ok(top) if top.has_bid => {
            if top.bid != BETTER_PRICE.to_string() {
                failures.push(format!("the best bid is {}, not {BETTER_PRICE}", top.bid));
            }
        }
        Ok(_) => failures.push("the book has no best bid with two resting buys".to_string()),
        Err(err) => failures.push(format!("getBestBidAsk unreadable: {err:?}")),
    }
    if !failures.is_empty() {
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // ── cancelling exactly one of them ────────────────────────────────────
    dex.cancel_inference_order(
        &note.address,
        ParamsOfCancelInferenceOrder { model_hash: model_hash.clone(), order_id: better_id },
        signer(),
    )
    .await
    .expect("cancelInferenceOrder accepted");
    if !wait_count(&dex, &ob, 1).await {
        failures.push("cancelling one order did not leave the book holding one".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // The book's own marker for an emptied slot is a zero amount — its
    // idempotency guard reads it that way, so this is the book's judgment and
    // not this test's convention.
    match dex.inference_get_order(&ob, better_id).await {
        Ok(order) if order.amount != 0 => failures
            .push(format!("the cancelled order {better_id} still holds {} ticks", order.amount)),
        Ok(_) => {}
        Err(err) => failures.push(format!("cancelled order {better_id} unreadable: {err:?}")),
    }
    match dex.inference_get_order(&ob, worse_id).await {
        Ok(order) if order.amount != TICKS || order.note != note.address => {
            failures.push(format!(
                "cancelling one order took the other with it: order {worse_id} is now \
                 amount={} note={}",
                order.amount, order.note
            ));
        }
        Ok(_) => {}
        Err(err) => failures.push(format!("surviving order {worse_id} unreadable: {err:?}")),
    }
    match dex.inference_get_best_bid_ask(&ob).await {
        Ok(top) if top.has_bid => {
            if top.bid != WORSE_PRICE.to_string() {
                failures.push(format!(
                    "the best bid is {} after the better one was cancelled, not {WORSE_PRICE}",
                    top.bid
                ));
            }
        }
        Ok(_) => failures.push("the book lost its bid side to a single cancel".to_string()),
        Err(err) => failures.push(format!("getBestBidAsk unreadable: {err:?}")),
    }

    // ── a buy whose deadline has already passed ───────────────────────────
    //
    // Refused by the book before it accepts the message, so nothing about it
    // reaches the queue where ids are handed out. Both of the book's counters
    // therefore have to stand exactly still — and they are the same two the
    // pair of accepted buys above moved.
    let before_stale = dex.inference_get_stats(&ob).await.expect("stats before the expired buy");
    let stale_deadline = now_unix().saturating_sub(STALE_BY);
    place_buy(
        &dex,
        &note.address,
        &model_hash,
        WORSE_PRICE,
        WORSE_ESCROW,
        stale_deadline,
        signer(),
    )
    .await;
    // Give it the time a placement that worked would have needed.
    tokio::time::sleep(Duration::from_secs(20)).await;

    let after_stale = dex.inference_get_stats(&ob).await.expect("stats after the expired buy");
    if after_stale.order_count != before_stale.order_count {
        failures.push(format!(
            "the book rested a buy whose deadline had passed: order count {} -> {}",
            before_stale.order_count, after_stale.order_count
        ));
    }
    if after_stale.next_order_id != before_stale.next_order_id {
        failures.push(format!(
            "the book gave an id to a buy whose deadline had passed: next id {} -> {}; the id is \
             assigned past the validation that was supposed to refuse it",
            before_stale.next_order_id, after_stale.next_order_id
        ));
    }

    finish(&dex, &note.address, &model_hash, signer(), failures).await;
}

async fn place_buy(
    dex: &Dex,
    note: &str,
    model_hash: &str,
    price: u128,
    escrow: u128,
    deadline: u64,
    signer: Signer,
) {
    dex.place_inference_buy(
        note,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.to_string(),
            max_price_per_tick: price,
            ticks: TICKS,
            escrow,
            // A plain limit: no taker flags, so it rests rather than crossing —
            // and there is nothing on the other side to cross with anyway.
            flags: 0,
            deadline,
        },
        signer,
    )
    .await
    .expect("placeInferenceBuy accepted");
}

/// Wait for the book to be holding exactly `want` orders.
async fn wait_count(dex: &Dex, ob: &str, want: u128) -> bool {
    let mut last = u128::MAX;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(stats) = dex.inference_get_stats(ob).await {
            last = stats.order_count;
            if last == want {
                return true;
            }
        }
    }
    eprintln!("[e2e_orders] order count stayed at {last}, wanted {want}");
    false
}

/// Clear whatever is still resting, then report. A bid left behind outlives
/// this test and keeps its escrow with it.
async fn finish(dex: &Dex, note: &str, model_hash: &str, signer: Signer, failures: Vec<String>) {
    let _ = dex
        .cancel_all_inference_orders(
            note,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
            signer,
        )
        .await;
    assert!(failures.is_empty(), "e2e_orders failures: {failures:#?}");
}
