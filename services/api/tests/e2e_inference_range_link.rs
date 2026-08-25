// A prediction market that settles from an inference book — the one fact that
// spans both halves of the product, and the only scene that produces it.
//
//   IX-SEQ-09 — a RANGE event carries the address of an `InferenceOrderBook`,
//               and the read model serves that link back on the PREDICTION
//               side: `/api/v1/prediction/markets?resolvesFrom=<book>` returns
//               the market, with `resolvesFrom.inferenceOrderBookAddress`
//               naming the book it was deployed against.
//
// Two notes of different currencies, because the two halves are not
// interchangeable: the inference book is deployed from a `PN-INF` note (SHELL,
// index 22), and the prediction market from a `PN-API` note (NACKL, index 5).
// A note's currency is fixed by its constructor, so one note cannot do both.
//
// The book address is worked out BEFORE the market is deployed — that is the
// whole shape of the scene. `addRangeEvent` takes the book as an argument, so
// the book has to exist first, and the link is made at event-creation time
// rather than discovered afterwards.
//
//   cargo test -p dodex-api --test e2e_inference_range_link -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::wait_inference_book_live;
use common::deploy_market::deploy_ephemeral_market;
use common::deploy_market::DeployOptions;
use common::deploy_market::RangeLink;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::read_model::api_prediction;
use common::read_model::get_json;
use common::read_model::poll_read_with;
use common::read_model::read_phases_enabled;
use common::read_model::GetOutcome;
use common::read_model::Probe;
use common::read_model::ReadBudget;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;

const POLL_TICK: Duration = Duration::from_secs(2);
const BOOK_TICKS: u32 = 45;

/// `PN-INF` index 22 deploys the book, `PN-API` index 5 deploys the market.
/// Both are reserved in `services/api/tests/pn_pool_split.rs`.
const INFERENCE_NOTE_INDEX: usize = 22;
const API_NOTE_INDEX: usize = 5;

/// The single bound splitting the reference price into two outcomes. Minimal
/// and valid: `OracleEventList.sol:201` refuses a first bound of zero, because
/// outcome 0 wins on `price < bounds[0]` and no price is below zero — an
/// outcome that could be staked on and never win.
const RANGE_BOUND: &str = "1000";

