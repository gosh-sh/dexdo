// Two endings an order can have that no other binary here produces, sharing a
// note and a shape but not a book.
//
//   IX-SEQ-12 — an order whose deadline passed reaches the read model as
//               EXPIRED, its OWN terminal status, while a neighbour without a
//               deadline stays LIVE. `CANCELLED` would be the easy wrong
//               answer: both are terminal and both leave the book, so a
//               projector that folded the two would pass every other test.
//   IX-SEQ-07 — a taker remainder the chain refunds WITHOUT a closing event
//               leaves a row nothing ever closes. The reconciler's sweep is
//               what ends it, and the only thing separating that provisional
//               cancel from a real one is `swept_at` — which has no HTTP
//               surface, so the assertion is SQL.
//
// The two tests share this file because they share a scene shape (one book, one
// note, one kind of ending) rather than a fact. They do NOT share a book: each
// deploys its own, so an id below is always the one that test caused.
//
//   cargo test -p dodex-api --test e2e_inference_expiry_sweep -- --ignored --nocapture
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
use common::read_model::api;
use common::read_model::get_json;
use common::read_model::poll_read_with;
use common::read_model::read_phases_enabled;
use common::read_model::GetOutcome;
use common::read_model::Probe;
use common::read_model::ReadBudget;
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use sqlx::PgPool;
use sqlx::Row;

const POLL_TICK: Duration = Duration::from_secs(2);
const BOOK_TICKS: u32 = 45;
const REST_TICKS: u32 = 30;
const OFFER_TICKS: u32 = 45;
const FILL_TICKS: u32 = 30;

/// The expiry scene's own read budget. See the budget note on that test for why
/// the 240s default cannot serve it.
/// The sweep test's own budget, split out from `DEFAULT_READ_BUDGET` on
/// 2026-08-24. It had been sharing the default with three binaries that finish
/// in 19–29s, while its own green runtime is 105.0s (#299) — most of it the
/// `inference_sweep_interval_ms` cycle it exists to wait for. When the default
/// was recalibrated to 90s against those three, this test would have been the
/// one casualty: a budget BELOW its healthy time, red on a working stand.
///
/// 170s keeps ~65s over the measurement, the same margin the others carry.
const SWEEP_READ_BUDGET: Duration = Duration::from_secs(170);

/// Recalibrated 2026-08-24 from 520s. Green runtime of this test is 205.2s
/// (#299); with the read model dead it burned the whole budget (#300). 270s
/// keeps ~60s over the measured figure — the bulk of that is waiting out the order's own deadline on chain, not polling.
const EXPIRY_READ_BUDGET: Duration = Duration::from_secs(270);

const PRICE_PER_TICK: u128 = 1_000_000_000;
const MAX_SELL_TTL: u64 = 3600;
const FLAG_IOC: u8 = 0x01;

/// `InferenceOrderBook.sol:1630` — `require(ticks >= 2, ERR_BAD_PARAM)`. The
/// book will not take an order for a single tick, and the refusal is invisible
/// from here: the note has no such check of its own, so it accepts the call and
/// forwards it, and the placement path is `bounce:false` end to end. A one-tick
/// bid therefore does not fail, it simply never becomes an order — which reads
/// as "the book is slow" and cost this test two wrong diagnoses before the
/// `require` was read.
const BID_TICKS: u128 = 2;
/// `>= ticks * (price + 2.5% fee)` = 2.05 SHELL at `PRICE_PER_TICK`; the book
/// checks it at `:1662`. Three is the same headroom `e2e_inference_orders`
/// gives the same price and size.
const BID_ESCROW: u128 = 3_000_000_000;

