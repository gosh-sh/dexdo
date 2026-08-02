// End-to-end dispute-timeout settlement smoke for the AI Registry inference market
// against a real shellnet, driven through `dodex_chain::Dex`.
//
// A streaming dispute that reaches its timeout with no concession settles by the
// SYMMETRIC MARK-FOR-MARK BURN (spec §4.2): the buyer's disputed sum
// `D = prepaid + frozen` is burned, and an equal `D` is burned from the seller's
// mirror bond. The seller gets NOTHING from the disputed ticks; only the unburned
// remainder of the bond (`bond - min(D, bond)`) is credited back, the remaining
// deposit refunds the buyer, and a disputed close earns no rebate.
//
// The load-bearing property is that this outcome is INVARIANT to the per-tick
// acceptance window. `resolveDisputeTimeout` applies no window gate at all in
// Streaming, so a timeout that fires while the window is still open and one that
// fires after it elapsed must settle identically — both sides lose the disputed
// value either way. Two deals run in parallel (self-trade: one note is buyer AND
// seller on both) purely to straddle that window:
//   * Case A — timeout fires at DISPUTE_WINDOW (600s), acceptance window OPEN;
//   * Case B — timeout fires after the acceptance window (1200s) has ELAPSED.
// Both must produce the same outcome; a divergence means a window gate crept back
// into the timeout path.
//
// `price_per_tick` is 2 SHELL so the acceptance window
// (`settle_window = clamp(P*600/1e9, 180, 3600) = 1200s`) is LONGER than the
// dispute window (600s) — without that the two cases could not be distinguished
// in time, and the invariance would be untested.
//
// The seller side is measured by `ticksFinalized`, not a `finalizedOwed` delta:
// the close also credits the unburned bond, which would otherwise read as a
// payout for the disputed tick.
//
// VERY slow: both windows scale with the 2-SHELL tick price, so it sleeps out a
// a 180s probe window AND a ~1200s acceptance window (~25 min).
//
//   cargo test -p dodex-api --test e2e_inference_dispute -- --ignored --nocapture
//
// === SECURITY NOTE === see e2e_inference.rs.

mod common;

use std::time::Duration;
use std::time::Instant;
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
use common::test_pns::TestPnPool;
use dodex_chain::Dex;
use dodex_contracts::airegistry::token_contract::ParamsOfOpen;
use dodex_contracts::dex::private_note::ParamsOfDeployInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfInferenceOrderBook;
use dodex_contracts::dex::private_note::ParamsOfPlaceInferenceBuy;
use dodex_contracts::dex::private_note::ParamsOfPostSellOffer;
use dodex_contracts::dex::private_note::ParamsOfPostSellerBond;
use dodex_contracts::dex::private_note::ParamsOfStreamDeal;

