//! An outcome token traded above par — 1.5 collateral for something that can
//! never redeem for more than 1.
//!
//! Prices are basis points against `FULL_PERCENT = 10000`, and the contracts
//! put **no upper bound** on them: `OrderBook` checks the tick multiple and
//! the minimum notional, and nothing else. Every price this suite has used so
//! far was below par, where a buy's collateral cost (`amount * price /
//! 10000`) is smaller than the token count it buys. Above par that inequality
//! flips, and any place in the code that had quietly assumed otherwise —
//! sizing a lock from the token count, or capping a price at one — would let
//! a buyer take tokens without paying for them.
//!
//! Nothing about this is exotic from the book's side. It is a price like any
//! other, which is exactly why it is worth a test: the arithmetic that breaks
//! here is the arithmetic nothing else exercises.
//!
//! ## What it asserts
//!
//! One ask at 1.5, one buy that crosses it:
//!
//! - the buyer receives exactly the tokens it bought, and the ask leaves the
//!   book — the trade really happened;
//! - the buyer's collateral falls by **at least the notional it offered**
//!   (`amount * 15000 / 10000`, which above par exceeds the token count).
//!   This is the claim: a cap at par anywhere would make the buyer pay no
//!   more than the token count, and the assertion would fail. It is a bound
//!   rather than an equality because the exact figure includes the
//!   contract's fee, and restating fee arithmetic here would check the
//!   implementation against itself;
//! - nothing is left in the buyer's `_lockedInOrders`, so the unusual price
//!   did not strand escrow;
//! - the seller's free collateral grows by **more than the token count** —
//!   the other side of the same claim, read on the account that received it.
//!
//! ## The same price met by a market buy
//!
//! A limit buy states its size in tokens and locks what they cost. A market
//! buy states the collateral instead and locks exactly that, so above par the
//! book cannot hand over `amount` tokens for it: at 1.5 a quote of 10 buys
//! 6.67, and a book that read the quote as a token count would credit the
//! seller 15 against a lock of 10 — five collateral nobody ever deposited.
//! What stops it is a cap, recomputed per fill, on what the remaining quote
//! can afford at that fill's price.
//!
//! Two asks at the same price make both halves of that readable at once:
//!
//! - the buy takes **only what its quote affords** out of the first ask,
//!   which stays on the book with the rest — an uncapped book would have
//!   emptied it;
//! - the seller is credited that fill's cost and not the token count, read
//!   as a bound so the fee stays out of the arithmetic;
//! - and the quote left over — one unit, too little for a single token at
//!   1.5 — **stops the walk** rather than being carried into the second ask.
//!   The second ask is untouched afterwards, which is the whole observation:
//!   a book that kept walking would have matched it for nothing.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::at;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::place_order_with_flags;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::market::FLAG_MARKET;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_ABOVE_PAR: u64 = 180;
const OUTCOME: u32 = 0;

/// 150% — half again what an outcome token can ever redeem for. A multiple of
/// the 10 bps tick, like any other price.
const ABOVE_PAR_BPS: u128 = 15_000;

/// Basis-point denominator, mirroring the contracts' `FULL_PERCENT`.
const FULL_PERCENT: u128 = 10_000;

/// The size traded, a multiple of the 0.01 NACKL lot.
const TRADE_AMOUNT: u128 = 20_000_000_000;

/// What the buyer offered for it: above par this exceeds `TRADE_AMOUNT`,
/// which is the whole point of the scenario.
const NOTIONAL: u128 = TRADE_AMOUNT * ABOVE_PAR_BPS / FULL_PERCENT;

/// Collateral the seller splits to get something to sell. Enough for the
/// limit trade above and both asks of the market-buy phase, with the quantised
/// split's own yield asserted before they are placed.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

// The premise, enforced by the compiler: below par this scenario would assert
// nothing that the ordinary trading scenarios do not already cover.
const _: () = assert!(ABOVE_PAR_BPS > FULL_PERCENT);
const _: () = assert!(NOTIONAL > TRADE_AMOUNT);

