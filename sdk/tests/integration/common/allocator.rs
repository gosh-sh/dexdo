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
//!   `uint256` hex string), `0`, or a map/array holding nothing but
//!   empty-or-zero values — recursively, so a map the contract zeroed in
//!   place rather than deleted (`_lockedInOrders[tt] = 0`, still present as
//!   a key) counts the same as one that was never populated.
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
//! `PrivateNote`s (`dex_test_notes.keys.json`), registers the slots it owns as
//! `Free` in the shared [`Ledger`], and hands out leases from those only — the
//! rest of the pool belongs to the api-e2e suite, which reads the same file.
//! Which slots those are is decided by the pool itself
//! ([`sdk_owned_indices`]): the notes labelled with a role this harness knows
//! when the pool declares profiles, the index tail when it does not. A leased note comes
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
use dodex_infrastructure::tvm_runner::account_boc_is_none;
use dodex_infrastructure::tvm_runner::decode_account_ecc;
use dodex_infrastructure::tvm_runner::decode_account_fields_json;
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

/// `null`, an all-zero numeric string (decimal or hex), `0`, an empty
/// string, or a map/array every one of whose *values* is itself
/// empty-or-zero — checked recursively, with the empty map/array as the base
/// case.
///
/// Two encoding quirks both fall out of the same general rule instead of a
/// per-field special case:
///
/// - **Scalar zero isn't always `"0"`.** `tvm_abi`'s detokenizer does not
///   encode every integer width the same way: a `uint256` decodes to a
///   zero-padded hex string (`"0x" + 64 hex digits`, per
///   `detokenize_big_uint`), while every narrower unsigned width decodes to a
///   plain decimal string. Matching only the literal `"0"` would accept a
///   narrow-width zero but reject a genuine `uint256` zero
///   (`"0x000…000"`), which made every real note permanently Dirty on any
///   `uint256` field in the sweep list (`_pendingBatchStakeHash`) — fixed by
///   stripping an optional `0x` and checking the remainder is non-empty and
///   all `'0'`, so the next `uint256` sweep field is handled automatically.
/// - **A cleared map entry is a zero value, not a missing key.**
///   `PrivateNote.sol` never `delete`s the maps in the sweep list; it zeroes
///   entries in place (`_lockedInOrders[tokenType] = 0`,
///   `_openOrdersByEvent[hash] -= 1` down to `0`, …). A note that finished a
///   full trade cycle therefore decodes those fields as maps that still
///   *contain* keys, whose values are zero — requiring the container itself
///   to be empty made every such note permanently Dirty too. Recursing into
///   every value (map or array) instead of demanding the container be empty
///   is what makes "contains no non-zero value anywhere" — exactly what "no
///   locks held, nothing outstanding" means — hold for a map with cleared
///   entries, and for nested containers (a map of maps) with no extra case.
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
        Value::Array(a) => a.iter().all(is_empty_or_zero),
        Value::Object(m) => m.values().all(is_empty_or_zero),
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
const PN_ABI: &str = include_str!("../../../../contracts/dex/PrivateNote.abi.json");

/// The default tail size (see [`Allocator::new`]) when `E2E_SDK_TAIL_COUNT` is
/// unset.
const DEFAULT_SDK_TAIL: usize = 3;

/// The role a rented note is about to play in a scenario, and — on a pool
/// baked from a generator spec — the group of notes it may be drawn from.
/// [`PnProfile::seed_label`] is the whole coupling to the generator: the
/// label it returns is what `DEX_TEST_NOTES_SPEC` writes into each note's
/// `profile` field.
// Only Dep/Trd/Shell are constructed anywhere; Cons/Cpn/Usdc/Inf/Rot name the
// rest of the role vocabulary a scenario may declare.
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

impl PnProfile {
    /// Every role, for tests that must cover the whole vocabulary rather than
    /// whichever variants happened to be spelled out at the time.
    pub const ALL: &'static [PnProfile] = &[
        PnProfile::Dep,
        PnProfile::Trd,
        PnProfile::Cons,
        PnProfile::Cpn,
        PnProfile::Shell,
        PnProfile::Usdc,
        PnProfile::Inf,
        PnProfile::Rot,
    ];

    /// The `profile` string a spec-baked note carries for this role. Written
    /// as an exhaustive match on purpose: a new variant fails to compile here
    /// until someone decides what the generator calls it.
    pub fn seed_label(&self) -> &'static str {
        match self {
            PnProfile::Dep => "PN-DEP",
            PnProfile::Trd => "PN-TRD",
            PnProfile::Cons => "PN-CONS",
            PnProfile::Cpn => "PN-CPN",
            PnProfile::Shell => "PN-SHELL",
            PnProfile::Usdc => "PN-USDC",
            PnProfile::Inf => "PN-INF",
            PnProfile::Rot => "PN-ROT",
        }
    }

    /// The inverse of [`PnProfile::seed_label`]. `None` means the label is
    /// not one this harness owns — a note reserved for another suite — which
    /// is a routing fact, not an error: the api-e2e suite draws from the same
    /// seed file and its notes must simply never be leased here.
    pub fn from_seed_label(label: &str) -> Option<PnProfile> {
        PnProfile::ALL.iter().copied().find(|p| p.seed_label() == label)
    }
}

