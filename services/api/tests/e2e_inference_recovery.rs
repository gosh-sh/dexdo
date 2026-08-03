// Two ways a seller can walk away from a funded deal, and the two different
// exits the buyer has.
//
// A deal binds the buyer's money the moment the book matches it. Everything
// after that is the seller's move — post the endpoint, then serve ticks — and
// nothing forces them to make it. So there are two dead ends:
//
//   * **never opened.** The escrow arrived and the seller never posted an
//     endpoint. Nothing was delivered, so nothing is owed: `cleanupUnopened`
//     refunds the whole deposit, hands the bond back unslashed, and destroys
//     the deal. It is permissionless — anyone may call it — because there is
//     no discretion left in it.
//
//   * **opened, then abandoned.** The seller froze the probe tick and went
//     silent. `reclaimOnTimeout` lets the BUYER out: on the probe the buyer
//     pays nothing at all, the bond goes back to the seller's withdrawable
//     balance, and the deal stays on chain because the seller may still have
//     earnings in it.
//
// Both are run here, on two deals set up side by side, because the interesting
// assertion is the one that separates them: after both timeouts have passed,
// `cleanup` on the OPENED deal must still refuse. Its guard is a permanent
// latch rather than a timer, and the only way to say that out loud is to point
// the same call at two deals of the same age and watch one work and one not.
//
// ## Why one test and not three
//
// Every refusal here is a post-`accept` revert on a fire-and-forget send, so it
// is read as the absence of its effect and needs a positive control — the same
// call, later, working. Both timers are also long: `MATCH_OPEN_TIMEOUT` is 600s
// from the match, and the reclaim window is the deal's own `settleWindow` plus
// grace, which at the cheapest tick the book will price is 900s from the open.
// Split across binaries those waits would be paid twice for no extra reading;
// side by side, one sleep covers both clocks and the controls fall out of the
// same run.
//
// The windows are READ off the deals rather than assumed: `settleWindow` scales
// with the tick price, so the 900s here is a property of this test's price and
// not a constant to hardcode.
//
//   cargo test -p dodex-api --test e2e_inference_recovery -- --ignored --nocapture
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

/// The cheapest tick the book will price, and so the shortest reclaim window
/// available: the window scales with the price and bottoms out at this one.
const PRICE_PER_TICK: u128 = 1_000_000_000;

const FEE_BPS: u128 = 250;
const BPS: u128 = 10_000;

/// What one tick costs the buyer, fee included.
const UNIT: u128 = PRICE_PER_TICK + PRICE_PER_TICK * FEE_BPS / BPS;

/// The smallest deal there is: a probe tick plus one streaming tick.
const DEAL_TICKS: u128 = 2;

/// More than the fill costs; the book returns the remainder to the buyer note
/// long before anything here is measured.
const BUY_ESCROW: u128 = 6_000_000_000;

const _: () = assert!(BUY_ESCROW >= DEAL_TICKS * UNIT);

/// Exactly `TokenContract._bondAmount()` = 2P. Sending more would work, but the
/// TC refunds the excess in a separate message, and an in-flight refund would
/// land in the middle of the balance readings below.
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK;

/// `MATCH_OPEN_TIMEOUT` — how long a funded deal waits for an `open()` before
/// anyone may clean it up. A contract constant, unlike the reclaim window.
const MATCH_OPEN_TIMEOUT: u64 = 600;

/// Slack added to whichever deadline lands last, so a block boundary or a
/// second of clock skew does not decide the run.
const DEADLINE_SLACK: u64 = 45;

/// What the buyer gets back in total: deal A's whole deposit, plus deal B's
/// frozen probe tick and everything still undelivered behind it. Nothing was
/// served on either, so between them the buyer pays for nothing.
const EXPECTED_BUYER_REFUND: u128 = 2 * DEAL_TICKS * UNIT;

/// What reaches the seller's NOTE: only deal A's bond. Deal B's bond is
/// returned too, but into the deal's withdrawable balance — the reclaimed deal
/// survives, so its money stays in it until the seller withdraws.
const EXPECTED_SELLER_REFUND: u128 = SELLER_BOND;

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

