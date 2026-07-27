// End-to-end dispute-timeout settlement smoke for the AI Registry inference market
// against a real shellnet, driven through `dodex_chain::Dex`. Ported from the
// contract-side `test_dispute_timeout.py` (§5 / directive 92).
//
// `resolveDisputeTimeout()` must apply the SAME window-gated close as `stop()`:
// a disputed streaming tick is finalized to the seller ONLY if its acceptance
// window has elapsed by the timeout. `price_per_tick` is chosen so the per-tick
// acceptance window (`settle_window = clamp(P*600/1e9, 180, 3600) = 1200s`) is
// LONGER than the dispute window (600s) — the only regime where the bug bit.
//
// Two deals run in parallel (self-trade: one note is buyer AND seller on both):
//   * Case A — timeout fires at DISPUTE_WINDOW, while the acceptance window is
//     STILL OPEN ⇒ the streaming tick is NOT accepted ⇒ seller paid ~0.
//   * Case B — timeout fires after the acceptance window has ELAPSED ⇒ the tick
//     IS accepted ⇒ seller keeps it.
//
// VERY slow: both windows scale with the 2-SHELL tick price, so it sleeps out a
// ~1200s probe window AND a ~1200s acceptance window (~45 min).
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
use common::airegistry::fund_seller_bond_via_giver;
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
// Probe acceptance is gated by the same price-scaled window, so this waits out
// W = 1200s, not the 180s floor.
const PROBE_WAIT: Duration = Duration::from_secs(1215);
const DISPUTE_WINDOW_S: u64 = 600;
const SETTLE_WINDOW_S: u64 = 1200;

fn unique_suffix() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires shellnet + seed_notes.json; sleeps out the ~1200s probe + ~1200s acceptance window (~45 min)"]
async fn inference_dispute_timeout_window_gated_settlement() {
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
    let owed_a0 = dex.token_contract_get_state(&tc_a).await.expect("A state").finalized_owed;
    let owed_b0 = dex.token_contract_get_state(&tc_b).await.expect("B state").finalized_owed;

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
    let owed_a1 =
        dex.token_contract_get_state(&tc_a).await.expect("A state after timeout").finalized_owed;
    let delta_a = owed_a1.saturating_sub(owed_a0);
    eprintln!("[e2e_dispute] A (window OPEN): owed {owed_a0} -> {owed_a1} delta={delta_a} tick={PRICE_PER_TICK}");
    if delta_a >= PRICE_PER_TICK / 2 {
        failures.push(format!(
            "Case A: window OPEN at timeout but seller paid the tick (delta={delta_a} >= tick/2)"
        ));
    }

    // Case B: resolve after the acceptance window has elapsed.
    sleep_until(advanced_at, SETTLE_WINDOW_S + 20).await;
    dex.token_contract_resolve_dispute_timeout(&tc_b, signer())
        .await
        .expect("resolveDisputeTimeout B");
    let _ = wait_state(&dex, &tc_b, |s| !s.opened).await;
    let owed_b1 =
        dex.token_contract_get_state(&tc_b).await.expect("B state after timeout").finalized_owed;
    let delta_b = owed_b1.saturating_sub(owed_b0);
    eprintln!("[e2e_dispute] B (window ELAPSED): owed {owed_b0} -> {owed_b1} delta={delta_b} tick={PRICE_PER_TICK}");
    if delta_b < PRICE_PER_TICK * 9 / 10 {
        failures.push(format!(
            "Case B: window ELAPSED but seller did not keep the tick (delta={delta_b} < 0.9*tick)"
        ));
    }

    finish(&dex, &note.address, &model_hash, &keys, failures).await;
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
    wait_until(dex, ob, |s| s.order_count >= 1).await;

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

    fund_seller_bond_via_giver(dex.context(), &tc, SELLER_BOND as u64)
        .await
        .map_err(|e| format!("fund seller bond: {e:?}"))?;
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
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if dex.inference_get_stats(ob).await.is_ok() {
            return;
        }
    }
    panic!("InferenceOrderBook did not become live within budget");
}

async fn wait_until<F>(dex: &Dex, ob: &str, pred: F)
where
    F: Fn(&dodex_contracts::airegistry::inference_order_book::ResultOfGetStats) -> bool,
{
    for _ in 0..POLL_TICKS {
        tokio::time::sleep(POLL_TICK).await;
        if let Ok(stats) = dex.inference_get_stats(ob).await
            && pred(&stats)
        {
            return;
        }
    }
    panic!("timed out waiting on order-book stats");
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