/// Why a note was pulled from the pool and will never be leased again this
/// generation. [`TaintReason::DirtyState`] and [`TaintReason::ShellDepleted`]
/// are the two reasons [`LeasedPn::release_clean`] can produce on its own; the
/// rest are for a caller that has already diagnosed the note some other way
/// (e.g. a scenario that knows a note it holds has outstanding debt) and wants
/// to record why via [`LeasedPn::taint`].
// Only Dead/DirtyState/ShellDepleted are constructed here: the remaining
// variants exist for a caller that has diagnosed a note some other way and
// records it via `LeasedPn::taint`.
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
///
/// `profile` is the label the generator's spec gave this note's group. It is
/// absent from a pool baked as N identical notes and `null` on rows the
/// generator wrote without a spec, so it is optional twice over — hence
/// `#[serde(default)]` on top of the `Option`.
#[derive(Deserialize)]
struct SeedNoteFile {
    pn_dih_hex: String,
    pn_address: String,
    pn_pubkey_hex: String,
    pn_seckey_hex: String,
    #[serde(default)]
    profile: Option<String>,
}

/// One pre-baked account from the seed file, ready to be leased.
#[derive(Clone)]
pub struct SeedNote {
    pub address: String,
    /// Decimal form of `pn_dih_hex` — what the contracts' getters and inputs
    /// expect.
    pub dih_dec: String,
    pub keys: KeyPair,
    /// The label the generator's spec gave this note, verbatim — `None` for a
    /// pool baked as N identical notes. Kept as the raw string rather than a
    /// [`PnProfile`] because a label this harness does not recognise is
    /// meaningful (it marks a note another suite owns), not a parse error.
    pub profile: Option<String>,
}

/// Parses the seed file at `seed` into every note it declares, in file order.
///
/// Split out of [`Allocator::with_seed_path_and_tail`] so a caller that needs
/// the pool's *contents* without the pool's *lifecycle* — preflight, which
/// validates every baked note and must not open or mutate the shared ledger —
/// reads the file through this one parser rather than a second copy of it.
pub fn load_seed_notes(seed: &Path) -> anyhow::Result<Vec<SeedNote>> {
    let bytes =
        std::fs::read(seed).with_context(|| format!("read seed notes file {}", seed.display()))?;
    let rows: Vec<SeedNoteFile> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse seed notes file {} as a JSON array", seed.display()))?;
    rows.into_iter()
        .map(|r| {
            Ok(SeedNote {
                address: r.pn_address,
                dih_dec: dih_hex_to_dec(&r.pn_dih_hex)?,
                keys: KeyPair { public: r.pn_pubkey_hex, secret: r.pn_seckey_hex },
                profile: r.profile,
            })
        })
        .collect()
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
pub fn seed_dir() -> anyhow::Result<PathBuf> {
    let path = seed_path()?;
    path.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("seed path {} has no parent directory", path.display()))
}

/// Pure core of [`sdk_tail_count`]: testable without touching process env,
/// the same split `context::endpoint_from`/`network_endpoint` already use.
/// Unset (`None`) defaults to [`DEFAULT_SDK_TAIL`]; a value that is set but
/// does not parse as a `usize` is an error, not a silent fall-through to the
/// default — `E2E_SDK_TAIL_COUNT=oops` is a pool-sizing typo, and defaulting
/// through it would hide exactly the kind of misconfiguration this knob
/// exists to make explicit.
fn tail_count_from(v: Option<&str>) -> anyhow::Result<usize> {
    match v {
        None => Ok(DEFAULT_SDK_TAIL),
        Some(s) => s.parse().map_err(|e| {
            anyhow!("E2E_SDK_TAIL_COUNT `{s}` is not a valid non-negative integer: {e}")
        }),
    }
}

fn sdk_tail_count() -> anyhow::Result<usize> {
    tail_count_from(std::env::var("E2E_SDK_TAIL_COUNT").ok().as_deref())
}

/// Which slots of the pool this suite may register and lease — the pure core
/// of the head/tail split, decided from the pool's own composition.
///
/// Two pools reach the harness from the same generator and need different
/// rules, which is why this is a decision and not a slice:
///
/// - **No note declares a profile** (`DEX_TEST_NOTES_CNT`): every note is
///   interchangeable, so nothing but position can separate this suite from
///   the api-e2e head — the last `tail` slots, exactly as before profiles
///   existed.
/// - **Notes declare profiles** (`DEX_TEST_NOTES_SPEC`): position stops
///   meaning anything and `tail` is ignored. This suite owns the notes whose
///   label it recognises ([`PnProfile::from_seed_label`]); a label it does
///   not own marks another suite's note and is skipped wherever it sits.
///
/// A pool that mixes the two — some labels, some `null` — is treated as
/// profiled, and the unlabelled notes are owned by nobody. The generator does
/// not produce that shape; if it ever does, leaving those notes alone is the
/// direction that cannot hand a scenario a note it did not ask for.
fn sdk_owned_indices(profiles: &[Option<&str>], tail: usize) -> Vec<usize> {
    if profiles.iter().all(Option::is_none) {
        return (profiles.len().saturating_sub(tail)..profiles.len()).collect();
    }
    profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_some_and(|l| PnProfile::from_seed_label(l).is_some()))
        .map(|(i, _)| i)
        .collect()
}

