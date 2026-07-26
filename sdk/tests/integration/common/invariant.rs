//! B0 invariant helper: exact per-currency conservation over a declared set of
//! contracts, per-account physical (ECC) deltas, and the quiescence barriers
//! that make a snapshot meaningful.
//!
//! Two independent views of the same scenario:
//!
//! - **Σ mode** ([`snapshot`] + [`assert_conserved`]) sums the *logical*
//!   balances the contracts keep in storage — PN `_balance` +
//!   `_lockedInOrders`, PMP unclaimed balance, RootPN protocol fees, order-book
//!   protocol fees — and requires the total per currency to come out unchanged.
//! - **Physical mode** ([`phys_snapshot`] + [`assert_phys_delta`]) reads the
//!   account's extra-currency (ECC) balance, which is where oracle and network
//!   fees actually travel. Those fees never touch a logical field, so the Σ
//!   view cannot see them being lost at all.
//!
//! Three properties are deliberate and must survive future edits:
//!
//! - **Exact integer equality, never a tolerance.** Catching the off-by-one is
//!   the entire purpose; a tolerance introduced to "fix a flake" silently
//!   deletes the check. Everything that legitimately crosses the boundary of
//!   the tracked set is declared by the scenario as an [`ExternalDelta`] — the
//!   snapshot never guesses.
//! - **Per currency, separately.** A PMP and an order book each hold exactly
//!   one currency, so each contributes to exactly one bucket. Without that
//!   binding a NACKL pool would also land in the SHELL total, and the first
//!   stake assertion would fail for a reason unrelated to the code under test.
//! - **Quiescence is a precondition of a snapshot.** The chain is
//!   asynchronous: a getter answers from committed state while the messages
//!   that finish the operation are still in flight, so a snapshot taken inside
//!   that window compares against a state that has not settled.
//!   [`await_quiescence`] is that barrier, and every phase settles by a
//!   different criterion — which is why [`Phase`] carries data instead of
//!   being a bare enum.
//!
//! Accuracy of Σ mode additionally assumes exclusive control of global state
//! (the scenario holds `b0.lock` for the whole comparison window): a
//! concurrent actor mutating RootPN would show up here as a leak.

// The scenario tests that consume this module land in later work; until then
// most of the surface has no caller inside the test binary.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use dodex_contracts::dex::order_book::OrderBook;
use dodex_contracts::dex::pmp::Pmp;
use dodex_contracts::dex::root_pn::ParamsOfGetProtocolFee;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_infrastructure::tvm_runner::account_boc_is_none;
use dodex_infrastructure::tvm_runner::decode_account_fields_json;
use dodex_sdk::dex_contract_params;
use serde_json::Value;

use crate::common::chain_reader::ChainReader;
use crate::common::context::CURRENCY_ID_SHELL;

/// ABIs are read from the repo's `contracts/` sources — the same files the
/// deployed bytecode is compiled from — so a storage-layout change surfaces
/// here instead of decoding into silently wrong numbers.
const PN_ABI: &str = include_str!("../../../../contracts/dex/PrivateNote.abi.json");
const PMP_ABI: &str = include_str!("../../../../contracts/dex/PMP.abi.json");

// Storage fields read straight off an account BOC. Named once so the
// ABI-pinning test can prove they still exist in the contract.
const PN_BALANCE: &str = "_balance";
const PN_LOCKED_IN_ORDERS: &str = "_lockedInOrders";
const PN_BUSY: &str = "_busy";
const PN_OPEN_ORDER_COUNT: &str = "_openOrderCount";
const PMP_NORM_REFUND_PENDING: &str = "_normRefundPending";

const QUIESCENCE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether a PMP is still holding funds of its own. `SelfDestructed` names the
/// account its residual balance was forwarded to — the deployer PN on a
/// win/residual close, RootPN on a reject/bounce — which must itself be
/// tracked, otherwise the money genuinely leaves the observed set.
#[derive(Debug, Clone)]
pub enum PmpState {
    Live,
    SelfDestructed { residual_to: String },
}

/// Whether an order book still holds protocol fees. After the drain they have
/// been forwarded to RootPN via `collectProtocolFee`, so continuing to add
/// `totalProtocolFees` would count the same money twice.
#[derive(Debug, Clone)]
pub enum ObState {
    Live,
    Drained,
}

/// A PMP is a single-currency contract (`PMP._tokenType`); the tracked
/// `token_type` is what binds its balance to one bucket of the Σ view.
#[derive(Debug, Clone)]
pub struct TrackedPmp {
    pub addr: String,
    pub token_type: u32,
    pub state: PmpState,
}

/// An order book is single-currency too (`OrderBook._tokenType`).
#[derive(Debug, Clone)]
pub struct TrackedOb {
    pub addr: String,
    pub token_type: u32,
    pub state: ObState,
}

/// Everything a scenario declares as "inside the boundary". Roles are typed
/// rather than flattened into one address list because each role is read
/// differently and contributes differently.
#[derive(Debug, Clone)]
pub struct TrackedContracts {
    pub root_pn: String,
    pub pns: Vec<String>,
    pub pmps: Vec<TrackedPmp>,
    pub order_books: Vec<TrackedOb>,
    /// Final recipient of the oracle fee — physical mode only.
    pub oracles: Vec<String>,
    pub oels: Vec<String>,
    /// Conservation is checked for each of these separately.
    pub token_types: Vec<u32>,
}

/// Logical balances at one instant: token type → account → amount. Amounts are
/// `i128` so a difference is expressible without a second type.
#[derive(Debug, Clone)]
pub struct InvariantSnapshot {
    pub per_tt: BTreeMap<u32, BTreeMap<String, i128>>,
}

