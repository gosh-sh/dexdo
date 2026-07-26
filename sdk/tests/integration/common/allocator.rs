//! Sweep verdict for a `PrivateNote` account returning to the shared test
//! pool: judges whether its decoded storage is genuinely back to a neutral
//! state (no locks held, no stakes outstanding, no pending operation half
//! finished) before another scenario is allowed to reuse it.
//!
//! Two lists close the judgment against the actual ABI:
//!
//! - [`SWEEP_MUST_BE_EMPTY_OR_ZERO`] — every stateful field (nonces, locks,
//!   stakes, debt, coupons, order-book bookkeeping) that must read back as
//!   `null`, an all-zero numeric string (decimal `"0"` or a zero-padded
//!   `uint256` hex string), `0`, or an empty map, array, or string.
//! - [`SWEEP_MUST_BE_FALSE`] — the boolean-typed fields among them; `false`
//!   isn't covered by the "zero" notion above, so they're judged separately.
//!
//! [`sweep_verdict`] is fail-closed: a name from either list that is
//! *absent* from the decoded JSON is Dirty, not Clean. An absent key means
//! the decode didn't produce what was expected (wrong ABI, changed layout),
//! and defaulting that to "clean" would hand out an account whose state was
//! never actually inspected.
//!
//! The completeness test in `tests` below (`sweep_lists_cover_private_note_abi_fields`)
//! pins both lists — plus a third, allowed-non-sweep list local to that
//! test — against every field the `PrivateNote` ABI actually declares.
//! Without it, a contract upgrade that adds a new stateful field would rot
//! this module silently: `sweep_verdict` would simply never look at the new
//! field, a dirty account would start passing as clean, and the damage
//! would surface as an unrelated test failing much later, in a different
//! scenario, for reasons nobody could trace back here. The test forces a
//! human classification decision instead: it fails the moment the ABI's
//! field set no longer matches what's listed, and stays failing until
//! someone decides, by name, which bucket the new field belongs to.
//!
//! The account-pool allocator that consumes this verdict — [`Allocator`] and
//! [`LeasedPn`], with the rent / taint / `release_clean` lifecycle — follows
//! below in this same file. [`Allocator`] loads the on-disk pool of pre-baked
//! `PrivateNote`s (`dex_test_notes.keys.json`), registers its tail slice as
//! `Free` in the shared [`Ledger`], and hands out leases from that tail only —
//! the head of the pool is reserved for the api-e2e suite. A leased note comes
//! back to the pool only through [`LeasedPn::release_clean`], which re-reads
//! the note's on-chain state and quarantines it (never reuses it) unless that
//! state is genuinely clean. Anything else — an explicit [`LeasedPn::taint`]
//! call, or a [`LeasedPn`] simply dropped without releasing — quarantines the
//! note too: the pool is small and shared across concurrent test processes, so
//! a note handed back in an unknown state is worse than a note lost for the
//! rest of the run.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use ackinacki_kit::tvm_client::crypto::KeyPair;
use anyhow::anyhow;
use anyhow::Context;
use dodex_infrastructure::tvm_runner::AccountEcc;
use dodex_sdk::proof::strip_0x;
use serde::Deserialize;
use serde_json::Value;

use crate::common::chain_reader::ChainReader;
use crate::common::context::CURRENCY_ID_SHELL;
use crate::common::ledger::Ledger;
use crate::common::ledger::NoteState;

/// Storage fields that must be empty/zero for a note to be safe to hand back
/// into the pool: nonces and locks for whatever operation was in flight, and
/// every stateful ledger field (stakes, debt, coupons, order-book locks and
/// reservations). Closed against the ground-truth `PrivateNote` ABI on this
/// branch — see `sweep_lists_cover_private_note_abi_fields`.
pub const SWEEP_MUST_BE_EMPTY_OR_ZERO: &[&str] = &[
    "_busy",
    // Nonce of the operation currently IN FLIGHT — must be 0 at rest.
    // Contrast `_opNonce` (allowed list below), the monotonic all-time
    // counter, which stays nonzero on a reused note by design.
    "_busyOpNonce",
    "_pendingBatchBuyLock",
    "_pendingBatchTokenType",
    "_pendingBatchStakeHash",
    "_pendingBatchSells",
    "_pendingBatchClientOrderIds",
    "_pendingPlaceBuyLock",
    "_pendingPlaceBuyTokenType",
    "_pendingPlaceClientOrderId",
    "_pendingTransferAmount",
    "_pendingTransferTokenType",
    "_stakes",
    "_debt",
    "_couponsValue",
    "_lockedInOrders",
    "_orderLocks",
    "_orderFeeReserves",
    "_openOrderCount",
    "_openOrdersByEvent",
    "_clientOrderIds",
    "_streamLocks",
    "_disputeLocks",
    "_streamLockCount",
    "_disputeLockCount",
];

