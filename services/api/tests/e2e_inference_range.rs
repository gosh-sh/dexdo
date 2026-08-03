// A numeric range market resolved from an inference book's own reference
// price — the whole chain, from the trade that sets the price to the market
// that settles on it.
//
// A range event is not resolved by anybody's vote. The oracle list that
// published it is its single trusted oracle, and what it votes is whatever
// price the bound `InferenceOrderBook` reports as its weekly median. So the
// question this answers is not "does a market resolve" but "does a **price**
// become an outcome", across four contracts and without a human in the loop:
//
//   InferenceOrderBook  a closed deal publishes its finalized ticks
//        ↓ reportFinalized                      → the day's VWAP, the median
//   OracleEventList     addRangeEvent binds bounds + that book to an event
//        ↓ confirmEvent                          → and sets the PMP's clock
//   PMP                 approved, its result window fixed by the list
//        ↓ resolveRange → requestWeeklyMedian → onWeeklyMedian
//   PMP                 resolved into the bucket the price falls in
//
// ## What actually gives a book a price
//
// Not a match. `_recordTrade` is reachable only from `reportFinalized`, which
// only the deal's `TokenContract` calls, and only from `_settleFees` — that is,
// on the deal's **close**. The contract says why in as many words: a match that
// is later refunded served nothing, so counting it would let anyone move the
// reference price by placing orders they never intend to honour.
//
// So this test runs a real deal to a real close before it has a price at all:
// offer, crossing buy, bond, open, wait out the probe window, advance (one tick
// finalized), stop. `MIN_LIQUIDITY` is 1, so that single finalized tick is
// enough — but it has to be finalized and it has to be closed.
//
// ## Why the bounds are what they are
//
// The deal trades at 2 SHELL, so the median is 2 SHELL. The bounds are 1, 3 and
// 5, giving four outcomes, and 2 falls in the second bucket:
//
//   price < 1 → 0 | 1 ≤ price < 3 → 1 | 3 ≤ price < 5 → 2 | 5 ≤ price → 3
//
// Outcome 1 is deliberately a middle bucket and deliberately not adjacent to a
// bound: the mapping has to walk its bounds rather than fall off either end,
// and no off-by-one in the comparison could produce it by accident.
//
// Four outcomes also means the market must be deployed with **four** initial
// stakes. A PMP whose `initialStakes.length` disagrees with the outcome count
// refunds the creator and self-destructs on the first approval — the same path
// `event_rejects` covers deliberately, and one this test would hit by accident
// if the bounds and the stakes were edited apart.
//
// ## Timing
//
// The two clocks are kept apart rather than overlapped. `addRangeEvent` is
// issued only after the deal has closed, so the event's deadline is measured
// from a moment when the price already exists. It has to be far enough out that
// `confirmEvent` still sees `MIN_RESULT_GAP` of lead when the market is
// deployed a minute later — the list re-checks that gap, and rejects the market
// outright if it has lapsed.
//
//   cargo test -p dodex-api --test e2e_inference_range -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::collections::HashMap;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use ackinacki_kit::contracts::account::AccountStatus;
use ackinacki_kit::contracts::account::ParamsOfWaitAccount;
use ackinacki_kit::contracts::traits::AccountAccessor;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::generate_random_sign_keys;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use common::airegistry::deploy_token_contract;
use common::airegistry::wait_inference_book_live;
use common::airegistry::wait_sell_offer_rested;
use common::airegistry::TokenDeal;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::e2e_setup::pubkey_hex_to_decimal;
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::dex_contract_params;
use dodex_chain::Dex;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::dex::oracle::Oracle;
use dodex_contracts::dex::oracle::ParamsOfGetEventListAddress;
use dodex_contracts::dex::oracle_event_list::OracleEventList;
use dodex_contracts::dex::oracle_event_list::ParamsOfAddRangeEvent;
use dodex_contracts::dex::oracle_event_list::ParamsOfGetRangeData;
use dodex_contracts::dex::oracle_event_list::ParamsOfResolveRange;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfPostSellerBond;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;
use dodex_contracts::dex::root_oracle::ParamsOfDeployOracle;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

