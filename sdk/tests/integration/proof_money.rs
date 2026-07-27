//! Proof-money: one prediction market through its whole life — deploy, stake,
//! freeze, split, trade, resolve, claim, self-destruct — asserting after
//! **every** phase that money was neither created nor destroyed, exactly, per
//! currency.
//!
//! This is the scenario the rest of the harness exists to serve, and it is the
//! executable specification of how the pieces fit: the ledger and its
//! `b0.lock` protocol, the note allocator, the provenance preflight, the
//! two-phase PMP setup, and the B0 conservation detector.
//!
//! # What it needs
//!
//! A local, from-scratch stand (freshly generated zerostate), addressed
//! entirely through the environment:
//!
//! - `E2E_NETWORK_ENDPOINT` — the stand's URL, scheme included.
//! - `E2E_SEED_NOTES` — the pre-baked `PrivateNote` pool. Its directory also
//!   holds the shared ledger and the `b0.lock` sidecar.
//! - `E2E_RUN_ID` — the ledger generation this run belongs to.
//! - `E2E_MANIFEST` / `DEXDO_SHA` — the build-time contract manifest and the
//!   commit it must belong to, both read by the preflight.
//!
//! `#[ignore]`, because none of that exists on a developer's machine by
//! default; the CI step that owns a stand selects this test by name and runs
//! it with `--run-ignored only`.
//!
//! # How to read the sequence
//!
//! Every phase is *action → barrier → snapshot → assertion*, and all four
//! steps are load-bearing:
//!
//! - The **assertion** is against the previous phase's snapshot, with
//!   [`ExternalDelta::none()`] throughout. Every account money can reach here
//!   — the three notes, the market, its order book, RootPN — is inside the
//!   tracked set, so nothing legitimately crosses its boundary and any
//!   non-zero delta is a real leak. A phase without an assertion is a phase a
//!   leak can hide in.
//! - The **barrier** comes first because the chain is asynchronous: a getter
//!   answers from committed state while the messages that finish the operation
//!   are still in flight, and money in flight has left the sender without
//!   reaching the recipient. A snapshot taken inside that window compares
//!   against a state that has not settled.
//! - Baselines an exact barrier compares against are read **before** the
//!   action that moves them. Read afterwards they describe the outcome, and
//!   the comparison degenerates into a tautology.
//!
//! The phases are ordered by the contracts, not by taste: staking only works
//! between `stakeStart` and `stakeEnd`, `splitFullSet` only after the market
//! freezes and only before `resultStart`, `claim` only after the order book
//! has finished draining. Each dependency is called out at the step that
//! relies on it.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::ClientContext;
use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::pmp::ParamsOfSubmitResolve;
use dodex_contracts::dex::pmp::ParamsOfSubmitSetTimings;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::private_note::ParamsOfPlaceOrder;
use dodex_contracts::dex::private_note::ParamsOfSetStake;
use dodex_contracts::dex::private_note::ParamsOfSplitFullSet;
use dodex_contracts::dex::private_note::ParamsOfStakeKey;
use dodex_contracts::dex::root_pn::ParamsOfGetProtocolFee;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_sdk::dex_contract_params;
use dodex_sdk::Dex;

use crate::common::allocator;
use crate::common::allocator::LeasedPn;
use crate::common::allocator::PnProfile;
use crate::common::chain_reader;
use crate::common::chain_reader::ChainReader;
use crate::common::context;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::context::NETWORK_FEE_AMOUNT;
use crate::common::context::ORACLE_FEE;
use crate::common::context::STAKE_AMOUNT;
use crate::common::context::TOKEN_TYPE_NACKL;
use crate::common::invariant;
use crate::common::invariant::ExpectedPhysDelta;
use crate::common::invariant::ExternalDelta;
use crate::common::invariant::ObState;
use crate::common::invariant::Phase;
use crate::common::invariant::PmpState;
use crate::common::invariant::TrackedContracts;
use crate::common::invariant::TrackedOb;
use crate::common::invariant::TrackedPmp;
use crate::common::locks;
use crate::common::misc::now_unix;
use crate::common::misc::pn_nackl;
use crate::common::misc::wait_active;
use crate::common::pmp;
use crate::common::pmp::OracleEventCtx;
use crate::common::preflight;

