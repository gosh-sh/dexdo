//! Rotating a note's owner key, and what the old one may do afterwards.
//!
//! A note is owned by one pubkey and `changeOwner` replaces it. That is the
//! recovery story for a lost device and the revocation story for a leaked key,
//! and the suite has never run it against a stand — so the half that matters,
//! the OLD key being refused, has never been observed at all.
//!
//! ## The refusal is only readable next to the acceptance
//!
//! A note validates the signature before `tvm.accept()`, so an operation signed
//! with a retired key is thrown out with nothing sent back. Fire-and-forget
//! means the caller sees the same `Ok` either way, and "the stake did not
//! register" is equally true of a key that was correctly refused, a market that
//! was closed, and a message that went missing.
//!
//! So the old key's attempt is made **where the new key's identical attempt
//! succeeds**: same market, same outcome, same amount, inside the same staking
//! window, seconds apart. The pair is the assertion. On its own, neither half
//! is about the key.
//!
//! ## Rotating back, and what that buys
//!
//! The note comes from a shared pool, so it has to end the run owned by the key
//! the pool has on file. Rotating back is therefore obligatory — and it is also
//! a second reading for free: the original key staking again proves the
//! rotation round-tripped rather than merely reporting a new pubkey.
//!
//! That second stake goes into the SAME market and the same outcome as the
//! first, which is the other thing nothing covered: a note staking twice into
//! one market accumulates, rather than replacing its position or being refused.
//! Both stakes are checked as exact balance movements.
//!
//!   cargo nextest run --manifest-path sdk/Cargo.toml --run-ignored only \
//!     -E 'test(=key_rotation::a_rotated_key_takes_over_and_the_old_one_stops_working_local)'

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use dodex_contracts::dex::private_note::ParamsOfChangeOwner;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_sdk::proof;

use crate::common::allocator;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::keys::gen_keys;
use crate::common::locks;
use crate::common::market::prepare_ephemeral_market;
use crate::common::misc::poll_until;
use crate::common::misc::wait_not_busy;

/// The staking window has to hold a rotation, a refused stake, an accepted one,
/// a rotation back and a second accepted stake — five acknowledged operations
/// on one note, which takes them strictly one at a time.
const STAKE_PERIOD_ROTATION: u64 = 420;

const OUTCOME: u32 = 0;

/// What each of the two accepted stakes puts in. A multiple of the 0.01 NACKL
/// lot and over the 1 NACKL minimum.
const STAKE: u128 = 20_000_000_000;

const _: () = assert!(STAKE.is_multiple_of(10_000_000));
const _: () = assert!(STAKE >= 1_000_000_000);

