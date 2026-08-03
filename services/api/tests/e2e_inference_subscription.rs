// A §8 subscription is a bid that a seller can actually sell into, and a poke
// does not disturb it.
//
// The suite already places a subscription and reads it back. What it does not
// say is whether the thing is *liquidity*: every assertion so far holds for a
// row written into a mapping that no seller can ever reach. Here a real sell
// offer crosses it, and the deal that comes out names the subscription's owner
// as its buyer.
//
// ## What a poke can and cannot be asked here
//
// `pokeSubscription` rolls a subscription onto whatever cycle the clock says it
// is on. A cycle is `SUB_CYCLE_LEN` = one week, and the book takes it as a
// compile-time constant, so the roll itself is out of reach of any test that
// has to finish — there is no shorter-lived subscription to place.
//
// What IS reachable is the boundary on the near side of the first roll. The
// book advances a cycle while
//
//     block.timestamp >= periodStart + (curCycle + 1) * SUB_CYCLE_LEN
//
// and that `+ 1` is the whole of what keeps a fresh subscription still. Drop it
// — or turn `>=` on the wrong quantity — and the loop fires on the first poke,
// refunds the cycle budget, and after four turns expires the subscription and
// returns the entire escrow. So a poke that leaves the row untouched is a real
// statement with a real failure mode, and it is checked twice: once on a
// pristine subscription and once on one that has been partially filled, where
// an early roll would also hand back budget the buyer has already spent.
//
// It is NOT a statement that the message arrived. The write path is
// fire-and-forget, the poke's only effect is one that must not happen, and a
// poke dropped by the network looks exactly like a poke that correctly did
// nothing. The unsigned send is deliberate for the same reason it is cheap: the
// book checks that the id names a live subscription and nothing about who is
// asking, so the weakest possible caller is the honest one to use.
//
//   cargo test -p dodex-api --test e2e_inference_subscription -- --ignored --nocapture
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
use common::airegistry::TokenDeal;
use common::e2e_setup::model_hash_dec;
use common::e2e_setup::network_endpoint;
use common::test_pns::TestPn;
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
use dodex_contracts::dex::private_note::ParamsOfCancelInferenceOrder;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceSubscription;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.

/// The cheapest tick the book will price: a limit price has to be a positive
/// whole multiple of `PRICE_STEP`, which is one SHELL.
const PRICE_PER_TICK: u128 = 1_000_000_000;

const FEE_BPS: u128 = 250;
const BPS: u128 = 10_000;

/// What one tick costs the buyer — the unit every funding bound is written in.
const UNIT: u128 = PRICE_PER_TICK + PRICE_PER_TICK * FEE_BPS / BPS;

/// Cycles a subscription's budget is split across (`SUB_CYCLES`).
const SUB_CYCLES: u128 = 4;

/// Ticks the subscription stands ready to buy. Deliberately more than one deal
/// takes, so the fill leaves it resting rather than consuming it: a fully
/// filled maker bid is refunded and removed, and there would be nothing left to
/// poke.
const SUB_TICKS: u128 = 8;

/// Budget behind the subscription. Two bounds apply and the tighter one here is
/// the per-cycle floor: every one of the `SUB_CYCLES` cycles has to be able to
/// fund a whole deal on its own.
const SUB_ESCROW: u128 = 10_000_000_000;

const _: () = assert!(SUB_ESCROW >= SUB_TICKS * UNIT, "escrow must cover the ticks offered");
const _: () = assert!(
    SUB_ESCROW / SUB_CYCLES >= 2 * UNIT,
    "each cycle must fund a probe plus a stream tick, or the book refuses the placement"
);

/// Ticks the seller's deal serves — the smallest a deal can be, and fewer than
/// the subscription offers.
const DEAL_TICKS: u128 = 2;

const _: () = assert!(DEAL_TICKS < SUB_TICKS, "the fill has to leave the subscription resting");

/// What the fill takes out of the subscription's budget: the book charges the
/// clearing price, which for a taker sell is the seller's own ask.
const FILL_COST: u128 = DEAL_TICKS * UNIT;