/// Distance from `setTimings` to `resultStart`. `PMP._computeStakeEnd` makes
/// the staking window 10% of it, so 300 buys a ~30 s window to place both
/// stakes in and ~270 s afterwards for freeze, split and the trade — every one
/// of which has to land before `resultStart` closes the market to further
/// pool changes.
const STAKE_PERIOD_PROOF: u64 = 300;

/// The staking window is short enough that a slow stand could push the second
/// stake past `stakeEnd`, where it fails for a reason that has nothing to do
/// with conservation. Asserting the remaining budget right after the stakes
/// turns that into a clear diagnosis instead of a puzzling revert later.
const STAKE_MARGIN_SECS: u64 = 8;

/// SHELL as a *token type* (the key of a note's `_balance` map), which is a
/// different namespace from [`CURRENCY_ID_SHELL`], the ECC currency id the
/// physical view reads — the two happen to share the number 2. Conservation is
/// checked for it as well as for NACKL: this market moves only NACKL, so
/// SHELL is the control currency, and a movement there is as much a defect as
/// a NACKL imbalance.
const TOKEN_TYPE_SHELL: u32 = dodex_sdk::proof::TokenType::Shell as u32;

/// Collateral each trader splits into outcome tokens. With the deployer's two
/// 100-NACKL initial stakes, freeze normalisation leaves both clean pools at
/// 100 NACKL, so the split basket is `Q = 2` and 100 NACKL yields 50 of each
/// outcome token — several times the order size below.
const SPLIT_COLLATERAL: u128 = 100_000_000_000;

/// 30 NACKL of outcome tokens at 0.5 → 15 NACKL notional, above the contracts'
/// `MIN_ORDER_NOTIONAL_NACKL` of 10 and well inside what a split produces.
const ORDER_AMOUNT: u128 = 30_000_000_000;
const ORDER_PRICE_BPS: &str = "5000";

/// The outcome the oracle resolves to, and the one the buyer stakes on.
const WINNING_OUTCOME: u32 = 0;
const LOSING_OUTCOME: u32 = 1;

/// Slack added when waiting out a contract deadline. The gates are compared
/// against block timestamps while this test compares against the host clock,
/// and a message that arrives a second early is simply rejected — leaving the
/// following barrier to time out on a condition that never had a chance to
/// become true.
const CHAIN_CLOCK_SLACK_SECS: u64 = 5;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 30 × 2 s = 60 s, the suite's standard ceiling for one acknowledged
/// operation.
const POLL_ATTEMPTS: usize = 30;