// ── the budget, by summand ────────────────────────────────────────────────
//
//   inference book live            90s   (BOOK_TICKS)
//   ephemeral market deploy       180s   (oracle → event → PMP → OrderBook)
//   prediction reconciler cadence  60s   (it runs on a minute)
//   read phase                    120s   (what remains of the read budget)
//                                ────
//                                450s of the 600s `ci-e2e` kill, leaving 150s.
/// Recalibrated 2026-08-24 from 420s. Green runtime of this test is 79.3s
/// (#299); with the read model dead it burned the whole budget (#300). 150s
/// keeps ~60s over the measured figure — the market deploy and the >=60s the range event needs dominate it.
const RANGE_READ_BUDGET: Duration = Duration::from_secs(150);

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_range_market_resolves_from_the_inference_book_it_was_deployed_against() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let inf_pool = TestPnPool::load_inference();
    let book_note = inf_pool.notes[INFERENCE_NOTE_INDEX % inf_pool.notes.len()].clone();
    let api_pool = TestPnPool::load();
    let market_note = api_pool.notes[API_NOTE_INDEX % api_pool.notes.len()].clone();
    let book_keys = KeyPair {
        public: book_note.owner_public_key_hex.clone(),
        secret: book_note.owner_secret_key_hex.clone(),
    };
    let book_signer = || Signer::Keys { keys: book_keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-range--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!(
        "[e2e_range] book_note={} market_note={} model_name={model_name}",
        book_note.address, market_note.address
    );

    let mut failures: Vec<String> = Vec::new();

    let read = if read_phases_enabled() {
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
            "[e2e_range] read phase skipped: E2E_READ_MODEL is not set, so no indexer is filling \
             the read model on this lane"
        );
        None
    };
    let budget = ReadBudget::with_total(RANGE_READ_BUDGET);

    // ── 1. the inference book, first, because the event needs its address ──
    dex.deploy_inference_order_book(
        &book_note.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        book_signer(),
    )
    .await
    .expect("deployInferenceOrderBook accepted");
    let ob = dex
        .get_inference_order_book_address(
            &book_note.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .expect("getInferenceOrderBookAddress");
    wait_inference_book_live(&dex, &ob, BOOK_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    eprintln!("[e2e_range] inference_order_book={ob}");

    // ── 2. the prediction market, linked to it ────────────────────────────
    let market = deploy_ephemeral_market(
        vec![network_endpoint()],
        &market_note,
        DeployOptions {
            range: Some(RangeLink {
                inference_order_book: ob.clone(),
                bounds: vec![RANGE_BOUND.to_string()],
            }),
            ..Default::default()
        },
    )
    .await
    .expect("deploy the range-linked ephemeral market");
    eprintln!("[e2e_range] pmp={} event_id={}", market.pmp_address, market.event_id);

    // ── 3. the read phase ─────────────────────────────────────────────────
    if let Some(service) = read.as_ref() {
        let markets =
            poll_read_with("IX-SEQ-09 range link on /prediction/markets", budget.left(), || {
                let service = std::sync::Arc::clone(service);
                let ob = ob.clone();
                async move {
                    let url = api_prediction(&format!("markets?resolvesFrom={ob}"));
                    match get_json(&service, &url).await {
                        GetOutcome::Retry(why) => Probe::Pending(why),
                        GetOutcome::Fatal(why) => Probe::Fatal(why),
                        GetOutcome::Ok(body) => {
                            let all = body["markets"].as_array().cloned().unwrap_or_default();
                            // Presence of a market under this filter is the fact;
                            // WHICH book it names and what else the block carries
                            // are asserted below. Requiring the address to match
                            // here would report a wrong link as a timeout.
                            if all.is_empty() {
                                Probe::Pending(format!(
                                "no market answers ?resolvesFrom={ob} yet — the range event may \
                                 not be projected"
                            ))
                            } else {
                                Probe::Ready(all)
                            }
                        }
                    }
                }
            })
            .await;

        match markets {
            Err(why) => failures.push(why),
            Ok(all) => {
                // The filter is by book address, and this run deployed exactly
                // one market against a book it created this run. More than one
                // means the filter is matching something it should not.
                if all.len() != 1 {
                    failures.push(format!(
                        "?resolvesFrom={ob} returned {} markets; this run deployed exactly one \
                         against a book of its own",
                        all.len()
                    ));
                }
                let Some(m) = all.first() else {
                    finish(failures).await;
                    return;
                };
                let rf = &m["resolvesFrom"];
                if rf.is_null() {
                    failures.push(
                        "the market came back under ?resolvesFrom but carries no resolvesFrom \
                         block: the filter and the projection disagree"
                            .to_string(),
                    );
                } else {
                    if rf["inferenceOrderBookAddress"].as_str() != Some(ob.as_str()) {
                        failures.push(format!(
                            "resolvesFrom.inferenceOrderBookAddress is {}, not the book this \
                             market was deployed against ({ob})",
                            rf["inferenceOrderBookAddress"]
                        ));
                    }
                    if rf["metric"].as_str() != Some("WEEKLY_MEDIAN_PRICE") {
                        failures.push(format!(
                            "resolvesFrom.metric is {}, and WEEKLY_MEDIAN_PRICE is the only metric \
                             a range market resolves on",
                            rf["metric"]
                        ));
                    }
                    // `model` is deliberately NOT asserted as present. It is
                    // filled from the inference book's own reconciliation, and
                    // a book deployed moments ago legitimately has none yet —
                    // the market is not hidden on that account (IX-GATE-19).
                    // Both a filled and a null `model` are correct answers, so
                    // asserting either would make this scene fail on the
                    // indexer's pace rather than on the link it is about.
                    eprintln!("[e2e_range] resolvesFrom.model={}", rf["model"]);
                }
            }
        }
    }

    finish(failures).await;
}

/// The one assertion. Nothing to clean up: this scene places no orders and
/// leaves no note holding a lock — the market and the book are both ephemeral
/// and per-run by construction.
async fn finish(failures: Vec<String>) {
    assert!(failures.is_empty(), "e2e_range_link failures: {failures:#?}");
}