/// One funded deal, and the book it lives on named by its model — which is
/// what the cleanup path needs to clear the notes' orders afterwards.
struct Deal {
    token_contract: String,
    model_hash: String,
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; sleeps out a 600s and a 900s window (~20 min)"]
async fn a_buyer_gets_out_of_a_deal_the_seller_abandoned_whichever_way_it_was_abandoned() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    let seller = pool.notes[16 % pool.notes.len()].clone();
    let buyer = pool.notes[17 % pool.notes.len()].clone();
    assert_ne!(
        seller.address, buyer.address,
        "the pool handed out one note for both sides; the whole subject here is money moving \
         from one party's deal back to the other party, which a single account cannot show"
    );

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    eprintln!("[e2e_recovery] seller={} buyer={}", seller.address, buyer.address);

    let mut failures: Vec<String> = Vec::new();

    // ── 1. the deal that gets opened, set up first ────────────────────────
    //
    // Its clock is the longer of the two and starts at `open()`, so starting it
    // before the other deal exists is what keeps the single sleep short.
    let abandoned = match setup_deal(&dex, &seller, &buyer, suffix, "abandoned").await {
        Ok(deal) => deal,
        Err(err) => {
            failures.push(err);
            finish(&dex, &seller, &buyer, &[], failures).await;
            return;
        }
    };
    dex.token_contract_open(
        &abandoned.token_contract,
        ParamsOfOpen { endpoint_cipher: "00".to_string() },
        signer_of(&seller),
    )
    .await
    .expect("TokenContract.open accepted");
    let opened = poll_tc(&dex, &abandoned.token_contract, "the stream to open", |s| s.opened).await;
    if !opened {
        failures.push("the deal meant to be abandoned mid-stream never opened".to_string());
        finish(&dex, &seller, &buyer, &[&abandoned], failures).await;
        return;
    }

    // ── 2. the deal that never gets opened ────────────────────────────────
    let unopened = match setup_deal(&dex, &seller, &buyer, suffix + 1, "unopened").await {
        Ok(deal) => deal,
        Err(err) => {
            failures.push(err);
            finish(&dex, &seller, &buyer, &[&abandoned], failures).await;
            return;
        }
    };

    // ── 3. when each deal comes due, read off the deals themselves ────────
    let unopened_state = dex
        .token_contract_get_state(&unopened.token_contract)
        .await
        .expect("state of the unopened deal");
    let abandoned_state = dex
        .token_contract_get_state(&abandoned.token_contract)
        .await
        .expect("state of the abandoned deal");
    let abandoned_config = dex
        .token_contract_get_config(&abandoned.token_contract)
        .await
        .expect("config of the abandoned deal");

    let cleanup_due = unopened_state.funded_time + MATCH_OPEN_TIMEOUT;
    let reclaim_due = abandoned_state.last_advance + abandoned_config.stream_timeout;
    eprintln!(
        "[e2e_recovery] cleanup due at {cleanup_due} (funded {} + {MATCH_OPEN_TIMEOUT}), reclaim \
         due at {reclaim_due} (opened {} + {}), now {}",
        unopened_state.funded_time,
        abandoned_state.last_advance,
        abandoned_config.stream_timeout,
        now_unix()
    );
    if unopened_state.deposit != DEAL_TICKS * UNIT {
        failures.push(format!(
            "the unopened deal holds {} rather than the {DEAL_TICKS} ticks it was funded for ({})",
            unopened_state.deposit,
            DEAL_TICKS * UNIT
        ));
    }
    if abandoned_state.frozen != PRICE_PER_TICK {
        failures.push(format!(
            "the opened deal froze {} as its probe tick rather than one tick of {PRICE_PER_TICK}",
            abandoned_state.frozen
        ));
    }