/// Rents pre-baked `PrivateNote`s out of a shared pool for the lifetime of an
/// e2e run. Only the slots this suite owns are ever leased — the rest belong
/// to the api-e2e suite, which draws from the same seed file independently.
/// [`sdk_owned_indices`] decides which those are: labels when the pool
/// declares them, the index tail when it does not.
pub struct Allocator {
    /// Every seed note, in file order.
    notes: Vec<SeedNote>,
    ledger: Arc<Ledger>,
    /// Indices into `notes` this suite may lease, ascending.
    owned: Vec<usize>,
    /// Whether the pool declares profiles at all. Decides whether a `rent`
    /// request is a filter (spec-baked pool) or just a role annotation
    /// (`DEX_TEST_NOTES_CNT` pool, where every note is interchangeable).
    profiled: bool,
}

impl Allocator {
    /// Thin env wrapper: resolves the seed path via [`seed_path`] and defers
    /// to [`Allocator::with_seed_path`].
    pub fn new(run_id: &str) -> anyhow::Result<Allocator> {
        let seed = seed_path()?;
        Allocator::with_seed_path(run_id, &seed)
    }

    /// Loads the seed file at an explicit path, with the tail size read from
    /// `E2E_SDK_TAIL_COUNT` (see [`sdk_tail_count`]) — thin wrapper over
    /// [`Allocator::with_seed_path_and_tail`].
    pub fn with_seed_path(run_id: &str, seed: &Path) -> anyhow::Result<Allocator> {
        Allocator::with_seed_path_and_tail(run_id, seed, sdk_tail_count()?)
    }

    /// Loads the seed file at an explicit path with an explicit tail size —
    /// testable without mutating process environment (`std::env::set_var` is
    /// `unsafe` as of Rust 2024) *and* without depending on whatever
    /// `E2E_SDK_TAIL_COUNT` happens to be set to in the ambient environment
    /// a test runs in. `with_seed_path` exists so a real e2e run doesn't have
    /// to pass the tail explicitly, but every hermetic test in this file goes
    /// through here with a literal tail so `E2E_SDK_TAIL_COUNT` can never
    /// change what they assert. The ledger lives in `seed`'s own directory,
    /// exactly like [`Allocator::new`]'s env-driven path.
    ///
    /// Registration is idempotent and additive only: [`Ledger::bootstrap`]
    /// starts a generation with an empty `notes` map, so without this no
    /// `rent` would ever find a `Free` note. This does one [`Ledger::with_txn`]
    /// that inserts a `Free` entry for every owned address *absent* from the
    /// map; an address already present — `Leased`, `Quarantined`, or `Free`
    /// from an earlier process's registration this same generation — is left
    /// untouched. That "absent only" rule is what keeps a quarantine from a
    /// prior run inside this generation from reviving just because a later
    /// process re-registers.
    ///
    /// On an unprofiled pool, errors if it has fewer notes than `tail`: a tail
    /// that reaches past the intended boundary would silently register, rent,
    /// and quarantine head slots reserved for the api-e2e suite — a pool sized
    /// wrong for the configured tail is a configuration error, not something
    /// to degrade through quietly. A profiled pool has no such boundary to
    /// overrun, `tail` decides nothing there, and its size is checked by
    /// nothing but a scenario failing to find the role it asked for.
    pub fn with_seed_path_and_tail(
        run_id: &str,
        seed: &Path,
        tail: usize,
    ) -> anyhow::Result<Allocator> {
        let notes = load_seed_notes(seed)?;
        let profiles: Vec<Option<&str>> = notes.iter().map(|n| n.profile.as_deref()).collect();
        let profiled = profiles.iter().any(Option::is_some);

        anyhow::ensure!(
            profiled || notes.len() >= tail,
            "seed file {} has {} note(s), fewer than the requested tail of {tail} — \
             the tail would reach into the api-e2e head",
            seed.display(),
            notes.len()
        );

        let owned = sdk_owned_indices(&profiles, tail);

        let dir = seed
            .parent()
            .ok_or_else(|| anyhow!("seed path {} has no parent directory", seed.display()))?;
        let ledger = Arc::new(Ledger::open(dir, run_id));

        let owned_addrs: Vec<String> = owned.iter().map(|&i| notes[i].address.clone()).collect();
        ledger.with_txn(|f| {
            for addr in &owned_addrs {
                f.notes.entry(addr.clone()).or_insert_with(|| NoteState::Free {
                    ecc_shell_remaining: None,
                    balances: BTreeMap::new(),
                });
            }
        })?;

        Ok(Allocator { notes, ledger, owned, profiled })
    }

    /// The notes this suite may lease, in file order.
    fn owned_notes(&self) -> impl Iterator<Item = &SeedNote> {
        self.owned.iter().map(|&i| &self.notes[i])
    }

    /// How many notes the pool holds per declared label — the evidence a
    /// failed `rent` reports instead of a bare "none left". Counts the whole
    /// pool, not just what this suite owns, so a request that fails because
    /// every matching note belongs to another suite says so.
    fn label_census(&self) -> String {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for n in &self.notes {
            *counts.entry(n.profile.as_deref().unwrap_or("<unprofiled>")).or_default() += 1;
        }
        counts.iter().map(|(l, c)| format!("{l}x{c}")).collect::<Vec<_>>().join(", ")
    }

