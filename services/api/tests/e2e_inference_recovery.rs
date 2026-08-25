// The way out of a funded deal the seller never opened, and the deal it must
// not touch.
//
// A deal binds the buyer's money the moment the book matches it. Everything
// after that is the seller's move — post the endpoint, then serve ticks — and
// nothing forces them to make it. `cleanupUnopened` is the exit for the case
// where they never made the first one: nothing was delivered, so nothing is
// owed, and the call refunds the buyer's whole deposit, hands the seller bond
// back unslashed and destroys the deal. It is permissionless because there is
// no discretion left in it.
//
// ## Half of a deleted test, and which half
//
// The original binary covered TWO exits and was deleted by the v4.0.33 contract
// sync (`4a6b0ce`). One of them is genuinely gone: `reclaimOnTimeout` — the
// buyer's way out of a deal that WAS opened and then abandoned — left the ABI
// with that sync and has no replacement, so the abandoned-mid-stream case is
// not covered here or anywhere.
//
// This one survived untouched. `cleanupUnopened` (`TokenContract.sol:1933`) is
// still there, still permissionless, still gated on `MATCH_OPEN_TIMEOUT`, and
// `PrivateNote.streamCleanup` (`:1009`) still forwards to it. So the half that
// can be restored is restored, and the half that cannot is named rather than
// quietly dropped.
//
// ## What separates the two deals here
//
// Two deals are set up side by side and only one may be swept. The other is
// opened and left alone, and pointing the same call at it after the same
// timeout has passed is what says the refusal is about the deal's state rather
// than about the clock.
//
// **Which guard that actually exercises is worth being exact about**, because
// the deleted version's comment was not. `cleanupUnopened` carries two:
//
//   require(!_opened,     ERR_ALREADY_OPEN)   // :1935
//   require(!_everOpened, ERR_ALREADY_OPEN)   // :1940 — the permanent latch
//
// An opened-and-abandoned deal has BOTH set, so the first one answers and the
// latch is never reached. The latch would need a deal with `_opened == false`
// and `_everOpened == true` — and today no such deal can exist, because every
// way out of an open stream destroys the contract (`stop` ends in
// `_payOwedAndDie`). The one path that used to leave a released deal standing
// was `reclaimOnTimeout`, which is exactly what this sync removed. So the latch
// is defensive code with no reachable state behind it, and this test exercises
// `!_opened`. Saying otherwise would be claiming coverage of a line no scenario
// can reach.
//
// ## What the money has to do
//
// Read off the deal rather than hardcoded, because one of the two figures is
// not the one the buyer sent. `cleanupUnopened` calls `_releaseBuyerBond`
// first (`:1943`), which folds `_buyerBond` INTO `_deposit` (`:1415-1419`) —
// so the single payment to the buyer is deposit plus bond, and an expectation
// written as "the escrow it funded" would be short by the bond and read as a
// missing refund.
//
// The seller's side is the bond alone, and it lands on the note rather than
// staying in the deal: the deal is destroyed, so there is nowhere else for it
// to be. Both are read as ECC deltas on the accounts — `_payShell` moves the
// figure, and the note's own `_balance` ledger never learns of it.
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
use dodex_contracts::dex::private_note::ParamsOfFundDeal;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45; // 90s budget per wait.
/// Both bonds are in-flight messages when `fundDeal` returns.
const BOND_TICKS: u32 = 30;

/// The seller's note and the buyer's. `PN-INF`, because a deal's escrow comes
/// out of the note's SHELL balance and only the constructor writes it — the
/// deleted version drew from `PN-API`, which predates that group existing.
const SELLER_NOTE_INDEX: usize = 24;
const BUYER_NOTE_INDEX: usize = 25;

/// The cheapest tick the book will price.
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

/// Exactly `TokenContract._bondAmount()` = 2P (`:554-556`). Sending more would
/// work — `fundDeal` refunds the excess (`:914`) — but the refund is a separate
/// message, and an in-flight one would land in the middle of the readings
/// below. DERIVED from the price, never hardcoded: a short bond bounces back
/// and leaves no trace but `bond_funded: false` two steps later.
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK;
const DEAL_GAS_SHELL: u128 = 1_000_000_000;