/// Movements the scenario declares as legitimately crossing the boundary of
/// the tracked set (an outside deposit, a withdrawal). Keyed by
/// `(account, token_type)` so the declaration says which account leaked and
/// where to look when it turns out to be wrong.
#[derive(Debug, Clone, Default)]
pub struct ExternalDelta(pub BTreeMap<(String, u32), i128>);

impl ExternalDelta {
    /// The base case: nothing crosses the boundary, so every currency must
    /// come out exactly even.
    pub fn none() -> Self {
        ExternalDelta(BTreeMap::new())
    }
}

/// Physical (extra-currency) balances at one instant: account → currency →
/// amount. Native grams are deliberately absent: gas is burned, not conserved.
#[derive(Debug, Clone)]
pub struct PhysicalSnapshot {
    pub per_addr: BTreeMap<String, BTreeMap<u32, u128>>,
}

/// Per-account physical movement the scenario expects, keyed by
/// `(account, currency)`.
pub type ExpectedPhysDelta = BTreeMap<(String, u32), i128>;

/// What "settled" means for the phase a snapshot is about to be taken after.
/// Each variant carries the data its barrier needs, because a generic "wait for
/// silence" cannot see money that is still in flight: the sender is already
/// decremented and the recipient is not yet credited, and a snapshot inside
/// that window fails conservation for a reason that is not a defect.
// The shared `After` prefix is the point: a call site reads as the moment in
// the scenario the barrier is placed at, not as a condition to look up.
#[allow(clippy::enum_variant_names)]
pub enum Phase<'a> {
    /// The approve branch of `OracleEventList` sends the oracle its fee and
    /// the PMP its approval as two independent messages, so the PN can go idle
    /// while the fee is still travelling. Amounts are absolute: all three
    /// accounts start this phase from a zero ECC balance.
    AfterDeploy {
        oracle: &'a str,
        oel: &'a str,
        pmp: &'a str,
        oracle_fee: u128,
        network_fee: u128,
    },
    AfterStake,
    /// Freeze normalisation refunds the notes and sets `_normRefundPending`;
    /// until the note acknowledges, the next `splitFullSet` reverts with
    /// `ERR_NORM_REFUND_PENDING`. Without this barrier the scenario itself
    /// breaks, not just its money check.
    AfterFreeze {
        pmp: &'a str,
    },
    AfterSplit,
    /// The one phase where orders legitimately rest in the book — which is
    /// exactly why it states the count instead of opting out of the check.
    /// Skipping it here would drop the open-order comparison from the phase
    /// that carries the most information about the book, and a default would
    /// let a scenario inherit an expectation it never thought about.
    AfterTrade {
        expected_open_orders: u32,
    },
    /// `resolve` decrements the pool and sends the creator fee asynchronously,
    /// and the order-book drain forwards protocol fees to RootPN on its own
    /// schedule. Both are checked as exact equalities against baselines read
    /// before resolve — a bare "it grew" would accept the wrong amount.
    /// `expected_protocol_fee` is `OrderBook.totalProtocolFees` read before
    /// resolve.
    AfterResolve {
        pmp: &'a str,
        deployer: &'a str,
        deployer_balance_before: u128,
        creator_fee: u128,
        root_pn_fee_before: u128,
        expected_protocol_fee: u128,
    },
    /// The PMP account can already be gone while its residual transfer is
    /// still in flight, so "the PMP disappeared" proves nothing on its own.
    /// This barrier additionally waits until conservation against `baseline`
    /// closes exactly — i.e. the residual has been *delivered*. That is a
    /// predicate, not an assertion: an intermediate mismatch here is the
    /// expected state, and the scenario's own [`assert_conserved`] afterwards
    /// merely restates the same fact outside the loop.
    AfterClaims {
        pmp: &'a str,
        residual_to: &'a str,
        baseline: &'a InvariantSnapshot,
    },
}

/// Accumulate `v` for `addr` inside `tt`'s bucket, never any other. Split out
/// of [`snapshot`] so the currency binding is provable without a network.
pub fn credit(per_tt: &mut BTreeMap<u32, BTreeMap<String, i128>>, tt: u32, addr: &str, v: i128) {
    *per_tt.entry(tt).or_default().entry(addr.to_string()).or_default() += v;
}

/// PMPs that still hold funds of their own. A self-destructed one is skipped
/// because its residual is already counted at the account that received it.
pub fn live_pmps(t: &TrackedContracts) -> impl Iterator<Item = &TrackedPmp> {
    t.pmps.iter().filter(|p| matches!(p.state, PmpState::Live))
}

/// Order books that still hold protocol fees. A drained one is skipped because
/// its fees are already counted inside `RootPN._protocolFees`.
pub fn live_order_books(t: &TrackedContracts) -> impl Iterator<Item = &TrackedOb> {
    t.order_books.iter().filter(|o| matches!(o.state, ObState::Live))
}