    /// Leases the first `Free` note that can play `profile`, in file order,
    /// marking it `Leased { pid, test }` in the same transaction that reads it
    /// — two concurrent `rent` calls can never be handed the same address.
    ///
    /// What "can play `profile`" means depends on the pool. On a spec-baked
    /// pool it is the notes labelled with that role and nothing else: a note
    /// baked for another role holds the wrong token type, or no SHELL, and
    /// handing it over would fail the scenario several steps later, somewhere
    /// that says nothing about the pool. On a `DEX_TEST_NOTES_CNT` pool every
    /// note is identical, so the role is an annotation and every owned note
    /// qualifies.
    pub fn rent(&self, profile: PnProfile, test: &str) -> anyhow::Result<LeasedPn> {
        let pid = std::process::id();
        let want = if self.profiled { Some(profile.seed_label()) } else { None };
        let candidates: Vec<&SeedNote> = self
            .owned_notes()
            .filter(|n| match want {
                None => true,
                Some(label) => n.profile.as_deref() == Some(label),
            })
            .collect();
        let picked = self.ledger.with_txn(|f| {
            for note in &candidates {
                if matches!(f.notes.get(&note.address), Some(NoteState::Free { .. })) {
                    f.notes.insert(
                        note.address.clone(),
                        NoteState::Leased { pid, test: test.to_string() },
                    );
                    return Some((*note).clone());
                }
            }
            None
        })?;
        let note = picked.ok_or_else(|| match want {
            Some(label) => anyhow!(
                "allocator: no Free note with profile `{label}` for test `{test}` — \
                 the pool holds {}, of which {} carry that label",
                self.label_census(),
                candidates.len()
            ),
            None => anyhow!(
                "allocator: no Free note left in the {}-slot tail for test `{test}`",
                candidates.len()
            ),
        })?;
        Ok(LeasedPn { note, ledger: Arc::clone(&self.ledger), released: false })
    }