/// Boolean-typed storage fields that must be exactly `false`. Split out from
/// the list above because `false` isn't an instance of the "null / zero /
/// empty" notion `sweep_verdict` uses for everything else.
pub const SWEEP_MUST_BE_FALSE: &[&str] =
    &["_pendingBatchActive", "_pendingForfeit", "_hasWithdrawn", "_hasTransferred"];

/// 10 SHELL — enough headroom for roughly ten `deployPMP` calls. The
/// allocator quarantines a note as `ShellDepleted` once its ECC balance
/// drops below this.
pub const MIN_DEPLOY_SHELL: u128 = 10_000_000_000;

/// Verdict for one note's decoded storage.
#[derive(Debug, PartialEq, Eq)]
pub enum SweepVerdict {
    Clean,
    Dirty { fields: Vec<String> },
}

/// `null`, `0`, an empty object/array/string, or a numeric string that is
/// entirely zero digits.
///
/// The last case exists because `tvm_abi`'s detokenizer does not encode every
/// integer width the same way: a `uint256` decodes to a zero-padded hex
/// string (`"0x" + 64 hex digits`, per `detokenize_big_uint`), while every
/// narrower unsigned width decodes to a plain decimal string. Matching only
/// the literal `"0"` would accept a narrow-width zero but reject a genuine
/// `uint256` zero (`"0x000…000"`), which would make every real note
/// permanently Dirty on any `uint256` field in the sweep list — this has to
/// stay general (strip an optional `0x`, then check the remainder is
/// non-empty and all `'0'`) so the next `uint256` field added to the sweep
/// list is handled automatically, not by special-casing a field name here.
fn is_empty_or_zero(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => {
            let digits = s.strip_prefix("0x").unwrap_or(s);
            s.is_empty() || (!digits.is_empty() && digits.chars().all(|c| c == '0'))
        }
        // Defensive fallback only: the decoder emits strings for every
        // numeric ABI field (see above), so a JSON number here would mean an
        // unexpected decode shape, not the primary path.
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Bool(_) => false,
    }
}

/// Verdict over decoded storage fields (the JSON `ChainReader::storage_fields`
/// produces). Fail-closed: a name from either sweep list that is missing
/// from `fields` entirely counts as Dirty, exactly like a present-but-dirty
/// value — never as an implicit, acceptable null.
pub fn sweep_verdict(fields: &Value) -> SweepVerdict {
    let mut dirty = Vec::new();

    for name in SWEEP_MUST_BE_EMPTY_OR_ZERO {
        let clean = matches!(fields.get(name), Some(v) if is_empty_or_zero(v));
        if !clean {
            dirty.push((*name).to_string());
        }
    }
    for name in SWEEP_MUST_BE_FALSE {
        let clean = matches!(fields.get(name), Some(Value::Bool(false)));
        if !clean {
            dirty.push((*name).to_string());
        }
    }

    if dirty.is_empty() {
        SweepVerdict::Clean
    } else {
        SweepVerdict::Dirty { fields: dirty }
    }
}

/// `PrivateNote` ABI, read from the same on-branch contract source the sweep
/// lists above are pinned against — used to decode a leased note's storage
/// before [`LeasedPn::release_clean`] judges it.
// Only `release_clean` reads this, and nothing in this hermetic test file
// calls it — it needs a live chain.
#[allow(dead_code)]
const PN_ABI: &str = include_str!("../../../../contracts/dex/PrivateNote.abi.json");

/// The default tail size (see [`Allocator::new`]) when `E2E_SDK_TAIL_COUNT` is
/// unset.
const DEFAULT_SDK_TAIL: usize = 3;

/// The role a rented note is about to play in a scenario. Reserved for
/// profile-aware pool partitioning: the allocator here draws every profile
/// from the same tail slice, uniformly — nothing yet gives one profile a
/// dedicated sub-range.
// Only Dep/Trd/Shell are constructed by this file's own tests; the rest are
// for scenario tests to come.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnProfile {
    Dep,
    Trd,
    Cons,
    Cpn,
    Shell,
    Usdc,
    Inf,
    Rot,
}

/// Why a note was pulled from the pool and will never be leased again this
/// generation. [`TaintReason::DirtyState`] and [`TaintReason::ShellDepleted`]
/// are the two reasons [`LeasedPn::release_clean`] can produce on its own; the
/// rest are for a caller that has already diagnosed the note some other way
/// (e.g. a scenario that knows a note it holds has outstanding debt) and wants
/// to record why via [`LeasedPn::taint`].
// Only DirtyState/ShellDepleted are constructed by this file's own tests; the
// rest are for a caller that has diagnosed a note some other way, added in
// scenario tests.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaintReason {
    Dead,
    Debt,
    HasTransferred,
    HasWithdrawn,
    DirtyState { fields: Vec<String> },
    ProfileDrift,
    ShellDepleted,
}

/// The persisted `Quarantined.reason` string for a [`TaintReason`]. `Debug` is
/// the whole format: a unit variant like `ShellDepleted` renders as exactly
/// that word, and `DirtyState { fields: [..] }` carries the actual offending
/// field names rather than a generic label — the entire point of quarantining
/// with a reason instead of a bare flag.
fn taint_reason_string(reason: &TaintReason) -> String {
    format!("{reason:?}")
}