/// Note 21 out of `PN-INF`, reserved for this binary in `pn_pool_split.rs`.
/// Both tests take it; they never run at the same time because the binary is in
/// the `serial-e2e-shared` group, which is pinned to one at a time.
const NOTE_INDEX: usize = 21;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn note_and_signer() -> (TestPn, KeyPair) {
    let pool = TestPnPool::load_inference();
    let note = pool.notes[NOTE_INDEX % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    (note, keys)
}

/// Deploys a book of this run's own and waits for it to answer. Returns the
/// address and the stats of the empty book, so the caller can read ids off a
/// counter that starts where a fresh book starts.
async fn fresh_book(
    dex: &Dex,
    note: &TestPn,
    signer: Signer,
    model_name: &str,
) -> (String, String, u128) {
    let model_hash = model_hash_dec(model_name);
    dex.deploy_inference_order_book(
        &note.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.to_string(),
        },
        signer,
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
    let fresh = wait_inference_book_live(dex, &ob, BOOK_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(fresh.order_count, 0, "a book this test just deployed already holds orders");
    (ob, model_hash, fresh.next_order_id)
}

/// Waits until the book reports at least `want` resting orders.
async fn wait_count(dex: &Dex, ob: &str, want: u128) -> bool {
    for _ in 0..REST_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(stats) = dex.inference_get_stats(ob).await
            && stats.order_count >= want
        {
            return true;
        }
    }
    false
}

/// The router and the database behind it, raised once per test — both, because
/// one of these scenes asserts over HTTP and the other over SQL, and the sweep
/// test needs each for a different half of the same fact.
///
/// `E2E_READ_MODEL` unset is the ordinary case and stays quiet; set with no
/// reachable database is an instruction the only lane that could honour it did
/// not honour, and that is a failure rather than a printed line.
async fn read_surfaces(
    tag: &str,
    failures: &mut Vec<String>,
) -> Option<(std::sync::Arc<salvo::Service>, PgPool)> {
    if !read_phases_enabled() {
        eprintln!(
            "[{tag}] read phase skipped: E2E_READ_MODEL is not set, so no indexer is filling the \
             read model on this lane"
        );
        return None;
    }
    match common::setup().await {
        Some((service, pool, _kek, _pn)) => Some((std::sync::Arc::new(service), pool)),
        None => {
            failures.push(
                "E2E_READ_MODEL asks for the read phase, but common::setup() found no database \
                 (TEST_DATABASE_URL unset, empty, or unreachable). This is the only lane that \
                 runs an indexer, so skipping here leaves the read model unchecked on the one \
                 run that could check it"
                    .to_string(),
            );
            None
        }
    }
}

// ── IX-SEQ-12: expiry is its own ending ───────────────────────────────────
//
// Budget, by summand:
//   book live                  90s   (BOOK_TICKS)
//   two bids rest             120s   (REST_TICKS, twice, worst case)
//   the deadline passes       195s   (DEADLINE_IN + CLOCK_SLACK, minus whatever
//                                     the two rests already spent — it is an
//                                     absolute wait, not an additional one)
//   expireOrder + read phase  120s   (what remains of the read budget)
//                            ────
//                            405s of the 600s `ci-e2e` kill.
//
// The read budget is sized rather than left at the 240s default for the reason
// spelled out in `common::read_model`: the clock starts before the chain waits,
// and here they run past 240s on their own, which would hand the read phase
// `left() = 0` and one probe fired the instant `expireOrder` lands — a
// guaranteed false red rather than a strict check.
#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn an_order_past_its_deadline_expires_and_its_neighbour_does_not() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    /// How far ahead the doomed order's deadline sits.
    ///
    /// Margin against the CHAIN's clock, which is what `:1643` compares the
    /// deadline against — `require(deadline == 0 || deadline > block.timestamp)`
    /// — rather than this host's. The repository already prices that skew in
    /// the other direction: `STALE_BY = 120` in `e2e_inference_orders.rs`
    /// backdates by two minutes to be sure a deadline reads as past.
    ///
    /// A CORRECTION IS RECORDED HERE ON PURPOSE. This constant was raised from
    /// 30 to 180 to explain "the bid with a deadline never rested" on dexdo
    /// pipeline #292, and the explanation was wrong: #293 failed identically at
    /// 180. The real cause was `ticks: 1` against `require(ticks >= 2)` at
    /// `:1630` — see `BID_TICKS`. The margin is kept because it is defensible
    /// on its own terms, not because it ever fixed anything; do not read it as
    /// evidence that clock skew was ever observed here.
    const DEADLINE_IN: u64 = 180;

    let (note, keys) = note_and_signer();
    let signer = || Signer::Keys { keys: keys.clone() };
    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-expiry--{suffix}");
    eprintln!("[e2e_expiry] note={} model_name={model_name}", note.address);

    let mut failures: Vec<String> = Vec::new();
    let read = read_surfaces("e2e_expiry", &mut failures).await;
    let budget = ReadBudget::with_total(EXPIRY_READ_BUDGET);

    let (ob, model_hash, first_id) = fresh_book(&dex, &note, signer(), &model_name).await;
    eprintln!("[e2e_expiry] order_book={ob}");

    // The doomed bid: a deadline the chain will pass while we watch.
    let doomed_id = first_id;
    let deadline = now_unix() + DEADLINE_IN;
    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: BID_TICKS,
            escrow: BID_ESCROW,
            flags: 0,
            deadline,
        },
        signer(),
    )
    .await
    .expect("placeInferenceBuy(with deadline) accepted");
    if !wait_count(&dex, &ob, 1).await {
        failures.push("the bid with a deadline never rested".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // The neighbour: no deadline at all, and therefore nothing for `expireOrder`
    // to act on. It is the control — without it, a sweep that expired the whole
    // book would look exactly like an expiry that hit its target.
    let after_first = dex.inference_get_stats(&ob).await.expect("stats after the first bid");
    let survivor_id = after_first.next_order_id;
    assert_ne!(doomed_id, survivor_id, "the book handed the same id to two placements");
    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: BID_TICKS,
            escrow: BID_ESCROW,
            flags: 0,
            deadline: 0,
        },
        signer(),
    )
    .await
    .expect("placeInferenceBuy(no deadline) accepted");
    if !wait_count(&dex, &ob, 2).await {
        failures.push("the bid without a deadline never rested".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // Wait past the deadline. The book compares it against the CHAIN's clock
    // (`InferenceOrderBook.sol:1163-1167`), not this host's, so the wait is
    // computed from the deadline that was actually sent and padded for the
    // difference between the two clocks. Resting the two bids has already eaten
    // part of it, which is why this is a subtraction rather than a fixed sleep.
    const CLOCK_SLACK: u64 = 15;
    let wait_for = (deadline + CLOCK_SLACK).saturating_sub(now_unix());
    eprintln!("[e2e_expiry] waiting {wait_for}s for the deadline to pass");
    if wait_for > 0 {
        tokio::time::sleep(Duration::from_secs(wait_for)).await;
    }

    // Permissionless and self-draining: `expireOrder(uint128) public`
    // (`InferenceOrderBook.sol:1710`) needs no signature and no owner, so the
    // expiry is provoked directly instead of by touching the book with a second
    // placement. The second placement works too — the match path walks the book
    // and drops what has expired — but it also adds an order, and an order this
    // scene did not account for is exactly what the control above is guarding.
    dex.inference_expire_order(&ob, doomed_id).await.expect("expireOrder accepted");

    // The ASSERTIONS here are HTTP alone — both statuses this scene cares about
    // are public. The pool is used for one thing only, and the gate below says
    // why.
    if let Some((service, pool)) = read.as_ref() {
        let want_expired = doomed_id.to_string();
        let want_live = survivor_id.to_string();
        let orders = poll_read_with("IX-SEQ-12 expiry in /orders", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let pool = pool.clone();
            let ob = ob.clone();
            let want = want_expired.clone();
            async move {
                let url = api(&format!("orders?inferenceOrderBookAddress={ob}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => {
                        let all = body["orders"].as_array().cloned().unwrap_or_default();
                        // Presence of a verdict, not the verdict itself: waiting
                        // for `EXPIRED` here would turn a wrong terminal status —
                        // a CANCELLED where an EXPIRED belongs, the exact
                        // confusion this test exists to catch — into an expired
                        // budget blamed on the indexer.
                        //
                        // `LIVE` is the wire name for the DB's `OPEN`.
                        let doomed = all
                            .iter()
                            .find(|o| o["orderId"].as_str() == Some(want.as_str()))
                            .cloned();
                        let status =
                            doomed.as_ref().and_then(|o| o["status"].as_str()).map(str::to_owned);
                        match status.as_deref() {
                            None | Some("LIVE") => Probe::Pending(format!(
                                "order {want} still LIVE or absent from /orders"
                            )),
                            Some("EXPIRED") => Probe::Ready(all),
                            // A TERMINAL STATUS THAT IS NOT THE ONE WE WANT IS
                            // NOT AUTOMATICALLY THE ANSWER. Two writers race for
                            // this row: `expireOrder` projects `EXPIRED`, and the
                            // phantom sweep — which sees the same order gone from
                            // the book and reads `amount == 0` — writes a
                            // PROVISIONAL `CANCELLED` with `swept_at` set. The
                            // expiry projector is contracted to overwrite exactly
                            // that (`inference_projectors.rs:701-708`:
                            // `takes_expiry` is `OPEN or (CANCELLED and swept_at
                            // is not null)`, and it clears `swept_at` on the way
                            // through), so a swept cancel here is a row still in
                            // flight, not a verdict.
                            //
                            // Pipeline #297 lost that race and reported a
                            // provisional cancel as a wrong terminal status. The
                            // distinction is not on the wire — the DTO carries no
                            // `swept_at` — so it is read from the column, and
                            // ONLY to decide whether to keep waiting. A CANCELLED
                            // with `swept_at` NULL is a real ending and is handed
                            // straight to the assertion below, which names it.
                            Some(_) => {
                                let swept: Option<bool> = sqlx::query_scalar(
                                    "select swept_at is not null from inference_orders \
                                     where orderbook_address = $1 and order_id = $2::numeric",
                                )
                                .bind(&ob)
                                .bind(&want)
                                .fetch_optional(&pool)
                                .await
                                .ok()
                                .flatten();
                                if swept == Some(true) {
                                    Probe::Pending(format!(
                                        "order {want} reads a provisional sweep cancel; the \
                                         expiry projector owes it an EXPIRED"
                                    ))
                                } else {
                                    Probe::Ready(all)
                                }
                            }
                        }
                    }
                }
            }
        })
        .await;

        match orders {
            Err(why) => failures.push(why),
            Ok(all) => {
                let by_id =
                    |id: &str| all.iter().find(|o| o["orderId"].as_str() == Some(id)).cloned();
                match by_id(&want_expired) {
                    None => failures.push(format!("order {want_expired} missing from /orders")),
                    Some(o) => {
                        if o["status"].as_str() != Some("EXPIRED") {
                            failures.push(format!(
                                "the expired order reads {} — EXPIRED is its own terminal status, \
                                 not a flavour of CANCELLED",
                                o["status"]
                            ));
                        }
                    }
                }
                match by_id(&want_live) {
                    None => failures.push(format!("neighbour {want_live} missing from /orders")),
                    Some(o) => {
                        if o["status"].as_str() != Some("LIVE") {
                            failures.push(format!(
                                "the neighbour without a deadline reads {} — expiry took an order \
                                 it was not asked for",
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

// ── IX-SEQ-07: the phantom, and what tells it from a real cancel ──────────
//
// Budget, by summand:
//   book live                     90s   (BOOK_TICKS)
//   TokenContract + offer rested  90s   (OFFER_TICKS)
//   the crossing buy fills         60s   (FILL_TICKS)
//   sweep cadence                 90s   (SWEEP_WAIT)
//   SQL read phase                 60s   (what remains of the read budget)
//                                ────
//                                390s of the 600s `ci-e2e` kill, leaving 210s.
#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_taker_remainder_nothing_closes_is_ended_by_the_sweep_and_says_so() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    /// The reconciler ticks every 15s and the sweep runs on a 30s cadence
    /// behind it; this is those two plus room for a gate to have just missed a
    /// pass. It is a wait for a PERIODIC JOB, not a stand-in for a fact — the
    /// fact itself is polled for below, and this only decides how long the
    /// polling is willing to be patient.
    const SWEEP_WAIT: Duration = Duration::from_secs(90);
    /// The offer rests for this many ticks; the taker asks for more, so the
    /// remainder is what the chain refunds without a closing event.
    const OFFER_TICKS_SIZE: u128 = 2;
    const TAKER_TICKS: u128 = 4;

    let (note, keys) = note_and_signer();
    let signer = || Signer::Keys { keys: keys.clone() };
    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-sweep--{suffix}");
    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    eprintln!("[e2e_sweep] note={} model_name={model_name}", note.address);

    let mut failures: Vec<String> = Vec::new();
    let read = read_surfaces("e2e_sweep", &mut failures).await;
    let budget = ReadBudget::with_total(SWEEP_READ_BUDGET);

    let (ob, model_hash, _first_id) = fresh_book(&dex, &note, signer(), &model_name).await;
    eprintln!("[e2e_sweep] order_book={ob}");

    // A resting SELL, smaller than what the taker will ask for.
    let tc = deploy_token_contract(
        dex.context(),
        &note.owner_public_key_hex,
        &note.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: OFFER_TICKS_SIZE,
        },
        keys.clone(),
    )
    .await
    .expect("deploy TokenContract");
    dex.post_sell_offer(
        &note.address,
        ParamsOfPostSellOffer { flags: 0, nonce, ttl: MAX_SELL_TTL },
        signer(),
    )
    .await
    .expect("postSellOffer accepted");
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, OFFER_TICKS, POLL_TICK).await {
        failures.push(diag);
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    // The taker asks for more than rests and forbids a remainder to rest.
    // What the book cannot fill it refunds — and it does so without emitting a
    // closing event for the taker's own row, which is precisely the hole the
    // sweep exists to fill.
    let stats = dex.inference_get_stats(&ob).await.expect("stats before the taker");
    let taker_id = stats.next_order_id;
    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: TAKER_TICKS,
            escrow: 6_000_000_000,
            flags: FLAG_IOC,
            deadline: 0,
        },
        signer(),
    )
    .await
    .expect("placeInferenceBuy(IOC, oversized) accepted");

    let mut filled = false;
    for _ in 0..FILL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.funded
        {
            filled = true;
            break;
        }
    }
    if !filled {
        failures.push("the oversized taker never crossed the resting offer".to_string());
        finish(&dex, &note.address, &model_hash, signer(), failures).await;
        return;
    }

    eprintln!("[e2e_sweep] waiting {}s for the sweep cadence", SWEEP_WAIT.as_secs());
    tokio::time::sleep(SWEEP_WAIT).await;

    // ── the read phase, and why it is SQL ─────────────────────────────────
    //
    // `/orders` would show this row as CANCELLED and so would a real cancel:
    // the DTO carries no `swept_at`, so over HTTP a provisional ending and a
    // genuine one are the same answer. The distinction lives in the column, so
    // the assertion lives in the database.
    if let Some((_service, pool)) = read.as_ref() {
        let want_id = taker_id.to_string();
        let swept = poll_read_with("IX-SEQ-07 phantom swept", budget.left(), || {
            let pool = pool.clone();
            let ob = ob.clone();
            let want = want_id.clone();
            async move {
                let row = sqlx::query(
                    "select status, swept_at is not null as swept \
                     from inference_orders \
                     where orderbook_address = $1 and order_id = $2::numeric",
                )
                .bind(&ob)
                .bind(&want)
                .fetch_optional(&pool)
                .await;
                match row {
                    Err(e) => Probe::Fatal(format!("inference_orders query failed: {e}")),
                    Ok(None) => Probe::Pending(format!("no inference_orders row for id {want}")),
                    Ok(Some(r)) => {
                        let status: String = r.get("status");
                        // Presence of an ending, not which ending: demanding
                        // CANCELLED here would report a row the sweep ended the
                        // wrong way as a budget timeout.
                        if status == "OPEN" {
                            Probe::Pending(format!(
                                "order {want} is still OPEN — sweep has not run"
                            ))
                        } else {
                            Probe::Ready((
                                status,
                                r.get::<Option<bool>, _>("swept").unwrap_or(false),
                            ))
                        }
                    }
                }
            }
        })
        .await;

        match swept {
            Err(why) => failures.push(why),
            Ok((status, was_swept)) => {
                if status != "CANCELLED" {
                    failures.push(format!(
                        "the phantom ended as {status}; a swept remainder is recorded CANCELLED"
                    ));
                }
                // The whole point. Without this the row is indistinguishable
                // from an order the trader cancelled on purpose.
                if !was_swept {
                    failures.push(
                        "the row is CANCELLED but `swept_at` is null — that is a REAL cancel, and \
                         nobody cancelled this order: the sweep is what ended it, so the sweep has \
                         to say so"
                            .to_string(),
                    );
                }
            }
        }
    }

    // The phantom must also be gone from the depth a trader sees. A row the
    // sweep ended but the depth still counts is liquidity that does not exist.
    if let Some((service, _pool)) = read.as_ref() {
        let depth = poll_read_with("IX-SEQ-07 depth without the phantom", budget.left(), || {
            let service = std::sync::Arc::clone(service);
            let ob = ob.clone();
            async move {
                let url = api(&format!("depth?inferenceOrderBookAddress={ob}"));
                match get_json(&service, &url).await {
                    GetOutcome::Retry(why) => Probe::Pending(why),
                    GetOutcome::Fatal(why) => Probe::Fatal(why),
                    GetOutcome::Ok(body) => Probe::Ready(body),
                }
            }
        })
        .await;

        match depth {
            Err(why) => failures.push(why),
            Ok(body) => {
                let bids = body["bids"].as_array().cloned().unwrap_or_default();
                let remainder = TAKER_TICKS - OFFER_TICKS_SIZE;
                let ghost = bids.iter().any(|lvl| {
                    lvl["ticks"].as_str().and_then(|t| t.parse::<u128>().ok()) == Some(remainder)
                });
                if ghost {
                    failures.push(format!(
                        "the depth still carries a bid level of {remainder} ticks — the swept \
                         remainder is being served as liquidity that does not exist on chain"
                    ));
                }
            }
        }
    }

    finish(&dex, &note.address, &model_hash, signer(), failures).await;
}

/// Cancels whatever the note still has resting on this book, then makes the one
/// assertion. Best-effort on the cancel: failing to clean up must not hide what
/// the scene collected.
async fn finish(dex: &Dex, note: &str, model_hash: &str, signer: Signer, failures: Vec<String>) {
    let _ = dex
        .cancel_all_inference_orders(
            note,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
            signer,
        )
        .await;
    assert!(failures.is_empty(), "e2e_expiry_sweep failures: {failures:#?}");
}
