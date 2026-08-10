//! The two bounce branches that move real money: a market that can never be
//! confirmed, and an order sent to a book that does not exist yet.
//!
//! `bounce_recovery` covers the branches a note can reach with nothing else on
//! chain. These two need a market — one that fails to come up, and one that
//! has come up but has not frozen — and they are where the collateral is.
//!
//! ## A market nobody can confirm
//!
//! `deployPMP` names the oracle event lists that must confirm the event. Name
//! one that was never deployed and the sequence is fixed: the note debits its
//! initial stakes and attaches `Σ oracleFee + NETWORK_FEE_AMOUNT` as
//! **physical** SHELL, the PMP is created, its constructor sends `confirmEvent`
//! to an address with no account, and that message bounces back into
//! `PMP.onBounce`, which refunds and self-destructs.
//!
//! What makes this worth its own scenario is that the loss it used to leave
//! behind was invisible to a conservation check. Value moved from the note to
//! `RootPN` — both inside the tracked set — so the system total stayed exactly
//! right while the deployer was out of pocket. **Only a per-account delta sees
//! it**, which is the reason the invariant helper grew a physical mode at all.
//!
//! As of contracts v4.0.30 that hole is half closed, and this scenario states
//! precisely which half. `onBounce` now mirrors `rejectEvent` and forwards the
//! bounced `msg.currencies` — the oracle fee — back to the deployer. The
//! network fee is not in that message: the PMP withheld it, so it leaves with
//! `selfdestruct(ROOT_PN_ADDRESS)` like the rest of the account. So the
//! deployer's physical SHELL is down by exactly the network fee, `RootPN`'s is
//! up by exactly the same, and the assertion says both.
//!
//! ## An order with no book to go to
//!
//! Before the freeze the `OrderBook` address is derivable but the account is
//! not there, and `PrivateNote.placeOrder` does not check: it locks
//! collateral, records the client order id, and sends. Buy and sell fail
//! differently and are both here — a buy has `_balance` moved into
//! `_pendingPlaceBuyLock`, a sell has outcome tokens taken out of
//! `stake.amount`. The batch form belongs to the batch unit, not here.
//!
//! `_opNonce` is the discriminator for this half. Restoring a balance leaves
//! the note looking exactly as it did before the order, so "the balance is
//! unchanged" is equally true of an order that was refused outright — but
//! `placeOrder` bumps the counter after its checks and nothing ever clears it.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`.

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::private_note::ParamsOfDeployPmp;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::root_pn::ParamsOfGetPmpAddress;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::DEPLOYER_SEED_AMOUNT;
use crate::common::context::NETWORK_FEE_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::at;
use crate::common::market::outcome_tokens;
use crate::common::market::place_limit;
use crate::common::market::prepare_ephemeral_market;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;

/// The market for the order half only has to exist and be approved; it is
/// never frozen, so the window this waits out is the only deadline it has.
const STAKE_PERIOD_BOUNCE: u64 = 300;

const OUTCOME: u32 = 0;

/// Big enough that a sell of the whole position clears the book's minimum
/// notional — the shared `stake` helper's amount would be refused by the note
/// long before it could reach a book that is not there, and the scenario would
/// then be asserting nothing.
const SELLABLE_STAKE: u128 = 40_000_000_000;

/// The doomed order's price, and the size of the buy that goes with it.
const BOUNCE_PRICE_BPS: u128 = 3_000;
const BOUNCE_BUY_AMOUNT: u128 = 40_000_000_000;

const MIN_ORDER_NOTIONAL_NACKL: u128 = 10_000_000_000;
const FULL_PERCENT: u128 = 10_000;
const LOT_SIZE_NACKL: u128 = 10_000_000;

// Both orders have to be well-formed enough to reach the book: the note
// applies the notional floor and the lot multiple itself, and an order it
// rejects never leaves, never bounces, and proves nothing about `onBounce`.
const _: () =
    assert!(BOUNCE_BUY_AMOUNT * BOUNCE_PRICE_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL_NACKL);