/// Sum the logical balances of the tracked set. Quiescence is a precondition:
/// call [`await_quiescence`] first, or the numbers describe a state that is
/// still moving.
///
/// An account that is not on chain — not deployed yet, or already
/// self-destructed — contributes **zero**, not an error. That is the correct
/// value, not a leniency: an account holding no state holds no money, and a
/// scenario legitimately takes its first snapshot before the PMP exists so the
/// delta is measured from nothing. It also keeps the two modes agreeing:
/// [`phys_snapshot`] already reads zero for the same account.
///
/// This does mean a typo'd address reads as a silent zero here. Catching that
/// is not this function's job and it is covered twice over: the stand's
/// preflight verifies the deployed code hashes of the known contracts, and a
/// wrong address breaks [`assert_phys_delta`], whose per-account expectation
/// must itself sum to zero. Do not re-introduce strictness here thinking the
/// protection is missing.
pub async fn snapshot(r: &ChainReader, t: &TrackedContracts) -> anyhow::Result<InvariantSnapshot> {
    // A residual sent outside the tracked set is money the Σ view can never
    // account for, so reject the declaration before reading anything.
    for pmp in &t.pmps {
        if let PmpState::SelfDestructed { residual_to } = &pmp.state {
            assert!(
                *residual_to == t.root_pn || t.pns.iter().any(|pn| pn == residual_to),
                "PMP {} declares its residual went to {residual_to}, which is not in the tracked \
                 set — conservation could not close even if the contracts were correct",
                pmp.addr
            );
        }
    }

    let mut per_tt: BTreeMap<u32, BTreeMap<String, i128>> = BTreeMap::new();
    for tt in &t.token_types {
        per_tt.entry(*tt).or_default();
    }

    for pn in &t.pns {
        let fields = pn_fields_opt(r, pn).await?;
        for (tt, held) in pn_holdings(fields.as_ref(), &t.token_types)? {
            credit(&mut per_tt, tt, pn, as_i128(held)?);
        }
    }

    // RootPN accrues the protocol fee per currency, so this is one call per
    // tracked token type rather than one call in total. Existence is checked
    // once, before the getters: a getter against a missing account fails, and
    // the failure would say nothing useful about which account it was.
    if !account_absent(r, &t.root_pn).await? {
        let root = RootPn::new(r.ctx.clone(), dex_contract_params(&t.root_pn));
        for tt in &t.token_types {
            let fee = root
                .get_protocol_fee(ParamsOfGetProtocolFee { token_type: *tt })
                .await
                .map_err(|e| anyhow!("RootPN {} getProtocolFee(tokenType={tt}): {e}", t.root_pn))?
                .value;
            credit(&mut per_tt, *tt, &t.root_pn, as_i128(fee)?);
        }
    } else {
        for tt in &t.token_types {
            credit(&mut per_tt, *tt, &t.root_pn, 0);
        }
    }

    for pmp in live_pmps(t) {
        if account_absent(r, &pmp.addr).await? {
            credit(&mut per_tt, pmp.token_type, &pmp.addr, 0);
            continue;
        }
        let contract = Pmp::new(r.ctx.clone(), dex_contract_params(&pmp.addr));
        let details = contract
            .get_details()
            .await
            .map_err(|e| anyhow!("PMP {} getDetails: {e}", pmp.addr))?;
        assert_token_type("PMP", &pmp.addr, details.token_type, pmp.token_type);
        let unclaimed = contract
            .get_unclaimed_balance()
            .await
            .map_err(|e| anyhow!("PMP {} getUnclaimedBalance: {e}", pmp.addr))?
            .value;
        credit(&mut per_tt, pmp.token_type, &pmp.addr, as_i128(unclaimed)?);
    }

    for ob in live_order_books(t) {
        if account_absent(r, &ob.addr).await? {
            credit(&mut per_tt, ob.token_type, &ob.addr, 0);
            continue;
        }
        let details = OrderBook::new(r.ctx.clone(), dex_contract_params(&ob.addr))
            .get_details()
            .await
            .map_err(|e| anyhow!("OrderBook {} getDetails: {e}", ob.addr))?;
        assert_token_type("OrderBook", &ob.addr, details.token_type, ob.token_type);
        credit(&mut per_tt, ob.token_type, &ob.addr, as_i128(details.total_protocol_fees)?);
    }

    Ok(InvariantSnapshot { per_tt })
}

/// Per-currency imbalance of `after` against `before`, net of what the
/// scenario declared as external. An empty map means every tracked currency
/// conserved exactly.
///
/// This is the non-panicking core: the `AfterClaims` barrier polls it while a
/// residual transfer is still in flight, where a mismatch is the expected
/// intermediate state and a panic would abort on the first iteration.
pub fn conserved_diff(
    before: &InvariantSnapshot,
    after: &InvariantSnapshot,
    external: &ExternalDelta,
) -> BTreeMap<u32, i128> {
    let mut token_types: BTreeSet<u32> = BTreeSet::new();
    token_types.extend(before.per_tt.keys().copied());
    token_types.extend(after.per_tt.keys().copied());
    token_types.extend(external.0.keys().map(|(_, tt)| *tt));

    let mut out = BTreeMap::new();
    for tt in token_types {
        let moved = sum_tt(after, tt) - sum_tt(before, tt);
        let declared: i128 =
            external.0.iter().filter(|((_, k), _)| *k == tt).map(|(_, v)| *v).sum();
        let imbalance = moved - declared;
        if imbalance != 0 {
            out.insert(tt, imbalance);
        }
    }
    out
}

/// Assertion wrapper over [`conserved_diff`]: panics with the offending token
/// type and the per-account breakdown that produced it.
pub fn assert_conserved(
    before: &InvariantSnapshot,
    after: &InvariantSnapshot,
    external: &ExternalDelta,
) {
    let diff = conserved_diff(before, after, external);
    if diff.is_empty() {
        return;
    }

    let mut msg = String::from("conservation violated (exact per-currency equality is required)");
    for (tt, imbalance) in &diff {
        let _ = write!(msg, "\n  tt={tt}: imbalance {imbalance}");
        let empty = BTreeMap::new();
        let b = before.per_tt.get(tt).unwrap_or(&empty);
        let a = after.per_tt.get(tt).unwrap_or(&empty);
        let mut addrs: BTreeSet<&str> = BTreeSet::new();
        addrs.extend(b.keys().map(String::as_str));
        addrs.extend(a.keys().map(String::as_str));
        for addr in addrs {
            let from = b.get(addr).copied().unwrap_or(0);
            let to = a.get(addr).copied().unwrap_or(0);
            if from != to {
                let _ = write!(msg, "\n    {addr}: {from} -> {to} (delta {})", to - from);
            }
        }
        for ((addr, k), v) in &external.0 {
            if k == tt {
                let _ = write!(msg, "\n    declared external {addr}: {v}");
            }
        }
    }
    panic!("{msg}");
}