    /// Allocates the next unique nonce for this run's ledger generation —
    /// see [`Ledger::next_nonce`].
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
    /// Fetches the note's current raw account state once. A structurally
    /// absent account — [`boc_is_absent`], the harness's single notion of
    /// "not there" (no account at all, or a BOC that decodes to
    /// `AccountNone`) — is quarantined `Dead` right here rather than falling
    /// through to `finish`, which has no fields to judge for an account that
    /// no longer exists. A transient fetch error propagates via `?` before
    /// either check runs, so a flaky RPC still falls through to `Drop`'s
    /// generic reason instead of being mislabeled as a destroyed account.
    /// Otherwise, decodes storage fields and ECC balance from the one fetched
    /// BOC and hands both to the synchronous, network-free core
    /// ([`LeasedPn::finish`]) that judges and records the outcome. Splitting
    /// fetch from decision this way is what makes the decision itself
    /// unit-testable without a network.
    pub async fn release_clean(mut self, r: &ChainReader) -> anyhow::Result<()> {
        let boc = r.account_boc(&self.note.address).await?;
        if boc_is_absent(boc.as_deref())? {
            let addr = self.note.address.clone();
            let reason = taint_reason_string(&TaintReason::Dead);
            self.ledger.with_txn(|f| {
                f.notes.insert(addr, NoteState::Quarantined { reason });
            })?;
            self.released = true;
            return Err(anyhow!(
                "release_clean: note {} no longer exists on chain",
                self.note.address
            ));
        }
        let boc = boc.expect("boc_is_absent(.., false) implies a Some boc");
        let fields = decode_account_fields_json(PN_ABI, &boc)
            .with_context(|| format!("decode storage fields of {}", self.note.address))?;
        let ecc = decode_account_ecc(&boc)
            .with_context(|| format!("decode physical balance of {}", self.note.address))?;
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

/// Whether `boc` — the account's raw state, or `None` if the gateway has no
/// account at all — is structurally absent: the harness's single notion of
/// "not there" (see `ChainReader::account_absent`), reused rather than
/// reinvented. `None` and a BOC that decodes to `AccountNone` both count;
/// neither is matched by error text — [`account_boc_is_none`] decides it
/// structurally, straight off the decoded account.
fn boc_is_absent(boc: Option<&str>) -> anyhow::Result<bool> {
    match boc {
        None => Ok(true),
        Some(b) => account_boc_is_none(b),
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
        // `clean_value_for_abi_type` defaults every map to `{}` — the shape
        // a note that has NEVER traded decodes to. `_lockedInOrders` and
        // `_openOrdersByEvent` are never `delete`d by `PrivateNote.sol`; it
        // zeros entries in place (`_lockedInOrders[tt] = 0`,
        // `_openOrdersByEvent[hash] -= 1` down to 0), so a note that
        // finished a full trade cycle really decodes these still holding
        // keys, at zero. Overriding them to that real post-trade shape here
        // is what would have caught the container-recursion bug: a map
        // required to be empty, rather than recursively all-zero, made a
        // real note like this permanently Dirty.
        m.insert("_lockedInOrders".to_string(), serde_json::json!({ "1": "0" }));
        m.insert("_openOrdersByEvent".to_string(), serde_json::json!({ "77": "0" }));
        assert!(matches!(sweep_verdict(&serde_json::Value::Object(m)), SweepVerdict::Clean));
    }

    #[test]
    fn sweep_map_with_a_nonzero_entry_is_dirty_and_named() {
        // The opposite direction of the post-trade shape above: a map
        // holding a real, non-zero value must still fail, and the
        // offending field must still be named — otherwise the recursive
        // fix could silently degrade into "every map is clean".
        let v = mostly_clean_fields("_lockedInOrders", serde_json::json!({ "1": "5" }));
        match sweep_verdict(&v) {
            SweepVerdict::Dirty { fields } => {
                assert!(fields.contains(&"_lockedInOrders".to_string()))
            }
            _ => panic!("a locked, nonzero order-book entry must be Dirty"),
        }
    }

    #[test]
    fn sweep_nested_map_of_zeros_is_clean_but_one_nonzero_leaf_is_not() {
        // Recursion is the point of the fix, not one level of unwrapping:
        // `_orderLocks` / `_orderFeeReserves` are `map(address, map(uint128,
        // uint128))`. A one-level-deep implementation (check the outer
        // map's values are scalars, forget maps can nest) would pass a test
        // that only exercises one level, so this pins two levels directly
        // against `is_empty_or_zero`, independent of the ABI/allocator
        // plumbing the other tests go through.
        let all_zero_nested = serde_json::json!({ "0:aaaa": { "1": "0", "2": "0" } });
        assert!(is_empty_or_zero(&all_zero_nested));

        let one_nonzero_leaf = serde_json::json!({ "0:aaaa": { "1": "0", "2": "9" } });
        assert!(!is_empty_or_zero(&one_nonzero_leaf));
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
        // Closed against the ground-truth ABI on this branch: 48 fields =
        // 21 must-be-empty-or-zero + 4 must-be-false + 23 allowed. This ABI
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

    /// `n` notes with no declared profile — the `DEX_TEST_NOTES_CNT` pool.
    fn fake_seed(dir: &Path, n: usize) -> PathBuf {
        fake_seed_profiled(dir, &vec![None; n])
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
        let a = Allocator::with_seed_path_and_tail("r1", &seed, 3).unwrap();
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
        let a = Allocator::with_seed_path_and_tail("r2", &seed, 3).unwrap();

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
        let a = Allocator::with_seed_path_and_tail("r3", &seed, 3).unwrap();
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
        let a = Allocator::with_seed_path_and_tail("r4", &seed, 3).unwrap();
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
        let a = Allocator::with_seed_path_and_tail("r5", &seed, 3).unwrap();
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
        let a = Allocator::with_seed_path_and_tail("r6", &seed, 3).unwrap();
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

    #[test]
    fn quarantine_survives_reregistration_by_a_new_allocator() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 10);
        Ledger::bootstrap(d.path(), "r7", None).unwrap();

        let a1 = Allocator::with_seed_path_and_tail("r7", &seed, 3).unwrap();
        let l = a1.rent(PnProfile::Trd, "t").unwrap();
        let addr = l.note.address.clone();
        l.taint(TaintReason::Dead);

        // A second construction over the SAME directory/generation must not
        // revive the quarantine — registration only fills in genuinely
        // absent addresses (see `with_seed_path_and_tail`'s doc comment).
        let a2 = Allocator::with_seed_path_and_tail("r7", &seed, 3).unwrap();

        let led = Ledger::open(d.path(), "r7");
        led.with_txn(|f| {
            assert!(
                matches!(f.notes.get(&addr), Some(NoteState::Quarantined { .. })),
                "re-registration must not revive a quarantined note"
            );
        })
        .unwrap();

        // Behavioral proof, through the real API: draining `a2`'s tail must
        // never hand the quarantined address back out.
        let mut rented = Vec::new();
        while let Ok(l) = a2.rent(PnProfile::Trd, "t2") {
            rented.push(l.note.address.clone());
        }
        assert!(!rented.contains(&addr), "quarantine revived after re-registration");
        assert_eq!(
            rented.len(),
            2,
            "tail has 3 slots; one is quarantined, so exactly 2 remain rentable"
        );
    }

    #[test]
    fn with_seed_path_and_tail_rejects_a_pool_smaller_than_the_tail() {
        let d = TempDir::new().unwrap();
        let seed = fake_seed(d.path(), 2);
        Ledger::bootstrap(d.path(), "r8", None).unwrap();
        let err = match Allocator::with_seed_path_and_tail("r8", &seed, 3) {
            Ok(_) => panic!("a 2-note pool must not satisfy a tail of 3"),
            Err(e) => e,
        };
        // Exact substrings, not bare digits — the seed path itself (a tempdir
        // name) can otherwise coincidentally contain '2' or '3' and pass
        // regardless of whether the message actually names either number.
        let msg = format!("{err}");
        assert!(msg.contains("has 2 note"), "error should name the pool size: {msg}");
        assert!(msg.contains("tail of 3"), "error should name the requested tail: {msg}");
    }

    #[test]
    fn load_seed_notes_reads_the_declared_profile() {
        // The generator emits `profile` only when it baked the pool from a
        // spec; on the `DEX_TEST_NOTES_CNT` path the field is present but
        // `null`. Both shapes have to survive the same parser, because both
        // reach the harness from the same generator.
        let d = TempDir::new().unwrap();
        let rows = serde_json::json!([
            {
                "pn_dih_hex": "0x0", "pn_address": format!("0:{:064x}", 0),
                "profile": "PN-TRD", "tokenType": 1, "value": 1u64,
                "pn_pubkey_hex": format!("{:064x}", 1), "pn_seckey_hex": format!("{:064x}", 2),
            },
            {
                "pn_dih_hex": "0x1", "pn_address": format!("0:{:064x}", 1),
                "profile": serde_json::Value::Null, "tokenType": 1, "value": 1u64,
                "pn_pubkey_hex": format!("{:064x}", 3), "pn_seckey_hex": format!("{:064x}", 4),
            },
        ]);
        let p = d.path().join("dex_test_notes.keys.json");
        std::fs::write(&p, serde_json::to_vec(&rows).unwrap()).unwrap();

        let notes = load_seed_notes(&p).unwrap();
        assert_eq!(notes[0].profile.as_deref(), Some("PN-TRD"));
        assert_eq!(notes[1].profile, None, "a null profile is an unclassified note, not a label");
    }

    /// One note per entry, with that entry's declared profile — the shape the
    /// generator writes from a `DEX_TEST_NOTES_SPEC`. A `None` entry writes
    /// the row with no `profile` key at all, which is how a pool baked from
    /// `DEX_TEST_NOTES_CNT` reads back.
    ///
    /// Mirrors the real `dex_test_notes.keys.json` exactly (see `SeedNoteFile`
    /// above): a short `pn_dih_hex`, and extra fields (`tokenType`, `value`)
    /// the parser must tolerate rather than choke on.
    fn fake_seed_profiled(dir: &Path, labels: &[Option<&str>]) -> PathBuf {
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let mut row = serde_json::json!({
                    "pn_dih_hex": format!("0x{i:x}"),
                    "pn_address": format!("0:{:064x}", i),
                    "tokenType": 1, "value": 1_000_000_000_000u64,
                    "pn_pubkey_hex": format!("{:064x}", i + 1),
                    "pn_seckey_hex": format!("{:064x}", i + 100),
                });
                if let Some(l) = label {
                    row["profile"] = serde_json::json!(l);
                }
                row
            })
            .collect();
        let p = dir.join("dex_test_notes.keys.json");
        std::fs::write(&p, serde_json::to_vec(&rows).unwrap()).unwrap();
        p
    }