const POLL_TICK: Duration = Duration::from_secs(2);
const POLL_TICKS: u32 = 45;
// `settle_window = clamp(P*600/1e9, 180, 3600)`. P = 1.1e9 ⇒ 660s > DISPUTE_WINDOW.
// The regime this test needs is `settle_window > DISPUTE_WINDOW (600s)`, and
// `settle_window = clamp(P*600/1e9, 180, 3600)`, so P must exceed 1 SHELL. P is
// also constrained to whole multiples of `PRICE_STEP` (1e9), which makes 2 SHELL
// the cheapest price that keeps the window open past the timeout: W = 1200s.
const PRICE_PER_TICK: u128 = 2_000_000_000;
const DEAL_TICKS: u128 = 4;
// >= ticks * (price + 2.5% fee) = 4 * 2.05e9 = 8.2e9.
const BUY_ESCROW: u128 = 10_000_000_000;
// Seller mirror bond = `TokenContract._bondAmount()` = 2P, plus a small margin;
// it scales with `price_per_tick`, so it must be derived from P (a fixed value
// under-funds it and `fundSellerBond` rejects the message).
const SELLER_BOND: u128 = 2 * PRICE_PER_TICK + PRICE_PER_TICK / 100;
// Probe acceptance is gated by the fixed `PROBE_WINDOW` (180s), not by the
// price-scaled `_settleWindow` — `advance` picks by phase and the probe is
// unaccepted at this point. Only the dispute and streaming windows below scale
// with the price, and they are what keep this test out of CI.
const PROBE_WAIT: Duration = Duration::from_secs(180 + 45);
const DISPUTE_WINDOW_S: u64 = 600;
const SETTLE_WINDOW_S: u64 = 1200;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; sleeps out a 180s probe + ~1200s acceptance window (~25 min)"]
async fn inference_dispute_timeout_burns_mark_for_mark() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info,ackinacki_kit=debug")
        .try_init();

    let pool = TestPnPool::load();
    // Test isolation: own note per binary (shared notes leak stream/dispute locks).
    let note = pool.notes[5 % pool.notes.len()].clone();
    let keys = KeyPair {
        public: note.owner_public_key_hex.clone(),
        secret: note.owner_secret_key_hex.clone(),
    };
    let signer = || Signer::Keys { keys: keys.clone() };

    let dex = Dex::from_endpoints(vec![network_endpoint()]).expect("Dex::from_endpoints");
    let suffix = unique_suffix();
    let model_name = format!("e2e-dispute--{suffix}");
    let model_hash = model_hash_dec(&model_name);
    eprintln!("[e2e_dispute] note={} model_name={model_name}", note.address);

    let mut failures: Vec<String> = Vec::new();

    // One book hosts both deals (distinct nonces ⇒ distinct TokenContracts).
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
    wait_book_live(&dex, &ob).await;
    eprintln!("[e2e_dispute] order_book={ob}");

    let nonce_base = (suffix % 1_000_000_000) as u64 + 1;
    // A = timeout while window OPEN; B = timeout after window ELAPSED.
    let tc_a = match setup_deal(&dex, &note, &keys, &model_name, &model_hash, &ob, nonce_base).await
    {
        Ok(tc) => tc,
        Err(e) => {
            failures.push(format!("deal A setup: {e}"));
            finish(&dex, &note.address, &model_hash, &keys, failures).await;
            return;
        }
    };
    let tc_b =
        match setup_deal(&dex, &note, &keys, &model_name, &model_hash, &ob, nonce_base + 1).await {
            Ok(tc) => tc,
            Err(e) => {
                failures.push(format!("deal B setup: {e}"));
                finish(&dex, &note.address, &model_hash, &keys, failures).await;
                return;
            }
        };
    eprintln!("[e2e_dispute] A={tc_a} B={tc_b} — both opened");

    // Wait the probe window once (both deals opened), then accept both probes.
    eprintln!("[e2e_dispute] sleeping {}s for the probe window…", PROBE_WAIT.as_secs());
    tokio::time::sleep(PROBE_WAIT).await;
    dex.token_contract_advance(&tc_a, signer()).await.expect("advance A");
    dex.token_contract_advance(&tc_b, signer()).await.expect("advance B");
    let advanced_at = Instant::now();
    if !wait_state(&dex, &tc_a, |s| s.probe_accepted).await
        || !wait_state(&dex, &tc_b, |s| s.probe_accepted).await
    {
        failures.push("advance did not accept the probe on both deals".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }
    let before_a = snapshot(&dex, &tc_a).await;
    let before_b = snapshot(&dex, &tc_b).await;
    eprintln!("[e2e_dispute] pre-dispute A={before_a:?} B={before_b:?}");

    // Buyer disputes both right after the streaming tick is prepaid.
    dex.stream_dispute(
        &note.address,
        ParamsOfStreamDeal { token_contract: tc_a.clone() },
        signer(),
    )
    .await
    .expect("dispute A");
    dex.stream_dispute(
        &note.address,
        ParamsOfStreamDeal { token_contract: tc_b.clone() },
        signer(),
    )
    .await
    .expect("dispute B");
    if !wait_state(&dex, &tc_a, |s| s.disputed).await
        || !wait_state(&dex, &tc_b, |s| s.disputed).await
    {
        failures.push("dispute did not register on both deals".to_string());
        finish(&dex, &note.address, &model_hash, &keys, failures).await;
        return;
    }

    // Case A: resolve at DISPUTE_WINDOW, while the acceptance window is still open.
    sleep_until(advanced_at, DISPUTE_WINDOW_S + 15).await;
    dex.token_contract_resolve_dispute_timeout(&tc_a, signer())
        .await
        .expect("resolveDisputeTimeout A");
    let _ = wait_state(&dex, &tc_a, |s| !s.opened).await;
    let outcome_a = settle_outcome(&dex, &tc_a, &before_a).await;
    eprintln!("[e2e_dispute] A (window OPEN): {outcome_a:?}");
    failures.extend(check_burn("A (window OPEN)", &before_a, &outcome_a));

    // Case B: resolve after the acceptance window has elapsed.
    sleep_until(advanced_at, SETTLE_WINDOW_S + 20).await;
    dex.token_contract_resolve_dispute_timeout(&tc_b, signer())
        .await
        .expect("resolveDisputeTimeout B");
    let _ = wait_state(&dex, &tc_b, |s| !s.opened).await;
    let outcome_b = settle_outcome(&dex, &tc_b, &before_b).await;
    eprintln!("[e2e_dispute] B (window ELAPSED): {outcome_b:?}");
    failures.extend(check_burn("B (window ELAPSED)", &before_b, &outcome_b));

    // The property the two deals exist to prove: the timeout close carries no
    // window gate, so an open and an elapsed acceptance window settle identically.
    // Both deals were funded from the same terms, so the outcomes are comparable.
    if outcome_a != outcome_b {
        failures.push(format!(
            "dispute timeout is not window-invariant: A (window OPEN) settled {outcome_a:?} but \
             B (window ELAPSED) settled {outcome_b:?} — a window gate crept back into \
             resolveDisputeTimeout"
        ));
    }

    finish(&dex, &note.address, &model_hash, &keys, failures).await;
}

