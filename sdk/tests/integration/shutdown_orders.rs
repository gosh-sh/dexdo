//! What happens to orders that are still resting when the market closes.
//!
//! `proof_money` reaches `resultStart` with an empty book — its one trade
//! filled both sides — so the shutdown it exercises has nothing to clean up.
//! The interesting case is the opposite one: a market resolves while orders
//! are still on the book, and their escrow has to come back. If it does not,
//! nobody notices. The book is destroyed in the same message that reports the
//! drain complete, the orders are gone from every index, and the collateral
//! and outcome tokens behind them are simply missing from notes that have no
//! record of ever having lost them.
//!
//! ## What it asserts
//!
//! One maker rests an ask, one taker rests a bid below it — deliberately not
//! crossing, so both survive to `resultStart` — with a batch of further bids
//! behind them, and the market is then resolved.
//!
//! The batch is there for one reason: a drain retires a fixed number of order
//! ids per call and schedules itself again while any remain, so a book with
//! more than that is the only one whose drain has to continue at all. The
//! count is asserted before the resolve rather than assumed, since a book
//! under the limit would quietly turn this back into the single-pass case.
//!
//! What the drain then has to do:
//!
//! - the **bid's collateral** returns to the taker: `_lockedInOrders` back to
//!   its pre-order value and `_balance` back to its own. The contract refunds
//!   the authoritative lock verbatim (`_orderLocks[ob][orderId]`), so this is
//!   an equality, not a bound;
//! - the **ask's outcome tokens** return to the maker's stake record;
//! - both notes' `_openOrderCount` falls back to zero, because the note-side
//!   counter and the book's resting set are updated by different contracts
//!   and a drain that refunded without decrementing would strand the note;
//! - **RootPN is credited the book's own tally** of protocol fees. The book
//!   holds them until it dies, so the drain is the only way they ever get
//!   there, and the tally has to be read while the book still exists;
//! - **the owner can take them out again**, and **only** the owner, and only
//!   as much as accrued. `withdrawProtocolFees` pays to an address its caller
//!   names, so the two guards on it are all that stands between every market's
//!   fees and anyone who asks. Both are tried after the successful withdrawal
//!   and read as the absence of a payment on both sides — the total RootPN
//!   still owes, and the balance of the account the caller named — because
//!   neither refusal is legible as an error from a send that does not wait for
//!   a transaction. The permitted call is the control that says both readings
//!   do move when they are allowed to, and doubles as a check that the stand's
//!   RootPN really is owned by the key its zerostate says it is.
//!
//! One filled trade precedes the resting pair, purely so there are fees to
//! hand over: without it both fee assertions would be true of a book that
//! never earned anything.
//!
//! ## What it does not assert, and why
//!
//! `claim` before the drain finishes is supposed to fail with
//! `ERR_ORDERBOOK_NOT_SHUTDOWN`. It is not asserted here: the guard sits after
//! `tvm.accept()` but leaves no trace a caller can read — no `_busy`, no state
//! change — so "the claim was rejected" and "the claim never arrived" are the
//! same observation from outside. A check that only re-read unchanged balances
//! would pass whether or not the gate exists, which is worse than no check.
//!
//! Note that the gate is not academic. `onOrderCancelled`'s sell branch drops
//! returned outcome tokens **silently** when the stake record is already gone,
//! so a claim that landed between the drain's completion report and its cancel
//! callbacks would destroy exactly what this scenario watches come back. This
//! scenario never claims, so it observes the intended path; proving the gate
//! itself needs delivery-order control the stand does not have.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use std::sync::Arc;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use dodex_contracts::dex::order_book::OrderBookOrder;
use dodex_contracts::dex::private_note::ParamsOfPlaceBatch;
use dodex_contracts::dex::root_pn::ParamsOfWithdrawProtocolFees;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;

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
use crate::common::market::resolve_and_drain;
use crate::common::market::split_full_set;
use crate::common::market::wait_owner_order;
use crate::common::misc::now_unix;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;
use crate::common::misc::wait_until;