#[tokio::test]
#[ignore = "requires a from-scratch local stand: E2E_NETWORK_ENDPOINT, E2E_SEED_NOTES, E2E_RUN_ID, E2E_MANIFEST"]
async fn proof_money_lifecycle_local() {
    let run_id = std::env::var("E2E_RUN_ID").expect("E2E_RUN_ID must be set by the bootstrapper");
    let ledger_dir = allocator::seed_dir().expect("seed notes directory");

    // One guard for the whole scenario, acquired once and held to the end.
    // Conservation is a statement about global state, so no other process may
    // land a transfer inside the measurement window. Every helper below takes
    // the guard by reference and never locks itself: a second acquisition of
    // the same file either self-deadlocks or silently downgrades this
    // exclusive hold to a shared one.
    let b0 = locks::ChainLockGuard::b0_exclusive(&ledger_dir).expect("acquire b0.lock exclusively");

    let r = chain_reader::ChainReader::new();

    // First, before anything touches the chain. The preflight asserts how the
    // stand was *generated* — code hashes against the manifest, and RootPN's
    // deploy book against the seed pool — and those equalities stop holding
    // the moment this scenario mints its first note-side movement.
    preflight::run_preflight(&r).await.expect("preflight: broken zerostate or provenance");

    let alloc = allocator::Allocator::new(&run_id).expect("open the note allocator");
    let deployer = alloc.rent(PnProfile::Dep, "proof").expect("rent the deployer note");
    let buyer = alloc.rent(PnProfile::Trd, "proof").expect("rent the buyer note");
    let seller = alloc.rent(PnProfile::Trd, "proof").expect("rent the seller note");

    let ctx = context::create_context();
    let dex = context::create_dex();

    // Phase one of the market setup: publish the oracle event. Its fee is paid
    // by the oracle, outside the measured window on purpose — see below.
    let nonce = alloc.next_nonce().expect("allocate an oracle-name nonce");
    let ev = pmp::prepare_oracle_event(&ctx, &dex, &b0, nonce).await;

    // The boundary of the money check. Everything money can reach in this
    // scenario is named here; the PMP and the order book join the set as they
    // are created, which a snapshot reads as zero until then.
    let mut tracked = TrackedContracts {
        root_pn: RootPn::DEFAULT_ADDRESS.to_string(),
        pns: vec![
            deployer.note.address.clone(),
            buyer.note.address.clone(),
            seller.note.address.clone(),
        ],
        pmps: Vec::new(),
        order_books: Vec::new(),
        oracles: vec![ev.oracle_address.clone()],
        oels: vec![ev.el_address.clone()],
        token_types: vec![TOKEN_TYPE_NACKL, TOKEN_TYPE_SHELL],
    };

    // Taken between the two setup phases, which is the whole reason the setup
    // is split in two: the PMP deployment fee then falls inside the measured
    // window, while the event-publish fee that precedes it does not.
    let sum0 = invariant::snapshot(&r, &tracked).await.expect("baseline sum snapshot");
    let phys0 = invariant::phys_snapshot(&r, &tracked).await.expect("baseline physical snapshot");

    // The manifest pins the code of contracts the zerostate placed; these two
    // were created by this run, so their provenance is checked here rather
    // than in the preflight.
    preflight::assert_raw_code(&r, &ev.oracle_address, "Oracle").await.expect("Oracle code");
    preflight::assert_raw_code(&r, &ev.el_address, "OracleEventList")
        .await
        .expect("OracleEventList code");

    // ---------------------------------------------------------------- deploy
    // Phase two: the deployer note deploys the market and funds it with two
    // initial stakes.
    let pmp_addr = pmp::deploy_pmp_with_deployer(&ctx, &dex, &deployer, &ev, &b0).await;
    tracked.pmps.push(TrackedPmp {
        addr: pmp_addr.clone(),
        token_type: TOKEN_TYPE_NACKL,
        state: PmpState::Live,
    });

    // The oracle fee and the network fee travel as physical SHELL and never
    // touch a logical balance, so the sum view is blind to them being lost.
    // This barrier polls the exact recipients until each holds exactly what it
    // is owed — the approve branch sends the fee and the approval as separate
    // messages, so the note can already be idle while the fee is still moving.
    invariant::await_quiescence(
        &r,
        &tracked,
        Phase::AfterDeploy {
            oracle: &ev.oracle_address,
            oel: &ev.el_address,
            pmp: &pmp_addr,
            oracle_fee: ORACLE_FEE,
            network_fee: NETWORK_FEE_AMOUNT,
        },
    )
    .await
    .expect("deploy quiescence");

    let phys1 =
        invariant::phys_snapshot(&r, &tracked).await.expect("physical snapshot after deploy");
    invariant::assert_phys_delta(&phys0, &phys1, &expected_deploy_delta(&deployer, &ev, &pmp_addr));
    preflight::assert_salted_code(&r, &pmp_addr, "PMP.tvc", "PrivateNote.tvc")
        .await
        .expect("PMP code is the shipped image salted with PrivateNote");

    // `acceptStake` rejects everything until the oracle sets the timings, so
    // this has to precede the staking window it defines. It moves no money of
    // its own, which is why it shares the deploy phase's assertion; the
    // approval poll is its acknowledgement.
    let requested_result_start = now_unix() + STAKE_PERIOD_PROOF;
    dex.submit_set_timings(
        &pmp_addr,
        ParamsOfSubmitSetTimings { result_start: requested_result_start },
        oracle_signer(&ev),
    )
    .await
    .expect("submit_set_timings");
    wait_pmp_approved(&dex, &pmp_addr).await;

    let s1 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after deploy");
    invariant::assert_conserved(&sum0, &s1, &ExternalDelta::none());

    // ----------------------------------------------------------------- stake
    // Both deadlines are read from the contract rather than recomputed here.
    // `stakeEnd` is derived by integer division from the timings the chain
    // actually recorded, and a second copy of that arithmetic would drift and
    // make the margin assertion below lie; `resultStart` is what the resolve
    // and split gates compare a block timestamp against. One read, so the two
    // describe the same state.
    let details = dex.get_pmp_details(&pmp_addr).await.expect("pmp details after approval");
    let stake_end = details.stake_end;
    let result_start = details.result_start;
    let key = ParamsOfStakeKey {
        event_id: details.event_id.clone(),
        oracle_list_hash: details.oracle_list_hash.clone(),
        token_type: TOKEN_TYPE_NACKL,
    };

    // Concurrent, because the window is ~30 s and two sequential
    // acknowledged stakes can eat most of it.
    tokio::join!(
        stake(&dex, &buyer, &key, WINNING_OUTCOME),
        stake(&dex, &seller, &key, LOSING_OUTCOME),
    );
    assert!(
        now_unix() + STAKE_MARGIN_SECS < stake_end,
        "staking budget exhausted: {}s left of the window, less than the {STAKE_MARGIN_SECS}s \
         margin — the stand is too slow for a {STAKE_PERIOD_PROOF}s market",
        stake_end.saturating_sub(now_unix()),
    );
    invariant::await_quiescence(&r, &tracked, Phase::AfterStake).await.expect("stake quiescence");

    // A stake moves collateral from a note into the market. Both are inside
    // the tracked set, so this still has to come out exactly even.
    let s2 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after stake");
    invariant::assert_conserved(&s1, &s2, &ExternalDelta::none());

    // ---------------------------------------------------------------- freeze
    // The market does not freeze itself when the staking window closes:
    // `freezeNow` is a public entry that anyone may call, and it is what
    // normalises the pools and deploys the order book.
    wait_until(stake_end).await;
    pmp_freeze_now(&ctx, &dex, &pmp_addr).await;
    let ob_addr = wait_order_book(&ctx, &dex, &pmp_addr).await;
    tracked.order_books.push(TrackedOb {
        addr: ob_addr.clone(),
        token_type: TOKEN_TYPE_NACKL,
        state: ObState::Live,
    });
    preflight::assert_salted_code(&r, &ob_addr, "OrderBook.tvc", "PrivateNote.tvc")
        .await
        .expect("OrderBook code is the shipped image salted with PrivateNote");

    // Normalisation refunds each pool's remainder to the deployer's note and
    // blocks split/merge until that note acknowledges it. Without this barrier
    // the split below reverts with `ERR_NORM_REFUND_PENDING` — the scenario
    // breaks, not just its money check.
    invariant::await_quiescence(&r, &tracked, Phase::AfterFreeze { pmp: &pmp_addr })
        .await
        .expect("freeze quiescence");

    let s3 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after freeze");
    invariant::assert_conserved(&s2, &s3, &ExternalDelta::none());

    // ----------------------------------------------------------------- split
    // Collateral in, a full set of outcome tokens out. The traders need the
    // tokens before they can put either side of a trade on the book.
    split_full_set(&dex, &buyer, &key).await;
    split_full_set(&dex, &seller, &key).await;
    invariant::await_quiescence(&r, &tracked, Phase::AfterSplit).await.expect("split quiescence");

    let s4 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after split");
    invariant::assert_conserved(&s3, &s4, &ExternalDelta::none());

    // ----------------------------------------------------------------- trade
    // A resting sell crossed by a buy at the same price. The fee the taker
    // pays splits into a rebate for the maker and a protocol fee the book
    // accrues until it drains — both inside the tracked set.
    let coid_base = u128::from(alloc.next_nonce().expect("allocate client order ids"));
    place_and_match(&dex, &buyer, &seller, &key, &ob_addr, coid_base).await;

    // Both orders filled completely, so nothing is left resting; the barrier
    // states that number rather than skipping the check on the phase that
    // says the most about the book.
    invariant::await_quiescence(&r, &tracked, Phase::AfterTrade { expected_open_orders: 0 })
        .await
        .expect("trade quiescence");

    let s5 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after trade");
    invariant::assert_conserved(&s4, &s5, &ExternalDelta::none());

    // --------------------------------------------------------------- resolve
    // Baselines first: the resolve barrier asserts exact equalities, and each
    // of these is about to be moved by the very action it measures. Read after
    // the fact they would describe the outcome and prove nothing.
    let deployer_bal_before = pn_balance(&r, &deployer.note.address).await;
    let rootpn_fee_before = root_pn_protocol_fee(&r).await;
    // Read before the drain forwards it to RootPN and zeroes the book's own
    // counter.
    let expected_protocol_fee = ob_total_protocol_fees(&dex, &ob_addr).await;

    wait_until(result_start).await;
    dex.submit_resolve(
        &pmp_addr,
        ParamsOfSubmitResolve { outcome_id: WINNING_OUTCOME },
        oracle_signer(&ev),
    )
    .await
    .expect("submit_resolve");
    wait_resolved(&dex, &pmp_addr, WINNING_OUTCOME).await;
    // Resolving triggers the order book's shutdown; the drain cancels and
    // refunds whatever rests, hands its protocol fees to RootPN, and destroys
    // the book in the same message that reports completion. `claim` is gated
    // on that report.
    wait_order_book_done(&dex, &pmp_addr).await;
    // Declared drained only now, and only because the report has arrived: a
    // snapshot or barrier would otherwise keep reading an account that no
    // longer exists, and counting fees RootPN has already been credited with.
    mark_ob_drained(&mut tracked, &ob_addr);

    // The creator fee is computed by `resolve` from the live pools, so unlike
    // the baselines above it can only be read afterwards.
    let creator_fee = read_creator_fee(&dex, &pmp_addr).await;
    invariant::await_quiescence(
        &r,
        &tracked,
        Phase::AfterResolve {
            pmp: &pmp_addr,
            deployer: &deployer.note.address,
            deployer_balance_before: deployer_bal_before,
            creator_fee,
            root_pn_fee_before: rootpn_fee_before,
            expected_protocol_fee,
        },
    )
    .await
    .expect("resolve quiescence");

    let s6 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after resolve");
    invariant::assert_conserved(&s5, &s6, &ExternalDelta::none());

    // ---------------------------------------------------------------- claims
    // All three winners, the deployer included. Its initial stakes are part of
    // the winning mass, and the market only closes once that mass has been
    // claimed down to what forfeiters left behind — skip this claim and the
    // PMP never self-destructs and the barrier below times out.
    claim(&dex, &buyer, &key).await;
    claim(&dex, &seller, &key).await;
    claim(&dex, &deployer, &key).await;

    // The PMP's account can be gone while its residual transfer is still in
    // flight, so "the market disappeared" proves nothing on its own. This
    // barrier waits for the money: it re-snapshots with the market declared
    // dead and holds until conservation against `s6` closes exactly.
    invariant::await_quiescence(
        &r,
        &tracked,
        Phase::AfterClaims { pmp: &pmp_addr, residual_to: &deployer.note.address, baseline: &s6 },
    )
    .await
    .expect("claims quiescence");

    // Only now, with the account confirmed gone and its residual confirmed
    // delivered. Declaring it earlier would drop the market out of the tracked
    // sum while it still held money, and conservation would "pass".
    mark_pmp_self_destructed(&mut tracked, &pmp_addr, &deployer.note.address);

    let s7 = invariant::snapshot(&r, &tracked).await.expect("sum snapshot after claims");
    invariant::assert_conserved(&s6, &s7, &ExternalDelta::none());

    // Hand the notes back only after the assertions have run: a release
    // rewrites the note's ledger entry, and a note that fails its sweep is
    // quarantined rather than silently reused.
    buyer.release_clean(&r).await.expect("release the buyer note");
    seller.release_clean(&r).await.expect("release the seller note");
    deployer.release_clean(&r).await.expect("release the deployer note");
}