const SHELL: u128 = 1_000_000_000;

/// What the deal trades at, and therefore the reference price the market
/// resolves from. Two SHELL rather than one so the price is neither the book's
/// minimum nor equal to any bound.
const RANGE_PRICE: u128 = 2 * SHELL;

const FEE_BPS: u128 = 250;
const BPS: u128 = 10_000;
const UNIT: u128 = RANGE_PRICE + RANGE_PRICE * FEE_BPS / BPS;

/// The deal is funded for the two ticks the contract's floor requires. Only the
/// probe tick is finalized here — the streaming tick's acceptance window is far
/// longer than this test's patience, and one finalized tick is all the median
/// needs.
const DEAL_TICKS: u128 = 2;
const BUY_ESCROW: u128 = 8 * SHELL;
const SELLER_BOND: u128 = 2 * RANGE_PRICE + RANGE_PRICE / 100;

const _: () = assert!(BUY_ESCROW >= DEAL_TICKS * UNIT);

/// The bounds, and the outcome the price above falls into. Written out rather
/// than computed so the expectation is a statement and not a restatement of the
/// mapping this test is checking.
const BOUNDS: [u128; 3] = [SHELL, 3 * SHELL, 5 * SHELL];
const EXPECTED_OUTCOME: u32 = 1;
const OUTCOMES: usize = BOUNDS.len() + 1;

const _: () = assert!(BOUNDS[0] < BOUNDS[1] && BOUNDS[1] < BOUNDS[2], "bounds must increase");
const _: () = assert!(
    RANGE_PRICE >= BOUNDS[0] && RANGE_PRICE < BOUNDS[1],
    "the price has to land in the bucket EXPECTED_OUTCOME names"
);
const _: () = assert!(
    RANGE_PRICE != BOUNDS[0] && RANGE_PRICE != BOUNDS[1],
    "and not on a bound, where an off-by-one in the comparison would be invisible"
);
const _: () = assert!(EXPECTED_OUTCOME > 0 && (EXPECTED_OUTCOME as usize) < OUTCOMES - 1);

/// What the creator seeds each outcome with. Four outcomes means four stakes.
const SEED_PER_OUTCOME: u128 = 100_000_000_000;
const ORACLE_FEE: u128 = 100;
const TOKEN_TYPE_NACKL: u32 = 1;

/// The fixed window the probe advance waits, measured from `open()`.
const PROBE_WAIT: Duration = Duration::from_secs(180 + 45);

/// How long the range event stays open once the price exists.
///
/// Bounded from below by `MIN_RESULT_GAP` twice over: `addRangeEvent` refuses a
/// deadline nearer than that, and `confirmEvent` re-checks the same lead when
/// the market is deployed — which happens a minute or so later, so the figure
/// has to cover that minute as well or the market is rejected rather than
/// confirmed.
const RANGE_LIFETIME: u64 = 240;
const DEADLINE_SLACK: u64 = 15;

const MIN_RESULT_GAP: u64 = 120;

/// How long the market's deploy-and-confirm round trip is allowed to take
/// between `addRangeEvent` and the list re-checking the gap in `confirmEvent`.
/// The lifetime has to cover the gap AND this, or the market is rejected for a
/// deadline that was legal when it was written and stale when it was read.
const DEPLOY_BUDGET: u64 = 90;
const _: () = assert!(RANGE_LIFETIME >= MIN_RESULT_GAP + DEPLOY_BUDGET);

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn signer_of(note: &TestPn) -> Signer {
    Signer::Keys {
        keys: KeyPair {
            public: note.owner_public_key_hex.clone(),
            secret: note.owner_secret_key_hex.clone(),
        },
    }
}