/// The floor a note puts under any market buy's quote — 10 NACKL, mirroring
/// `MIN_ORDER_NOTIONAL_NACKL`.
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// The market buy's quote: the minimum exactly, which at this price also
/// leaves it a unit short of one more token once the affordable part is spent.
const MARKET_QUOTE: u128 = MIN_ORDER_NOTIONAL;

/// What that quote can afford at `ABOVE_PAR_BPS`, floored the way the book
/// floors it. Above par it is *less* than the quote, which is the entire
/// reason the cap exists.
const AFFORDABLE_BASE: u128 = MARKET_QUOTE * FULL_PERCENT / ABOVE_PAR_BPS;

/// What that affordable fill costs.
const CAPPED_SPEND: u128 = AFFORDABLE_BASE * ABOVE_PAR_BPS / FULL_PERCENT;

/// The quote left over — too little to buy a single token at this price,
/// which is what makes it dust rather than a remainder to keep spending.
const DUST_QUOTE: u128 = MARKET_QUOTE - CAPPED_SPEND;

/// Each of the two asks that buy meets, a multiple of the 0.01 NACKL lot.
const MARKET_ASK_AMOUNT: u128 = 10_000_000_000;

/// What the first of them keeps once the cap has bound.
const FIRST_ASK_REMAINING: u128 = MARKET_ASK_AMOUNT - AFFORDABLE_BASE;

/// An upper bound on the taker fee, not the contract's rate — restating that
/// here would check the implementation against itself. A tenth of a percent is
/// more than double the real rate and still far below the gap between a capped
/// fill and an uncapped one.
const FEE_CAP_PERMILLE: u128 = 1;