/// One row of the on-disk `dex_test_notes.keys.json` seed file — the
/// ground-truth format the real generator emits (mirrored exactly by this
/// module's `fake_seed` test helper). `pn_dih_hex` is a short hex index
/// (`"0x0"`, `"0x1"`, …), not a padded 256-bit hash; extra fields the real
/// generator emits (`tokenType`, `value`) are simply not named here, so serde
/// ignores them instead of failing the parse.
#[derive(Deserialize)]
struct SeedNoteFile {
    pn_dih_hex: String,
    pn_address: String,
    pn_pubkey_hex: String,
    pn_seckey_hex: String,
}

/// One pre-baked account from the seed file, ready to be leased.
// `dih_dec`/`keys` are read by scenario tests that actually perform signed
// operations against the leased note; this file's own tests only rent,
// release, and taint.
#[allow(dead_code)]
#[derive(Clone)]
pub struct SeedNote {
    pub address: String,
    /// Decimal form of `pn_dih_hex` — what the contracts' getters and inputs
    /// expect.
    pub dih_dec: String,
    pub keys: KeyPair,
}

/// Hex (optionally `0x`-prefixed, any length — the seed file's short form
/// `"0x0"`/`"0x1"` included) to decimal. Returns an error instead of panicking
/// so a malformed seed file surfaces as a load error, not a crashed test
/// binary.
fn dih_hex_to_dec(hex: &str) -> anyhow::Result<String> {
    let digits = strip_0x(hex);
    num_bigint::BigUint::parse_bytes(digits.as_bytes(), 16)
        .map(|n| n.to_str_radix(10))
        .ok_or_else(|| anyhow!("pn_dih_hex `{hex}` is not valid hex"))
}

/// Path to the seed notes file: `E2E_SEED_NOTES` if set, else `PN_POOL_PATH`,
/// else an error — there is no bundled default, because a real seed file
/// carries live secret keys and can never be committed. Used by
/// [`Allocator::new`]; [`Allocator::with_seed_path`] takes the path directly
/// instead, precisely so tests do not have to go through here.
pub fn seed_path() -> anyhow::Result<PathBuf> {
    for var in ["E2E_SEED_NOTES", "PN_POOL_PATH"] {
        if let Ok(v) = std::env::var(var) {
            return Ok(PathBuf::from(v));
        }
    }
    Err(anyhow!("seed notes path not set: export E2E_SEED_NOTES or PN_POOL_PATH"))
}

/// Directory the seed file lives in — also where the shared [`Ledger`] and
/// its lock sidecar live, since [`Allocator::new`] opens the ledger at the
/// seed file's own directory.
// No caller in this crate yet — for helpers that, like the allocator itself,
// need to find the ledger from the environment rather than from an explicit
// path.
#[allow(dead_code)]
pub fn seed_dir() -> anyhow::Result<PathBuf> {
    let path = seed_path()?;
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("seed path {} has no parent directory", path.display()))
}