/// Deal state going into the dispute: what the burn is computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreDispute {
    /// Ticks already finalized to the seller (the probe tick).
    ticks_finalized: u128,
    /// Seller credit accrued so far.
    owed: u128,
    /// `D = prepaid + frozen` — the buyer's disputable sum, burned on timeout.
    disputable: u128,
    /// Mirror bond held against `D`.
    bond: u128,
}

/// What the timeout actually did, expressed so the two cases are comparable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettleOutcome {
    /// Ticks finalized AFTER the close — must not have grown.
    ticks_finalized: u128,
    /// Seller credit added by the close: the unburned bond and nothing else.
    owed_delta: u128,
    /// Bond still held — the close consumes it entirely.
    bond_left: u128,
}

async fn snapshot(dex: &Dex, tc: &str) -> PreDispute {
    let state = dex.token_contract_get_state(tc).await.expect("getState before dispute");
    let fees = dex.token_contract_get_fees(tc).await.expect("getFees before dispute");
    let bond = dex.token_contract_get_seller_bond(tc).await.expect("getSellerBond before dispute");
    PreDispute {
        ticks_finalized: fees.ticks_finalized,
        owed: state.finalized_owed,
        disputable: state.prepaid + state.frozen,
        bond: bond.bond_held,
    }
}

async fn settle_outcome(dex: &Dex, tc: &str, before: &PreDispute) -> SettleOutcome {
    let state = dex.token_contract_get_state(tc).await.expect("getState after timeout");
    let fees = dex.token_contract_get_fees(tc).await.expect("getFees after timeout");
    let bond = dex.token_contract_get_seller_bond(tc).await.expect("getSellerBond after timeout");
    SettleOutcome {
        ticks_finalized: fees.ticks_finalized,
        owed_delta: state.finalized_owed.saturating_sub(before.owed),
        bond_left: bond.bond_held,
    }
}

/// Assert the mark-for-mark burn on one deal.
fn check_burn(case: &str, before: &PreDispute, after: &SettleOutcome) -> Vec<String> {
    let mut out = Vec::new();
    // A disputed tick is never finalized: the seller earns nothing from it,
    // whatever the acceptance window said.
    if after.ticks_finalized != before.ticks_finalized {
        out.push(format!(
            "Case {case}: a disputed tick was finalized to the seller (ticksFinalized {} -> {}); \
             the timeout must burn it, not pay it",
            before.ticks_finalized, after.ticks_finalized
        ));
    }
    // Only the bond the burn did not consume comes back. Anything more means the
    // seller was paid for disputed value; anything less means the return leaked.
    let want_back = before.bond.saturating_sub(before.disputable);
    if after.owed_delta != want_back {
        out.push(format!(
            "Case {case}: timeout credited the seller {} but the unburned bond is \
             bond({}) - D({}) = {want_back}",
            after.owed_delta, before.bond, before.disputable
        ));
    }
    // The close settles the bond in full — burned, returned, or both.
    if after.bond_left != 0 {
        out.push(format!(
            "Case {case}: bond still held after the close: {} (want 0)",
            after.bond_left
        ));
    }
    // The burn is only symmetric if there was bond to burn against D.
    if before.bond < before.disputable {
        out.push(format!(
            "Case {case}: bond({}) < D({}) — the mirror burn cannot be symmetric, so this run \
             does not exercise the property",
            before.bond, before.disputable
        ));
    }
    out
}

