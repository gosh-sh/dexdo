//! A market denominated in a six-decimal token, and the fee that rounds away
//! inside it.
//!
//! Every market the suite builds is priced in NACKL, which has nine decimals
//! and lot and minimum sizes to match. USDC has six, and every size the
//! contracts quantise to is a thousand times smaller: a lot is 10 000 units
//! rather than 10 000 000, and an order has to be worth 1 USDC rather than
//! 10 NACKL. Nothing about the arithmetic is currency-aware — the same
//! expressions run whatever the token — which is exactly why running them
//! only ever at one scale proves nothing about the other.
//!
//! The currency is part of a market's identity, hashed into its address
//! beside the event and the oracle list, so this is not a variation on an
//! existing market but a different one; the fixture had to learn to build it.
//!
//! ## What it does
//!
//! A whole lifecycle in USDC — the creator seeds both outcomes, a maker
//! splits collateral into outcome tokens, an order crosses, the market
//! resolves and the winner claims — with the trade asserted in exact units,
//! because at this scale a factor-of-a-thousand slip is the failure that
//! matters and it is not subtle in the numbers.
//!
//! ## And the fee that disappears
//!
//! The taker fee is `notional × 45 / 100000`, floored. In NACKL nothing can
//! make that zero: the smallest order worth placing is 10 NACKL, and even a
//! single-lot fill against it is worth far more than the 2 223 units the fee
//! needs to stay under. In USDC it is reachable — a market sell is not
//! quantised to the lot, so a small enough one produces a fill whose whole
//! notional is under that threshold, and the fee floors to nothing.
//!
//! That is a real branch: the taker keeps its proceeds whole, the maker is
//! rebated three quarters of nothing, and the book's protocol counter does
//! not move. A fee that rounded *up* instead, or a proceeds credit gated on
//! `proceeds > fee` that mishandled equality, would both show here and
//! nowhere else in the suite.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use ackinacki_kit::tvm_client::abi::Signer;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::at;
use crate::common::market::freeze_prepared_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::place_order_with_flags;
use crate::common::market::prepare_ephemeral_market_in;
use crate::common::market::resolve_and_drain;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::market::FLAG_MARKET;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

const TOKEN_TYPE_USDC: u32 = dodex_sdk::proof::TokenType::Usdc as u32;

const STAKE_PERIOD_USDC: u64 = 300;
const OUTCOME: u32 = 0;
const FULL_PERCENT: u128 = 10_000;

/// USDC's own quantisation, a thousandth of NACKL's at every step:
/// `LOT_SIZE_USDC`, `MIN_ORDER_NOTIONAL_USDC`, `MIN_VALUE_USDC`.
const LOT: u128 = 10_000;
const MIN_ORDER_NOTIONAL: u128 = 1_000_000;
const MIN_STAKE: u128 = 10_000;

/// What the creator seeds each outcome with — 100 USDC, against a note baked
/// with 1 000. In NACKL this figure is a hundred thousand times larger, which
/// is the whole reason the fixture takes it as a parameter.
const SEED_PER_OUTCOME: u128 = 100_000_000;

/// The maker's inventory, and the ask it writes against it.
const SPLIT_COLLATERAL: u128 = 200_000_000;
const ASK_BPS: u128 = 6_000;
const ASK_AMOUNT: u128 = 20_000_000;

/// What the taker lifts, leaving the ask partly filled.
const TAKE_AMOUNT: u128 = 5_000_000;