/// Distance to `resultStart`. A tenth of it is the staking window the fixture
/// waits out; the rest has to cover a split, a batch and three placements with
/// room to spare, and whatever is left over is spent idling until the market
/// closes.
const STAKE_PERIOD_SHUTDOWN: u64 = 240;

/// How much budget must remain once both orders are resting. Below this the
/// stand is too slow for the market this scenario sizes, and saying so beats
/// a revert later that looks like a contract defect.
const RESTING_MARGIN_SECS: u64 = 20;

const OUTCOME: u32 = 0;

/// Bid strictly below ask: neither fills, so both are still there to be
/// cleaned up when the market closes. That is the whole setup.
const BID_BPS: &str = "6000";
const ASK_BPS: &str = "8000";
const ORDER_AMOUNT: u128 = 30_000_000_000;

/// Collateral the maker splits to get something to offer. Enough for the
/// crossing pair below as well as the resting ask.
const SPLIT_COLLATERAL: u128 = 200_000_000_000;

/// One filled trade before the resting pair, purely so the book accrues
/// protocol fees for the drain to hand over. Priced where both sides cross.
const TRADE_BPS: &str = "7000";
const TRADE_AMOUNT: u128 = 30_000_000_000;

/// A batch of bids resting alongside the pair, sized so the book carries more
/// orders than one drain pass can walk. Mirrors `MAX_SHUTDOWN_BATCH`, which is
/// the number of order ids the book retires per call.
const MAX_SHUTDOWN_BATCH: u128 = 10;

/// The batch itself — `MAX_BATCH_SIZE`, the most a single placement may carry.
const EXTRA_BIDS: u128 = 10;

/// Each of them, at `BID_BPS`. Worth 12 NACKL, clear of the minimum notional.
const EXTRA_BID_AMOUNT: u128 = 20_000_000_000;

/// The largest amount `withdrawProtocolFees` could ever be asked for, which is
/// therefore always more than has accrued — no reading of the current total is
/// needed, and nothing another scenario does can make it valid.
const MORE_THAN_ACCRUED: u128 = u128::MAX;

/// How long the refused withdrawals are given to fail to happen. A payment
/// that was going to land has landed by then — the successful one above was
/// visible well inside a single poll budget.
const NEGATIVE_SETTLE_SECS: u64 = 15;