#[tokio::test]
#[ignore = "requires a local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID"]
async fn a_rotated_key_takes_over_and_the_old_one_stops_working_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");
    let b0 = locks::ChainLockGuard::shared(&ledger_dir).expect("acquire b0.lock shared");

    let r = chain_reader::ChainReader::new();
    let ctx = &r.ctx;
    let dex = &r.dex;

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let creator = alloc.rent(PnProfile::Dep, "key_rotation").expect("rent the creator note");
    let rotator = alloc.rent(PnProfile::Trd, "key_rotation").expect("rent the note to rotate");

    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let market =
        prepare_ephemeral_market(ctx, dex, &b0, &creator, nonce, STAKE_PERIOD_ROTATION).await;

    let original = rotator.note.keys.clone();
    let rotated = gen_keys(ctx.clone());
    let original_pubkey = proof::pubkey_to_dec(&original.public);
    let rotated_pubkey = proof::pubkey_to_dec(&rotated.public);

    // ── 1. the note changes hands ─────────────────────────────────────────
    change_owner(dex, &rotator.note.address, &rotated_pubkey, &original).await;
    poll_until("the note never took the new owner key", || async {
        owner_of(dex, &rotator.note.address).await == proof::parse_u256(&rotated_pubkey)
    })
    .await;

    // ── 2. the old key, where the new one is about to work ────────────────
    //
    // Deliberately first: run after the acceptance, a refusal would also be
    // explained by the note being busy with the operation before it.
    let before_refused = pn_balance(&r, &rotator.note.address).await;
    let stakes_before = stake_count(dex, &rotator.note.address).await;
    set_stake_signed(dex, &rotator.note.address, &market.key, &original).await;
    assert_eq!(
        pn_balance(&r, &rotator.note.address).await,
        before_refused,
        "a stake signed with the retired key moved the note's balance"
    );
    assert_eq!(
        stake_count(dex, &rotator.note.address).await,
        stakes_before,
        "a stake signed with the retired key was recorded against the note"
    );

    // ── 3. and the same call under the new one ────────────────────────────
    set_stake_signed(dex, &rotator.note.address, &market.key, &rotated).await;
    poll_until("the rotated key could not stake either", || async {
        pn_balance(&r, &rotator.note.address).await == before_refused - STAKE
    })
    .await;
    assert_eq!(
        stake_count(dex, &rotator.note.address).await,
        stakes_before + 1,
        "the rotated key's stake moved the balance without leaving a record"
    );

    // ── 4. handed back, and working again ─────────────────────────────────
    //
    // Obligatory rather than tidy: the note is rented from a pool that holds
    // the original key, and a run that ends without this leaves it unusable to
    // everyone. A panic before here drops the lease without releasing it, which
    // quarantines the note instead of handing it back broken.
    change_owner(dex, &rotator.note.address, &original_pubkey, &rotated).await;
    poll_until("the note never took its original key back", || async {
        owner_of(dex, &rotator.note.address).await == proof::parse_u256(&original_pubkey)
    })
    .await;

    // A second stake into the SAME market on the SAME outcome. It says the
    // round trip restored a usable key rather than only a reported pubkey —
    // and it says a repeated stake accumulates instead of replacing the
    // position or being refused.
    let before_second = pn_balance(&r, &rotator.note.address).await;
    set_stake_signed(dex, &rotator.note.address, &market.key, &original).await;
    poll_until("the restored key could not stake", || async {
        pn_balance(&r, &rotator.note.address).await == before_second - STAKE
    })
    .await;
    assert_eq!(
        pn_balance(&r, &rotator.note.address).await,
        before_refused - 2 * STAKE,
        "two stakes of {STAKE} into one market did not cost the note both of them"
    );
    assert_eq!(
        stake_count(dex, &rotator.note.address).await,
        stakes_before + 1,
        "the second stake into the same market opened a second record; a repeated stake is \
         supposed to accumulate into the one already there"
    );

    creator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
    rotator.taint(allocator::TaintReason::DirtyState { fields: vec!["_stakes".to_string()] });
}

/// Hand the note to `new_pubkey`, signing with whoever owns it now.
async fn change_owner(dex: &dodex_sdk::Dex, pn_address: &str, new_pubkey: &str, signer: &KeyPair) {
    dex.change_owner(
        pn_address,
        ParamsOfChangeOwner { new_pubkey: new_pubkey.to_string() },
        Signer::Keys { keys: signer.clone() },
    )
    .await
    .expect("change_owner accepted");
    wait_not_busy(dex, pn_address, "change_owner").await;
}

/// Stake with an explicitly chosen key rather than the one the pool lease
/// carries — which is the whole point here, and why this does not go through
/// `market::stake_amount`.
async fn set_stake_signed(
    dex: &dodex_sdk::Dex,
    pn_address: &str,
    key: &ParamsOfStakeKey,
    signer: &KeyPair,
) {
    let _ = dex
        .set_stake(
            pn_address,
            ParamsOfSetStake {
                event_id: key.event_id.clone(),
                oracle_list_hash: key.oracle_list_hash.clone(),
                token_type: key.token_type,
                outcome: OUTCOME,
                amount: STAKE,
                use_coupon: false,
            },
            Signer::Keys { keys: signer.clone() },
        )
        .await;
    wait_not_busy(dex, pn_address, "set_stake").await;
}

async fn owner_of(dex: &dodex_sdk::Dex, pn_address: &str) -> num_bigint::BigUint {
    proof::parse_u256(
        &dex.get_private_note_details(pn_address).await.expect("note details").ephemeral_pubkey,
    )
}

async fn stake_count(dex: &dodex_sdk::Dex, pn_address: &str) -> usize {
    dex.get_stakes(pn_address).await.expect("stakes").stakes.len()
}

async fn pn_balance(r: &chain_reader::ChainReader, pn_address: &str) -> u128 {
    invariant::pn_balance_opt(r, pn_address, TOKEN_TYPE_NACKL)
        .await
        .expect("read note balance")
        .unwrap_or_else(|| panic!("note {pn_address} is not on chain"))
}