/// Read the physical (extra-currency) balance of every tracked account.
///
/// Unlike the Σ view this keeps dead contracts: the delta of a self-destructed
/// PMP is a meaningful `-balance` the scenario asserts, whereas adding its
/// balance into a currency total would double-count funds already credited to
/// the recipient.
pub async fn phys_snapshot(
    r: &ChainReader,
    t: &TrackedContracts,
) -> anyhow::Result<PhysicalSnapshot> {
    let mut per_addr = BTreeMap::new();
    for addr in tracked_addresses(t) {
        let ecc = r
            .account_ecc(&addr)
            .await
            .with_context(|| format!("read physical balance of {addr}"))?
            .ecc;
        per_addr.insert(addr, ecc);
    }
    Ok(PhysicalSnapshot { per_addr })
}

/// Compare the physical movement against what the scenario declared, exactly,
/// account by account.
///
/// The expectation is first required to conserve per currency on its own: an
/// expectation that does not add up to zero is a bug in the test, and saying so
/// directly is far clearer than the mismatch report it would otherwise produce.
///
/// Comparison runs over the union of the keys of `before`, `after` and
/// `expected`; a pair missing on one side reads as zero. That is not defensive
/// padding — a scenario snapshots before the PMP is deployed, so that account
/// legitimately exists only in `after`.
pub fn assert_phys_delta(
    before: &PhysicalSnapshot,
    after: &PhysicalSnapshot,
    expected: &ExpectedPhysDelta,
) {
    let mut expected_per_tt: BTreeMap<u32, i128> = BTreeMap::new();
    for ((_, tt), v) in expected {
        *expected_per_tt.entry(*tt).or_default() += v;
    }
    let unbalanced: Vec<String> = expected_per_tt
        .iter()
        .filter(|(_, sum)| **sum != 0)
        .map(|(tt, sum)| format!("tt={tt}: {sum}"))
        .collect();
    assert!(
        unbalanced.is_empty(),
        "expected physical deltas do not themselves conserve: {}",
        unbalanced.join(", ")
    );

    let mut keys: BTreeSet<(&str, u32)> = BTreeSet::new();
    for snap in [before, after] {
        for (addr, per_tt) in &snap.per_addr {
            keys.extend(per_tt.keys().map(|tt| (addr.as_str(), *tt)));
        }
    }
    keys.extend(expected.keys().map(|(addr, tt)| (addr.as_str(), *tt)));

    let mut mismatches: Vec<String> = Vec::new();
    for (addr, tt) in keys {
        let from = phys_at(before, addr, tt);
        let to = phys_at(after, addr, tt);
        // Not an `as` cast: a wrapped balance would turn a loss into a gain,
        // which is the one failure this module must never report as success.
        let actual = signed(to) - signed(from);
        let want = expected.get(&(addr.to_string(), tt)).copied().unwrap_or(0);
        if actual != want {
            mismatches.push(format!(
                "{addr} tt={tt}: expected delta {want}, actual {actual} ({from} -> {to})"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "physical ECC deltas differ from the declared expectation:\n  {}",
        mismatches.join("\n  ")
    );
}

/// Block until the tracked set has settled for `phase`, or fail after
/// [`QUIESCENCE_TIMEOUT`].
///
/// An account that does not exist yet is a *pending* condition, not a failure:
/// a tracked contract is routinely deployed part-way through a phase, and
/// waiting is precisely what fixes it. Every other read failure aborts
/// immediately — a getter that errors means the wrong address or the wrong
/// ABI, which waiting cannot fix. A condition that is merely not met yet is
/// polled and, on timeout, reported verbatim, including the numbers that never
/// converged.
pub async fn await_quiescence(
    r: &ChainReader,
    t: &TrackedContracts,
    phase: Phase<'_>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + QUIESCENCE_TIMEOUT;
    loop {
        let pending = match base_pending(r, t, &phase).await? {
            Some(reason) => Some(reason),
            None => phase_pending(r, t, &phase).await?,
        };
        let Some(reason) = pending else { return Ok(()) };

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "quiescence barrier timed out after {}s, still pending: {reason}",
                QUIESCENCE_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(QUIESCENCE_POLL_INTERVAL).await;
    }
}

/// Conditions every phase shares. An empty `_busy` alone does not prove the
/// callbacks landed — cancel refunds and `collectProtocolFee` travel as their
/// own messages — which is why the phase-specific checks below are written
/// against the *recipients*.
async fn base_pending(
    r: &ChainReader,
    t: &TrackedContracts,
    phase: &Phase<'_>,
) -> anyhow::Result<Option<String>> {
    for pn in &t.pns {
        let Some(fields) = pn_fields_opt(r, pn).await? else {
            return Ok(Some(format!("PN {pn} is not deployed yet")));
        };
        let busy = field(&fields, PN_BUSY)?;
        if !busy.is_null() {
            return Ok(Some(format!("PN {pn} is still busy with {busy}")));
        }
        let open = field_u32(&fields, PN_OPEN_ORDER_COUNT)?;
        let want = expected_open_orders(phase);
        if open != want {
            return Ok(Some(format!("PN {pn} has {open} open order(s), expected {want}")));
        }
    }

    // `_queueSize` counts the place/cancel/batch entries the book has accepted
    // but not finished processing. `getDetails` does not expose it, so this is
    // its own getter.
    for ob in live_order_books(t) {
        let size = OrderBook::new(r.ctx.clone(), dex_contract_params(&ob.addr))
            .get_queue_size()
            .await
            .map_err(|e| anyhow!("OrderBook {} getQueueSize: {e}", ob.addr))?
            .size;
        if size != 0 {
            return Ok(Some(format!(
                "OrderBook {} still has {size} unprocessed operation(s) queued",
                ob.addr
            )));
        }
    }

    Ok(None)
}

/// How many orders each phase expects to find resting. Every phase but
/// `AfterTrade` leaves the notes with a flat order state; `AfterTrade` states
/// its own number, so the check is never skipped — only parameterised.
fn expected_open_orders(phase: &Phase<'_>) -> u32 {
    match phase {
        Phase::AfterTrade { expected_open_orders } => *expected_open_orders,
        _ => 0,
    }
}

async fn phase_pending(
    r: &ChainReader,
    t: &TrackedContracts,
    phase: &Phase<'_>,
) -> anyhow::Result<Option<String>> {
    match phase {
        Phase::AfterDeploy { oracle, oel, pmp, oracle_fee, network_fee } => {
            // Fees travel as physical SHELL, invisible to the logical fields.
            for (role, addr, want) in [
                ("Oracle", *oracle, *oracle_fee),
                ("OracleEventList", *oel, 0),
                ("PMP", *pmp, *network_fee),
            ] {
                let got =
                    r.account_ecc(addr).await?.ecc.get(&CURRENCY_ID_SHELL).copied().unwrap_or(0);
                if got != want {
                    return Ok(Some(format!(
                        "{role} {addr} holds {got} ECC[{CURRENCY_ID_SHELL}], expected {want}"
                    )));
                }
            }
            Ok(None)
        }

        Phase::AfterStake | Phase::AfterSplit | Phase::AfterTrade { .. } => Ok(None),

        Phase::AfterFreeze { pmp } => {
            let Some(fields) = storage_fields_opt(r, pmp, PMP_ABI).await? else {
                return Ok(Some(format!("PMP {pmp} is not deployed yet")));
            };
            if field_bool(&fields, PMP_NORM_REFUND_PENDING)? {
                return Ok(Some(format!(
                    "PMP {pmp} is still waiting for the normalisation refund to be acknowledged"
                )));
            }
            Ok(None)
        }

        Phase::AfterResolve {
            pmp,
            deployer,
            deployer_balance_before,
            creator_fee,
            root_pn_fee_before,
            expected_protocol_fee,
        } => {
            // The creator fee and the protocol fee are both denominated in the
            // PMP's own currency.
            let token_type = tracked_pmp(t, pmp)?.token_type;

            // The drain flag lives on the PMP; the order book's own
            // `getShutdownState` is a different struct and does not answer this.
            let done = Pmp::new(r.ctx.clone(), dex_contract_params(*pmp))
                .get_shutdown_state()
                .await
                .map_err(|e| anyhow!("PMP {pmp} getShutdownState: {e}"))?
                .order_book_done;
            if !done {
                return Ok(Some(format!("PMP {pmp} has not finished draining its order book")));
            }

            let want = deployer_balance_before + creator_fee;
            let Some(got) = pn_balance_opt(r, deployer, token_type).await? else {
                return Ok(Some(format!("deployer PN {deployer} is not deployed yet")));
            };
            if got != want {
                return Ok(Some(format!(
                    "deployer PN {deployer} holds {got} of tt={token_type}, expected {want} \
                     (baseline {deployer_balance_before} + creator fee {creator_fee})"
                )));
            }

            let want = root_pn_fee_before + expected_protocol_fee;
            let got = protocol_fee(r, &t.root_pn, token_type).await?;
            if got != want {
                return Ok(Some(format!(
                    "RootPN protocol fee for tt={token_type} is {got}, expected {want} \
                     (baseline {root_pn_fee_before} + {expected_protocol_fee})"
                )));
            }

            Ok(None)
        }

        Phase::AfterClaims { pmp, residual_to, baseline } => {
            // Deliberately `account_absent`, not a bare `account_boc(..) ==
            // None`: a self-destructed PMP can still answer with a BOC that
            // carries no account, and treating that as "still alive" would
            // hang this barrier for its full timeout on a condition that is
            // already satisfied — with no assertion message to show for it.
            if !account_absent(r, pmp).await? {
                return Ok(Some(format!("PMP {pmp} has not self-destructed yet")));
            }
            // The account being gone says nothing about where its money is, so
            // settle on the money instead: re-snapshot with the PMP declared
            // dead and wait for conservation against the baseline to close.
            let settled = with_pmp_self_destructed(t, pmp, residual_to)?;
            let diff =
                conserved_diff(baseline, &snapshot(r, &settled).await?, &ExternalDelta::none());
            if !diff.is_empty() {
                return Ok(Some(format!(
                    "residual of PMP {pmp} has not reached {residual_to}; imbalance {diff:?}"
                )));
            }
            Ok(None)
        }
    }
}

fn sum_tt(snap: &InvariantSnapshot, tt: u32) -> i128 {
    snap.per_tt.get(&tt).map(|m| m.values().sum()).unwrap_or(0)
}

fn phys_at(snap: &PhysicalSnapshot, addr: &str, tt: u32) -> u128 {
    snap.per_addr.get(addr).and_then(|m| m.get(&tt)).copied().unwrap_or(0)
}

fn tracked_addresses(t: &TrackedContracts) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert(t.root_pn.clone());
    out.extend(t.pns.iter().cloned());
    out.extend(t.pmps.iter().map(|p| p.addr.clone()));
    out.extend(t.order_books.iter().map(|o| o.addr.clone()));
    out.extend(t.oracles.iter().cloned());
    out.extend(t.oels.iter().cloned());
    out
}

fn tracked_pmp<'a>(t: &'a TrackedContracts, addr: &str) -> anyhow::Result<&'a TrackedPmp> {
    t.pmps
        .iter()
        .find(|p| p.addr == addr)
        .ok_or_else(|| anyhow!("PMP {addr} is not in the tracked set"))
}