// ---------------------------------------------------------------------------
// Tracked-set transitions
// ---------------------------------------------------------------------------

/// Declare `ob`'s protocol fees forwarded to RootPN and its account gone.
/// Called only once the book has reported its shutdown complete — see the call
/// site for why the ordering matters.
fn mark_ob_drained(tracked: &mut TrackedContracts, ob: &str) {
    let entry = tracked
        .order_books
        .iter_mut()
        .find(|o| o.addr == ob)
        .unwrap_or_else(|| panic!("order book {ob} is not in the tracked set"));
    entry.state = ObState::Drained;
}

/// Declare `pmp` self-destructed, with its residual delivered to
/// `residual_to`. The recipient has to be tracked as well, or the money really
/// does leave the observed set.
fn mark_pmp_self_destructed(tracked: &mut TrackedContracts, pmp: &str, residual_to: &str) {
    let entry = tracked
        .pmps
        .iter_mut()
        .find(|p| p.addr == pmp)
        .unwrap_or_else(|| panic!("PMP {pmp} is not in the tracked set"));
    entry.state = PmpState::SelfDestructed { residual_to: residual_to.to_string() };
}

/// What the deploy moves physically: the deployer note pays the oracle fee and
/// the network fee in ECC SHELL, the oracle receives the first, the market
/// receives the second, and the event list is left untouched. Stated as a
/// closed set so it sums to zero — an expectation that does not is rejected as
/// a bug in the test rather than compared account by account.
fn expected_deploy_delta(
    deployer: &LeasedPn,
    ev: &OracleEventCtx,
    pmp_addr: &str,
) -> ExpectedPhysDelta {
    BTreeMap::from([
        (
            (deployer.note.address.clone(), CURRENCY_ID_SHELL),
            -signed(ORACLE_FEE + NETWORK_FEE_AMOUNT),
        ),
        ((ev.oracle_address.clone(), CURRENCY_ID_SHELL), signed(ORACLE_FEE)),
        ((ev.el_address.clone(), CURRENCY_ID_SHELL), 0),
        ((pmp_addr.to_string(), CURRENCY_ID_SHELL), signed(NETWORK_FEE_AMOUNT)),
    ])
}

