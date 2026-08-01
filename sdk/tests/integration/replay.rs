//! The same signed message, sent twice.
//!
//! Every other scenario in this suite asks the SDK to make a call, and the
//! SDK builds a fresh message each time — new timestamp, new expiry, new
//! hash. So the one thing none of them can do is send the *same* message
//! again, which is the only way to exercise the protection built for exactly
//! that: `afterSignatureCheck` records the hash of every external message a
//! note accepts and refuses one it has seen before.
//!
//! The failure it guards against is not a rejected call. It is a call that
//! happens **twice** — one signed instruction to place an order, replayed by
//! anyone who saw it, taking a second lock and putting a second order on the
//! book for something its owner asked for once.
//!
//! ## What makes this one different to read
//!
//! Unlike almost every other guard in the suite, this one sits *before*
//! `tvm.accept()` — it is the compiler-invoked hook that runs after the
//! signature is verified and before the body. A message it rejects is never
//! accepted at all, so the node has no reason to take it, and the second send
//! may well come back as an error rather than in silence.
//!
//! That makes the error tempting to assert, and it is still the wrong thing
//! to assert: what matters is not how the second send was answered but that
//! the operation happened once. So the reading is three independent traces of
//! the same instruction — one order on the book, one lock taken, one advance
//! of `_opNonce` — with the first send's success as the control that the
//! message was well-formed to begin with.
//!
//! ## The window the whole scenario has to live inside
//!
//! The same hook that rejects a repeat also rejects a message that has
//! expired, and one whose expiry is more than five minutes out. Those two
//! bracket everything below: a replay sent after the expiry it carries is
//! turned away for **being late**, not for being a repeat, and the three
//! readings would be satisfied by a message that never reached the guard at
//! all. So the expiry is set explicitly rather than left to the client's
//! default of well under a minute, as far out as the hook will take it, and
//! the scenario asserts at the end that it was still inside that window when
//! the last replay went out. Without that assertion a slow stand turns this
//! scenario into one that passes without testing anything.
//!
//! The instruction replayed is an order rather than a stake, for two reasons.
//! `_opNonce` moves only where the note dispatches to a book, so it is a
//! reading a stake would not give; and an order needs no open staking window,
//! which keeps the scenario off a deadline it would otherwise be racing.
//!
//! Reads `E2E_NETWORK_ENDPOINT`, `E2E_SEED_NOTES` and `E2E_RUN_ID`; no
//! manifest and no preflight, for the reasons `usdc_release` gives.

use std::sync::Arc;

use ackinacki_kit::contracts::traits::AbiAccessor;
use ackinacki_kit::contracts::traits::AddressAccessor;
use ackinacki_kit::contracts::traits::EncodeMessage;
use ackinacki_kit::tvm_client::abi::CallSet;
use ackinacki_kit::tvm_client::abi::FunctionHeader;
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::processing;
use ackinacki_kit::tvm_client::processing::ParamsOfSendMessage;
use dodex_contracts::dex::private_note::PrivateNote;
use dodex_sdk::dex_contract_params;
use serde_json::json;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::locks;
use crate::common::market::deploy_ephemeral_market;
use crate::common::market::wait_owner_order;
use crate::common::misc::now_unix;
use crate::common::misc::wait_not_busy;

const STAKE_PERIOD_REPLAY: u64 = 400;
const OUTCOME: u32 = 0;
const LOT: u128 = 10_000_000;

/// How long the one signed message stays valid.
///
/// The note's replay hook refuses an expiry more than five minutes ahead of
/// the block it arrives in, so this is that ceiling less a minute of slack for
/// the chain's clock sitting behind the host's. Everything the scenario sends
/// has to go out inside it, which the assertion at the end enforces.
const MESSAGE_LIFETIME: u64 = 240;

/// What the one signed instruction asks for. Large enough that a second lock
/// would be unmistakable in the escrow.
const ORDER_BPS: u128 = 5_000;
const ORDER_AMOUNT: u128 = 25_000_000_000;