const _: () = assert!(ASK_AMOUNT.is_multiple_of(LOT) && TAKE_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(ASK_AMOUNT * ASK_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
const _: () = assert!(TAKE_AMOUNT * ASK_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
const _: () = assert!(TAKE_AMOUNT < ASK_AMOUNT);

/// What that fill costs, exactly.
const TAKE_COST: u128 = TAKE_AMOUNT * ASK_BPS / FULL_PERCENT;

/// The bid the fee-floor phase rests, and the sliver sold into it.
///
/// `TAKER_FEE_RATE / FEE_DENOMINATOR` is `45 / 100000`, so a fill is charged
/// nothing at all once its notional is under 2 223 units. A market sell is
/// not quantised to the lot, which is what makes a fill that small reachable
/// — and only in a token whose units are this fine.
const DUST_BID_BPS: u128 = 6_000;
const DUST_BID_AMOUNT: u128 = 20_000_000;
const DUST_SELL: u128 = 3_000;
const DUST_NOTIONAL: u128 = DUST_SELL * DUST_BID_BPS / FULL_PERCENT;
const TAKER_FEE_RATE: u128 = 45;
const FEE_DENOMINATOR: u128 = 100_000;

const _: () = assert!(DUST_BID_AMOUNT * DUST_BID_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);
// The whole point of the phase: this fill is charged nothing.
const _: () = assert!(DUST_NOTIONAL * TAKER_FEE_RATE / FEE_DENOMINATOR == 0);
// And it is not zero for the trivial reason of being nothing at all.
const _: () = assert!(DUST_NOTIONAL > 0);
// A sell this small cannot be a limit order — it would fail the lot and the
// minimum notional both. Only the market path can carry it.
const _: () = assert!(!DUST_SELL.is_multiple_of(LOT));
const _: () = assert!(DUST_NOTIONAL < MIN_ORDER_NOTIONAL);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_market_in_a_six_decimal_token_trades_and_rounds_its_fee_away_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Usdc, "usdc_market").expect("rent the USDC creator");
    let maker = alloc.rent(PnProfile::Usdc, "usdc_market").expect("rent the USDC maker");
    let taker = alloc.rent(PnProfile::Usdc, "usdc_market").expect("rent the USDC taker");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let prepared = prepare_ephemeral_market_in(
        ctx,
        dex,
        &b0,
        &creator,
        nonce,
        STAKE_PERIOD_USDC,
        TOKEN_TYPE_USDC,
        SEED_PER_OUTCOME,
    )
    .await;

    assert_eq!(
        prepared.key.token_type, TOKEN_TYPE_USDC,
        "the market came up denominated in something other than USDC"
    );

    // A stake at USDC's own minimum, which a NACKL market would refuse as a
    // thousandth of its own.
    stake_at_the_minimum(dex, &maker, &prepared).await;

    let market = freeze_prepared_market(ctx, dex, prepared).await;
    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;
    let inventory = outcome_tokens(dex, &maker).await;
    assert!(
        at(&inventory, OUTCOME) >= ASK_AMOUNT,
        "the split left the maker {} outcome-{OUTCOME} tokens, short of the {ASK_AMOUNT} its ask \
         needs — the bid it rests later is paid for in collateral, not tokens",
        at(&inventory, OUTCOME)
    );

    // ── a trade, in units a thousand times finer ──────────────────────────
    let base = nonce as u128 * 10;
    let (ask_cid, buy_cid) = (base + 1, base + 2);

    place_limit(dex, &maker, &market.key, OUTCOME, false, &ASK_BPS.to_string(), ASK_AMOUNT, ask_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, ask_cid, true).await;

    let taker_tokens_before = at(&outcome_tokens(dex, &taker).await, OUTCOME);
    let taker_held_before = held(&r, &taker.note.address).await;
    let maker_free_before = free(&r, &maker.note.address).await;

    place_limit(dex, &taker, &market.key, OUTCOME, true, &ASK_BPS.to_string(), TAKE_AMOUNT, buy_cid)
        .await;
    poll_until("the taker never reached the ask", || async {
        order_size(dex, &market.order_book, &maker.note.dih_dec, ask_cid).await != Some(ASK_AMOUNT)
    })
    .await;
    wait_not_busy(dex, &taker.note.address, "a USDC buy").await;
    wait_not_busy(dex, &maker.note.address, "a USDC fill").await;

    assert_eq!(
        at(&outcome_tokens(dex, &taker).await, OUTCOME),
        taker_tokens_before + TAKE_AMOUNT,
        "the taker did not receive exactly the {TAKE_AMOUNT} outcome tokens it bought"
    );
    assert_eq!(
        order_size(dex, &market.order_book, &maker.note.dih_dec, ask_cid).await,
        Some(ASK_AMOUNT - TAKE_AMOUNT),
        "the ask does not hold what the fill left it"
    );

    // Priced in a token with a thousandth of NACKL's granularity, so a fill
    // computed at the wrong scale misses by three orders of magnitude rather
    // than by a rounding unit. The upper bound carries the fee.
    let taker_paid = taker_held_before - held(&r, &taker.note.address).await;
    assert!(
        taker_paid >= TAKE_COST,
        "the taker paid {taker_paid} for tokens worth {TAKE_COST} in USDC units"
    );
    assert!(
        taker_paid <= TAKE_COST + TAKE_COST / 1000,
        "the taker paid {taker_paid} for a fill worth {TAKE_COST} — beyond any plausible fee"
    );
    assert!(
        free(&r, &maker.note.address).await > maker_free_before,
        "the maker was not paid for the tokens it sold"
    );

    // ── and a fill too small to charge for ────────────────────────────────
    //
    // The bid rests at a price a market sell can hit; the sell is a sliver,
    // small enough that its whole notional is under what the fee rate needs
    // to produce a single unit. What the seller receives has to be the
    // notional untouched, and the book has to keep nothing.
    let dust_bid_cid = base + 3;
    place_limit(
        dex,
        &maker,
        &market.key,
        OUTCOME,
        true,
        &DUST_BID_BPS.to_string(),
        DUST_BID_AMOUNT,
        dust_bid_cid,
    )
    .await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, dust_bid_cid, true).await;

    let fees_before = book_fees(dex, &market.order_book).await;
    let seller_free_before = free(&r, &taker.note.address).await;

    place_order_with_flags(
        dex,
        &taker,
        &market.key,
        OUTCOME,
        false,
        "0",
        DUST_SELL,
        FLAG_MARKET,
        base + 4,
    )
    .await;
    poll_until("the sliver never reached the bid", || async {
        order_size(dex, &market.order_book, &maker.note.dih_dec, dust_bid_cid).await
            != Some(DUST_BID_AMOUNT)
    })
    .await;
    wait_not_busy(dex, &taker.note.address, "a sale too small to be charged for").await;

    assert_eq!(
        free(&r, &taker.note.address).await,
        seller_free_before + DUST_NOTIONAL,
        "the seller received something other than the whole {DUST_NOTIONAL} its sliver was worth \
         — a fee that floors to nothing must take nothing"
    );
    assert_eq!(
        book_fees(dex, &market.order_book).await,
        fees_before,
        "the book kept a protocol share of a fee that rounded to zero"
    );

    // ── and the market settles ────────────────────────────────────────────
    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, OUTCOME).await;
    wait_not_busy(dex, &maker.note.address, "the drain reaching the maker").await;

    let before_claim = free(&r, &taker.note.address).await;
    dex.claim(
        &taker.note.address,
        market.key.clone(),
        Signer::Keys { keys: taker.note.keys.clone() },
    )
    .await
    .expect("claim");
    wait_not_busy(dex, &taker.note.address, "claiming a USDC market").await;
    assert!(
        free(&r, &taker.note.address).await > before_claim,
        "the taker held winning USDC outcome tokens and was paid nothing for them"
    );

    for note in [creator, maker, taker] {
        note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    }
}