/// Copy of the tracked set with `pmp` declared self-destructed, so a snapshot
/// stops reading a contract that no longer exists and stops counting money
/// that has moved to `residual_to`.
fn with_pmp_self_destructed(
    t: &TrackedContracts,
    pmp: &str,
    residual_to: &str,
) -> anyhow::Result<TrackedContracts> {
    tracked_pmp(t, pmp)?;
    let mut out = t.clone();
    for p in &mut out.pmps {
        if p.addr == pmp {
            p.state = PmpState::SelfDestructed { residual_to: residual_to.to_string() };
        }
    }
    Ok(out)
}

/// The module's single notion of "the contract is not there": either the
/// gateway has no account for the address, or it hands back a BOC that carries
/// no account (`AccountNone`).
///
/// Both halves matter. A never-deployed address answers with the first; a
/// contract that lived and then self-destructed can keep answering with the
/// second, so a barrier that only checked the fetch would wait out its whole
/// timeout on a self-destruct that already happened. Existence is decided
/// structurally in both cases — never by matching an error string.
async fn account_absent(r: &ChainReader, addr: &str) -> anyhow::Result<bool> {
    match r.account_boc(addr).await.with_context(|| format!("fetch account BOC of {addr}"))? {
        None => Ok(true),
        Some(boc) => {
            account_boc_is_none(&boc).with_context(|| format!("decode account state of {addr}"))
        }
    }
}