const _: () = assert!(SELLABLE_STAKE * BOUNCE_PRICE_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL_NACKL);
const _: () = assert!(BOUNCE_BUY_AMOUNT.is_multiple_of(LOT_SIZE_NACKL));
const _: () = assert!(SELLABLE_STAKE.is_multiple_of(LOT_SIZE_NACKL));

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_bounced_deploy_and_a_bounced_order_both_give_the_money_back_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let doomed = alloc.rent(PnProfile::Dep, "bounce_deploy").expect("rent the doomed-market note");
    let nonce = alloc.next_nonce().expect("allocate a nonce");

    // ── 1. a market whose oracle list is not there ────────────────────────
    // The event id and the oracle name only have to be unique to this run:
    // nothing will ever answer to either, which is the point.
    let event_id = format!("0x{:064x}", 0xD00Du64 + nonce);
    let absent_oracle = format!("absent-oracle-{nonce}");

    let root_pn = RootPn::new(ctx.clone(), dex_contract_params(RootPn::DEFAULT_ADDRESS));
    let pmp = root_pn
        .get_pmp_address(ParamsOfGetPmpAddress {
            event_id: event_id.clone(),
            names: vec![absent_oracle.clone()],
            token_type: TOKEN_TYPE_NACKL,
        })
        .await
        .expect("get_pmp_address")
        .pmp_address;

    let note_shell_before = account_shell(&r, &doomed.note.address).await;
    let root_shell_before = account_shell(&r, RootPn::DEFAULT_ADDRESS).await;
    let note_nackl_before = pn_balance(&r, &doomed.note.address).await;

    dex.deploy_pmp(
        &doomed.note.address,
        ParamsOfDeployPmp {
            event_id: event_id.clone(),
            oracle_fee: vec![ORACLE_FEE],
            token_type: TOKEN_TYPE_NACKL,
            names: vec![absent_oracle],
            index: vec![0],
            initial_stakes: vec![DEPLOYER_SEED_AMOUNT, DEPLOYER_SEED_AMOUNT],
        },
        Signer::Keys { keys: doomed.note.keys.clone() },
    )
    .await
    .expect("deploy_pmp");

    // The barrier is the whole settled state, because the refund arrives as
    // three independent messages — the stakes callback, the fee transfer, and
    // the self-destruct — and any two of them can land before the third. That
    // makes the assertions below a restatement of the barrier rather than an
    // independent check, which is fine here and needs no separate
    // discriminator: the expected physical delta is a *non-zero* loss, so a
    // `deployPMP` that was refused outright cannot satisfy it either.
    // The note pays out `ORACLE_FEE + NETWORK_FEE_AMOUNT` and gets the oracle
    // fee back, so the settled figure is the baseline less the network fee.
    let want_note_shell = note_shell_before - NETWORK_FEE_AMOUNT;
    poll_until(&format!("the doomed market {pmp} never unwound"), || async {
        r.account_absent(&pmp).await.expect("read the doomed market's account")
            && pn_balance(&r, &doomed.note.address).await == note_nackl_before
            && account_shell(&r, &doomed.note.address).await == want_note_shell
    })
    .await;

    assert_eq!(
        pn_balance(&r, &doomed.note.address).await,
        note_nackl_before,
        "the bounced deploy did not return the note's initial stakes"
    );
    assert_eq!(
        account_shell(&r, &doomed.note.address).await,
        note_shell_before - NETWORK_FEE_AMOUNT,
        "the deployer is out of pocket by something other than the network fee: the oracle fee \
         rides back on the bounced message and `onBounce` forwards it, the network fee never \
         left the market and leaves with its self-destruct"
    );
    assert_eq!(
        account_shell(&r, RootPn::DEFAULT_ADDRESS).await,
        root_shell_before + NETWORK_FEE_AMOUNT,
        "the network fee the deployer lost did not arrive at RootPN — the destination the \
         market's self-destruct names"
    );
    assert!(
        dex.get_stakes(&doomed.note.address).await.expect("pn stakes").stakes.is_empty(),
        "the note still holds a stake record for a market that no longer exists"
    );

    // ── 2. an order sent before the book exists ───────────────────────────
    let deployer = alloc.rent(PnProfile::Dep, "bounce_deploy").expect("rent the deployer note");
    let trader = alloc.rent(PnProfile::Trd, "bounce_deploy").expect("rent the trader note");
    let market_nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        prepare_ephemeral_market(ctx, dex, &b0, &deployer, market_nonce, STAKE_PERIOD_BOUNCE).await;

    // A position large enough to sell. The shared helper stakes a fixed, much
    // smaller amount, and a sell of that would be refused by the note for
    // being under the book's minimum notional — before it could bounce.
    dex.set_stake(
        &trader.note.address,
        ParamsOfSetStake {
            event_id: market.key.event_id.clone(),
            oracle_list_hash: market.key.oracle_list_hash.clone(),
            token_type: TOKEN_TYPE_NACKL,
            outcome: OUTCOME,
            amount: SELLABLE_STAKE,
            use_coupon: false,
        },
        Signer::Keys { keys: trader.note.keys.clone() },
    )
    .await
    .expect("set_stake");
    wait_not_busy(dex, &trader.note.address, "the sellable stake").await;

    let position_before = at(&outcome_tokens(dex, &trader).await, OUTCOME);
    assert_eq!(
        position_before, SELLABLE_STAKE,
        "the stake did not reach the note's position, so the sell below would be refused for a \
         reason this scenario is not testing"
    );

    // The buy: collateral leaves `_balance` for `_pendingPlaceBuyLock` and has
    // to come back.
    let free_before = pn_balance(&r, &trader.note.address).await;
    let nonce_before = op_nonce(&r, &trader.note.address).await;
    place_limit(
        dex,
        &trader,
        &market.key,
        OUTCOME,
        true,
        &BOUNCE_PRICE_BPS.to_string(),
        BOUNCE_BUY_AMOUNT,
        market_nonce as u128 * 10,
    )
    .await;
    wait_not_busy(dex, &trader.note.address, "the bounced buy").await;

    let nonce_after_buy = op_nonce(&r, &trader.note.address).await;
    assert!(
        nonce_after_buy > nonce_before,
        "the note never accepted the buy ({nonce_before} -> {nonce_after_buy}), so nothing was \
         sent, nothing bounced, and the balance below is unchanged for the wrong reason"
    );
    assert_eq!(
        pn_balance(&r, &trader.note.address).await,
        free_before,
        "the bounced buy did not release the collateral it locked"
    );
    assert_eq!(
        pn_locked(&r, &trader.note.address).await,
        0,
        "the bounced buy left collateral escrowed against an order that never reached a book"
    );

    // The sell: outcome tokens leave `stake.amount` and have to come back.
    place_limit(
        dex,
        &trader,
        &market.key,
        OUTCOME,
        false,
        &BOUNCE_PRICE_BPS.to_string(),
        SELLABLE_STAKE,
        market_nonce as u128 * 10 + 1,
    )
    .await;
    wait_not_busy(dex, &trader.note.address, "the bounced sell").await;

    let nonce_after_sell = op_nonce(&r, &trader.note.address).await;
    assert!(
        nonce_after_sell > nonce_after_buy,
        "the note never accepted the sell ({nonce_after_buy} -> {nonce_after_sell})"
    );
    assert_eq!(
        at(&outcome_tokens(dex, &trader).await, OUTCOME),
        position_before,
        "the bounced sell did not return the outcome tokens it took out of the stake"
    );

    // The note is operable after both: a third order with a client id one of
    // the bounced ones already used would be refused if the bounce had not
    // freed it.
    let nonce_before_reuse = op_nonce(&r, &trader.note.address).await;
    place_limit(
        dex,
        &trader,
        &market.key,
        OUTCOME,
        true,
        &BOUNCE_PRICE_BPS.to_string(),
        BOUNCE_BUY_AMOUNT,
        market_nonce as u128 * 10,
    )
    .await;
    wait_not_busy(dex, &trader.note.address, "the client-id reuse").await;
    assert!(
        op_nonce(&r, &trader.note.address).await > nonce_before_reuse,
        "the note refused an order carrying a client id the bounced buy had used, so the bounce \
         did not free it"
    );

    // The doomed note ends where it started, minus a network fee that is not
    // state: its stake record was deleted by the refund and asserted gone
    // above. Offered back to the pool rather than tainted — the sweep is the
    // authority on whether it really is clean, and its verdict here is worth
    // having on record.
    doomed.release_clean(&r).await.expect("release the doomed-market note");
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    trader.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// The account's own physical SHELL — `currencies[2]` on the account itself,
/// not the custodied `_balance` a note keeps for its owner. Oracle and network
/// fees travel this way and never touch a logical balance.
async fn account_shell(r: &chain_reader::ChainReader, addr: &str) -> u128 {
    r.account_ecc(addr)
        .await
        .unwrap_or_else(|e| panic!("read physical ECC of {addr}: {e:?}"))
        .ecc
        .get(&CURRENCY_ID_SHELL)
        .copied()
        .unwrap_or(0)
}

async fn op_nonce(r: &chain_reader::ChainReader, pn_address: &str) -> u64 {
    invariant::pn_op_nonce(r, pn_address).await.expect("read the note's operation counter")
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
