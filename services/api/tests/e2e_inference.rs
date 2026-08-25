// End-to-end smoke test for the AI Registry inference order book against a
// real shellnet, driven directly through `dodex_chain::Dex`, with read-model
// phases over the production router (`common::setup()`) interleaved.
//
// The chain phases assert what the BOOK holds; the read phases assert that it
// reached the read model and the public API. They are different claims: a
// getter can be right while projection is broken, which is exactly the gap
// wave 3 exists to close. The read phases need TWO things — E2E_READ_MODEL=1
// (an indexer is filling the read model) and a reachable TEST_DATABASE_URL — and
// run only where both hold. Unset E2E_READ_MODEL skips them with a printed
// reason; set with no database is a failure, not a skip. Their failures join
// `failures` like every other — nothing here may panic before the orders are
// cancelled.
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
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget — book deploy is an internal message.

/// Producer for the current run. Uniqueness lives HERE, not in the version:
/// the `?producer=` filter below must select exactly one market, and with a
/// shared producer it would be satisfied by another run's leftover market —
/// i.e. it would be vacuously true exactly where it is supposed to catch a
/// real failure.
fn unique_producer() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("e2e-{nanos}")
}

/// Model name for the run: THREE-PART. `parse_model_ref`
/// (`inference_reconciler.rs:1069-1086`) fills in `producer`/`name`/`version`
/// only when there are exactly three non-empty parts joined by `--`; with a
/// two-part name they stay NULL, and the `?producer=` filter fails to find
/// its own scene's market (IX-GATE-17). The book address is derived from the
/// name (`sha256(modelName)`), so a unique producer also keeps the book
/// fresh on every run.
fn model_name_for(producer: &str) -> String {
    format!("{producer}--probe--v1")
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
    let producer = unique_producer();
    let model_name = model_name_for(&producer);
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

    // Router comes up HERE, not at the end: phase IX-SEQ-02 (task 3) sits
    // between placement and cancellation, and bringing it up mid-scenario
    // would mean holding escrow on the note for extra seconds.
    //
    // `Arc`, because `salvo::Service` is not `Clone` (`salvo_core::service`),
    // and each probe captures its own copy.
    //
    // `None` does NOT exit the test: there are chained checks below and a
    // final `assert!(failures.is_empty(), …)`. An early `return` would turn
    // a red chain run green exactly where there is simply no database.
    // Two conditions, not one: a reachable database AND an indexer filling it.
    // See `read_model::read_phases_enabled` — the shellnet lane sets
    // TEST_DATABASE_URL for a Postgres nobody writes to, and running the read phases
    // there burns the whole budget proving the lane has no indexer.
    // An opt-in that cannot be honoured is a FAILURE, not a skip. Being told to run
    // the read phases and finding no database means the one lane that can check the
    // read model did not check it — and a printed line on a green run is exactly
    // how that goes unnoticed. Not set at all is the ordinary case and stays quiet.
    let read: Option<std::sync::Arc<salvo::Service>> = if read_phases_enabled() {
        let service = common::setup().await.map(|(s, _pool, _kek, _pn)| std::sync::Arc::new(s));
        if service.is_none() {
            failures.push(
                "E2E_READ_MODEL asks for the read phases, but common::setup() found no database \
                 (TEST_DATABASE_URL unset, empty, or unreachable). This is the only lane that \
                 runs an indexer, so skipping here leaves the read model unchecked on the one \
                 run that could check it"
                    .to_string(),
            );
        }
        service
    } else {
        eprintln!(
            "[e2e_inference] read phases skipped: E2E_READ_MODEL is not set, so no indexer is filling \
             the read model on this lane"
        );
        None
    };
    if read.is_none() {
        eprintln!(
            "[e2e_inference] read phases skipped: needs E2E_READ_MODEL=1 (an indexer is filling \
             the read model) and TEST_DATABASE_URL"
        );
    }

    // Wait budget — ONE per binary (decision E). A per-fact budget would push
    // `e2e_inference` past `terminate-after`, and in the "read model stuck"
    // scenario nextest would kill the test before the final `assert!`,
    // losing the collected failures.
    let budget = ReadBudget::start();

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

    // ---- read model: the order arrived ----
    //
    // IX-SEQ-02, and this phase sits BEFORE the cancellation below: after the
    // cancel there is nothing to assert about a LIVE remainder.
    //
    // `order_id` is an `Option`: the chained block above leaves it `None`
    // when the order never surfaced, and its own failure is already recorded
    // there. There is nothing to duplicate, so this phase simply does not
    // run.
    if let (Some(service), Some(id)) = (read.as_ref(), order_id) {
        let want_id = id.to_string();
        // Hoisted: both the `/orders` remainder check below and the `/depth`
        // level check further down compare against this same expected size.
        let want_ticks = ticks.to_string();
        let order = poll_read_with("IX-SEQ-02 order in /orders", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let ob = ob.clone();
            let want = want_id.clone();
            async move {
                // `status=LIVE` is the wire name for an open order
                // (`services/api/src/inference.rs:363`: LIVE, FILLED, CANCELLED, EXPIRED).
                let url = api(&format!("orders?inferenceOrderBookAddress={ob}&status=LIVE"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => {
                        let found = body["orders"]
                            .as_array()
                            .and_then(|a| {
                                a.iter().find(|o| o["orderId"].as_str() == Some(want.as_str()))
                            })
                            .cloned();
                        match found {
                            Some(o) => Probe::Ready(o),
                            None => Probe::Pending(format!("order {want} not yet in /orders")),
                        }
                    }
                }
            }
        })
        .await;

        match order {
            Err(why) => failures.push(why),
            Ok(o) => {
                if o["side"].as_str() != Some("BUY") {
                    failures.push(format!("side: want BUY, got {}", o["side"]));
                }
                if o["status"].as_str() != Some("LIVE") {
                    failures.push(format!("status: want LIVE, got {}", o["status"]));
                }
                // The remainder fields are `ticks` / `ticksInitial`
                // (`InferenceOrderDto`, `services/api/src/inference.rs:327-347`).
                // The order was never crossed, so the remainder equals the initial size.
                if o["ticks"].as_str() != Some(want_ticks.as_str()) {
                    failures.push(format!("ticks: want {want_ticks}, got {}", o["ticks"]));
                }
                if o["ticksInitial"].as_str() != Some(want_ticks.as_str()) {
                    failures.push(format!(
                        "ticksInitial: want {want_ticks}, got {}",
                        o["ticksInitial"]
                    ));
                }
            }
        }

        // Depth is a separate claim, not a consequence of `/orders`: `/depth`
        // aggregates by level, while `/orders` returns the order's own row.
        // With `quantity_precision = 0` the level's quantity passes through
        // unscaled, i.e. the string "2".
        let bids = poll_read_with("IX-SEQ-02 level in /depth", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let ob = ob.clone();
            async move {
                let url = api(&format!("depth?inferenceOrderBookAddress={ob}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => match body["bids"].as_array() {
                        Some(b) if !b.is_empty() => Probe::Ready(b.clone()),
                        _ => Probe::Pending("bids are still empty".into()),
                    },
                }
            }
        })
        .await;

        match bids {
            Err(why) => failures.push(why),
            Ok(levels) => {
                if levels.len() != 1 {
                    failures.push(format!(
                        "the book should hold exactly one resting BUY level, got {}",
                        levels.len()
                    ));
                }
                if levels[0][1].as_str() != Some(want_ticks.as_str()) {
                    failures
                        .push(format!("level remainder: want {want_ticks}, got {}", levels[0][1]));
                }
            }
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

    // ---- read model: the book arrived and is found by producer ----
    //
    // IX-SEQ-01 and IX-GATE-17. The chained checks above read the book's own
    // getters — i.e. they confirm the state of the CHAIN and say nothing
    // about whether that state reached the read model. Every outcome below
    // goes into `failures`: nothing here may panic.
    if let Some(service) = read.as_ref() {
        let market = poll_read_with("IX-SEQ-01 book visible", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let ob = ob.clone();
            async move {
                let url = api(&format!("markets?inferenceOrderBookAddress={ob}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => {
                        match body["markets"].as_array().and_then(|a| a.first().cloned()) {
                            Some(m) => Probe::Ready(m),
                            None => Probe::Pending(format!("book {ob} not yet in /markets")),
                        }
                    }
                }
            }
        })
        .await;

        match market {
            Err(why) => failures.push(why),
            Ok(m) => {
                // Checks — ONCE, after the poll (decision C): a wrong value
                // must land in `failures` here, not eat into the budget.
                //
                // The field is `model.ref`, not `modelRef`: `InferenceModelDto`
                // is nested under `model` and carries `#[serde(rename =
                // "ref")]` (`services/api/src/inference.rs:78-85`).
                if m["model"]["ref"].as_str() != Some(model_name.as_str()) {
                    failures
                        .push(format!("model.ref: want {model_name}, got {}", m["model"]["ref"]));
                }
                if m["model"]["producer"].as_str() != Some(producer.as_str()) {
                    failures.push(format!(
                        "model.producer: want {producer}, got {}",
                        m["model"]["producer"]
                    ));
                }
                if m["model"]["name"].as_str() != Some("probe") {
                    failures.push(format!("model.name: want probe, got {}", m["model"]["name"]));
                }
                if m["model"]["version"].as_str() != Some("v1") {
                    failures.push(format!("model.version: want v1, got {}", m["model"]["version"]));
                }
                // Precision constants — exactly what the reconciler writes
                // (`inference_reconciler.rs`: PRICE_PRECISION = 9,
                // QUANTITY_PRECISION = 0).
                if m["pricePrecision"].as_i64() != Some(9) {
                    failures.push(format!("pricePrecision: want 9, got {}", m["pricePrecision"]));
                }
                if m["quantityPrecision"].as_i64() != Some(0) {
                    failures
                        .push(format!("quantityPrecision: want 0, got {}", m["quantityPrecision"]));
                }
            }
        }

        // IX-GATE-17: the producer filter finds EXACTLY this market.
        let filtered = poll_read_with("IX-GATE-17 producer filter", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let producer = producer.clone();
            async move {
                let url = api(&format!("markets?producer={producer}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => match body["markets"].as_array() {
                        Some(a) if !a.is_empty() => Probe::Ready(a.clone()),
                        _ => Probe::Pending(format!("producer={producer} filter still empty")),
                    },
                }
            }
        })
        .await;

        match filtered {
            Err(why) => failures.push(why),
            Ok(list) => {
                if list.len() != 1 {
                    failures.push(format!(
                        "a unique producer must select exactly one market, got {}",
                        list.len()
                    ));
                }
                if list[0]["inferenceOrderBookAddress"].as_str() != Some(ob.as_str()) {
                    failures.push(format!(
                        "filter found the wrong book: {}",
                        list[0]["inferenceOrderBookAddress"]
                    ));
                }
            }
        }
    }

    assert!(failures.is_empty(), "e2e_inference failures: {failures:#?}");
}
