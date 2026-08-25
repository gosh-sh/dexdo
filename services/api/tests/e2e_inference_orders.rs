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
// ## Read model: does a precise cancel show up as precise?
//
// The chain readings above prove the BOOK told the truth. A read-model phase
// after the point cancel asks a different question: did that truth reach the
// public API? It polls the production router (`common::setup()`, raised
// in-process) until `/orders` reports the cancelled id as CANCELLED, then
// asserts on BOTH orders — the neighbour staying LIVE is the load-bearing
// half, since `cancelAllInferenceOrders` would make that same assertion a
// coin flip. Runs only where E2E_READ_MODEL=1 (an indexer is filling the read
// model) AND TEST_DATABASE_URL is reachable: unset E2E_READ_MODEL skips it with
// a printed reason, set with no database is a failure rather than a skip. Its
// failures join `failures` like every other check here — nothing in it may panic
// before the note's resting orders are cleared by `finish`.
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
// A negative needs a positive control, and this one is a second buy issued
// AFTER the stale one, with a deadline far in the future, that the book must
// accept. The test then reads which of the two took the contested id — the
// deadline stored on that order names its owner. Not "did the counter move":
// that question has a wrong answer available to it whenever the poll lands
// between the two messages.
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
use common::read_model::api;
use common::read_model::get_json;
use common::read_model::poll_read_with;
use common::read_model::read_phases_enabled;
use common::read_model::GetOutcome;
use common::read_model::Probe;
use common::read_model::ReadBudget;
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

    let pool = TestPnPool::load_inference();
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

    // Router for the read-model phase. `None` does not exit the test: below,
    // `finish(...)` cancels the resting orders and only then asserts
    // `failures.is_empty()`, so an early `return` here would skip both the
    // cleanup and any chain-phase failures already collected.
    // Two conditions, not one: a reachable database AND an indexer filling it.
    // See `read_model::read_phases_enabled` — the shellnet lane sets
    // TEST_DATABASE_URL for a Postgres nobody writes to, and running the read phase
    // there burns the whole budget proving the lane has no indexer.
    // An opt-in that cannot be honoured is a FAILURE, not a skip. Being told to run
    // the read phase and finding no database means the one lane that can check the
    // read model did not check it — and a printed line on a green run is exactly
    // how that goes unnoticed. Not set at all is the ordinary case and stays quiet.
    let read: Option<std::sync::Arc<salvo::Service>> = if read_phases_enabled() {
        let service = common::setup().await.map(|(s, _pool, _kek, _pn)| std::sync::Arc::new(s));
        if service.is_none() {
            failures.push(
                "E2E_READ_MODEL asks for the read phase, but common::setup() found no database \
                 (TEST_DATABASE_URL unset, empty, or unreachable). This is the only lane that \
                 runs an indexer, so skipping here leaves the read model unchecked on the one \
                 run that could check it"
                    .to_string(),
            );
        }
        service
    } else {
        eprintln!(
            "[e2e_orders] read phase skipped: E2E_READ_MODEL is not set, so no indexer is filling \
             the read model on this lane"
        );
        None
    };
    if read.is_none() {
        eprintln!(
            "[e2e_orders] read phase skipped: needs E2E_READ_MODEL=1 (an indexer is filling \
             the read model) and TEST_DATABASE_URL"
        );
    }
    let budget = ReadBudget::start();

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
            let bid = price_of(&top.bid);
            if bid != BETTER_PRICE {
                failures.push(format!("the best bid is {bid}, not {BETTER_PRICE}"));
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
            let bid = price_of(&top.bid);
            if bid != WORSE_PRICE {
                failures.push(format!(
                    "the best bid is {bid} after the better one was cancelled, not {WORSE_PRICE}"
                ));
            }
        }
        Ok(_) => failures.push("the book lost its bid side to a single cancel".to_string()),
        Err(err) => failures.push(format!("getBestBidAsk unreadable: {err:?}")),
    }

    // ── a buy whose deadline has already passed ───────────────────────────
    //
    // Refused by the book before it accepts the message, so nothing about it
    // reaches the queue where ids are handed out. The next id the book hands out
    // must therefore go to the NEXT accepted placement, not to this one.
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

    // A POSITIVE CONTROL, not a wait. The claim here is a negative — the book
    // must not rest a buy whose deadline has passed — and a fixed sleep proves
    // it only if the chain is never slower than the sleep. It is not: on a
    // loaded stand the stale placement can land after the window and the test
    // goes green over a real defect. So a placement that MUST work is issued
    // after it. `finish` cancels every resting order, so the control leaves
    // nothing behind.
    //
    // What is read is the CONTESTED ID, not a counter. Polling "has next_order_id
    // moved" breaks on the first increment whoever caused it: if the book
    // regresses and rests the stale buy, the counter walks before → +1 (stale) →
    // +2 (control), and a tick landing in the gap between the two messages reads
    // +1 and satisfies both "moved by exactly one" assertions — green on precisely
    // the defect this exists to catch, and likelier on the loaded stand that
    // motivated the control in the first place. Reading which placement TOOK
    // `before_stale.next_order_id` has no such gap: exactly one of the two can
    // own it, its deadline says which, and neither the poll cadence nor the chain's
    // speed can change the answer.
    let control_deadline = now_unix() + 3600;
    place_buy(
        &dex,
        &note.address,
        &model_hash,
        WORSE_PRICE,
        WORSE_ESCROW,
        control_deadline,
        signer(),
    )
    .await;

    // Poll the contested id until SOMETHING owns it. A book that has not handed it
    // out yet answers with an unreadable or zeroed order (the getter either reverts
    // on the missing key or returns a default-constructed struct — both are "not
    // yet", and neither can be mistaken for a resting order, whose deadline is
    // always non-zero here).
    let contested_id = before_stale.next_order_id;
    let mut owner_deadline = None;
    for _ in 0..POLL_TICKS {
        if let Ok(order) = dex.inference_get_order(&ob, contested_id).await
            && order.deadline != 0
        {
            owner_deadline = Some(order.deadline);
            break;
        }
        tokio::time::sleep(POLL_TICK).await;
    }
    let Some(owner_deadline) = owner_deadline else {
        failures.push(format!(
            "nothing took order id {contested_id} within {}s — the control placement never \
             landed, so this run measured the chain rather than the deadline check and the \
             stale-deadline claim is unproven rather than disproven",
            POLL_TICKS as u64 * POLL_TICK.as_secs()
        ));
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    };

    if owner_deadline == stale_deadline {
        failures.push(format!(
            "order id {contested_id} carries deadline {stale_deadline}, which is the buy whose \
             deadline had already passed — the book rested it and pushed the control to \
             {}. It is refused before tvm.accept(), so it must never take an id",
            contested_id + 1
        ));
    } else if owner_deadline != control_deadline {
        failures.push(format!(
            "order id {contested_id} carries deadline {owner_deadline}, which is neither the \
             stale buy's {stale_deadline} nor the control's {control_deadline} — something \
             else placed into this book during the test and the check below is not scoped \
             to what it thinks it is"
        ));
    } else {
        // The control owns the contested id, so both messages have been processed
        // and the counters are settled: the control accounts for exactly one, and
        // the stale buy for none. Read them only now — asked earlier, the same
        // numbers would be racing the second message.
        let after_stale =
            dex.inference_get_stats(&ob).await.expect("stats once the control took its id");
        if after_stale.order_count != before_stale.order_count + 1 {
            failures.push(format!(
                "order count moved from {} to {} — the control placement accounts for exactly \
                 one, so anything else means the book rested a second order in this window",
                before_stale.order_count, after_stale.order_count
            ));
        }
        if after_stale.next_order_id != before_stale.next_order_id + 1 {
            failures.push(format!(
                "next id moved from {} to {} — the control took {contested_id} and nothing \
                 else may have taken one",
                before_stale.next_order_id, after_stale.next_order_id
            ));
        }
    }

    // ---- read model: the cancel arrived ----
    //
    // IX-SEQ-08. TWO facts are checked, and the second matters more than the
    // first: the cancelled order becomes CANCELLED, while its neighbour stays
    // LIVE. Cancelling ALL orders would have produced the first fact too —
    // that is exactly why this reads the point `cancelInferenceOrder`, not
    // the sweep.
    if let Some(service) = read.as_ref() {
        let cancelled_id = better_id.to_string();
        let survivor_id = worse_id.to_string();
        let orders = poll_read_with("IX-SEQ-08 cancel in /orders", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let ob = ob.clone();
            let want = cancelled_id.clone();
            async move {
                let url = api(&format!("orders?inferenceOrderBookAddress={ob}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => {
                        let all = body["orders"].as_array().cloned().unwrap_or_default();
                        // Poll for PRESENCE of the fact — the order left `LIVE`,
                        // i.e. some terminal verdict was projected — and leave
                        // WHICH verdict to the asserts below. Requiring
                        // "CANCELLED" here instead would report a wrong terminal
                        // status (an EXPIRED where a CANCELLED belongs) as an
                        // expired budget, which names the indexer's speed for
                        // what is actually a projector defect.
                        //
                        // `LIVE`, not `OPEN`: `OPEN` is the DB value, and the
                        // wire carries the public name
                        // (`OrderStatus::as_public`). Comparing against `OPEN`
                        // here is always true, which would make this poll return
                        // on the placement itself and drop the wait for the
                        // cancel entirely — see the neighbour check below, which
                        // reads `LIVE` for the same reason.
                        let settled = all.iter().find(|o| {
                            o["orderId"].as_str() == Some(want.as_str())
                                && o["status"].as_str() != Some("LIVE")
                        });
                        match settled {
                            Some(_) => Probe::Ready(all),
                            None => Probe::Pending(format!(
                                "order {want} still LIVE or absent from /orders"
                            )),
                        }
                    }
                }
            }
        })
        .await;

        match orders {
            Err(why) => failures.push(why),
            Ok(all) => {
                let by_id = |id: &str| all.iter().find(|o| o["orderId"].as_str() == Some(id));
                match by_id(&cancelled_id) {
                    None => failures.push(format!("order {cancelled_id} missing from /orders")),
                    Some(o) => {
                        // THE status assert, not a defensive re-read: the poll
                        // above waits only for the order to leave `LIVE` (the
                        // wire name — see the note on the poll), so a wrong
                        // terminal verdict lands here, named as a wrong status
                        // rather than reported as a read-model timeout.
                        if o["status"].as_str() != Some("CANCELLED") {
                            failures.push(format!(
                                "cancelled order: want CANCELLED, got {}",
                                o["status"]
                            ));
                        }
                        // The remainder is PRESERVED, not zeroed: a cancel is
                        // not a fill. The order was never crossed, so both
                        // `ticks` and `ticksInitial` must still read the
                        // original size. Comparing them only to EACH OTHER
                        // would also pass if the projector zeroed both —
                        // exactly the failure this is meant to catch, since a
                        // cancelled order would then be indistinguishable
                        // from one that filled completely.
                        let want_ticks = TICKS.to_string();
                        if o["ticks"].as_str() != Some(want_ticks.as_str()) {
                            failures.push(format!(
                                "cancelled order's remainder not preserved: ticks={}, want \
                                 {want_ticks}",
                                o["ticks"]
                            ));
                        }
                        if o["ticksInitial"].as_str() != Some(want_ticks.as_str()) {
                            failures.push(format!(
                                "cancelled order's ticksInitial changed: ticksInitial={}, want \
                                 {want_ticks}",
                                o["ticksInitial"]
                            ));
                        }
                    }
                }
                match by_id(&survivor_id) {
                    None => failures
                        .push(format!("neighbour order {survivor_id} disappeared from /orders")),
                    Some(o) => {
                        if o["status"].as_str() != Some("LIVE") {
                            failures.push(format!(
                                "the neighbour order must stay LIVE, but it is {} — meaning ALL \
                                 orders were cancelled, not just one",
                                o["status"]
                            ));
                        }
                    }
                }
            }
        }
    }

    finish(&dex, &note.address, &model_hash, signer(), failures).await;
}

/// A price the way the ABI hands it back.
///
/// `uint256` comes across as a 0x-prefixed hex string, not a decimal one, so
/// comparing it against `2000000000` fails on representation while the price
/// itself is exactly right — which is what this test did on its first run
/// against a stand. Parse rather than compare text, and take either shape, so a
/// decoder that changes its mind about the format cannot read as a change in
/// the book's prices.
fn price_of(raw: &str) -> u128 {
    let t = raw.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u128::from_str_radix(hex, 16).ok(),
        None => t.parse::<u128>().ok(),
    };
    parsed.unwrap_or_else(|| panic!("price {raw} is neither a decimal nor a hex uint256"))
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
