//! The contracts' internal wiring, called from outside by someone with no
//! right to.
//!
//! A market's parts talk to each other through ordinary public functions.
//! `acceptStake` is how a note tells a market it has staked; `shutdown` is how
//! a market tells its book to drain; `onOrderBookShutdownComplete` is how the
//! book reports back. Every one of them is reachable from an external message
//! by anybody, and every one is guarded by a check on who sent it.
//!
//! Those checks are the only thing standing between the protocol and its own
//! vocabulary. Nothing else stops a stranger from telling a market it has been
//! staked into, or telling it that its book has finished draining when the
//! book still holds everyone's escrow — which would open claims early and let
//! the drain's refunds land on records that are already gone.
//!
//! None of these calls answers. `senderIs`-style guards sit past
//! `tvm.accept()` and the caller's send does not wait for a transaction, so
//! each is read as the absence of the thing it would have caused. The
//! readings are deliberately different from one another — a pool total, a
//! shutdown flag, a stake record, an order — because a single one of them
//! standing still could be a message that never arrived, while all of them
//! standing still while the market keeps working could not.
//!
//! ## What it says to the market, and what must not happen
//!
//! | said                              | would have meant                     |
//! |-----------------------------------|--------------------------------------|
//! | `acceptStake`                     | a pool grows with nothing behind it  |
//! | `approveEvent`                    | an oracle nobody appointed confirms  |
//! | `onOrderBookShutdownComplete`     | claims open while escrow is out      |
//! | `OrderBook.shutdown`              | the book drains on a stranger's word |
//! | `OrderBook.cancelAllOrders`       | somebody else's quotes withdrawn     |
//! | `OrderBook.executeBatch`          | orders placed in another note's name |
//!
//! And afterwards the market has to still work, which is the control the
//! whole scenario rests on: a genuine order from the note that owns it rests
//! as usual.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use std::collections::HashMap;
use std::sync::Arc;

use ackinacki_kit::tvm_client::abi::Signer;
use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::order_book::ParamsOfCancelAllOrders as ObCancelAll;
use dodex_contracts::dex::order_book::ParamsOfExecuteBatch;
use dodex_contracts::dex::pmp::ParamsOfAcceptStake;
use dodex_contracts::dex::pmp::ParamsOfApproveEvent;
use dodex_contracts::dex::pmp::Pmp;
use dodex_sdk::dex_contract_params;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::keys::gen_keys;
use crate::common::locks;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::place_limit;
use crate::common::market::stake_amount;
use crate::common::market::wait_owner_order;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_IMPOSTOR: u64 = 360;
const OUTCOME: u32 = 0;
const FULL_PERCENT: u128 = 10_000;
const LOT: u128 = 10_000_000;
const MIN_ORDER_NOTIONAL: u128 = 10_000_000_000;

/// What the note really stakes, before any of the impersonation starts.
const STAKE: u128 = 20_000_000_000;

/// What the impostor claims was staked. Large enough that a pool which grew
/// by it could not be mistaken for rounding.
const CLAIMED_STAKE: u128 = 500_000_000_000;

const _: () = assert!(STAKE.is_multiple_of(LOT));
const _: () = assert!(CLAIMED_STAKE > 10 * STAKE, "a lie has to be visible in the total");

/// The order the note rests before the impersonation, and the one it rests
/// after — the control that says the market still works.
const ORDER_BPS: u128 = 5_000;
const ORDER_AMOUNT: u128 = 25_000_000_000;