/// Deploy a TokenContract for `nonce`, fund it via a sell↔buy handover, post the
/// seller bond, and `open()` it (freezing the probe tick). Returns the TC.
async fn setup_deal(
    dex: &Dex,
    note: &common::test_pns::TestPn,
    keys: &KeyPair,
    model_name: &str,
    model_hash: &str,
    ob: &str,
    nonce: u64,
) -> Result<String, String> {
    let signer = || Signer::Keys { keys: keys.clone() };
    let tc = deploy_token_contract(
        dex.context(),
        &note.owner_public_key_hex,
        &note.address,
        nonce,
        TokenDeal {
            model_name: model_name.to_string(),
            price_per_tick: PRICE_PER_TICK,
            max_ticks: DEAL_TICKS,
        },
        keys.clone(),
    )
    .await
    .map_err(|e| format!("deploy TokenContract: {e:?}"))?;

    dex.post_sell_offer(&note.address, ParamsOfPostSellOffer { flags: 0, nonce }, signer())
        .await
        .map_err(|e| format!("postSellOffer: {e:?}"))?;
    wait_sell_offer_rested(dex, ob, &tc, POLL_TICKS, POLL_TICK).await?;

    dex.place_inference_buy(
        &note.address,
        ParamsOfPlaceInferenceBuy {
            model_hash: model_hash.to_string(),
            max_price_per_tick: PRICE_PER_TICK,
            ticks: DEAL_TICKS,
            escrow: BUY_ESCROW,
            flags: 1,
            deadline: 0,
        },
        signer(),
    )
    .await
    .map_err(|e| format!("placeInferenceBuy: {e:?}"))?;
    if !wait_state(dex, &tc, |s| s.funded).await {
        return Err("TokenContract never funded by the match".to_string());
    }

    // The TC takes the bond from its `_sellerNote` and refuses every other sender.
    dex.post_seller_bond(
        &note.address,
        ParamsOfPostSellerBond { nonce, amount: SELLER_BOND },
        signer(),
    )
    .await
    .map_err(|e| format!("postSellerBond: {e:?}"))?;
    let mut bond_funded = false;
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(b) = dex.token_contract_get_seller_bond(&tc).await
            && b.bond_funded
        {
            bond_funded = true;
            break;
        }
    }
    if !bond_funded {
        return Err("seller bond never registered".to_string());
    }

    dex.token_contract_open(&tc, ParamsOfOpen { endpoint_cipher: "00".to_string() }, signer())
        .await
        .map_err(|e| format!("open: {e:?}"))?;
    if !wait_state(dex, &tc, |s| s.opened).await {
        return Err("stream never opened".to_string());
    }
    Ok(tc)
}

/// Sleep until `secs_from` seconds have elapsed since `base` (no-op if already past).
async fn sleep_until(base: Instant, secs_from: u64) {
    let target = Duration::from_secs(secs_from);
    let elapsed = base.elapsed();
    if let Some(remaining) = target.checked_sub(elapsed) {
        eprintln!("[e2e_dispute] sleeping {}s (to T+{secs_from}s)…", remaining.as_secs());
        tokio::time::sleep(remaining).await;
    }
}

async fn wait_book_live(dex: &Dex, ob: &str) {
    wait_inference_book_live(dex, ob, POLL_TICKS, POLL_TICK)
        .await
        .unwrap_or_else(|err| panic!("{err}"));
}

async fn wait_state<F>(dex: &Dex, tc: &str, pred: F) -> bool
where
    F: Fn(&dodex_contracts::airegistry::token_contract::ResultOfGetState) -> bool,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(state) = dex.token_contract_get_state(tc).await
            && pred(&state)
        {
            return true;
        }
    }
    false
}

async fn finish(dex: &Dex, note: &str, model_hash: &str, keys: &KeyPair, failures: Vec<String>) {
    use dodex_contracts::dex::private_note::ParamsOfCancelAllInferenceOrders;
    let _ = dex
        .cancel_all_inference_orders(
            note,
            ParamsOfCancelAllInferenceOrders { model_hash: model_hash.to_string() },
            Signer::Keys { keys: keys.clone() },
        )
        .await;
    assert!(failures.is_empty(), "e2e_dispute failures: {failures:#?}");
}