/// `PrivateNote.MAX_SELL_TTL` — the longest lifetime a SELL offer may ask for.
const MAX_SELL_TTL: u64 = 3600;

/// `FLAG_IOC`: cross what rests and leave no remainder behind.
const FLAG_IOC: u8 = 0x01;

/// `CURRENCIES_ID_SHELL` (`contracts/dex/modifiers/modifiers.sol:202`) — the key
/// SHELL sits under in a note's `_balance` map.
const SHELL_CURRENCY_ID: u32 = 2;

/// `MATCH_OPEN_TIMEOUT` (`modifiers.sol:27`) — how long a funded deal waits for
/// an `open()` before anyone may sweep it. A contract constant, and the only
/// clock this test has to outwait now that the reclaim window has gone with the
/// call that used it.
const MATCH_OPEN_TIMEOUT: u64 = 600;

/// Slack on the deadline, so a block boundary or a second of clock skew does
/// not decide the run.
const DEADLINE_SLACK: u64 = 45;

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
#[ignore = "requires shellnet + seed_notes.json; sleeps out MATCH_OPEN_TIMEOUT (~15 min)"]
async fn a_deal_the_seller_never_opened_is_swept_and_one_that_was_opened_is_not() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load_inference();
    let seller = pool.notes[SELLER_NOTE_INDEX % pool.notes.len()].clone();
    let buyer = pool.notes[BUYER_NOTE_INDEX % pool.notes.len()].clone();
    assert_ne!(
        seller.address, buyer.address,
        "the pool handed out one note for both sides; the whole subject here is money moving \
         from one party's deal back to the other party, which a single account cannot show"
    );

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    eprintln!("[e2e_recovery] seller={} buyer={}", seller.address, buyer.address);

    let mut failures: Vec<String> = Vec::new();

    // ── 1. the deal that never gets opened, set up FIRST ──────────────────
    //
    // Its clock is the only one left and it starts at the match, so starting it
    // before the control deal exists is what keeps the single sleep short: the
    // control's setup runs inside the window rather than after it.
    let unopened = match setup_deal(&dex, &seller, &buyer, suffix, "unopened").await {
        Ok(deal) => deal,
        Err(err) => {
            failures.push(err);
            finish(&dex, &seller, &buyer, &[], failures).await;
            return;
        }
    };

    // ── 2. and the control: a deal that IS opened, and then left alone ────
    //
    // Self-traded, deliberately. What it has to be is OPEN and the same age;
    // who its parties are says nothing about the guard, and a second buyer note
    // would be a row of the pool spent on nothing.
    let opened = match setup_deal(&dex, &seller, &seller, suffix + 1, "opened").await {
        Ok(deal) => deal,
        Err(err) => {
            failures.push(err);
            finish(&dex, &seller, &buyer, &[&unopened], failures).await;
            return;
        }
    };
    if let Err(err) = dex
        .token_contract_open(
            &opened.token_contract,
            ParamsOfOpen { endpoint_cipher: "00".to_string() },
            signer_of(&seller),
        )
        .await
    {
        failures.push(format!("the control deal refused to open: {err:?}"));
        finish(&dex, &seller, &buyer, &[&unopened, &opened], failures).await;
        return;
    }
    if !poll_tc(&dex, &opened.token_contract, "the control stream to open", |s| s.opened).await {
        failures.push(
            "the control deal never opened, so it cannot say what cleanup refuses".to_string(),
        );
        finish(&dex, &seller, &buyer, &[&unopened, &opened], failures).await;
        return;
    }

    // ── 3. when the sweep comes due, read off the deal itself ─────────────
    let unopened_state = dex
        .token_contract_get_state(&unopened.token_contract)
        .await
        .expect("state of the unopened deal");
    let opened_state = dex
        .token_contract_get_state(&opened.token_contract)
        .await
        .expect("state of the opened deal");
    let cleanup_due = unopened_state.funded_time + MATCH_OPEN_TIMEOUT;
    eprintln!(
        "[e2e_recovery] cleanup due at {cleanup_due} (funded {} + {MATCH_OPEN_TIMEOUT}), now {}",
        unopened_state.funded_time,
        now_unix()
    );
    if unopened_state.deposit != DEAL_TICKS * UNIT {
        failures.push(format!(
            "the unopened deal holds {} rather than the {DEAL_TICKS} ticks it was funded for ({})",
            unopened_state.deposit,
            DEAL_TICKS * UNIT
        ));
    }

    // ── 4. the exit is not open yet ───────────────────────────────────────
    //
    // The refusal is a revert after `tvm.accept()` on a fire-and-forget send, so
    // it reports nothing back. It is read as the deal being untouched — and it
    // is answered by the same call succeeding in step 6, which is what makes
    // "untouched" mean the guard rather than a lost message.
    dex.stream_cleanup(
        &buyer.address,
        ParamsOfStreamDeal { token_contract: unopened.token_contract.clone() },
        signer_of(&buyer),
    )
    .await
    .expect("early streamCleanup accepted");
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
    if !failures.is_empty() {
        finish(&dex, &seller, &buyer, &[&unopened, &opened], failures).await;
        return;
    }

    // ── 5. wait the window out ────────────────────────────────────────────
    let target = cleanup_due + DEADLINE_SLACK;
    let now = now_unix();
    if now < target {
        eprintln!("[e2e_recovery] sleeping {}s for the no-show window…", target - now);
        tokio::time::sleep(Duration::from_secs(target - now)).await;
    }

    // What the deal is about to pay out, read now rather than assumed: the
    // buyer's single payment is deposit PLUS the bond `_releaseBuyerBond` folds
    // into it, and only the chain knows the second figure.
    let buyer_bond = dex
        .token_contract_get_buyer_bond(&unopened.token_contract)
        .await
        .map(|b| b.bond_held)
        .unwrap_or(0);
    let expected_buyer_refund = unopened_state.deposit + buyer_bond;
    eprintln!(
        "[e2e_recovery] the sweep owes the buyer {} (deposit {} + bond {buyer_bond}) and the \
         seller's note {SELLER_BOND}",
        expected_buyer_refund, unopened_state.deposit
    );

    // Everything the book and the bonds had in flight has long landed by now,
    // so what the notes hold is stable and a delta across the sweep is its own.
    let buyer_before = shell_of(&dex, &buyer.address).await;
    let seller_before = shell_of(&dex, &seller.address).await;

    // ── 6a. cleanup still refuses the deal that WAS opened ────────────────
    //
    // Same call, same age, past the same timeout — and it must do nothing. The
    // control is 6b: the identical call against the deal that never was.
    dex.stream_cleanup(
        &seller.address,
        ParamsOfStreamDeal { token_contract: opened.token_contract.clone() },
        signer_of(&seller),
    )
    .await
    .expect("streamCleanup on the opened deal accepted");
    settle().await;
    match dex.token_contract_get_state(&opened.token_contract).await {
        Ok(s) if s.funded && s.opened && s.deposit == opened_state.deposit => {}
        Ok(s) => failures.push(format!(
            "cleanup destroyed a deal that had been opened: funded={} opened={} deposit={}. An \
             opened deal has earnings to settle and is not a permissionless sweep's to take",
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
    if !poll_gone(&dex, &unopened.token_contract).await {
        let state = dex.token_contract_get_state(&unopened.token_contract).await;
        failures.push(format!(
            "the never-opened deal survived its cleanup: {state:?}. Nothing was delivered on it, \
             so there is nothing for it to still be holding"
        ));
    }

    // ── 7. and the money is where the sweep says it should be ─────────────
    let paid = poll_until("the sweep to pay out", || async {
        shell_of(&dex, &buyer.address).await >= buyer_before + expected_buyer_refund
    })
    .await;
    let buyer_after = shell_of(&dex, &buyer.address).await;
    let seller_after = shell_of(&dex, &seller.address).await;
    eprintln!(
        "[e2e_recovery] buyer {buyer_before} → {buyer_after}, seller {seller_before} → \
         {seller_after}"
    );
    if !paid || buyer_after - buyer_before != expected_buyer_refund {
        failures.push(format!(
            "the buyer got back {}; nothing was served, so the whole {expected_buyer_refund} — \
             deposit and the bond folded into it — should have returned",
            buyer_after.saturating_sub(buyer_before)
        ));
    }
    // The bond is not slashed and it is not paid out as earnings either: a
    // no-show delivered nothing, so the seller is left exactly whole. It lands
    // on the NOTE because the deal it was sitting in no longer exists.
    if seller_after - seller_before != SELLER_BOND {
        failures.push(format!(
            "the seller's note took {} out of the sweep; a no-show is neither slashed nor paid, \
             so its bond of {SELLER_BOND} should come back untouched and nothing else with it",
            seller_after.saturating_sub(seller_before)
        ));
    }

    finish(&dex, &seller, &buyer, &[&unopened, &opened], failures).await;
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
        ParamsOfPostSellOffer { flags: 0, nonce, ttl: MAX_SELL_TTL },
        signer_of(seller),
    )
    .await
    .map_err(|e| format!("{label}: postSellOffer: {e:?}"))?;
    wait_sell_offer_rested(dex, &order_book, &token_contract, POLL_TICKS, POLL_TICK)
        .await
        .map_err(|e| format!("{label}: {e}"))?;

    dex.place_inference_buy(
        &buyer.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.clone(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: DEAL_TICKS,
            escrow: BUY_ESCROW,
            flags: FLAG_IOC,
            deadline: 0,
        },
        signer_of(buyer),
    )
    .await
    .map_err(|e| format!("{label}: placeInferenceBuy: {e:?}"))?;
    if !poll_tc(dex, &token_contract, "the match to fund the deal", |s| s.funded).await {
        return Err(format!("{label}: the match never handed the escrow to the TokenContract"));
    }

    // The bond is posted on BOTH deals: what happens to it is half of what the
    // sweep has to say, and a deal without one would have nothing to say about
    // it either way. `fundDeal` covers the seller's half only — the buyer's was
    // funded inline by its own note on the fill (`PrivateNote.sol:752`).
    dex.fund_deal(
        &seller.address,
        ParamsOfFundDeal {
            nonce,
            gas_shell: DEAL_GAS_SHELL,
            amount: SELLER_BOND,
            endpoint_cipher: None,
        },
        signer_of(seller),
    )
    .await
    .map_err(|e| format!("{label}: fundDeal: {e:?}"))?;

    // Both bonds, not just the seller's: the control deal has to be able to
    // `open`, and `TokenContract.sol:984-985` requires the pair.
    for _ in 0..BOND_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        let seller_in = matches!(
            dex.token_contract_get_seller_bond(&token_contract).await,
            Ok(b) if b.bond_funded
        );
        let buyer_in = matches!(
            dex.token_contract_get_buyer_bond(&token_contract).await,
            Ok(b) if b.bond_held > 0
        );
        if seller_in && buyer_in {
            return Ok(Deal { token_contract, model_hash });
        }
    }
    Err(format!(
        "{label}: the deal's bonds never both landed: seller={:?} buyer={:?}",
        dex.token_contract_get_seller_bond(&token_contract).await,
        dex.token_contract_get_buyer_bond(&token_contract).await
    ))
}