/// Decoded storage fields, or `None` when the account is absent by
/// [`account_absent`]'s definition.
async fn storage_fields_opt(
    r: &ChainReader,
    addr: &str,
    abi_json: &str,
) -> anyhow::Result<Option<Value>> {
    let Some(boc) =
        r.account_boc(addr).await.with_context(|| format!("fetch account BOC of {addr}"))?
    else {
        return Ok(None);
    };
    if account_boc_is_none(&boc).with_context(|| format!("decode account state of {addr}"))? {
        return Ok(None);
    }
    decode_account_fields_json(abi_json, &boc)
        .with_context(|| format!("decode storage fields of {addr}"))
        .map(Some)
}

async fn pn_fields_opt(r: &ChainReader, pn: &str) -> anyhow::Result<Option<Value>> {
    storage_fields_opt(r, pn, PN_ABI).await
}

async fn pn_balance_opt(r: &ChainReader, pn: &str, tt: u32) -> anyhow::Result<Option<u128>> {
    let Some(fields) = pn_fields_opt(r, pn).await? else { return Ok(None) };
    Ok(Some(field_uint_map(&fields, PN_BALANCE)?.get(&tt).copied().unwrap_or(0)))
}

/// A note's holdings per currency: its free balance plus what is escrowed
/// against its resting orders. Dropping the escrow would read every placed
/// order as a loss.
///
/// `None` fields mean the account is not on chain, and every requested currency
/// then reads zero — see [`snapshot`] for why that is the correct value rather
/// than an error.
fn pn_holdings(fields: Option<&Value>, token_types: &[u32]) -> anyhow::Result<BTreeMap<u32, u128>> {
    let (balance, locked) = match fields {
        Some(f) => (field_uint_map(f, PN_BALANCE)?, field_uint_map(f, PN_LOCKED_IN_ORDERS)?),
        None => (BTreeMap::new(), BTreeMap::new()),
    };
    Ok(token_types
        .iter()
        .map(|tt| {
            (*tt, balance.get(tt).copied().unwrap_or(0) + locked.get(tt).copied().unwrap_or(0))
        })
        .collect())
}

async fn protocol_fee(r: &ChainReader, root_pn: &str, tt: u32) -> anyhow::Result<u128> {
    Ok(RootPn::new(r.ctx.clone(), dex_contract_params(root_pn))
        .get_protocol_fee(ParamsOfGetProtocolFee { token_type: tt })
        .await
        .map_err(|e| anyhow!("RootPN {root_pn} getProtocolFee(tokenType={tt}): {e}"))?
        .value)
}

/// A tracked set naming the wrong currency for a contract would silently skew
/// a total, so it fails here instead — loudly, at the first snapshot.
fn assert_token_type(role: &str, addr: &str, on_chain: u32, tracked: u32) {
    assert_eq!(
        on_chain, tracked,
        "{role} {addr} is token type {on_chain} on chain, but the tracked set says {tracked}"
    );
}

fn as_i128(v: u128) -> anyhow::Result<i128> {
    i128::try_from(v).map_err(|_| anyhow!("balance {v} does not fit in i128"))
}

/// Signed view of a balance for the assertion helpers, which have no error
/// channel of their own.
fn signed(v: u128) -> i128 {
    i128::try_from(v).unwrap_or_else(|_| panic!("balance {v} does not fit in i128"))
}

fn field<'a>(fields: &'a Value, name: &str) -> anyhow::Result<&'a Value> {
    fields.get(name).ok_or_else(|| anyhow!("decoded storage has no `{name}` field"))
}