/// Stake exactly `MIN_VALUE_USDC` — a figure a NACKL market would reject out
/// of hand as a thousandth of its own minimum, and the cheapest reading that
/// the per-token gates are being applied rather than a single global one.
async fn stake_at_the_minimum(
    dex: &dodex_sdk::Dex,
    note: &allocator::LeasedPn,
    prepared: &crate::common::market::PreparedMarket,
) {
    crate::common::market::stake_amount(dex, note, &prepared.key, OUTCOME, MIN_STAKE, false).await;
    let stakes = dex.get_stakes(&note.note.address).await.expect("stakes").stakes;
    assert!(
        !stakes.is_empty(),
        "a stake of {MIN_STAKE} — USDC's own minimum — was refused; the market is applying \
         another token's gate"
    );
}

async fn order_size(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
) -> Option<u128> {
    dex.get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner")
        .orders
        .into_iter()
        .find(|o| o.client_order_id == client_order_id)
        .map(|o| o.amount)
}

async fn book_fees(dex: &dodex_sdk::Dex, ob_addr: &str) -> u128 {
    dex.get_order_book_details(ob_addr).await.expect("order book details").total_protocol_fees
}

async fn free(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_USDC)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn held(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    free(r, pn_address).await
        + invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_USDC)
            .await
            .expect("read note escrow")
            .unwrap_or(0)
}