    #[test]
    fn rent_serves_the_note_carrying_the_requested_profile() {
        // The SHELL note sits in the middle, so picking "the first free one"
        // would return a NACKL note and the scenario would fail much later,
        // on a deploy, for reasons nobody would trace back to the allocator.
        let d = TempDir::new().unwrap();
        let seed = fake_seed_profiled(
            d.path(),
            &[Some("PN-API"), Some("PN-TRD"), Some("PN-SHELL"), Some("PN-TRD")],
        );
        Ledger::bootstrap(d.path(), "p1", None).unwrap();
        let a = Allocator::with_seed_path_and_tail("p1", &seed, 3).unwrap();

        let leased = a.rent(PnProfile::Shell, "t").unwrap();
        assert_eq!(index_of(&leased.note.address), 2);
    }

    #[test]
    fn rent_never_serves_a_note_owned_by_another_suite() {
        // Every note this harness owns is taken; the two PN-API notes are
        // still free, and must stay untouchable rather than become a
        // last-resort fallback.
        let d = TempDir::new().unwrap();
        let seed = fake_seed_profiled(
            d.path(),
            &[Some("PN-API"), Some("PN-TRD"), Some("PN-API"), Some("PN-TRD")],
        );
        Ledger::bootstrap(d.path(), "p2", None).unwrap();
        let a = Allocator::with_seed_path_and_tail("p2", &seed, 3).unwrap();

        let l1 = a.rent(PnProfile::Trd, "t").unwrap();
        let l2 = a.rent(PnProfile::Trd, "t").unwrap();
        for l in [&l1, &l2] {
            assert!(matches!(index_of(&l.note.address), 1 | 3));
        }
        assert!(a.rent(PnProfile::Trd, "t").is_err(), "PN-API notes are not a fallback");
    }