const _: () = assert!(ORDER_AMOUNT.is_multiple_of(LOT));
const _: () = assert!(ORDER_AMOUNT * ORDER_BPS / 10_000 >= 10_000_000_000);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn one_signed_instruction_is_carried_out_once_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "replay").expect("rent the creator note");
    let note = alloc.rent(PnProfile::Trd, "replay").expect("rent the staking note");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market = deploy_ephemeral_market(ctx, dex, &b0, &creator, nonce, STAKE_PERIOD_REPLAY).await;
    let cid = nonce as u128 * 10 + 1;

    // One message, encoded once and kept. Everything below sends this exact
    // BOC — the same bytes, the same signature, the same hash — which is what
    // no ordinary SDK call can do, since each builds a fresh one.
    let pn = PrivateNote::new(Arc::clone(ctx), dex_contract_params(&note.note.address));
    let expire_at = now_unix() + MESSAGE_LIFETIME;
    let call = CallSet {
        function_name: "placeOrder".to_string(),
        header: Some(FunctionHeader { expire: Some(expire_at as u32), time: None, pubkey: None }),
        input: Some(json!({
            "eventId": market.key.event_id,
            "oracleListHash": market.key.oracle_list_hash,
            "tokenType": market.key.token_type,
            "outcomeId": OUTCOME,
            "isBuy": true,
            "price": ORDER_BPS.to_string(),
            "amount": ORDER_AMOUNT.to_string(),
            "flags": 0,
            "minAmount": "0",
            "epochId": "0",
            "clientOrderId": cid.to_string(),
        })),
    };
    let encoded = pn
        .encode_message(Some(call), None, Signer::Keys { keys: note.note.keys.clone() })
        .await
        .expect("encode the message this scenario sends twice");

    let locked_before = locked(&r, &note.note.address).await;
    let nonce_before = op_nonce(&r, &note.note.address).await;

    // The first send is the control: without it, everything below is equally
    // true of a message that was never valid.
    send_raw(ctx, &pn, &encoded.message).await;
    wait_owner_order(dex, &market.order_book, &note.note.dih_dec, cid, true).await;
    wait_not_busy(dex, &note.note.address, "the first send of the message").await;

    let locked_after_first = locked(&r, &note.note.address).await;
    let nonce_after_first = op_nonce(&r, &note.note.address).await;
    assert!(
        locked_after_first > locked_before,
        "the order this scenario replays escrowed nothing, so a second lock would not show"
    );
    assert!(
        nonce_after_first > nonce_before,
        "the note's counter did not move even for the send that worked, so the readings below \
         cannot tell a refused replay from a note that does nothing at all"
    );

    // ── and again, byte for byte ──────────────────────────────────────────
    //
    // Twice more, because a protection that only holds for the first repeat is
    // not one. Whether the node takes these at all is not the subject — the
    // three traces the instruction left are.
    for attempt in 1..=2 {
        send_raw(ctx, &pn, &encoded.message).await;
        wait_not_busy(dex, &note.note.address, "a replayed message").await;

        let resting = owner_orders(dex, &market.order_book, &note.note.dih_dec).await;
        assert_eq!(
            resting.len(),
            1,
            "replay {attempt} put a second order on the book for one signed instruction"
        );
        assert_eq!(
            locked(&r, &note.note.address).await,
            locked_after_first,
            "replay {attempt} took a second lock for one signed instruction"
        );
        assert_eq!(
            op_nonce(&r, &note.note.address).await,
            nonce_after_first,
            "replay {attempt} reached the book far enough to advance the note's counter"
        );
    }

    // The readings above are only about replay while the message they replay
    // is still one the note would otherwise have taken. Past its expiry the
    // same hook refuses it for being late, and every one of them would hold
    // for a message that never reached the replay check at all.
    assert!(
        now_unix() < expire_at,
        "the replays went out after the message they replay had expired, so nothing above          distinguishes a refused repeat from a message that was simply too late"
    );

    note.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Post an already-encoded message to the chain.
///
/// The SDK's own calls encode and send in one step, which is why they cannot
/// be replayed: each produces a different message. This takes the BOC and
/// nothing else, so the same bytes can be posted again — and it deliberately
/// ignores what comes back, because a message the node refuses to accept and
/// one it accepts and aborts are both "the operation did not happen", which
/// is what the counters say better.
async fn send_raw(
    ctx: &Arc<ackinacki_kit::tvm_client::ClientContext>,
    pn: &PrivateNote,
    message: &str,
) {
    // The same shape the kit's own `send_message` builds, with the message
    // supplied rather than encoded: the abi and the dApp id come off the
    // contract, since neither is derivable from the address.
    let params = ParamsOfSendMessage {
        message: message.to_string(),
        abi: Some(pn.abi().clone()),
        thread_id: None,
        send_events: false,
        dapp_id: pn.dapp_id().to_string(),
    };
    let _ = processing::send_message(ctx.clone(), params, |_| async {}).await;
}

async fn owner_orders(
    dex: &dodex_sdk::Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
) -> Vec<u128> {
    dex.get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
        .await
        .expect("get_orders_by_owner")
        .orders
        .into_iter()
        .map(|o| o.client_order_id)
        .collect()
}

async fn locked(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_locked_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note escrow")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}

async fn op_nonce(r: &chain_reader::ChainReader, pn_address: &str) -> u64 {
    invariant::pn_op_nonce(r, pn_address).await.expect("read the note's op nonce")
}