// The cap only binds against an ask larger than the quote can afford; against
// a smaller one the ask's own size decides the fill and the reading says
// nothing about a cap.
const _: () = assert!(MARKET_ASK_AMOUNT > AFFORDABLE_BASE);
// And this is the gap it closes: read as a token count, the same quote would
// take the whole ask and owe half again what it locked.
const _: () = assert!(MARKET_ASK_AMOUNT * ABOVE_PAR_BPS / FULL_PERCENT > MARKET_QUOTE);
// The leftover has to be genuinely unspendable at this price, or the walk
// continues into the second ask and there is no stop to observe.
const _: () = assert!(DUST_QUOTE > 0);
const _: () = assert!(DUST_QUOTE * FULL_PERCENT / ABOVE_PAR_BPS == 0);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_trade_above_par_costs_more_than_the_tokens_it_buys_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "price_above_par").expect("rent the deployer note");
    let seller = alloc.rent(PnProfile::Trd, "price_above_par").expect("rent the seller note");
    let buyer = alloc.rent(PnProfile::Trd, "price_above_par").expect("rent the buyer note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_ABOVE_PAR).await;

    split_full_set(dex, &seller, &market.key, SPLIT_COLLATERAL).await;

    let price = ABOVE_PAR_BPS.to_string();
    let ask_coid = nonce as u128 * 10;
    let buy_coid = ask_coid + 1;

    let buyer_free_before = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_before = pn_locked(&r, &buyer.note.address).await;
    let buyer_tokens_before = outcome_tokens(dex, &buyer).await;
    let seller_free_before = pn_balance(&r, &seller.note.address).await;

    place_limit(dex, &seller, &market.key, OUTCOME, false, &price, TRADE_AMOUNT, ask_coid).await;
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, true).await;

    place_limit(dex, &buyer, &market.key, OUTCOME, true, &price, TRADE_AMOUNT, buy_coid).await;
    // The ask leaving the book is the fill; without it every reading below
    // would be that of a trade that never happened.
    wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, ask_coid, false).await;
    wait_not_busy(dex, &buyer.note.address, "buy above par").await;
    wait_not_busy(dex, &seller.note.address, "ask filled").await;

    assert_eq!(
        at(&outcome_tokens(dex, &buyer).await, OUTCOME),
        at(&buyer_tokens_before, OUTCOME) + TRADE_AMOUNT,
        "the buyer did not receive the {TRADE_AMOUNT} tokens it bought"
    );

    // The claim. Above par the notional exceeds the token count, so a cap at
    // par anywhere in the pricing would show up as the buyer paying no more
    // than `TRADE_AMOUNT`.
    let buyer_free_after = pn_balance(&r, &buyer.note.address).await;
    let buyer_locked_after = pn_locked(&r, &buyer.note.address).await;
    let paid = (buyer_free_before + buyer_locked_before)
        .checked_sub(buyer_free_after + buyer_locked_after)
        .unwrap_or_else(|| {
            panic!(
                "the buyer's collateral grew across a purchase: {} -> {}",
                buyer_free_before + buyer_locked_before,
                buyer_free_after + buyer_locked_after
            )
        });
    assert!(
        paid >= NOTIONAL,
        "the buyer paid {paid} for {TRADE_AMOUNT} tokens offered at {ABOVE_PAR_BPS} bps, less \
         than the {NOTIONAL} it offered — a price above par was not charged in full"
    );

    assert_eq!(
        buyer_locked_after, buyer_locked_before,
        "a fully filled buy above par left collateral locked"
    );

    // The same claim from the receiving side.
    let seller_free_after = pn_balance(&r, &seller.note.address).await;
    assert!(
        seller_free_after > seller_free_before + TRADE_AMOUNT,
        "the seller received {} for {TRADE_AMOUNT} tokens sold above par, no more than the \
         token count itself",
        seller_free_after.saturating_sub(seller_free_before)
    );

    // ── the same price, met by a market buy ───────────────────────────────
    //
    // Above par a quote does not convert one-for-one into tokens, so every
    // fill is capped by what the quote left can afford at that fill's price.
    // Two asks read both consequences: the cap on the first, and what happens
    // to the unspendable remainder when the second is still there to walk to.
    let seller_tokens = at(&outcome_tokens(dex, &seller).await, OUTCOME);
    assert!(
        seller_tokens >= 2 * MARKET_ASK_AMOUNT,
        "the seller holds {seller_tokens} outcome-{OUTCOME} tokens, not the {} the two asks \
         need — the quantised split yielded less than this phase assumes",
        2 * MARKET_ASK_AMOUNT
    );

    let first_ask_coid = ask_coid + 2;
    let second_ask_coid = ask_coid + 3;
    let market_coid = ask_coid + 4;

    // Placed one at a time. They share a price level, and which of the two the
    // buy reaches first is the whole arrangement — two placements in flight at
    // once would leave their order to chance.
    for coid in [first_ask_coid, second_ask_coid] {
        place_limit(dex, &seller, &market.key, OUTCOME, false, &price, MARKET_ASK_AMOUNT, coid)
            .await;
        wait_owner_order(dex, &market.order_book, &seller.note.dih_dec, coid, true).await;
    }

    let mkt_buyer_held_before = pn_balance(&r, &buyer.note.address).await
        + pn_locked(&r, &buyer.note.address).await;
    let mkt_buyer_tokens_before = at(&outcome_tokens(dex, &buyer).await, OUTCOME);
    let mkt_buyer_locked_before = pn_locked(&r, &buyer.note.address).await;
    let mkt_seller_free_before = pn_balance(&r, &seller.note.address).await;
    let book_fees_before = book_fees(dex, &market.order_book).await;

    place_order_with_flags(
        dex,
        &buyer,
        &market.key,
        OUTCOME,
        true,
        "0",
        MARKET_QUOTE,
        FLAG_MARKET,
        market_coid,
    )
    .await;

    // The first ask is only partly consumed, so it never leaves the owner
    // index and cannot be waited on by absence. Waiting on it *changing* is
    // the weakest signal that says the buy landed; what it changed to is
    // asserted below rather than polled for.
    poll_until("the market buy never reached the first ask", || async {
        ask_remaining(dex, &market.order_book, &seller.note.dih_dec, first_ask_coid).await
            != Some(MARKET_ASK_AMOUNT)
    })
    .await;
    wait_not_busy(dex, &buyer.note.address, "market buy above par").await;
    wait_not_busy(dex, &seller.note.address, "capped fill").await;

    // The cap: the buy took what its quote afforded, not what the ask offered.
    assert_eq!(
        at(&outcome_tokens(dex, &buyer).await, OUTCOME),
        mkt_buyer_tokens_before + AFFORDABLE_BASE,
        "a quote of {MARKET_QUOTE} at {ABOVE_PAR_BPS} bps bought something other than the \
         {AFFORDABLE_BASE} tokens it can afford"
    );
    assert_eq!(
        ask_remaining(dex, &market.order_book, &seller.note.dih_dec, first_ask_coid).await,
        Some(FIRST_ASK_REMAINING),
        "the first ask does not hold the {FIRST_ASK_REMAINING} the capped fill should have left \
         it; gone entirely means the quote was spent as if it were a token count"
    );

    // The stop: the leftover quote cannot buy a single token, so the walk ends
    // rather than reaching the ask behind it.
    assert_eq!(
        ask_remaining(dex, &market.order_book, &seller.note.dih_dec, second_ask_coid).await,
        Some(MARKET_ASK_AMOUNT),
        "the second ask was touched by a buy with {DUST_QUOTE} of quote left — too little to \
         fill one token at {ABOVE_PAR_BPS} bps"
    );

    // Nothing of the market order stayed behind, dust included.
    let after_market = dex
        .get_orders_by_owner(&market.order_book, buyer.note.dih_dec.clone())
        .await
        .expect("get_orders_by_owner");
    assert!(
        after_market.orders.is_empty(),
        "the market buy left {} order(s) resting",
        after_market.orders.len()
    );
    assert_eq!(
        pn_locked(&r, &buyer.note.address).await,
        mkt_buyer_locked_before,
        "the market buy kept collateral locked; the unspent quote is returned, not held"
    );

    // What the seller was credited is the cost of the capped fill — bounded
    // rather than equated, so the fee stays out of it. An uncapped book would
    // have credited the whole ask's notional, half again the buyer's quote.
    let seller_gained = pn_balance(&r, &seller.note.address).await - mkt_seller_free_before;
    assert!(
        seller_gained >= CAPPED_SPEND,
        "the seller received {seller_gained} for a fill worth {CAPPED_SPEND}"
    );
    assert!(
        seller_gained <= CAPPED_SPEND + CAPPED_SPEND * FEE_CAP_PERMILLE / 1000,
        "the seller received {seller_gained} for a fill worth {CAPPED_SPEND} — toward the {} the \
         whole ask would have been worth, so the quote was credited as if it were a token count",
        MARKET_ASK_AMOUNT * ABOVE_PAR_BPS / FULL_PERCENT
    );

    // And it all came from the buyer: conservation on the collateral leg, which
    // is where collateral conjured out of a mis-scaled quote would show up.
    let buyer_paid = mkt_buyer_held_before
        - (pn_balance(&r, &buyer.note.address).await + pn_locked(&r, &buyer.note.address).await);
    let protocol_gained = book_fees(dex, &market.order_book).await - book_fees_before;
    assert_eq!(
        buyer_paid,
        seller_gained + protocol_gained,
        "the buyer paid {buyer_paid}, the seller received {seller_gained} and the book kept \
         {protocol_gained}: {} came from nowhere",
        (seller_gained + protocol_gained) as i128 - buyer_paid as i128
    );

    seller.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    buyer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// The protocol's share of the fees this book has taken so far. Read off the
/// book rather than off RootPN: the hand-over only happens at shutdown, so
/// until then RootPN knows nothing about a trade already paid for.
async fn book_fees(dex: &dodex_sdk::Dex, ob_addr: &str) -> u128 {
    dex.get_order_book_details(ob_addr).await.expect("order book details").total_protocol_fees
}

/// What is left of a resting order, or `None` once it has left the book.
async fn ask_remaining(
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

/// The note's free NACKL — `_balance`, without what its resting orders hold.
async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

/// The note's NACKL held against its resting orders — `_lockedInOrders`.
async fn pn_locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}