/// Where the owner sends the withdrawn fees. RootPN takes the destination
/// dApp as a parameter here (unlike the note-side withdraw, which stays in
/// its sender's), so a leased note is a legitimate target and — being leased
/// — the only kind whose balance nothing else can move.
const FEE_DEST_DAPP: &str = "4";

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_drain_refunds_resting_orders_and_hands_over_protocol_fees_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "shutdown_orders").expect("rent the deployer note");
    let maker = alloc.rent(PnProfile::Trd, "shutdown_orders").expect("rent the maker note");
    let taker = alloc.rent(PnProfile::Trd, "shutdown_orders").expect("rent the taker note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_SHUTDOWN).await;

    split_full_set(dex, &maker, &market.key, SPLIT_COLLATERAL).await;

    let ask_coid = nonce as u128 * 10;
    let bid_coid = ask_coid + 1;
    let (trade_ask, trade_buy) = (ask_coid + 2, ask_coid + 3);

    // One trade first. The drain hands the book's protocol fees to RootPN,
    // and with no fill there would be none to hand over — the fee assertions
    // below would then be true of a book that never earned anything.
    place_limit(dex, &maker, &market.key, OUTCOME, false, TRADE_BPS, TRADE_AMOUNT, trade_ask).await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, trade_ask, true).await;
    place_limit(dex, &taker, &market.key, OUTCOME, true, TRADE_BPS, TRADE_AMOUNT, trade_buy).await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, trade_ask, false).await;
    wait_not_busy(dex, &maker.note.address, "trade ask").await;
    wait_not_busy(dex, &taker.note.address, "trade buy").await;

    // Baselines, read after the trade and before either resting order exists.
    // Taken any earlier they would carry the trade's own movements; taken
    // later they would describe the escrowed state and the comparison after
    // the drain would be a tautology.
    let maker_tokens_before = outcome_tokens(dex, &maker).await;
    let taker_free_before = pn_balance(&r, &taker.note.address).await;
    let taker_locked_before = pn_locked(&r, &taker.note.address).await;

    place_limit(dex, &maker, &market.key, OUTCOME, false, ASK_BPS, ORDER_AMOUNT, ask_coid).await;
    wait_owner_order(dex, &market.order_book, &maker.note.dih_dec, ask_coid, true).await;
    place_limit(dex, &taker, &market.key, OUTCOME, true, BID_BPS, ORDER_AMOUNT, bid_coid).await;
    wait_owner_order(dex, &market.order_book, &taker.note.dih_dec, bid_coid, true).await;
    wait_not_busy(dex, &maker.note.address, "rest ask").await;
    wait_not_busy(dex, &taker.note.address, "rest bid").await;

    // A batch of further bids, so the drain has more to retire than it can
    // reach in one call. The book walks a fixed number of order ids per pass
    // and schedules itself again while any are left; a drain that reported
    // itself complete after the first pass would abandon everything behind it,
    // and the refund assertions below are what would notice.
    let batch_first_coid = ask_coid + 10;
    let batch: Vec<OrderBookOrder> = (0..EXTRA_BIDS)
        .map(|i| OrderBookOrder {
            outcome_id: OUTCOME,
            is_buy: true,
            flags: 0,
            price: BID_BPS.to_string(),
            amount: EXTRA_BID_AMOUNT,
            min_amount: 0,
            epoch_id: 0,
            client_order_id: batch_first_coid + i,
        })
        .collect();
    dex.place_batch(
        &taker.note.address,
        ParamsOfPlaceBatch {
            event_id: market.key.event_id.clone(),
            oracle_list_hash: market.key.oracle_list_hash.clone(),
            token_type: market.key.token_type,
            orders: batch,
            cancel_ids: Vec::new(),
        },
        Signer::Keys { keys: taker.note.keys.clone() },
    )
    .await
    .expect("place_batch");
    wait_owner_order(
        dex,
        &market.order_book,
        &taker.note.dih_dec,
        batch_first_coid + EXTRA_BIDS - 1,
        true,
    )
    .await;
    wait_not_busy(dex, &taker.note.address, "rest the batch of bids").await;

    assert!(
        now_unix() + RESTING_MARGIN_SECS < market.result_start,
        "only {}s left before the market closes, less than the {RESTING_MARGIN_SECS}s margin — \
         the stand is too slow for a {STAKE_PERIOD_SHUTDOWN}s market, and the orders may not \
         have been resting for the whole window this scenario claims to test",
        market.result_start.saturating_sub(now_unix())
    );

    // Both orders really did escrow something; without this the refunds
    // asserted below could all be no-ops.
    assert!(
        pn_locked(&r, &taker.note.address).await > taker_locked_before,
        "the bid locked no collateral"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME) + ORDER_AMOUNT,
        at(&maker_tokens_before, OUTCOME),
        "the ask did not take the maker's outcome tokens"
    );

    // The book's own tally, read while it still exists: the drain hands
    // exactly this to RootPN and then destroys the book, so afterwards there
    // is nothing left to compare against.
    let details =
        dex.get_order_book_details(&market.order_book).await.expect("order book details");
    let book_fees = details.total_protocol_fees;

    // And what makes the drain a multi-pass one: it retires `MAX_SHUTDOWN_BATCH`
    // order ids per call, and the book holds more than that — both in orders
    // still resting and in ids ever issued, which is what the pass actually
    // walks. Stated here rather than assumed, because a change to either
    // constant would quietly turn this back into the single-pass case the rest
    // of the suite already covers.
    assert!(
        details.order_count > MAX_SHUTDOWN_BATCH,
        "{} orders are resting, not more than the {MAX_SHUTDOWN_BATCH} a single drain pass \
         retires",
        details.order_count
    );
    assert!(
        details.next_order_id > MAX_SHUTDOWN_BATCH + 1,
        "the book has issued {} order ids, few enough for one pass to walk them all",
        details.next_order_id - 1
    );

    assert!(
        book_fees > 0,
        "the book accrued no protocol fees, so the hand-over and withdrawal below would be \
         assertions about nothing"
    );
    let root_fees_before = invariant::protocol_fee(&r, RootPn::DEFAULT_ADDRESS, TOKEN_TYPE_NACKL)
        .await
        .expect("read RootPN protocol fees");

    wait_until(market.result_start).await;
    resolve_and_drain(dex, &market.pmp, &market.oracle, OUTCOME).await;
    wait_not_busy(dex, &maker.note.address, "ask refunded").await;
    wait_not_busy(dex, &taker.note.address, "bid refunded").await;

    // The collateral behind the bid came back, exactly.
    assert_eq!(
        pn_locked(&r, &taker.note.address).await,
        taker_locked_before,
        "the drain left the bid's collateral locked on a book that no longer exists"
    );
    assert_eq!(
        pn_balance(&r, &taker.note.address).await,
        taker_free_before,
        "the bid's collateral did not return to the taker's free balance"
    );

    // And the outcome tokens behind the ask.
    assert_eq!(
        at(&outcome_tokens(dex, &maker).await, OUTCOME),
        at(&maker_tokens_before, OUTCOME),
        "the drain did not return the ask's outcome tokens to the maker's stake"
    );

    // The note-side counters agree that nothing is resting. They are written
    // by the note, the book's index by the book: a drain that refunded but
    // never told the notes would leave both permanently unable to withdraw.
    assert_eq!(open_orders(&r, &maker.note.address).await, 0, "the maker still counts an order");
    assert_eq!(open_orders(&r, &taker.note.address).await, 0, "the taker still counts an order");

    // The drain also hands the book's protocol fees to RootPN, which is the
    // only way they ever get there — the book holds them until it dies.
    let root_fees_after = invariant::protocol_fee(&r, RootPn::DEFAULT_ADDRESS, TOKEN_TYPE_NACKL)
        .await
        .expect("read RootPN protocol fees");
    assert_eq!(
        root_fees_after,
        root_fees_before + book_fees,
        "RootPN was credited something other than the book's own tally of {book_fees}"
    );

    // And the owner can take them out. Owner-only, so this doubles as a check
    // that the stand's RootPN really is owned by the key its zerostate says.
    let owner = root_pn_owner_keys(&ledger_dir);
    let dest_before = r
        .account_ecc(&taker.note.address)
        .await
        .expect("read destination ECC")
        .ecc
        .get(&TOKEN_TYPE_NACKL)
        .copied()
        .unwrap_or(0);

    RootPn::new(Arc::clone(ctx), dex_contract_params(RootPn::DEFAULT_ADDRESS))
        .withdraw_protocol_fees(
            ParamsOfWithdrawProtocolFees {
                to: taker.note.address.clone(),
                dapp_id: FEE_DEST_DAPP.to_string(),
                token_type: TOKEN_TYPE_NACKL,
                amount: book_fees,
            },
            Signer::Keys { keys: owner },
        )
        .await
        .expect("withdraw_protocol_fees");

    poll_until("RootPN never paid the protocol fees out", || async {
        invariant::protocol_fee(&r, RootPn::DEFAULT_ADDRESS, TOKEN_TYPE_NACKL)
            .await
            .expect("read RootPN protocol fees")
            == root_fees_before
    })
    .await;
    let dest_after = r
        .account_ecc(&taker.note.address)
        .await
        .expect("read destination ECC")
        .ecc
        .get(&TOKEN_TYPE_NACKL)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        dest_after,
        dest_before + book_fees,
        "the withdrawn fees left RootPN's books but did not arrive"
    );

    // ── and the two ways it must not work ─────────────────────────────────
    //
    // RootPN holds every market's protocol fees together and pays them to an
    // address its caller names, so these two guards are the whole of what
    // stands between one book's earnings and anyone who asks.
    //
    // Neither refusal is legible as an error. The bounds check fires after
    // `tvm.accept()`, so it leaves an aborted transaction the send never
    // waits for; the owner check fires before it, and where the node draws
    // that line is not something a scenario should encode. What both leave is
    // the absence of a payment, read on both sides — the total RootPN still
    // owes, and the balance of the account the caller tried to pay. The
    // successful withdrawal just above is the control that says a permitted
    // call does move both.
    let root_pn = RootPn::new(Arc::clone(ctx), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let fees_before_negatives =
        invariant::protocol_fee(&r, RootPn::DEFAULT_ADDRESS, TOKEN_TYPE_NACKL)
            .await
            .expect("read RootPN protocol fees");

    // More than has ever accrued — asked for as the largest amount the
    // parameter can carry, so no reading of the current total is needed and
    // nothing another scenario does can make it legitimate.
    let _ = root_pn
        .withdraw_protocol_fees(
            ParamsOfWithdrawProtocolFees {
                to: taker.note.address.clone(),
                dapp_id: FEE_DEST_DAPP.to_string(),
                token_type: TOKEN_TYPE_NACKL,
                amount: MORE_THAN_ACCRUED,
            },
            Signer::Keys { keys: root_pn_owner_keys(&ledger_dir) },
        )
        .await;

    // The same call, correct in every respect but the signature. The note's
    // own keys are a real keypair that is simply not the root owner's, which
    // is the case that matters: the destination is the caller's to choose, so
    // anyone able to sign could name their own.
    let _ = root_pn
        .withdraw_protocol_fees(
            ParamsOfWithdrawProtocolFees {
                to: taker.note.address.clone(),
                dapp_id: FEE_DEST_DAPP.to_string(),
                token_type: TOKEN_TYPE_NACKL,
                amount: 1,
            },
            Signer::Keys { keys: taker.note.keys.clone() },
        )
        .await;

    // Settled by the same barrier the successful withdrawal used: if either
    // call were going to pay out, it would have by the time a poll of this
    // length has run its course.
    tokio::time::sleep(std::time::Duration::from_secs(NEGATIVE_SETTLE_SECS)).await;
    assert_eq!(
        invariant::protocol_fee(&r, RootPn::DEFAULT_ADDRESS, TOKEN_TYPE_NACKL)
            .await
            .expect("read RootPN protocol fees"),
        fees_before_negatives,
        "a refused withdrawal moved protocol fees anyway"
    );
    let dest_settled = r
        .account_ecc(&taker.note.address)
        .await
        .expect("read destination ECC")
        .ecc
        .get(&TOKEN_TYPE_NACKL)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        dest_settled, dest_after,
        "a refused withdrawal paid the caller's chosen destination anyway"
    );

    maker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    taker.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
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

/// How many orders the note believes it has resting.
async fn open_orders(r: &chain_reader::ChainReader, pn_address: &str) -> u32 {
    invariant::pn_open_order_count(r, pn_address).await.expect("read open order count")
}

/// RootPN's owner keypair, which the zerostate generator writes beside the
/// seed notes as `PMPRoot.keys.json` — the same key it hands RootPN's
/// constructor as `pubkey`.
fn root_pn_owner_keys(ledger_dir: &std::path::Path) -> KeyPair {
    let path = ledger_dir.join("PMPRoot.keys.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read RootPN owner keys at {}: {e}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parse RootPN owner keys at {}: {e}", path.display()))
}