/// A `uint256` the way the ABI hands it back: a 0x-prefixed hex string, not a
/// decimal one.
fn as_u128(raw: &str) -> u128 {
    let t = raw.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u128::from_str_radix(hex, 16).ok(),
        None => t.parse::<u128>().ok(),
    };
    parsed.unwrap_or_else(|| panic!("{raw} is neither a decimal nor a hex uint256"))
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; runs a deal to close, then a 240s range window (~11 min)"]
async fn a_range_market_resolves_into_the_bucket_its_books_price_falls_in() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    let note = pool.notes[13 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let ctx = dex.context();
    let suffix = unique_suffix();
    let model_name = format!("e2e-range--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!("[e2e_range] note={} model_name={model_name}", note.address);

    // ── 1. a deal, run to a close, so the book has a price at all ─────────
    //
    // Self-trade: the parties are not the subject here, the price is. What
    // matters is that the deal is genuinely closed — a match alone publishes
    // nothing, by design, so that orders nobody honours cannot move the
    // reference price.
    dex.deploy_inference_order_book(
        &note.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        signer_of(&note),
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
    wait_inference_book_live(&dex, &ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));

    let nonce = (suffix % 1_000_000_000) as u64 + 1;
    let tc = deploy_token_contract(
        ctx.clone(),
        &note.owner_public_key_hex,
        &note.address,
        nonce,
        TokenDeal {
            model_name: model_name.clone(),
            price_per_tick: RANGE_PRICE,
            max_ticks: DEAL_TICKS,
        },
        keys.clone(),
    )
    .await
    .expect("deploy TokenContract");
    eprintln!("[e2e_range] order_book={ob} token_contract={tc}");

    let mut failures: Vec<String> = Vec::new();

    dex.post_sell_offer(&note.address, ParamsOfPostSellOffer { flags: 0, nonce }, signer_of(&note))
        .await
        .expect("postSellOffer accepted");
    if let Err(diag) = wait_sell_offer_rested(&dex, &ob, &tc, POLL_TICKS, POLL_TICK).await {
        failures.push(diag);
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }
    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: RANGE_PRICE,
            ticks: DEAL_TICKS,
            escrow: BUY_ESCROW,
            flags: 1,
            deadline: 0,
        },
        signer_of(&note),
    )
    .await
    .expect("placeInferenceBuy accepted");
    if !poll_deal(&dex, &tc, "funded", |s| s.funded).await {
        failures.push("the match never handed the escrow to the TokenContract".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    dex.post_seller_bond(
        &note.address,
        ParamsOfPostSellerBond { nonce, amount: SELLER_BOND },
        signer_of(&note),
    )
    .await
    .expect("postSellerBond accepted");
    let mut bonded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(b) = dex.token_contract_get_seller_bond(&tc).await
            && b.bond_funded
        {
            bonded = true;
            break;
        }
    }
    if !bonded {
        failures.push("the seller's mirror bond never registered".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    dex.token_contract_open(
        &tc,
        ParamsOfOpen { endpoint_cipher: "00".to_string() },
        signer_of(&note),
    )
    .await
    .expect("TokenContract.open accepted");
    if !poll_deal(&dex, &tc, "opened", |s| s.opened).await {
        failures.push("the stream never opened".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    eprintln!("[e2e_range] sleeping {}s for the probe window…", PROBE_WAIT.as_secs());
    tokio::time::sleep(PROBE_WAIT).await;
    dex.token_contract_advance(&tc, signer_of(&note)).await.expect("advance accepted");
    if !poll_deal(&dex, &tc, "probe accepted", |s| s.probe_accepted).await {
        failures.push("the probe was never accepted, so no tick was ever finalized".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // The close is what publishes. Until this lands the book has no price and
    // `resolveRange` below would revert inside a `bounce:false` send, which is
    // the one failure on this path nothing ever reports.
    dex.stream_stop(
        &note.address,
        ParamsOfStreamDeal { token_contract: tc.clone() },
        signer_of(&note),
    )
    .await
    .expect("streamStop accepted");
    if !poll_deal(&dex, &tc, "closed", |s| !s.opened).await {
        failures
            .push("the deal never closed, so it never published its finalized tick".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // ── 2. and the book reports it as its reference price ─────────────────
    let mut median = 0;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(p) = dex.inference_get_weekly_median_price(&ob).await {
            median = as_u128(&p);
            if median > 0 {
                break;
            }
        }
    }
    eprintln!("[e2e_range] weekly median = {median}");
    if median != RANGE_PRICE {
        failures.push(format!(
            "the book's weekly median is {median}, not the {RANGE_PRICE} its only closed deal \
             traded at — every outcome below would be a mapping of the wrong number"
        ));
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // ── 3. an oracle publishing a range event bound to that book ──────────
    let oracle_keys = generate_random_sign_keys(ctx.clone()).expect("oracle keys");
    let ephemeral_keys = generate_random_sign_keys(ctx.clone()).expect("ephemeral keys");
    let oracle_name = format!("RangeE2E-{suffix:x}");
    dex.deploy_oracle(
        ParamsOfDeployOracle {
            oracle_pubkey: pubkey_hex_to_decimal(&oracle_keys.public),
            oracle_name: oracle_name.clone(),
        },
        Signer::Keys { keys: ephemeral_keys },
    )
    .await
    .expect("deploy_oracle accepted");
    // `get_oracle_address` is a RootOracle getter and answers from a derivation,
    // so it is happy long before anything exists at that address. Everything
    // after it is asked OF the oracle, and of the list the oracle's constructor
    // deploys — both have to be Active first, or the getter comes back
    // `AccountIsNotActive` rather than late. A fixed sleep is not the same
    // thing: it is a guess about a deploy whose duration nobody controls.
    let oracle_address = dex.get_oracle_address(oracle_name.clone()).await.expect("oracle address");
    wait_active(Oracle::new(ctx.clone(), dex_contract_params(&oracle_address)), "Oracle").await;
    let el_address = dex
        .get_event_list_address(&oracle_address, ParamsOfGetEventListAddress { index: 0 })
        .await
        .expect("event list address");
    wait_active(
        OracleEventList::new(ctx.clone(), dex_contract_params(&el_address)),
        "the oracle's event list",
    )
    .await;

    let el = OracleEventList::new(ctx.clone(), dex_contract_params(&el_address));
    let event_name = format!("RangeEvt{suffix:x}");
    let deadline = now_unix() + RANGE_LIFETIME;
    let mut outcome_names = HashMap::new();
    for i in 0..OUTCOMES as u32 {
        outcome_names.insert(i, format!("bucket {i}"));
    }
    el.add_range_event(
        ParamsOfAddRangeEvent {
            event_name: event_name.clone(),
            oracle_fee: ORACLE_FEE,
            deadline,
            describe: "which bucket does the reference price fall in?".to_string(),
            bounds: BOUNDS.iter().map(|b| b.to_string()).collect(),
            outcome_names: outcome_names.clone(),
            ob: ob.clone(),
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .expect("addRangeEvent accepted");

    let mut event_id = String::new();
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(ev) = dex.get_events(&el_address).await
            && let Some((id, _)) = ev
                .events
                .iter()
                .find(|(_, e)| e.get("eventName").and_then(|v| v.as_str()) == Some(&event_name))
        {
            event_id = id.clone();
            break;
        }
    }
    if event_id.is_empty() {
        failures.push("the range event never appeared in the list".to_string());
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // A range event and a plain one are indistinguishable in `_events`; the
    // bounds and the bound book live in `_rangeData`, and that is what makes
    // this event resolve from a price rather than from a vote.
    match el.get_range_data(ParamsOfGetRangeData { event_id: event_id.clone() }).await {
        Ok(rd) => {
            if !rd.exists || rd.ob != ob {
                failures.push(format!(
                    "the event is not bound to this book: exists={} ob={}",
                    rd.exists, rd.ob
                ));
            }
            let got: Vec<u128> = rd.bounds.iter().map(|b| as_u128(b)).collect();
            if got != BOUNDS.to_vec() {
                failures.push(format!("the list stored bounds {got:?}, not {BOUNDS:?}"));
            }
        }
        Err(err) => failures.push(format!("getRangeData unreadable: {err:?}")),
    }
    if !failures.is_empty() {
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // ── 4. a market on it, whose clock the list sets by itself ────────────
    dex.deploy_pmp(
        &note.address,
        ParamsOfDeployPmp {
            event_id: event_id.clone(),
            oracle_fee: vec![ORACLE_FEE],
            token_type: TOKEN_TYPE_NACKL,
            names: vec![oracle_name.clone()],
            index: vec![0],
            // One per outcome. A market whose stake count disagrees with the
            // event's outcome count refunds and self-destructs on the first
            // approval, which is a different scenario's subject entirely.
            initial_stakes: vec![SEED_PER_OUTCOME; OUTCOMES],
        },
        signer_of(&note),
    )
    .await
    .expect("deploy_pmp accepted");

    let pmp = dex
        .get_pmp_address(event_id.clone(), vec![oracle_name.clone()], TOKEN_TYPE_NACKL)
        .await
        .expect("get_pmp_address");
    eprintln!("[e2e_range] event_id={event_id} pmp={pmp}");

    let mut approved = false;
    let mut oracle_list_hash = String::new();
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(d) = dex.get_pmp_details(&pmp).await
            && d.number_of_oracle_events > 0
            && d.approved_oracle_events >= d.number_of_oracle_events
        {
            oracle_list_hash = d.oracle_list_hash.clone();
            approved = true;
            break;
        }
    }
    if !approved {
        failures.push(
            "the market never reached its oracle's confirmation — a range event whose deadline \
             lost its result-gap lead by the time the list saw it is rejected outright"
                .to_string(),
        );
        finish(&dex, &note, &model_hash, failures).await;
        return;
    }

    // The list, not the creator, fixed the result window: a range event's
    // confirmation carries `submitSetTimings(deadline)` with it. Nothing else
    // in this suite sets a market's clock without a separate call.
    let details = dex.get_pmp_details(&pmp).await.expect("pmp details");
    if details.result_start != deadline {
        failures.push(format!(
            "the market's result window opens at {}, not at the event deadline {deadline} its \
             list was supposed to set",
            details.result_start
        ));
    }

    // ── 5. the deadline passes, and the price becomes an outcome ──────────
    let target = deadline + DEADLINE_SLACK;
    let now = now_unix();
    if now < target {
        eprintln!("[e2e_range] sleeping {}s for the event deadline…", target - now);
        tokio::time::sleep(Duration::from_secs(target - now)).await;
    }

    el.resolve_range(
        ParamsOfResolveRange {
            event_id: event_id.clone(),
            oracle_list_hash: oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
        },
        Signer::Keys { keys: oracle_keys.clone() },
    )
    .await
    .expect("resolveRange accepted");

    let mut resolved = None;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(d) = dex.get_pmp_details(&pmp).await
            && let Some(o) = d.resolved_outcome
        {
            resolved = Some(o);
            break;
        }
    }
    match resolved {
        Some(o) if o == EXPECTED_OUTCOME => {
            eprintln!("[e2e_range] resolved into bucket {o} from a median of {median}");
        }
        Some(o) => failures.push(format!(
            "the market resolved into bucket {o}; a median of {median} against bounds {BOUNDS:?} \
             is bucket {EXPECTED_OUTCOME}"
        )),
        None => failures.push(
            "the market never resolved. `resolveRange` sends `requestWeeklyMedian` with \
             bounce:false, so a book that refused it reports nothing and the callback simply \
             never arrives"
                .to_string(),
        ),
    }

    finish(&dex, &note, &model_hash, failures).await;
}

/// Wait for an account to exist and be runnable before asking it anything.
async fn wait_active<A: AccountAccessor>(handle: A, label: &str) {
    handle
        .wait_account(ParamsOfWaitAccount {
            status: AccountStatus::Active,
            attempts: Some(POLL_TICKS as u8),
            attempts_timeout: Some(POLL_TICK.as_millis() as u64),
        })
        .await
        .unwrap_or_else(|e| panic!("wait {label} active: {e:?}"));
}

async fn poll_deal<F>(dex: &Dex, tc: &str, what: &str, probe: F) -> bool
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
    eprintln!("[e2e_range] never reached: {what}");
    false
}

async fn finish(dex: &Dex, note: &TestPn, model_hash: &str, failures: Vec<String>) {
    let _ = dex
        .cancel_all_inference_orders(
            &note.address,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
            signer_of(note),
        )
        .await;
    assert!(failures.is_empty(), "e2e_range failures: {failures:#?}");
}