fn signed(v: u128) -> i128 {
    i128::try_from(v).unwrap_or_else(|_| panic!("amount {v} does not fit in i128"))
}

// ---------------------------------------------------------------------------
// Chain operations
// ---------------------------------------------------------------------------

/// The oracle's own keys — `submitSetTimings` and `submitResolve` are votes,
/// authenticated against the oracle set the market was deployed with.
fn oracle_signer(ev: &OracleEventCtx) -> Signer {
    Signer::Keys { keys: ev.oracle_keys.clone() }
}

/// Stake [`STAKE_AMOUNT`] on `outcome` and wait for the market to acknowledge
/// it. A note refuses a second operation while the first is in flight, so the
/// acknowledgement is a precondition of whatever the scenario does next, not
/// only of the barrier.
async fn stake(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey, outcome: u32) {
    dex.set_stake(
        &note.note.address,
        ParamsOfSetStake {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            outcome,
            amount: STAKE_AMOUNT,
            use_coupon: false,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("set_stake");
    wait_not_busy(dex, &note.note.address, "set_stake").await;
}

/// Freeze the market now that the staking window has closed. Unsigned: the
/// entry point checks the deadline and the market's state, never a caller
/// identity.
///
/// Re-sent while the market is still unfrozen, because the deadline is checked
/// against a block timestamp: a message that arrives a moment early is
/// rejected before the contract accepts it, and an external message that is
/// rejected leaves no trace at the sender — sending is fire-and-forget, so
/// nothing here can observe the rejection and a single attempt would leave the
/// scenario waiting for an order book nobody is going to deploy. The state is
/// re-read before every attempt so a market that is already frozen is never
/// sent to at all.
async fn pmp_freeze_now(ctx: &Arc<ClientContext>, dex: &Dex, pmp_addr: &str) {
    let contract = Pmp::new(Arc::clone(ctx), dex_contract_params(pmp_addr));
    for _ in 0..POLL_ATTEMPTS {
        if dex.get_pmp_details(pmp_addr).await.expect("pmp details").frozen {
            return;
        }
        contract.freeze_now(Signer::None).await.expect("freeze_now");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("PMP {pmp_addr} did not freeze within {}s", poll_budget_secs());
}

/// Split collateral into a full set of outcome tokens. Only legal between the
/// freeze and `resultStart`, and only once the deployer note has acknowledged
/// the normalisation refund.
async fn split_full_set(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey) {
    dex.split_full_set(
        &note.note.address,
        ParamsOfSplitFullSet {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            collateral: SPLIT_COLLATERAL,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("split_full_set");
    wait_not_busy(dex, &note.note.address, "split_full_set").await;
}

/// One trade: the seller rests a limit sell, the buyer crosses it at the same
/// price. The maker has to be on the book before the taker arrives, or the buy
/// rests instead of matching and the trade never happens.
///
/// `coid_base` comes from the ledger's nonce counter rather than the clock:
/// client order ids are unique per note only while the order is live, and a
/// nonce cannot collide with a concurrent run the way a same-second timestamp
/// can.
async fn place_and_match(
    dex: &Dex,
    buyer: &LeasedPn,
    seller: &LeasedPn,
    key: &ParamsOfStakeKey,
    ob_addr: &str,
    coid_base: u128,
) {
    let sell_coid = coid_base;
    let buy_coid = coid_base + 1;

    place_limit(dex, seller, key, false, sell_coid).await;
    wait_owner_order(dex, ob_addr, &seller.note.dih_dec, sell_coid, true).await;

    place_limit(dex, buyer, key, true, buy_coid).await;
    // Gone from the owner index on both sides is what proves a fill rather
    // than two orders resting past each other.
    wait_owner_order(dex, ob_addr, &seller.note.dih_dec, sell_coid, false).await;
    wait_owner_order(dex, ob_addr, &buyer.note.dih_dec, buy_coid, false).await;
}

async fn place_limit(
    dex: &Dex,
    note: &LeasedPn,
    key: &ParamsOfStakeKey,
    is_buy: bool,
    client_order_id: u128,
) {
    dex.place_order(
        &note.note.address,
        ParamsOfPlaceOrder {
            event_id: key.event_id.clone(),
            oracle_list_hash: key.oracle_list_hash.clone(),
            token_type: key.token_type,
            outcome_id: WINNING_OUTCOME,
            is_buy,
            price: ORDER_PRICE_BPS.to_string(),
            amount: ORDER_AMOUNT,
            flags: 0,
            min_amount: 0,
            epoch_id: 0,
            client_order_id,
        },
        Signer::Keys { keys: note.note.keys.clone() },
    )
    .await
    .expect("place_order");
}

/// Claim the note's winnings. Accepted only once the order book has finished
/// draining, and it is what decays the market's winning pool toward the
/// threshold at which it closes itself.
async fn claim(dex: &Dex, note: &LeasedPn, key: &ParamsOfStakeKey) {
    dex.claim(&note.note.address, key.clone(), Signer::Keys { keys: note.note.keys.clone() })
        .await
        .expect("claim");
    wait_not_busy(dex, &note.note.address, "claim").await;
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// Sleep until `deadline` has passed on the chain's side of the clock. See
/// [`CHAIN_CLOCK_SLACK_SECS`] for why the host clock alone is not enough.
async fn wait_until(deadline_unix: u64) {
    let target = deadline_unix + CHAIN_CLOCK_SLACK_SECS;
    let now = now_unix();
    if now < target {
        tokio::time::sleep(Duration::from_secs(target - now)).await;
    }
}

async fn wait_pmp_approved(dex: &Dex, pmp_addr: &str) {
    poll_until(&format!("PMP {pmp_addr} did not become approved"), || async {
        dex.get_pmp_details(pmp_addr).await.expect("pmp details").approved
    })
    .await;
}

/// Wait for the order book the freeze deploys, and return its address. The
/// address is derived deterministically and the getter answers with it long
/// before the account exists, so the wait is on the account being active, not
/// on the getter answering.
async fn wait_order_book(ctx: &Arc<ClientContext>, dex: &Dex, pmp_addr: &str) -> String {
    let ob_addr = dex.get_order_book_address(pmp_addr).await.expect("get_order_book_address");
    let contract = OrderBook::new(Arc::clone(ctx), dex_contract_params(&ob_addr));
    wait_active(&contract, "OrderBook").await;
    ob_addr
}

async fn wait_resolved(dex: &Dex, pmp_addr: &str, outcome_id: u32) {
    poll_until(&format!("PMP {pmp_addr} did not resolve to outcome {outcome_id}"), || async {
        dex.get_pmp_details(pmp_addr).await.expect("pmp details").resolved_outcome
            == Some(outcome_id)
    })
    .await;
}

async fn wait_order_book_done(dex: &Dex, pmp_addr: &str) {
    poll_until(&format!("order book of PMP {pmp_addr} did not finish draining"), || async {
        dex.get_pmp_shutdown_state(pmp_addr).await.expect("pmp shutdown state").order_book_done
    })
    .await;
}

/// Wait for a client order id to appear in (or disappear from) the book's
/// owner index.
async fn wait_owner_order(
    dex: &Dex,
    ob_addr: &str,
    deposit_identifier_hash: &str,
    client_order_id: u128,
    want_present: bool,
) {
    let what = if want_present { "appear in" } else { "disappear from" };
    poll_until(
        &format!("order {client_order_id} of {deposit_identifier_hash} did not {what} the book"),
        || async {
            let owned = dex
                .get_orders_by_owner(ob_addr, deposit_identifier_hash.to_string())
                .await
                .expect("get_orders_by_owner");
            owned.orders.iter().any(|o| o.client_order_id == client_order_id) == want_present
        },
    )
    .await;
}

/// Wait until an operation sent to `pn_address` has been acknowledged.
///
/// Two phases, because the note's `_busy` marker is only set once the chain
/// picks the external message up: a caller that checks straight away sees the
/// pre-message `None` and concludes the operation is finished before it has
/// even started. Phase one waits for the marker to appear, phase two for it to
/// clear. Never observing it is treated as "already done" — the operation can
/// legitimately complete between two polls — since every phase barrier
/// re-checks the same marker before its snapshot.
async fn wait_not_busy(dex: &Dex, pn_address: &str, op: &str) {
    let mut saw_busy = false;
    for _ in 0..POLL_ATTEMPTS {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let d = dex.get_private_note_details(pn_address).await.expect("pn details");
        if d.busy_address.is_some() {
            saw_busy = true;
            break;
        }
    }
    if !saw_busy {
        return;
    }
    poll_until(&format!("note {pn_address} stayed busy after {op}"), || async {
        dex.get_private_note_details(pn_address).await.expect("pn details").busy_address.is_none()
    })
    .await;
}

/// Poll `probe` until it answers `true`, or panic after [`POLL_ATTEMPTS`]
/// tries. `what` is phrased as the failure, so the panic reads as a statement
/// about what never happened.
async fn poll_until<F, Fut>(what: &str, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..POLL_ATTEMPTS {
        if probe().await {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("{what} within {}s", poll_budget_secs());
}

fn poll_budget_secs() -> u64 {
    POLL_ATTEMPTS as u64 * POLL_INTERVAL.as_secs()
}

// ---------------------------------------------------------------------------
// Baseline readers
// ---------------------------------------------------------------------------

/// The market's creator fee, which `resolve` computes from the live pools.
async fn read_creator_fee(dex: &Dex, pmp_addr: &str) -> u128 {
    dex.get_pmp_details(pmp_addr).await.expect("pmp details").creator_fee
}

/// A note's free NACKL balance — the same `_balance` field the resolve barrier
/// compares against, so baseline and barrier describe the same quantity.
async fn pn_balance(r: &ChainReader, pn_address: &str) -> u128 {
    pn_nackl(&r.dex.get_private_note_details(pn_address).await.expect("pn details"))
}

/// RootPN's accrued NACKL protocol fee, where the order book's fees land once
/// it drains.
async fn root_pn_protocol_fee(r: &ChainReader) -> u128 {
    RootPn::new(Arc::clone(&r.ctx), dex_contract_params(RootPn::DEFAULT_ADDRESS))
        .get_protocol_fee(ParamsOfGetProtocolFee { token_type: TOKEN_TYPE_NACKL })
        .await
        .expect("RootPN getProtocolFee")
        .value
}

/// Fees the order book has taken and not yet handed over. Meaningful only
/// before the drain, which forwards them and zeroes the counter.
async fn ob_total_protocol_fees(dex: &Dex, ob_addr: &str) -> u128 {
    dex.get_order_book_details(ob_addr).await.expect("order book details").total_protocol_fees
}