/// The note's LOGICAL SHELL balance — `_balance[CURRENCIES_ID_SHELL]`, read out
/// of `getDetails`.
///
/// NOT the account's physical ECC, and the difference is the whole reason
/// pipeline #296 read two zero deltas across a sweep that had plainly happened.
/// The deleted version of this file measured physical ECC and was right to at
/// the time; v4.0.33 changed what it was measuring. SHELL is now a bookkeeping
/// NUMBER rather than a currency the deal holds: `_payShell`
/// (`TokenContract.sol:493`) subtracts from `_balance` and calls
/// `creditFromDeal` on the note, which adds to the note's own `_balance` map —
/// nothing physical moves at any point. `PrivateNote.sol:898` says so in as
/// many words about the mirror path: "one pot: the buy is paid from
/// `_balance[CURRENCIES_ID_SHELL]` and nothing physical moves."
///
/// What DOES move physically is gas: `fundDeal`'s `gasShell`, and the residual
/// the destruct sweeps back to the seller's note. So a physical reading here
/// would be measuring gas accounting and calling it a refund — in #296 it
/// showed the seller down exactly the 2 × 1e9 of gas the two deals were funded
/// with, and the bond return nowhere at all.
async fn shell_of(dex: &Dex, note: &str) -> u128 {
    dex.get_private_note_details(note)
        .await
        .ok()
        .and_then(|d| d.balance.get(&SHELL_CURRENCY_ID.to_string()).copied())
        .unwrap_or(0)
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