const _: () = assert!(
    FILL_COST <= SUB_ESCROW / SUB_CYCLES,
    "one cycle's budget has to cover the deal, or the book would trim the fill"
);

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
#[ignore = "requires shellnet + seed_notes.json"]
async fn a_subscription_is_liquidity_a_seller_can_reach_and_a_poke_leaves_it_alone() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    let seller = pool.notes[14 % pool.notes.len()].clone();
    let buyer = pool.notes[15 % pool.notes.len()].clone();
    assert_ne!(
        seller.address, buyer.address,
        "the pool handed out one note for both sides; a seller filling its own subscription \
         proves nothing about a subscription being reachable by anyone else"
    );

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-sub--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!(
        "[e2e_sub] seller={} buyer={} model_name={model_name}",
        seller.address, buyer.address
    );

    let mut failures: Vec<String> = Vec::new();

    // ── 1. an empty book ──────────────────────────────────────────────────
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
    let stats = wait_inference_book_live(&dex, &ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
    // The id the book will hand the subscription. Reading it beats guessing:
    // ids come off a counter the book owns, and a scan over low ids would find
    // a subscription placed by an earlier run just as happily.
    let sub_id = stats.next_order_id;
    eprintln!("[e2e_sub] order_book={ob} next_order_id={sub_id}");

    // ── 2. the buyer's standing bid ───────────────────────────────────────
    dex.place_inference_subscription(
        &buyer.address,
        ParamsOfPlaceInferenceSubscription {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: SUB_TICKS,
            // A standing multi-cycle bid takes no taker bits at all.
            flags: 0,
            escrow: SUB_ESCROW,
            auto_renew: true,
        },
        signer_of(&buyer),
    )
    .await
    .expect("placeInferenceSubscription accepted");

    let placed = poll_sub(&dex, &ob, sub_id, "the subscription to rest", |s| s.exists).await;
    if !placed {
        failures.push(format!(
            "no subscription ever appeared at id {sub_id}, the id the book was about to hand out"
        ));
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    let fresh = dex.inference_get_subscription(&ob, sub_id).await.expect("subscription");
    eprintln!(
        "[e2e_sub] placed: curCycle={} cycleBudget={} cycleSpent={} autoRenew={}",
        fresh.cur_cycle, fresh.cycle_budget, fresh.cycle_spent, fresh.auto_renew
    );
    if fresh.cycle_budget != SUB_ESCROW / SUB_CYCLES {
        failures.push(format!(
            "the cycle budget is {}, not the escrow split {SUB_CYCLES} ways ({})",
            fresh.cycle_budget,
            SUB_ESCROW / SUB_CYCLES
        ));
    }
    if fresh.cur_cycle != 0 || fresh.cycle_spent != 0 {
        failures.push(format!(
            "a subscription just placed is already on cycle {} with {} spent",
            fresh.cur_cycle, fresh.cycle_spent
        ));
    }

    // ── 3. a poke from nobody, before anything has been spent ─────────────
    //
    // The first cycle has barely started, so the correct behaviour is to do
    // nothing at all. An off-by-one in the roll condition would instead refund
    // this cycle's budget and step the counter.
    dex.inference_poke_subscription(&ob, sub_id).await.expect("pokeSubscription accepted");
    settle().await;
    let poked = dex.inference_get_subscription(&ob, sub_id).await.expect("subscription after poke");
    if !poked.exists {
        failures.push(
            "a poke inside the first cycle expired the subscription; the roll only becomes due a \
             full cycle after it was placed"
                .to_string(),
        );
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }
    if poked.cur_cycle != 0 || poked.cycle_spent != 0 || poked.cycle_budget != fresh.cycle_budget {
        failures.push(format!(
            "a poke inside the first cycle moved the subscription: curCycle {} → {}, cycleSpent \
             {} → {}, cycleBudget {} → {}",
            fresh.cur_cycle,
            poked.cur_cycle,
            fresh.cycle_spent,
            poked.cycle_spent,
            fresh.cycle_budget,
            poked.cycle_budget
        ));
    }
    match dex.inference_get_order(&ob, sub_id).await {
        Ok(order) => {
            if order.escrow != SUB_ESCROW || order.amount != SUB_TICKS {
                failures.push(format!(
                    "the poke returned part of the escrow before its cycle was over: escrow {} \
                     (placed with {SUB_ESCROW}), ticks {} (placed with {SUB_TICKS})",
                    order.escrow, order.amount
                ));
            }
        }
        Err(err) => {
            failures.push(format!("the resting bid became unreadable after a poke: {err:?}"))
        }
    }
    if !failures.is_empty() {
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // ── 4. a seller sells into it ─────────────────────────────────────────
    //
    // The offer is the taker here: it arrives after the bid is resting, so the
    // book matches it against the subscription instead of resting it.
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
    eprintln!("[e2e_sub] token_contract={tc}");

    dex.post_sell_offer(
        &seller.address,
        ParamsOfPostSellOffer { flags: 0, nonce },
        signer_of(&seller),
    )
    .await
    .expect("postSellOffer accepted");

    let mut funded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(&tc).await
            && state.funded
        {
            funded = true;
            break;
        }
    }
    if !funded {
        failures.push(
            "the sell offer never turned into a funded deal. A subscription that no seller can \
             cross is a row in a mapping, not liquidity"
                .to_string(),
        );
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }

    // The deal's buyer is the note behind the subscription — the book carried
    // the owner through the fill, and this is the only place that shows it.
    match dex.token_contract_get_parties(&tc).await {
        Ok(parties) => {
            if parties.buyer != buyer.address {
                failures.push(format!(
                    "the deal names {} as its buyer, not the note whose subscription it filled ({})",
                    parties.buyer, buyer.address
                ));
            }
        }
        Err(err) => failures.push(format!("getParties unreadable: {err:?}")),
    }

    let filled =
        dex.inference_get_subscription(&ob, sub_id).await.expect("subscription after fill");
    eprintln!(
        "[e2e_sub] filled: exists={} curCycle={} cycleSpent={}",
        filled.exists, filled.cur_cycle, filled.cycle_spent
    );
    if !filled.exists {
        failures.push(format!(
            "the deal consumed the whole subscription; it offered {SUB_TICKS} ticks and the deal \
             took {DEAL_TICKS}"
        ));
        finish(&dex, &seller, &buyer, &model_hash, failures).await;
        return;
    }
    // Spend is charged against the cycle, which is what throttles a
    // subscription: without it the budget would be a total rather than a rate.
    if filled.cycle_spent != FILL_COST {
        failures.push(format!(
            "the fill charged {} to the cycle; {DEAL_TICKS} ticks at {PRICE_PER_TICK} plus the \
             platform fee is {FILL_COST}",
            filled.cycle_spent
        ));
    }
    match dex.inference_get_order(&ob, sub_id).await {
        Ok(order) => {
            if order.amount != SUB_TICKS - DEAL_TICKS {
                failures.push(format!(
                    "the bid still offers {} ticks; it offered {SUB_TICKS} and sold {DEAL_TICKS}",
                    order.amount
                ));
            }
            if order.escrow != SUB_ESCROW - FILL_COST {
                failures.push(format!(
                    "the bid holds {} of escrow; {SUB_ESCROW} less the {FILL_COST} the deal cost \
                     is {}",
                    order.escrow,
                    SUB_ESCROW - FILL_COST
                ));
            }
        }
        Err(err) => failures.push(format!("the partially filled bid is unreadable: {err:?}")),
    }

    // ── 5. and a poke still leaves it alone, now that it has spent ────────
    //
    // Same call as before, against a subscription with money already committed
    // to this cycle. An early roll here would not just step a counter — it would
    // hand back budget the buyer has spent.
    dex.inference_poke_subscription(&ob, sub_id).await.expect("second pokeSubscription accepted");
    settle().await;
    match dex.inference_get_subscription(&ob, sub_id).await {
        Ok(after) => {
            if !after.exists {
                failures
                    .push("a poke after the fill expired the subscription mid-cycle".to_string());
            } else if after.cur_cycle != filled.cur_cycle || after.cycle_spent != filled.cycle_spent
            {
                failures.push(format!(
                    "a poke rolled a cycle that is not over: curCycle {} → {}, cycleSpent {} → {}",
                    filled.cur_cycle, after.cur_cycle, filled.cycle_spent, after.cycle_spent
                ));
            }
        }
        Err(err) => {
            failures.push(format!("subscription unreadable after the second poke: {err:?}"))
        }
    }

    // ── 6. cancelling takes the subscription with the order ───────────────
    //
    // The book drops the subscription row on every removal path, so a cancelled
    // bid must not leave a `getSubscription` that still answers `exists` — a
    // stale row would be pokeable, and its budget refundable, long after the
    // order behind it is gone.
    dex.cancel_inference_order(
        &buyer.address,
        ParamsOfCancelInferenceOrder { model_hash: model_hash.clone(), order_id: sub_id },
        signer_of(&buyer),
    )
    .await
    .expect("cancelInferenceOrder accepted");
    let gone = poll_sub(&dex, &ob, sub_id, "the subscription to be dropped", |s| !s.exists).await;
    if !gone {
        failures.push(
            "cancelling the order left its subscription behind; the row outlives the bid it \
             belongs to"
                .to_string(),
        );
    }

    finish(&dex, &seller, &buyer, &model_hash, failures).await;
}

/// Give a fire-and-forget send time to land before reading what it did — or,
/// for a call whose whole point is that it changes nothing, before concluding
/// that nothing changed.
async fn settle() {
    tokio::time::sleep(POLL_TICK * 8).await;
}

async fn poll_sub<F>(dex: &Dex, ob: &str, id: u128, what: &str, probe: F) -> bool
where
    F: Fn(&dodex_contracts::airegistry::inference_order_book::ResultOfGetSubscription) -> bool,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(sub) = dex.inference_get_subscription(ob, id).await
            && probe(&sub)
        {
            return true;
        }
    }
    eprintln!("[e2e_sub] never reached: {what}");
    false
}

/// Clear both notes off the book whatever happened. A subscription left resting
/// holds its escrow across runs and would be the first bid the next one crosses.
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
    assert!(failures.is_empty(), "e2e_subscription failures: {failures:#?}");
}