/// `map(uint32, uint128)` decodes to a JSON object of decimal strings keyed by
/// decimal strings.
fn field_uint_map(fields: &Value, name: &str) -> anyhow::Result<BTreeMap<u32, u128>> {
    let obj = field(fields, name)?
        .as_object()
        .ok_or_else(|| anyhow!("storage field `{name}` is not a map"))?;
    let mut out = BTreeMap::new();
    for (key, value) in obj {
        let tt: u32 = key.parse().with_context(|| format!("parse `{name}` key `{key}`"))?;
        let raw = value
            .as_str()
            .ok_or_else(|| anyhow!("storage field `{name}[{key}]` is not a string"))?;
        let amount: u128 =
            raw.parse().with_context(|| format!("parse `{name}[{key}]` value `{raw}`"))?;
        out.insert(tt, amount);
    }
    Ok(out)
}

/// Integers narrower than 256 bits decode to decimal strings, not JSON numbers.
fn field_u32(fields: &Value, name: &str) -> anyhow::Result<u32> {
    let raw = field(fields, name)?
        .as_str()
        .ok_or_else(|| anyhow!("storage field `{name}` is not a string"))?;
    raw.parse().with_context(|| format!("parse `{name}` value `{raw}`"))
}

fn field_bool(fields: &Value, name: &str) -> anyhow::Result<bool> {
    field(fields, name)?.as_bool().ok_or_else(|| anyhow!("storage field `{name}` is not a bool"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn snap(entries: &[(u32, &str, i128)]) -> InvariantSnapshot {
        let mut per_tt: BTreeMap<u32, BTreeMap<String, i128>> = BTreeMap::new();
        for (tt, addr, v) in entries {
            per_tt.entry(*tt).or_default().insert((*addr).to_string(), *v);
        }
        InvariantSnapshot { per_tt }
    }

    #[test]
    fn conserved_exact_zero_passes() {
        let b = snap(&[(1, "pn_a", 100), (1, "pmp", 0)]);
        let a = snap(&[(1, "pn_a", 40), (1, "pmp", 60)]);
        assert_conserved(&b, &a, &ExternalDelta::none());
    }

    #[test]
    #[should_panic(expected = "tt=1")]
    fn one_unit_leak_panics() {
        let b = snap(&[(1, "pn_a", 100)]);
        let a = snap(&[(1, "pn_a", 99)]);
        assert_conserved(&b, &a, &ExternalDelta::none());
    }

    #[test]
    fn declared_external_delta_accounted() {
        let b = snap(&[(1, "pn_a", 100)]);
        let a = snap(&[(1, "pn_a", 70)]);
        let mut ext = ExternalDelta::none();
        ext.0.insert(("pn_a".into(), 1), -30);
        assert_conserved(&b, &a, &ext);
    }

    #[test]
    fn conserved_diff_reports_imbalance_without_panicking() {
        let b = snap(&[(1, "pn_a", 100)]);
        let a = snap(&[(1, "pn_a", 99)]);
        let d = conserved_diff(&b, &a, &ExternalDelta::none());
        assert_eq!(d.get(&1), Some(&-1i128));
    }

    #[test]
    fn a_loss_in_one_currency_is_not_masked_by_a_gain_in_another() {
        // The reason conservation is checked per currency at all: pooling the
        // buckets would report this pair of movements as perfectly balanced.
        let b = snap(&[(1, "pn_a", 100), (2, "pn_a", 0)]);
        let a = snap(&[(1, "pn_a", 99), (2, "pn_a", 1)]);
        let d = conserved_diff(&b, &a, &ExternalDelta::none());
        assert_eq!(d.get(&1), Some(&-1i128));
        assert_eq!(d.get(&2), Some(&1i128));
    }

    #[test]
    fn a_currency_present_on_only_one_side_is_still_compared() {
        // The shape the `AfterClaims` barrier polls: the dying PMP's bucket
        // exists in the baseline and its account is gone from the current
        // snapshot, so tt=2 appears on the `before` side only.
        let b = snap(&[(1, "pn_a", 100), (2, "pmp", 7)]);
        let a = snap(&[(1, "pn_a", 100)]);
        let d = conserved_diff(&b, &a, &ExternalDelta::none());
        assert_eq!(d.get(&2), Some(&-7i128));
        assert_eq!(d.get(&1), None);

        // And a currency named only by the declaration: an external delta
        // against a currency neither snapshot mentions must still be demanded,
        // not silently dropped.
        let mut ext = ExternalDelta::none();
        ext.0.insert(("pn_a".into(), 3), 5);
        let d = conserved_diff(&b, &b, &ext);
        assert_eq!(d.get(&3), Some(&-5i128));
    }

    #[test]
    fn an_account_that_is_not_on_chain_contributes_zero() {
        // A snapshot taken before the contracts exist must measure the delta
        // from nothing, not abort. Zero is the correct balance of an account
        // holding no state — the same reading `phys_snapshot` already gives it.
        let h = pn_holdings(None, &[1, 2]).unwrap();
        assert_eq!(h.get(&1), Some(&0));
        assert_eq!(h.get(&2), Some(&0));
    }

    #[test]
    fn a_note_holds_its_free_balance_plus_what_is_escrowed_in_orders() {
        let fields = serde_json::json!({
            "_balance": { "1": "100", "2": "7" },
            "_lockedInOrders": { "1": "40" },
        });
        let h = pn_holdings(Some(&fields), &[1, 2, 3]).unwrap();
        // Counting only `_balance` would read a resting order as a loss.
        assert_eq!(h.get(&1), Some(&140));
        assert_eq!(h.get(&2), Some(&7));
        // A currency the note has never held still reads zero, not absent.
        assert_eq!(h.get(&3), Some(&0));
    }

    #[test]
    fn only_after_trade_expects_resting_orders() {
        // Skipping the check for `AfterTrade` would drop it from the phase it
        // says the most about; the variant carries the number so the barrier
        // compares there like everywhere else.
        assert_eq!(expected_open_orders(&Phase::AfterTrade { expected_open_orders: 2 }), 2);
        assert_eq!(expected_open_orders(&Phase::AfterStake), 0);
        assert_eq!(expected_open_orders(&Phase::AfterSplit), 0);
        assert_eq!(expected_open_orders(&Phase::AfterFreeze { pmp: "0:pmp" }), 0);
    }

    #[test]
    #[should_panic(expected = "0:dead")]
    fn phys_delta_mismatch_names_account() {
        let b = PhysicalSnapshot { per_addr: [("0:dead".into(), [(2u32, 100u128)].into())].into() };
        let a = PhysicalSnapshot { per_addr: [("0:dead".into(), [(2u32, 90u128)].into())].into() };
        let exp: ExpectedPhysDelta = [(("0:dead".into(), 2), 0i128)].into();
        assert_phys_delta(&b, &a, &exp);
    }

    #[test]
    fn phys_delta_sums_to_zero_over_tracked_set() {
        let b = PhysicalSnapshot {
            per_addr: [
                ("a".into(), [(2u32, 100u128)].into()),
                ("b".into(), [(2u32, 0u128)].into()),
            ]
            .into(),
        };
        let a = PhysicalSnapshot {
            per_addr: [
                ("a".into(), [(2u32, 60u128)].into()),
                ("b".into(), [(2u32, 40u128)].into()),
            ]
            .into(),
        };
        let exp: ExpectedPhysDelta = [(("a".into(), 2), -40i128), (("b".into(), 2), 40i128)].into();
        assert_phys_delta(&b, &a, &exp);
    }

    #[test]
    #[should_panic(expected = "do not themselves conserve")]
    fn phys_delta_rejects_an_expectation_that_loses_money() {
        // The per-account comparison alone would accept this: "a" really does
        // drop 40 and the expectation really does say -40. What is wrong is
        // the expectation itself — nothing declares where the 40 went.
        let b = PhysicalSnapshot { per_addr: [("a".into(), [(2u32, 100u128)].into())].into() };
        let a = PhysicalSnapshot { per_addr: [("a".into(), [(2u32, 60u128)].into())].into() };
        let exp: ExpectedPhysDelta = [(("a".into(), 2), -40i128)].into();
        assert_phys_delta(&b, &a, &exp);
    }

    #[test]
    fn credit_lands_only_in_own_token_type() {
        let mut per_tt: BTreeMap<u32, BTreeMap<String, i128>> = BTreeMap::new();
        per_tt.insert(1, BTreeMap::new());
        per_tt.insert(2, BTreeMap::new());
        credit(&mut per_tt, 1, "pmp_nackl", 70);
        assert_eq!(per_tt[&1]["pmp_nackl"], 70);
        assert!(per_tt[&2].is_empty());
    }

    #[test]
    fn phys_missing_before_is_zero() {
        let b = PhysicalSnapshot { per_addr: [("pn".into(), [(1u32, 100u128)].into())].into() };
        let a = PhysicalSnapshot {
            per_addr: [
                ("pn".into(), [(1u32, 60u128)].into()),
                ("pmp".into(), [(1u32, 40u128)].into()),
            ]
            .into(),
        };
        let exp: ExpectedPhysDelta =
            [(("pn".into(), 1), -40i128), (("pmp".into(), 1), 40i128)].into();
        assert_phys_delta(&b, &a, &exp);
    }

    #[test]
    fn dead_contracts_are_excluded_from_the_tracked_sum() {
        let t = TrackedContracts {
            root_pn: "root".into(),
            pns: vec!["pn_a".into()],
            pmps: vec![
                TrackedPmp { addr: "pmp_live".into(), token_type: 1, state: PmpState::Live },
                TrackedPmp {
                    addr: "pmp_dead".into(),
                    token_type: 1,
                    state: PmpState::SelfDestructed { residual_to: "pn_a".into() },
                },
            ],
            order_books: vec![
                TrackedOb { addr: "ob_live".into(), token_type: 1, state: ObState::Live },
                TrackedOb { addr: "ob_drained".into(), token_type: 1, state: ObState::Drained },
            ],
            oracles: vec![],
            oels: vec![],
            token_types: vec![1],
        };

        let pmps: Vec<&str> = live_pmps(&t).map(|p| p.addr.as_str()).collect();
        let obs: Vec<&str> = live_order_books(&t).map(|o| o.addr.as_str()).collect();

        assert_eq!(pmps, ["pmp_live"]);
        assert_eq!(obs, ["ob_live"]);
    }

    #[test]
    fn storage_field_names_exist_in_the_contract_abis() {
        let checks: [(&str, &str, &[&str]); 2] = [
            (
                PN_ABI,
                "PrivateNote",
                &[PN_BALANCE, PN_LOCKED_IN_ORDERS, PN_BUSY, PN_OPEN_ORDER_COUNT],
            ),
            (PMP_ABI, "PMP", &[PMP_NORM_REFUND_PENDING]),
        ];
        for (abi_json, label, names) in checks {
            let abi: serde_json::Value = serde_json::from_str(abi_json).expect("parse abi json");
            let fields = abi["fields"].as_array().expect("abi has a `fields` array");
            for name in names {
                assert!(
                    fields.iter().any(|f| f["name"] == *name),
                    "{label} ABI declares no storage field `{name}`"
                );
            }
        }
    }
}