const _: () = assert!(ORDER_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(ORDER_AMOUNT * ORDER_BPS / FULL_PERCENT >= MIN_ORDER_NOTIONAL);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn the_protocols_own_vocabulary_is_not_available_to_strangers_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "impostor_calls").expect("rent the deployer note");
    let note = alloc.rent(PnProfile::Trd, "impostor_calls").expect("rent the honest note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        deploy_ephemeral_market(ctx, dex, &b0, &deployer, nonce, STAKE_PERIOD_IMPOSTOR).await;

    // Something real to lie about: a stake, and an order holding escrow.
    let base = nonce as u128 * 10;
    let honest_cid = base + 1;
    place_limit(dex, &note, &market.key, OUTCOME, true, &ORDER_BPS.to_string(), ORDER_AMOUNT, honest_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, honest_cid, true).await;
    wait_not_busy(dex, &note.note.address, "the order the impostor tries to cancel").await;

    let impostor = Signer::Keys { keys: gen_keys(ctx.clone()) };
    let pmp = Pmp::new(Arc::clone(ctx), dex_contract_params(&market.pmp));
    let book = OrderBook::new(Arc::clone(ctx), dex_contract_params(&market.order_book));

    let pool_before = pool_total(&r, &market.pmp).await;
    let locked_before = locked(&r, &note.note.address).await;
    let free_before = free(&r, &note.note.address).await;

    // ── telling the market it has been staked into ────────────────────────
    let _ = pmp
        .accept_stake(
            ParamsOfAcceptStake {
                outcome_id: OUTCOME,
                stake_amount: CLAIMED_STAKE,
                deposit_identifier_hash: note.note.dih_dec.clone(),
                bet_type: 0,
            },
            impostor.clone(),
        )
        .await;

    // ── and that an oracle has confirmed its event ────────────────────────
    let mut outcomes = HashMap::new();
    outcomes.insert(0_u32, "Team A".to_string());
    outcomes.insert(1_u32, "Team B".to_string());
    let _ = pmp
        .approve_event(
            ParamsOfApproveEvent {
                oracle_pubkey: "0".to_string(),
                outcome_names: outcomes,
                describe: "Who wins?".to_string(),
                name: market.oracle.oracle_name.clone(),
                trust_addr: None,
            },
            impostor.clone(),
        )
        .await;

    // ── and that its book has finished draining ───────────────────────────
    //
    // The worst of the six if it worked: claims open while the book still
    // holds everyone's escrow, and the refunds the drain has yet to send
    // would land on records that have already been deleted.
    let _ = pmp.on_order_book_shutdown_complete(impostor.clone()).await;

    // ── telling the book to drain ─────────────────────────────────────────
    let _ = book.shutdown(impostor.clone()).await;

    // ── to withdraw somebody else's quotes ────────────────────────────────
    let _ = book
        .cancel_all_orders(
            ObCancelAll { deposit_identifier_hash: note.note.dih_dec.clone(), op_nonce: 1 },
            impostor.clone(),
        )
        .await;

    // ── and to place an order in their name ───────────────────────────────
    let _ = book
        .execute_batch(
            ParamsOfExecuteBatch {
                deposit_identifier_hash: note.note.dih_dec.clone(),
                orders: Vec::new(),
                cancel_ids: Vec::new(),
                op_nonce: 1,
            },
            impostor,
        )
        .await;

    // Give every one of them the time it would have needed to work.
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;

    // ── none of it happened ───────────────────────────────────────────────
    assert_eq!(
        pool_total(&r, &market.pmp).await,
        pool_before,
        "a stranger's word grew the market's pool"
    );
    let pmp_state = Pmp::new(Arc::clone(ctx), dex_contract_params(&market.pmp))
        .get_shutdown_state()
        .await
        .expect("pmp shutdown state");
    assert!(
        !pmp_state.order_book_done,
        "the market believes its book has finished draining because a stranger said so — claims \
         are open while the escrow is still out"
    );
    let book_state = book.get_shutdown_state().await.expect("order book shutdown state");
    assert!(
        !book_state.shutting_down,
        "the book started draining on a stranger's word"
    );
    assert_eq!(
        order_size(dex, &market.order_book, &note.note.dih_dec, honest_cid).await,
        Some(ORDER_AMOUNT),
        "a stranger cancelled the note's order"
    );
    assert_eq!(
        (free(&r, &note.note.address).await, locked(&r, &note.note.address).await),
        (free_before, locked_before),
        "one of the impersonated calls moved the note's money"
    );

    // ── and the market still works ────────────────────────────────────────
    //
    // The control the whole scenario rests on: every reading above is equally
    // true of six messages that were never delivered.
    let control_cid = base + 2;
    stake_amount(dex, &note, &market.key, OUTCOME, STAKE, false).await;
    place_limit(dex, &note, &market.key, OUTCOME, true, &ORDER_BPS.to_string(), ORDER_AMOUNT, control_cid)
        .await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, control_cid, true).await;
    assert!(
        pool_total(&r, &market.pmp).await > pool_before,
        "the market took nothing from the note that really did stake, so its silence above says \
         nothing about the impostor"
    );

    note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    deployer.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
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

/// The market's own total — `PMP._totalPool`, which every accepted stake
/// grows and nothing else does.
async fn pool_total(r: &chain_reader::ChainReader, pmp: &str) -> u128 {
    invariant::pmp_total_pool(r, pmp).await.expect("read the market's pool")
}

async fn free(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}