    // ── 4. neither exit is open yet ───────────────────────────────────────
    //
    // Both refusals are reverts after `tvm.accept()` on a fire-and-forget send,
    // so neither reports anything back. They are read as the deals being
    // untouched — and each is answered by the same call succeeding in step 6,
    // which is what makes "untouched" mean the guard rather than a lost message.
    dex.stream_cleanup(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: unopened.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("early streamCleanup accepted");
    dex.stream_reclaim(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: abandoned.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("early streamReclaim accepted");
    settle().await;

    match dex.token_contract_get_state(&unopened.token_contract).await {
        Ok(s) if s.funded && s.deposit == unopened_state.deposit => {}
        Ok(s) => failures.push(format!(
            "a cleanup {}s before the deal came due took effect anyway: funded={} deposit={}",
            cleanup_due.saturating_sub(now_unix()),
            s.funded,
            s.deposit
        )),
        Err(err) => failures.push(format!(
            "the unopened deal was destroyed by an early cleanup — it no longer answers: {err:?}"
        )),
    }
    match dex.token_contract_get_state(&abandoned.token_contract).await {
        Ok(s) if s.opened && s.frozen == abandoned_state.frozen => {}
        Ok(s) => failures.push(format!(
            "a reclaim {}s before the window closed took effect anyway: opened={} frozen={} \
             deposit={}",
            reclaim_due.saturating_sub(now_unix()),
            s.opened,
            s.frozen,
            s.deposit
        )),
        Err(err) => failures.push(format!("the abandoned deal became unreadable: {err:?}")),
    }
    if !failures.is_empty() {
        finish(&dex, &seller, &buyer, &[&abandoned, &unopened], failures).await;
        return;
    }

    // ── 5. wait out whichever clock runs longest ──────────────────────────
    let target = cleanup_due.max(reclaim_due) + DEADLINE_SLACK;
    let now = now_unix();
    if now < target {
        eprintln!("[e2e_recovery] sleeping {}s for both windows…", target - now);
        tokio::time::sleep(Duration::from_secs(target - now)).await;
    }

    // Everything the book and the bond posting had in flight has long landed by
    // now, so what the notes hold is stable and a delta across the two
    // recoveries is theirs alone.
    let buyer_before = shell_of(&dex, &buyer.address).await;
    let seller_before = shell_of(&dex, &seller.address).await;

    // ── 6a. cleanup still refuses the deal that WAS opened ────────────────
    //
    // Same call, same age, past the same timeout — and it must do nothing,
    // because the deal carries a permanent latch saying it was once open. The
    // control is 6b: the identical call against the deal that never was.
    dex.stream_cleanup(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: abandoned.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("streamCleanup on the opened deal accepted");
    settle().await;
    match dex.token_contract_get_state(&abandoned.token_contract).await {
        Ok(s) if s.funded && s.opened && s.deposit == abandoned_state.deposit => {}
        Ok(s) => failures.push(format!(
            "cleanup destroyed a deal that had been opened: funded={} opened={} deposit={}. The \
             opened case has earnings to settle and belongs to reclaim, not to a permissionless \
             sweep",
            s.funded, s.opened, s.deposit
        )),
        Err(err) => failures.push(format!(
            "cleanup destroyed a deal that had been opened — it no longer answers: {err:?}"
        )),
    }

    // ── 6b. and takes the one that never was ──────────────────────────────
    dex.stream_cleanup(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: unopened.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("streamCleanup accepted");
    let swept = poll_gone(&dex, &unopened.token_contract).await;
    if !swept {
        let state = dex.token_contract_get_state(&unopened.token_contract).await;
        failures.push(format!(
            "the never-opened deal survived its cleanup: {state:?}. Nothing was delivered on it, \
             so there is nothing for it to still be holding"
        ));
    }

    // ── 6c. and the buyer walks out of the abandoned stream ───────────────
    dex.stream_reclaim(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: abandoned.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("streamReclaim accepted");
    let released = poll_tc(&dex, &abandoned.token_contract, "the stream to release", |s| {
        !s.opened && s.frozen == 0 && s.deposit == 0
    })
    .await;
    let after = dex
        .token_contract_get_state(&abandoned.token_contract)
        .await
        .expect("state after the reclaim");
    eprintln!(
        "[e2e_recovery] reclaimed: opened={} frozen={} deposit={} finalizedOwed={}",
        after.opened, after.frozen, after.deposit, after.finalized_owed
    );
    if !released {
        failures.push(format!(
            "the reclaim left the stream holding money: opened={} frozen={} deposit={}",
            after.opened, after.frozen, after.deposit
        ));
    }
    // The seller went silent on the PROBE, which is the tick the buyer never
    // agreed to pay for. So the deal owes the seller exactly the bond back and
    // not one tick more — a no-show is not slashed, and it is not paid either.
    if after.finalized_owed != SELLER_BOND {
        failures.push(format!(
            "the abandoned deal owes its seller {}; a probe nobody accepted should leave exactly \
             the returned bond of {SELLER_BOND}",
            after.finalized_owed
        ));
    }
    match dex.token_contract_get_fees(&abandoned.token_contract).await {
        Ok(fees) if fees.ticks_finalized == 0 => {}
        Ok(fees) => failures.push(format!(
            "the abandoned deal finalized {} tick(s); the seller never accepted the probe",
            fees.ticks_finalized
        )),
        Err(err) => failures.push(format!("fees of the abandoned deal unreadable: {err:?}")),
    }

    // ── 7. and the money is where each exit says it should be ─────────────
    let refunded = poll_until("the two exits to pay out", || async {
        shell_of(&dex, &buyer.address).await >= buyer_before + EXPECTED_BUYER_REFUND
    })
    .await;
    let buyer_after = shell_of(&dex, &buyer.address).await;
    let seller_after = shell_of(&dex, &seller.address).await;
    eprintln!(
        "[e2e_recovery] buyer {buyer_before} → {buyer_after}, seller {seller_before} → \
         {seller_after}"
    );
    if !refunded || buyer_after - buyer_before != EXPECTED_BUYER_REFUND {
        failures.push(format!(
            "the buyer got back {} across both exits; nothing was served on either deal, so the \
             whole {EXPECTED_BUYER_REFUND} they funded should have returned",
            buyer_after.saturating_sub(buyer_before)
        ));
    }
    // Asymmetric on purpose: cleanup destroys its deal, so the bond has nowhere
    // to live but the seller's note. Reclaim leaves its deal standing, so that
    // bond stays inside it as withdrawable balance — asserted above.
    if seller_after - seller_before != EXPECTED_SELLER_REFUND {
        failures.push(format!(
            "the seller's note took {} out of the two exits; only the destroyed deal pays its \
             bond out to the note ({EXPECTED_SELLER_REFUND}), the reclaimed one keeps it inside",
            seller_after.saturating_sub(seller_before)
        ));
    }

    finish(&dex, &seller, &buyer, &[&abandoned, &unopened], failures).await;
}

/// Deploy a book, put a deal on it, and cross it with the buyer's order — up to
/// the point where the seller's next move is the one they will not make.
async fn setup_deal(
    dex: &Dex,
    seller: &TestPn,
    buyer: &TestPn,
    nonce_seed: u128,
    label: &str,
) -> Result<Deal, String> {
    let model_name = format!("e2e-recovery-{label}--{nonce_seed}");
    let model_hash = model_hash_dec(&model_name);

    dex.deploy_inference_order_book(
        &seller.address,
        ParamsOfDeployInferenceOrderBook {
            model_hash: model_hash.clone(),
            model_name: model_name.clone(),
        },
        signer_of(seller),
    )
    .await
    .map_err(|e| format!("{label}: deployInferenceOrderBook: {e:?}"))?;
    let order_book = dex
        .get_inference_order_book_address(
            &seller.address,
            ParamsOfInferenceOrderBook { model_hash: model_hash.clone() },
        )
        .await
        .map_err(|e| format!("{label}: getInferenceOrderBookAddress: {e:?}"))?;
    wait_inference_book_live(dex, &order_book, POLL_TICKS, POLL_TICK)
        .await
        .map_err(|e| format!("{label}: {e}"))?;

    let nonce = (nonce_seed % 1_000_000_000) as u64 + 1;
    let token_contract = deploy_token_contract(
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
    .map_err(|e| format!("{label}: deploy TokenContract: {e:?}"))?;
    eprintln!("[e2e_recovery] {label}: order_book={order_book} token_contract={token_contract}");

    dex.post_sell_offer(
        &seller.address,
        ParamsOfPostSellOffer { flags: 0, nonce },
        signer_of(seller),
    )
    .await
    .map_err(|e| format!("{label}: postSellOffer: {e:?}"))?;
    wait_sell_offer_rested(dex, &order_book, &token_contract, POLL_TICKS, POLL_TICK)
        .await
        .map_err(|e| format!("{label}: {e}"))?;

    // The bond is posted on BOTH deals: what happens to it is half of what
    // separates the two exits, and a deal without one would have nothing to say
    // about it either way.
    dex.post_seller_bond(
        &seller.address,
        ParamsOfPostSellerBond { nonce, amount: SELLER_BOND },
        signer_of(seller),
    )
    .await
    .map_err(|e| format!("{label}: postSellerBond: {e:?}"))?;

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
        signer_of(buyer),
    )
    .await
    .map_err(|e| format!("{label}: placeInferenceBuy: {e:?}"))?;

    let mut bonded = false;
    let mut funded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if !funded {
            funded = dex
                .token_contract_get_state(&token_contract)
                .await
                .map(|s| s.funded)
                .unwrap_or(false);
        }
        if !bonded {
            bonded = dex
                .token_contract_get_seller_bond(&token_contract)
                .await
                .map(|b| b.bond_funded)
                .unwrap_or(false);
        }
        if funded && bonded {
            return Ok(Deal { token_contract, model_hash });
        }
    }
    Err(format!("{label}: the deal never became funded ({funded}) and bonded ({bonded})"))
}

/// Physical SHELL sitting on a note. The refunds land here as ECC, not in the
/// note's own `_balance` ledger, so this is the reading that tracks them.
async fn shell_of(dex: &Dex, note: &str) -> u128 {
    dex.dex_account_shell(note).await.map(|a| a.shell).unwrap_or(0)
}

/// Give a fire-and-forget send time to land before reading what it did.
async fn settle() {
    tokio::time::sleep(POLL_TICK * 8).await;
}

async fn poll_tc<F>(dex: &Dex, tc: &str, what: &str, probe: F) -> bool
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
    eprintln!("[e2e_recovery] never reached: {what}");
    false
}

/// Wait for a deal to stop being a live account. `cleanupUnopened` ends in a
/// `selfdestruct`, so the deal does not merely empty out — it goes away, and an
/// account that still runs code is a different outcome from one that does not.
async fn poll_gone(dex: &Dex, tc: &str) -> bool {
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        match dex.self_rooted_account_shell(tc).await {
            Ok(account) if account.acc_type != "Active" => {
                eprintln!("[e2e_recovery] deal swept: acc_type={}", account.acc_type);
                return true;
            }
            _ => continue,
        }
    }
    false
}

async fn poll_until<F, Fut>(what: &str, probe: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if probe().await {
            return true;
        }
    }
    eprintln!("[e2e_recovery] never reached: {what}");
    false
}

/// Clear both notes off every book this run touched, then report.
async fn finish(
    dex: &Dex,
    seller: &TestPn,
    buyer: &TestPn,
    deals: &[&Deal],
    failures: Vec<String>,
) {
    for deal in deals {
        for note in [seller, buyer] {
            let _ = dex
                .cancel_all_inference_orders(
                    &note.address,
                    ParamsOfCancelAllInferenceOrders { model_hash: deal.model_hash.clone() },
                    signer_of(note),
                )
                .await;
        }
    }
    assert!(failures.is_empty(), "e2e_recovery failures: {failures:#?}");
}