fn sdk_tail_count() -> usize {
    std::env::var("E2E_SDK_TAIL_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SDK_TAIL)
}

/// Rents pre-baked `PrivateNote`s out of a shared pool for the lifetime of an
/// e2e run. Only the pool's tail (last `sdk_tail` entries, in seed-file order)
/// is ever leased — the head is reserved for the api-e2e suite, which draws
/// from the same seed file independently.
pub struct Allocator {
    /// Every seed note, in file order.
    notes: Vec<SeedNote>,
    ledger: Arc<Ledger>,
    sdk_tail: usize,
}

impl Allocator {
    /// Thin env wrapper: resolves the seed path via [`seed_path`] and defers
    /// to [`Allocator::with_seed_path`].
    // No caller in this hermetic test file — tests use `with_seed_path`
    // directly to avoid mutating env; a real e2e run is the caller of this
    // constructor.
    #[allow(dead_code)]
    pub fn new(run_id: &str) -> anyhow::Result<Allocator> {
        let seed = seed_path()?;
        Allocator::with_seed_path(run_id, &seed)
    }

    /// Loads the seed file at an explicit path — testable without mutating
    /// process environment (`std::env::set_var` is `unsafe` as of Rust 2024).
    /// The ledger lives in `seed`'s own directory, exactly like
    /// [`Allocator::new`]'s env-driven path.
    ///
    /// Registration is idempotent and additive only: [`Ledger::bootstrap`]
    /// starts a generation with an empty `notes` map, so without this no
    /// `rent` would ever find a `Free` note. This does one [`Ledger::with_txn`]
    /// that inserts a `Free` entry for every tail address *absent* from the
    /// map; an address already present — `Leased`, `Quarantined`, or `Free`
    /// from an earlier process's registration this same generation — is left
    /// untouched. That "absent only" rule is what keeps a quarantine from a
    /// prior run inside this generation from reviving just because a later
    /// process re-registers.
    pub fn with_seed_path(run_id: &str, seed: &Path) -> anyhow::Result<Allocator> {
        let bytes = std::fs::read(seed)
            .with_context(|| format!("read seed notes file {}", seed.display()))?;
        let rows: Vec<SeedNoteFile> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse seed notes file {} as a JSON array", seed.display()))?;
        let notes: Vec<SeedNote> = rows
            .into_iter()
            .map(|r| {
                Ok(SeedNote {
                    address: r.pn_address,
                    dih_dec: dih_hex_to_dec(&r.pn_dih_hex)?,
                    keys: KeyPair { public: r.pn_pubkey_hex, secret: r.pn_seckey_hex },
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let dir = seed
            .parent()
            .ok_or_else(|| anyhow!("seed path {} has no parent directory", seed.display()))?;
        let ledger = Arc::new(Ledger::open(dir, run_id));

        let sdk_tail = sdk_tail_count();
        let tail_start = notes.len().saturating_sub(sdk_tail);
        let tail_addrs: Vec<String> =
            notes[tail_start..].iter().map(|n| n.address.clone()).collect();
        ledger.with_txn(|f| {
            for addr in &tail_addrs {
                f.notes.entry(addr.clone()).or_insert_with(|| NoteState::Free {
                    ecc_shell_remaining: None,
                    balances: BTreeMap::new(),
                });
            }
        })?;

        Ok(Allocator { notes, ledger, sdk_tail })
    }

    fn tail(&self) -> &[SeedNote] {
        let start = self.notes.len().saturating_sub(self.sdk_tail);
        &self.notes[start..]
    }

    /// Leases the first `Free` note found in the tail, in file order, marking
    /// it `Leased { pid, test }` in the same transaction that reads it — two
    /// concurrent `rent` calls can never be handed the same address. Errors
    /// once every tail note is `Leased` or `Quarantined`.
    pub fn rent(&self, _profile: PnProfile, test: &str) -> anyhow::Result<LeasedPn> {
        let pid = std::process::id();
        let tail = self.tail();
        let picked = self.ledger.with_txn(|f| {
            for note in tail {
                if matches!(f.notes.get(&note.address), Some(NoteState::Free { .. })) {
                    f.notes.insert(
                        note.address.clone(),
                        NoteState::Leased { pid, test: test.to_string() },
                    );
                    return Some(note.clone());
                }
            }
            None
        })?;
        let note = picked.ok_or_else(|| {
            anyhow!(
                "allocator: no Free note left in the {}-slot tail for test `{test}`",
                tail.len()
            )
        })?;
        Ok(LeasedPn { note, ledger: Arc::clone(&self.ledger), released: false })
    }

    /// Allocates the next unique nonce for this run's ledger generation —
    /// see [`Ledger::next_nonce`].
    // No caller in this hermetic test file yet; scenario tests that need a
    // unique nonce per operation use it.
    #[allow(dead_code)]
    pub fn next_nonce(&self) -> anyhow::Result<u64> {
        Ok(self.ledger.next_nonce()?)
    }
}

/// A leased `PrivateNote`. Must be resolved before it goes out of scope —
/// [`LeasedPn::release_clean`] to hand it back after verifying it is clean,
/// [`LeasedPn::taint`] to quarantine it under a known reason — or [`Drop`]
/// quarantines it generically to keep an unknown-state note from ever being
/// leased again.
pub struct LeasedPn {
    pub note: SeedNote,
    ledger: Arc<Ledger>,
    released: bool,
}

impl LeasedPn {
    /// Fetches the note's current on-chain storage fields and ECC balance,
    /// then hands both to the synchronous, network-free core ([`LeasedPn::finish`])
    /// that judges and records the outcome. Splitting fetch from decision this
    /// way is what makes the decision itself unit-testable without a network.
    // No caller in this hermetic test file — it needs a live chain; `finish`
    // below is what this file's own tests exercise directly instead.
    #[allow(dead_code)]
    pub async fn release_clean(mut self, r: &ChainReader) -> anyhow::Result<()> {
        let fields = r.storage_fields(&self.note.address, PN_ABI).await?;
        let ecc = r.account_ecc(&self.note.address).await?;
        self.finish(fields, ecc)
    }

    /// The decision core behind [`LeasedPn::release_clean`]: judges the
    /// already-fetched storage/balance and performs exactly one ledger
    /// transition for whichever outcome it reaches. `Ledger::with_txn` is
    /// local file I/O, not network I/O, so this can be — and is — fully
    /// synchronous, which is what lets a test call it directly with
    /// hand-built `fields`/`ecc` instead of a live chain.
    ///
    /// Every terminal transition marks `self.released = true` immediately
    /// after its `with_txn` call succeeds, *before* this function returns —
    /// including the two `Err` outcomes. `release_clean` owns `self` by value
    /// and drops it the moment this call returns, so skipping this would let
    /// `Drop` run right after and overwrite the precise `DirtyState`/
    /// `ShellDepleted` reason just written with the generic `"drop"` one,
    /// destroying exactly the diagnostic the quarantine exists to preserve.
    /// If the `with_txn` call itself fails (e.g. `StaleRun`), `released` is
    /// deliberately left `false` so `Drop` gets a fallback attempt at
    /// recording *something* — nothing was written the first time, so there
    /// is nothing for it to clobber.
    fn finish(&mut self, fields: Value, ecc: AccountEcc) -> anyhow::Result<()> {
        let addr = self.note.address.clone();

        if let SweepVerdict::Dirty { fields: dirty } = sweep_verdict(&fields) {
            let reason = taint_reason_string(&TaintReason::DirtyState { fields: dirty.clone() });
            self.ledger.with_txn(|f| {
                f.notes.insert(addr, NoteState::Quarantined { reason });
            })?;
            self.released = true;
            return Err(anyhow!("release_clean: note left dirty: {dirty:?}"));
        }

        let shell = ecc.ecc.get(&CURRENCY_ID_SHELL).copied().unwrap_or(0);
        if shell < MIN_DEPLOY_SHELL {
            let reason = taint_reason_string(&TaintReason::ShellDepleted);
            self.ledger.with_txn(|f| {
                f.notes.insert(addr, NoteState::Quarantined { reason });
            })?;
            self.released = true;
            return Err(anyhow!(
                "release_clean: shell depleted ({shell} < {MIN_DEPLOY_SHELL} minimum)"
            ));
        }

        let balances = balance_by_tt(&fields);
        self.ledger.with_txn(|f| {
            f.notes.insert(addr, NoteState::Free { ecc_shell_remaining: Some(shell), balances });
        })?;
        self.released = true;
        Ok(())
    }

    /// Quarantines the note under an explicit, known reason instead of
    /// waiting for `Drop`'s generic one. Best-effort: if the ledger write
    /// itself fails, `released` is left `false` so `Drop` gets a fallback
    /// attempt, for the same reason [`LeasedPn::finish`] leaves it `false` on
    /// a failed write.
    pub fn taint(mut self, reason: TaintReason) {
        let addr = self.note.address.clone();
        let reason_str = taint_reason_string(&reason);
        let wrote = self.ledger.with_txn(|f| {
            f.notes.insert(addr, NoteState::Quarantined { reason: reason_str });
        });
        if wrote.is_ok() {
            self.released = true;
        }
    }
}

impl Drop for LeasedPn {
    /// No network I/O — `Drop` is synchronous and may run during a runtime
    /// shutdown. A note not explicitly released or tainted is quarantined
    /// under the generic reason `"drop"`: the point of quarantining at all is
    /// that an unknown-state note must never be handed out again, and a test
    /// that panicked or was killed before calling `release_clean`/`taint` is
    /// exactly that unknown state.
    ///
    /// If the ledger write fails — most plausibly `StaleRun`, this process's
    /// generation having already been superseded — this does nothing at all
    /// rather than panic: a panic inside `drop` during unwinding aborts the
    /// process, which a destructor must never risk.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let addr = self.note.address.clone();
        let _ = self.ledger.with_txn(|f| {
            f.notes.insert(addr, NoteState::Quarantined { reason: "drop".to_string() });
        });
    }
}

/// The logical `_balance` storage field, keyed by token type — what
/// [`LeasedPn::finish`] records into `NoteState::Free.balances` on a clean
/// release. Recorded as-is rather than required to be zero: balance drift
/// (a test left some residual holding behind) is information to carry
/// forward, not a reason to quarantine — that judgment already happened via
/// `sweep_verdict` above.
fn balance_by_tt(fields: &Value) -> BTreeMap<u32, u128> {
    let Some(obj) = fields.get("_balance").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| {
            let tt: u32 = k.parse().ok()?;
            let amount: u128 = v.as_str()?.parse().ok()?;
            Some((tt, amount))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn sweep_flags_nonempty_stakes_and_true_flags() {
        // Boolean fields are exercised with a real false/true, not just
        // presence/absence.
        let v = serde_json::json!({ "_stakes": {"0":"5"}, "_hasWithdrawn": true,
            "_pendingBatchActive": false, "_pendingForfeit": false, "_busy": null, "_debt": "0" });
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_stakes".to_string()));
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
            }
            _ => panic!("должен быть Dirty"),
        }
    }

    /// The ABI declares each field's ("fields" section) name and Solidity
    /// type; this reads that map from the same ABI file the completeness
    /// test parses.
    fn private_note_abi_types() -> std::collections::HashMap<String, String> {
        let abi: serde_json::Value =
            serde_json::from_str(include_str!("../../../../contracts/dex/PrivateNote.abi.json"))
                .unwrap();
        abi["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (f["name"].as_str().unwrap().to_string(), f["type"].as_str().unwrap().to_string())
            })
            .collect()
    }

    /// The JSON shape `tvm_abi`'s detokenizer actually produces for a
    /// zero/empty value of the given ABI type — NOT a guess. `uint256`
    /// encodes to a zero-padded hex string, every narrower unsigned width to
    /// a plain decimal string, maps/arrays to an empty object/array,
    /// `optional(..)` to `null` when absent, `bool` to `false`. Panics on an
    /// ABI type this mapping doesn't cover yet, so a future field of a new
    /// shape fails loudly here instead of silently building a test fixture
    /// the real decoder could never produce.
    fn clean_value_for_abi_type(abi_type: &str) -> serde_json::Value {
        if abi_type.ends_with("[]") {
            serde_json::json!([])
        } else if abi_type.starts_with("map(") {
            serde_json::json!({})
        } else if abi_type.starts_with("optional(") {
            serde_json::Value::Null
        } else if abi_type == "bool" {
            serde_json::json!(false)
        } else if abi_type == "uint256" {
            serde_json::json!(format!("0x{}", "0".repeat(64)))
        } else if abi_type.starts_with("uint") {
            serde_json::json!("0")
        } else {
            panic!("clean_value_for_abi_type: unhandled ABI type `{abi_type}` — extend the mapping")
        }
    }

    #[test]
    fn sweep_clean_note_passes() {
        // Built from the ABI's actual field types (via `clean_value_for_abi_type`),
        // not from a guessed shape — this is what caught the original bug: a
        // hardcoded `null` for every field can never surface a `uint256`
        // field's real hex-zero encoding.
        let types = private_note_abi_types();
        let mut m = serde_json::Map::new();
        for f in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
            let ty = types.get(*f).unwrap_or_else(|| panic!("field {f} missing from ABI"));
            m.insert((*f).into(), clean_value_for_abi_type(ty));
        }
        assert!(matches!(sweep_verdict(&serde_json::Value::Object(m)), SweepVerdict::Clean));
    }

    #[test]
    fn sweep_uint256_hex_zero_is_clean() {
        // `_pendingBatchStakeHash` is the sweep list's one `uint256` field.
        // Regression guard for the bug `sweep_clean_note_passes` could not
        // catch on its own: a lone all-zero hex string must be judged clean,
        // the same as the decimal `"0"` the narrower widths use.
        assert!(is_empty_or_zero(&serde_json::json!(format!("0x{}", "0".repeat(64)))));
        assert!(!is_empty_or_zero(&serde_json::json!(format!("0x{}1", "0".repeat(63)))));
    }

    #[test]
    fn sweep_bool_field_rejects_non_boolean_shapes() {
        // `SWEEP_MUST_BE_FALSE` requires the JSON boolean `false` exactly —
        // a string or a numeric zero must not be accepted as a stand-in.
        let v = serde_json::json!({ "_hasWithdrawn": "false", "_hasTransferred": 0 });
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
                assert!(fields.contains(&"_hasTransferred".to_string()));
            }
            _ => panic!("string/number stand-ins for false must be Dirty"),
        }
    }

    #[test]
    fn sweep_missing_required_field_fails() {
        // Fail-closed: a key named by a sweep list but absent from the
        // decoded fields entirely is NOT equivalent to a clean null. Checked
        // for one field from each list — the zero list and the boolean list
        // use different clean-value checks, so a missing key has to be
        // caught on both paths, not just one.
        let v = serde_json::json!({ "_stakes": null }); // no other keys at all
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_busy".to_string()));
                assert!(fields.contains(&"_hasWithdrawn".to_string()));
            }
            _ => panic!("отсутствующее обязательное поле обязано давать Dirty"),
        }
    }

    /// Every field the `PrivateNote` ABI declares must be explicitly
    /// classified: in one of the two sweep lists above, or in
    /// `ALLOWED_NON_SWEEP` here (immutable/code fields, plus gosh's
    /// replay-protection bookkeeping that mutates on every operation but
    /// doesn't block reuse). A contract upgrade that adds a new stateful
    /// field fails this test until a human decides, by name, which bucket
    /// it belongs to — the alternative is a silent hole: `sweep_verdict`
    /// would never look at the new field, and a dirty account would start
    /// passing as clean.
    #[test]
    fn sweep_lists_cover_private_note_abi_fields() {
        // Closed against the ground-truth ABI on this branch: 53 fields =
        // 25 must-be-empty-or-zero + 4 must-be-false + 24 allowed. This ABI
        // has no `_timestamp` field; instead it carries `messages` /
        // `lastMessage` — gosh replay-protection state that mutates during
        // operations but does not block reuse.
        const ALLOWED_NON_SWEEP: &[&str] = &[
            "_pubkey",
            "_constructorFlag",
            "messages",
            "lastMessage", // system bookkeeping
            "_depositIdentifierHash",
            "_ephemeralPubkey",
            "_balance",
            "_pmpCode",
            "_privateNoteCode",
            "_orderBookCode",
            "_inferenceOrderBookCode",
            "_oracleCodeHash",
            "_oracleCodeDepth",
            "_oracleEventListCodeHash",
            "_oracleEventListCodeDepth",
            "_tokenContractCodeHash",
            "_tokenContractCodeDepth",
            "_rootModelCodeHash",
            "_rootModelCodeDepth",
            "_debtTokenType",
            "_couponsTokenType",
            "_lastStreamLockChange",
            "_lastHash", // hash of the last external message (replay guard); survives operations
            "_opNonce",  // monotonic count of all operations ever run; nonzero on a reused note
        ];
        let abi_fields: Vec<String> = private_note_abi_types().into_keys().collect();
        for f in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
            assert!(
                abi_fields.contains(&f.to_string()),
                "поле {f} из sweep-списка отсутствует в ABI — обновить список"
            );
        }
        for f in &abi_fields {
            let known = SWEEP_MUST_BE_EMPTY_OR_ZERO.contains(&f.as_str())
                || SWEEP_MUST_BE_FALSE.contains(&f.as_str())
                || ALLOWED_NON_SWEEP.contains(&f.as_str());
            assert!(
                known,
                "НОВОЕ stateful-поле ABI {f} не классифицировано sweep'ом — молчаливая дыра запрещена"
            );
        }
    }

    // ---- Allocator: rent / taint / release_clean ----------------------

    /// Mirrors the real `dex_test_notes.keys.json` shape exactly (see
    /// `SeedNoteFile` above): a short `pn_dih_hex`, and extra fields
    /// (`tokenType`, `value`) the parser must tolerate rather than choke on.
    fn fake_seed(dir: &Path, n: usize) -> PathBuf {
        let rows: Vec<_> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "pn_dih_hex": format!("0x{i:x}"),
                    "pn_address": format!("0:{:064x}", i),
                    "tokenType": 1, "value": 1_000_000_000_000u64,
                    "pn_pubkey_hex": format!("{:064x}", i + 1),
                    "pn_seckey_hex": format!("{:064x}", i + 100),
                })
            })
            .collect();
        let p = dir.join("dex_test_notes.keys.json");
        std::fs::write(&p, serde_json::to_vec(&rows).unwrap()).unwrap();
        p
    }

    /// Recovers the index `fake_seed` encoded into an address
    /// (`0:{index:064x}`) so a test can assert on which slot got rented
    /// without hardcoding an address string.
    fn index_of(address: &str) -> usize {
        let hex = address.trim_start_matches("0:");
        usize::from_str_radix(hex, 16).unwrap()
    }

    #[test]
    fn rent_serves_only_tail_indices() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r1", None).unwrap();
        let a = Allocator::with_seed_path("r1", &seed).unwrap();
        let l1 = a.rent(PnProfile::Dep, "t").unwrap();
        let l2 = a.rent(PnProfile::Trd, "t").unwrap();
        let l3 = a.rent(PnProfile::Trd, "t").unwrap();
        for l in [&l1, &l2, &l3] {
            let idx = index_of(&l.note.address); // адреса fake-сида кодируют индекс
            assert!(idx >= 7, "голова (0..6) зарезервирована за api-e2e");
        }
        assert!(a.rent(PnProfile::Trd, "t").is_err(), "хвост исчерпан — 4-й rent обязан упасть");
    }

    #[test]
    fn drop_without_release_quarantines() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r2", None).unwrap();
        let a = Allocator::with_seed_path("r2", &seed).unwrap();

        let dropped_addr = {
            let l = a.rent(PnProfile::Trd, "t").unwrap();
            l.note.address.clone()
        }; // `l` drops here — no release_clean, no taint.

        // Structural proof: the ledger records it Quarantined with the
        // generic drop reason, not merely "some other state".
        let led = Ledger::open(d.path(), "r2");
        led.with_txn(|f| {
            let reason = match f.notes.get(&dropped_addr) {
                Some(NoteState::Quarantined { reason }) => reason.clone(),
                _ => panic!("dropped note must be Quarantined, not left Free or Leased"),
            };
            assert_eq!(reason, "drop");
        })
        .unwrap();

        // Behavioral proof: draining the rest of the tail through the same
        // public API a real scenario would use never hands this address back
        // out — unavailable, not merely "changed".
        let mut rented = Vec::new();
        while let Ok(l) = a.rent(PnProfile::Trd, "t2") {
            rented.push(l.note.address.clone());
        }
        assert_eq!(
            rented.len(),
            2,
            "tail has 3 slots; one is quarantined, so exactly 2 remain rentable"
        );
        assert!(!rented.contains(&dropped_addr), "a quarantined note must never be rented again");
    }

    #[test]
    fn taint_reason_recorded() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r3", None).unwrap();
        let a = Allocator::with_seed_path("r3", &seed).unwrap();
        let l = a.rent(PnProfile::Shell, "t").unwrap();
        let addr = l.note.address.clone();
        l.taint(TaintReason::ShellDepleted);

        let led = Ledger::open(d.path(), "r3");
        led.with_txn(|f| {
            let reason = match f.notes.get(&addr) {
                Some(NoteState::Quarantined { reason }) => reason.clone(),
                _ => panic!("expected the note to be Quarantined after taint()"),
            };
            assert_eq!(reason, "ShellDepleted");
        })
        .unwrap();
    }

    /// Every sweep-listed field set to the clean value its real ABI type
    /// decodes to at rest (built from the helpers above, not hand-picked) —
    /// the base a `finish` test starts from before optionally dirtying or
    /// extending it.
    fn all_clean_fields() -> serde_json::Map<String, Value> {
        let types = private_note_abi_types();
        let mut m = serde_json::Map::new();
        for name in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
            let ty = types.get(*name).unwrap();
            m.insert((*name).to_string(), clean_value_for_abi_type(ty));
        }
        m
    }

    /// [`all_clean_fields`] with exactly one sweep-listed field overridden
    /// dirty, so a recorded reason can be checked precisely against that one
    /// field instead of a loose "contains something" guess.
    fn mostly_clean_fields(dirty_field: &str, dirty_value: Value) -> Value {
        let mut m = all_clean_fields();
        m.insert(dirty_field.to_string(), dirty_value);
        Value::Object(m)
    }

    #[test]
    fn dirty_release_keeps_reason_after_drop() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r4", None).unwrap();
        let a = Allocator::with_seed_path("r4", &seed).unwrap();
        let mut l = a.rent(PnProfile::Dep, "t").unwrap();
        let addr = l.note.address.clone();

        let dirty_fields = mostly_clean_fields("_stakes", serde_json::json!({ "0": "5" }));
        let ecc = AccountEcc { grams: 0, ecc: BTreeMap::new() };
        let err = l.finish(dirty_fields, ecc).unwrap_err();
        assert!(format!("{err}").contains("_stakes"));

        drop(l); // must NOT overwrite the reason `finish` already recorded.

        let led = Ledger::open(d.path(), "r4");
        led.with_txn(|f| {
            let reason = match f.notes.get(&addr) {
                Some(NoteState::Quarantined { reason }) => reason.clone(),
                _ => panic!("expected the note to remain Quarantined after drop"),
            };
            assert_ne!(reason, "drop", "drop overwrote the specific reason with the generic one");
            assert!(reason.contains("_stakes"), "reason lost the actual offending field: {reason}");
            for name in SWEEP_MUST_BE_EMPTY_OR_ZERO.iter().chain(SWEEP_MUST_BE_FALSE) {
                if *name != "_stakes" {
                    assert!(
                        !reason.contains(name),
                        "unexpected field `{name}` reported dirty: {reason}"
                    );
                }
            }
        })
        .unwrap();
    }

    #[test]
    fn shell_depleted_release_quarantines_and_survives_drop() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r5", None).unwrap();
        let a = Allocator::with_seed_path("r5", &seed).unwrap();
        let mut l = a.rent(PnProfile::Trd, "t").unwrap();
        let addr = l.note.address.clone();

        let clean_fields = Value::Object(all_clean_fields());
        let ecc = AccountEcc { grams: 0, ecc: [(CURRENCY_ID_SHELL, MIN_DEPLOY_SHELL - 1)].into() };

        let err = l.finish(clean_fields, ecc).unwrap_err();
        assert!(format!("{err}").contains("shell depleted"));

        drop(l); // must NOT overwrite the ShellDepleted reason with "drop".

        let led = Ledger::open(d.path(), "r5");
        led.with_txn(|f| {
            let reason = match f.notes.get(&addr) {
                Some(NoteState::Quarantined { reason }) => reason.clone(),
                _ => panic!("expected the note to remain Quarantined after drop"),
            };
            assert_eq!(reason, "ShellDepleted");
        })
        .unwrap();
    }

    #[test]
    fn release_clean_records_free_with_balances() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r6", None).unwrap();
        let a = Allocator::with_seed_path("r6", &seed).unwrap();
        let mut l = a.rent(PnProfile::Trd, "t").unwrap();
        let addr = l.note.address.clone();

        // `_balance` isn't sweep-listed at all (`ALLOWED_NON_SWEEP`), so it's
        // free to hold a real, nonzero value without being judged Dirty for it.
        let mut m = all_clean_fields();
        m.insert("_balance".to_string(), serde_json::json!({ "1": "42" }));
        let clean_fields = Value::Object(m);
        let ecc = AccountEcc { grams: 0, ecc: [(CURRENCY_ID_SHELL, MIN_DEPLOY_SHELL + 1)].into() };

        l.finish(clean_fields, ecc).unwrap();

        let led = Ledger::open(d.path(), "r6");
        led.with_txn(|f| match f.notes.get(&addr) {
            Some(NoteState::Free { ecc_shell_remaining, balances }) => {
                assert_eq!(*ecc_shell_remaining, Some(MIN_DEPLOY_SHELL + 1));
                assert_eq!(balances.get(&1), Some(&42));
            }
            _ => panic!("expected the note back as Free after a clean release"),
        })
        .unwrap();
    }
}