    #[test]
    fn rent_for_an_absent_profile_names_what_the_pool_actually_holds() {
        // A failure that only said "no free note" would send whoever hits it
        // looking for a leak. The pool census says the real thing: this pool
        // was never baked with that role.
        let d = TempDir::new().unwrap();
        let seed = fake_seed_profiled(d.path(), &[Some("PN-TRD"), Some("PN-TRD"), Some("PN-DEP")]);
        Ledger::bootstrap(d.path(), "p3", None).unwrap();
        let a = Allocator::with_seed_path_and_tail("p3", &seed, 3).unwrap();

        // `LeasedPn` is deliberately not `Debug` (it carries secret keys), so
        // the error comes out by match rather than `unwrap_err`.
        let err = match a.rent(PnProfile::Usdc, "t") {
            Ok(_) => panic!("a pool with no PN-USDC note must not satisfy a USDC request"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(msg.contains("PN-USDC"), "the requested label belongs in the message: {msg}");
        assert!(msg.contains("PN-TRD"), "so does what the pool holds instead: {msg}");
        assert!(msg.contains("PN-DEP"), "the whole census, not just the biggest group: {msg}");
    }

    #[test]
    fn a_profiled_pool_smaller_than_the_tail_still_loads() {
        // `tail` governs the unprofiled pool only. Rejecting a profiled pool
        // for being shorter than a number that no longer decides anything
        // would make the two paths disagree about what a valid pool is.
        let d = TempDir::new().unwrap();
        let seed = fake_seed_profiled(d.path(), &[Some("PN-TRD")]);
        Ledger::bootstrap(d.path(), "p4", None).unwrap();
        let a = Allocator::with_seed_path_and_tail("p4", &seed, 3).unwrap();
        assert_eq!(index_of(&a.rent(PnProfile::Trd, "t").unwrap().note.address), 0);
    }

    #[test]
    fn an_unprofiled_pool_still_serves_every_role_uniformly() {
        // No labels anywhere, so a role request is not a filter and must not
        // become one — a pool baked by note count, or any seed file predating
        // profiles, still has to serve every scenario.
        let d = TempDir::new().unwrap();
        let seed = fake_seed_profiled(d.path(), &[None, None, None, None]);
        Ledger::bootstrap(d.path(), "p5", None).unwrap();
        let a = Allocator::with_seed_path_and_tail("p5", &seed, 2).unwrap();

        assert!(a.rent(PnProfile::Usdc, "t").is_ok(), "an unlabelled note serves any role");
        assert!(a.rent(PnProfile::Shell, "t").is_ok());
        assert!(a.rent(PnProfile::Trd, "t").is_err(), "and the tail still bounds the pool");
    }

    /// What the scenarios in this crate rent at once, per role, and why that
    /// many. The stand's pool is baked from a spec checked into this repo, so
    /// this is the one place where the two can be compared — without it, a
    /// spec edit that drops a group surfaces as a scenario failing to rent,
    /// 20 minutes into a pipeline, on a stand that then has to be rebuilt.
    ///
    /// - `Dep` 10: `proof_money` rents one and gives it back, so it costs
    ///   nothing. `parallel_setup` rents two *at once* — one per process — and
    ///   quarantines both. `resting_orders`, `market_orders`,
    ///   `matching_ladder`, `shutdown_orders`, `price_above_par` and
    ///   `cancelled_event` deploy a market each and spend one apiece.
    ///   `bounce_deploy` takes two: one for the market it never freezes, and
    ///   one for the market that fails to come up — the second is offered
    ///   back, since a bounced deploy leaves the note where it found it.
    ///   `book_segments` and `mm_cycle` deploy one each, and `coupon_debt`
    ///   three — a coupon needs a note that has already lost a market, been
    ///   paid out of a second and bet its debt in a third, and none of the
    ///   three can be reused because a claim is what ends each one.
    ///   `forfeit_close` deploys one more, and spends it in the one way no
    ///   other scenario does: its creator forfeits rather than claims, which
    ///   is what closes the market. `oracle_quorum` deploys the seventeenth,
    ///   and is the only market-deploying scenario that rents no trader at
    ///   all — nothing stakes into it, because what it watches is the vote
    ///   that fails to execute. `multi_market` deploys two, because its whole
    ///   subject is one note holding positions in more than one at a time.
    ///   Two of the nineteen are gone before any of those steps starts.
    /// - `Trd` 13: `proof_money` takes two and returns both, and
    ///   `usdc_release` borrows one and returns it — none of that costs
    ///   anything. `resting_orders`, `market_orders`, `matching_ladder`,
    ///   `shutdown_orders` and `price_above_par` take two each and spend
    ///   them; `bounce_recovery` takes one more and spends it too —
    ///   `initTransfer` latches `_hasTransferred`, which the sweep counts as
    ///   dirty for good. `cancelled_event` takes the twelfth and
    ///   `bounce_deploy` the thirteenth; `book_segments` takes two more, one
    ///   per side of the orders it keeps apart, and `mm_cycle` two more
    ///   again, for the maker and the taker that lifts its quote.
    ///   `coupon_debt` takes one, and spends it in a way nothing else does:
    ///   the note ends holding a coupon and a debt, both of which the sweep
    ///   counts as dirty and neither of which any other scenario wants.
    ///   `forfeit_close` takes two: the one that walks away before the
    ///   resolve and the one that claims a winning outcome after it.
    ///   `multi_market` takes one and leaves it holding a stake and a resting
    ///   order in a market that outlives the scenario.
    /// - `Usdc` 1: `usdc_release`'s source note. Nothing returns it — a
    ///   withdrawn note latches `_hasWithdrawn` and is quarantined for good.
    ///
    /// The pool grows by three notes per market-deploying scenario, because a
    /// note that has traded holds a stake record and outcome tokens and can
    /// never be handed back clean. That is the sweep doing its job, not a
    /// defect — but it is the cost the spec pays per scenario, and it is why
    /// these numbers are derived here rather than rounded up.
    ///
    /// `cancelled_event` is budgeted the same way and may well cost nothing:
    /// it is the one scenario that unwinds everything it does — the stake is
    /// refunded, the record deleted, the order drained, the market gone — so
    /// both its notes have a real chance of passing the sweep. The spec is
    /// sized for them failing it anyway, because a pool sized on that hope is
    /// a pipeline that dies 20 minutes in when the hope turns out wrong.
    ///
    /// Counted as **spent per generation, not held at once**, which is the
    /// distinction that matters: the steps share one ledger generation
    /// (`E2E_RUN_ID` is the pipeline number, bootstrapped only by the proof
    /// step), and a quarantined note never comes back within it. A peak-
    /// concurrency figure would have said two `Dep` notes suffice, and the
    /// step that found out otherwise would have been a pipeline run.
    const SCENARIOS_RENT: &[(PnProfile, usize)] =
        &[(PnProfile::Dep, 19), (PnProfile::Trd, 21), (PnProfile::Usdc, 1)];

    /// The spec the e2e pipeline bakes the stand's note pool from.
    const STAND_NOTES_SPEC: &str = include_str!("../../../../tests/e2e/dex_test_notes.spec.json");

    fn stand_spec_counts() -> BTreeMap<String, u64> {
        let groups: Vec<Value> = serde_json::from_str(STAND_NOTES_SPEC).unwrap();
        groups
            .iter()
            .map(|g| (g["profile"].as_str().unwrap().to_string(), g["count"].as_u64().unwrap()))
            .collect()
    }

    #[test]
    fn the_stand_spec_supplies_every_role_the_scenarios_rent() {
        let counts = stand_spec_counts();
        for (profile, needed) in SCENARIOS_RENT {
            let have = counts.get(profile.seed_label()).copied().unwrap_or(0);
            assert!(
                have >= *needed as u64,
                "the stand spec declares {have} `{}` note(s); the scenarios hold {needed} at once",
                profile.seed_label()
            );
        }
    }

    #[test]
    fn every_label_in_the_stand_spec_belongs_to_a_suite() {
        // A typo'd label is not a parse error anywhere — it just bakes notes
        // nobody owns, and the suite that wanted them fails to rent much
        // later. `PN-API` is the api-e2e suite's; everything else has to be a
        // role this harness knows.
        for label in stand_spec_counts().keys() {
            assert!(
                label == "PN-API" || PnProfile::from_seed_label(label).is_some(),
                "`{label}` is owned by neither suite"
            );
        }
    }

    #[test]
    fn an_unprofiled_pool_is_owned_by_its_tail() {
        // The `DEX_TEST_NOTES_CNT` pool carries no labels at all, so the only
        // thing that can separate this suite's notes from the api-e2e head is
        // still the index tail.
        let profiles = vec![None; 10];
        assert_eq!(sdk_owned_indices(&profiles, 3), vec![7, 8, 9]);
    }

    #[test]
    fn a_profiled_pool_is_owned_by_label_regardless_of_position() {
        // The point of the whole change: ownership stops depending on where
        // the spec happened to put a group. Here the harness's notes sit at
        // the head and the foreign ones at the tail — the exact arrangement
        // that would make an index tail hand out the wrong notes.
        let profiles = vec![Some("PN-TRD"), Some("PN-DEP"), Some("PN-API"), Some("PN-API")];
        assert_eq!(sdk_owned_indices(&profiles, 3), vec![0, 1]);
    }

    #[test]
    fn a_pool_of_only_foreign_labels_is_owned_by_nothing() {
        // Not an error at this layer: registration simply has nothing to
        // register, and `rent` is where a scenario finds out, with the pool
        // census in the message.
        let profiles = vec![Some("PN-API"); 4];
        assert_eq!(sdk_owned_indices(&profiles, 3), Vec::<usize>::new());
    }

    #[test]
    fn every_profile_round_trips_through_its_seed_label() {
        // A collision would show up here too: two variants sharing a label
        // means the second one cannot come back from `from_seed_label`.
        for p in PnProfile::ALL {
            assert_eq!(
                PnProfile::from_seed_label(p.seed_label()),
                Some(*p),
                "{p:?} does not survive a round trip through `{}`",
                p.seed_label()
            );
        }

        // Tripwire, not decoration: this match has no wildcard arm, so adding
        // a variant to `PnProfile` stops compiling right here — which is the
        // reminder that `ALL` above needs the new variant, without which the
        // loop would keep passing while covering one role less.
        match PnProfile::Dep {
            PnProfile::Dep
            | PnProfile::Trd
            | PnProfile::Cons
            | PnProfile::Cpn
            | PnProfile::Shell
            | PnProfile::Usdc
            | PnProfile::Inf
            | PnProfile::Rot => {}
        }
    }

    #[test]
    fn an_unowned_label_is_not_a_profile_of_this_harness() {
        // The api-e2e suite draws from the same file. Its notes must read as
        // "someone else's", not as a parse failure and not as a wildcard.
        assert_eq!(PnProfile::from_seed_label("PN-API"), None);
    }

    #[test]
    fn tail_count_from_defaults_when_unset() {
        assert_eq!(tail_count_from(None).unwrap(), DEFAULT_SDK_TAIL);
    }

    #[test]
    fn tail_count_from_parses_a_valid_value() {
        assert_eq!(tail_count_from(Some("7")).unwrap(), 7);
    }

    #[test]
    fn tail_count_from_rejects_a_malformed_value() {
        // `E2E_SDK_TAIL_COUNT=oops` must be a load error, not a silent
        // fall-through to `DEFAULT_SDK_TAIL` — that would hide a pool-sizing
        // typo instead of surfacing it.
        let err = tail_count_from(Some("oops")).unwrap_err();
        assert!(format!("{err}").contains("oops"));
    }

    #[test]
    fn boc_is_absent_treats_a_missing_boc_as_absent() {
        // The `None` half of the harness's single absence notion — the other
        // half (a BOC that decodes to `AccountNone`) is `account_boc_is_none`,
        // already covered by its own tests in `tvm_runner.rs`.
        assert!(boc_is_absent(None).unwrap());
    }
}
