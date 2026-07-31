// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! The manifest of every external event the `contracts/dex` suite can emit:
//! the routing id, what rides on it, and which test unit is answerable for
//! asserting the payload.
//!
//! ## Why a manifest
//!
//! An external event is the only thing the indexer ever sees of a contract's
//! intent. It reaches the gateway addressed to `makeAddrExtern(EVENT_ID, 256)`
//! and the reported `dst` is that id in 64 hex digits, so **routing depends on
//! the id, not on the event name**. The ids are declared in one file and the
//! emissions live in eight others — precisely the arrangement in which the two
//! drift apart in silence. Nothing objects when a constant is declared and
//! never used, when an emit is re-pointed at a neighbouring id, or when a new
//! event appears that nothing downstream knows about.
//!
//! Two such drifts are already visible in the table below, and neither is a
//! typo: `PRIVATENOTE_SPLIT_CONFIRMED` carries `FullSetStakeConfirmed` rather
//! than anything named Split, and the like-named `PRIVATENOTE_FULLSET_STAKE_*`
//! constants carry nothing at all.
//!
//! ## What is checked, and what is only recorded
//!
//! Everything derived from the contracts is re-derived on every run:
//!
//! - the row set equals the declared constants, so a new or renumbered event
//!   fails here before it can reach a consumer;
//! - `emits` equals the event each id is actually emitted with, so a
//!   re-pointed emit fails;
//! - `Reserved` means *referenced by no `makeAddrExtern` anywhere under
//!   `contracts/dex`* — a classification, not an opinion;
//! - every emit statement in the suite resolves to a row, so the parser
//!   failing to understand a new emit style is itself a failure rather than a
//!   silent gap;
//! - `IGNORABLE_EVENT_IDS`, which drops events by `dst` before they are ever
//!   decoded, agrees with the manifest on both id and name.
//!
//! The `owner` column is the one thing this file cannot derive: which unit of
//! the e2e matrix produces the event on a stand and therefore owns the
//! field-level assertion. It is checked only for presence, plus a pinned count
//! of the events no unit claims — a ratchet, so that number can fall
//! deliberately but never grow by accident. Payload assertions themselves
//! belong to the scenarios, not here; a manifest that also asserted fields
//! would need a chain, and would stop running in ordinary CI.
//!
//! Scope is `contracts/dex`. The airegistry books number their events in a
//! separate 1000-block of their own; `PRIVATENOTE_INFERENCE_*` appears here
//! because `PrivateNote` emits it, and it is the note's mirror of an
//! inference order rather than the book's own event.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use dodex_infrastructure::config::event_type_dst;
use dodex_infrastructure::config::IGNORABLE_EVENT_IDS;

/// One external event id.
struct Event {
    /// The `modifiers.sol` constant naming the id.
    konst: &'static str,
    /// The routing id itself: what the gateway reports as `dst`.
    id: u32,
    /// `Kind.EventName` emitted to this id — the same spelling the indexer
    /// uses for `event_type`. `None` when the constant is declared and nothing
    /// emits it.
    emits: Option<&'static str>,
    /// Who is answerable for the payload.
    owner: Owner,
}

enum Owner {
    /// The e2e-matrix unit that produces this event on a stand. Assertion of
    /// its fields belongs to that unit's scenario.
    Unit(&'static str),
    /// Emitted by the contracts, claimed by no unit of the matrix. The reason
    /// says why, so the gap is a decision on record rather than an oversight.
    Unclaimed(&'static str),
    /// Declared and emitted nowhere, so there is no payload to own.
    Reserved,
}

/// Events that no matrix unit claims. The two checks below pin both the set
/// and its size: closing one is a one-line edit here, growing the set by
/// accident is a test failure.
const UNCLAIMED_BUDGET: usize = 2;

/// Every external event id `contracts/dex` declares, in declaration order.
///
/// Unit labels are the work units of the e2e matrix
/// (`specs/2026-07-22-dex-e2e-test-matrix.md`, §11).
const MANIFEST: &[Event] = &[
    // ── RootPN ────────────────────────────────────────────────────────────
    Event {
        konst: "ROOTPN_PRIVATE_NOTE_DEPLOYED",
        id: 101,
        emits: Some("RootPN.PrivateNoteDeployed"),
        // Stand notes come from the zerostate, which deploys them without
        // going through `deployPrivateNote` — so on a stand this fires only
        // once a unit deploys a note itself.
        owner: Owner::Unit("B26"),
    },
    Event {
        konst: "ROOTPN_NULLIFIER_DEPLOYED",
        id: 102,
        emits: Some("RootPN.NullifierDeployed"),
        owner: Owner::Unit("B26"),
    },
    Event { konst: "ROOTPN_ORACLE_DEPLOYED", id: 103, emits: None, owner: Owner::Reserved },
    Event {
        konst: "ROOTPN_TOKENS_WITHDRAWN",
        id: 154,
        emits: Some("RootPN.TokensWithdrawn"),
        owner: Owner::Unit("B13"),
    },
    Event {
        konst: "ROOTPN_PROTOCOL_FEE_COLLECTED",
        id: 155,
        emits: Some("RootPN.ProtocolFeeCollected"),
        owner: Owner::Unit("B3"),
    },
    Event {
        konst: "ROOTPN_PROTOCOL_FEE_WITHDRAWN",
        id: 156,
        emits: Some("RootPN.ProtocolFeeWithdrawn"),
        owner: Owner::Unit("B3"),
    },
    // ── Oracle / OracleEventList ──────────────────────────────────────────
    Event {
        konst: "ORACLE_DEPLOYED",
        id: 104,
        // Named for the oracle, carries the event-list deployment.
        emits: Some("Oracle.OracleEventListDeployed"),
        owner: Owner::Unit("B28"),
    },
    Event { konst: "ORACLE_EVENT_LIST_DEPLOYED", id: 105, emits: None, owner: Owner::Reserved },
    Event {
        konst: "ORACLE_EVENT_CONFIRMED",
        id: 106,
        emits: Some("OracleEventList.EventConfirmed"),
        owner: Owner::Unit("B19"),
    },
    Event {
        konst: "ORACLE_LIST_DESCRIPTION_UPDATED",
        id: 107,
        emits: Some("OracleEventList.DescriptionUpdated"),
        owner: Owner::Unit("B28"),
    },
    // ── PrivateNote ───────────────────────────────────────────────────────
    Event {
        konst: "PRIVATENOTE_PMP_DEPLOYED",
        id: 111,
        emits: Some("PrivateNote.PMPDeployed"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PRIVATENOTE_OWNER_CHANGED",
        id: 112,
        emits: Some("PrivateNote.OwnerChanged"),
        owner: Owner::Unclaimed(
            "PN-O01: owner rotation is exercised by the live pn_basic suite, and no stand unit \
             changes an owner — the pool's notes are keyed by their seed keypair",
        ),
    },
    Event {
        konst: "PRIVATENOTE_STAKE_CONFIRMED",
        id: 113,
        emits: Some("PrivateNote.StakeConfirmed"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PRIVATENOTE_INFERENCE_PLACED",
        id: 1100,
        emits: Some("PrivateNote.InferenceOrderPlacedConfirmed"),
        owner: Owner::Unit("B21"),
    },
    Event {
        konst: "PRIVATENOTE_INFERENCE_FILLED",
        id: 1101,
        emits: Some("PrivateNote.InferenceFilledConfirmed"),
        owner: Owner::Unit("B21"),
    },
    Event {
        konst: "PRIVATENOTE_CLAIM_ACCEPTED",
        id: 114,
        emits: Some("PrivateNote.ClaimAccepted"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PRIVATENOTE_STAKE_CANCELLED",
        id: 115,
        emits: Some("PrivateNote.StakeCancelled"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PRIVATENOTE_FULLSET_STAKE_CONFIRMED",
        id: 116,
        // Declared as if live, and dead: `FullSetStakeConfirmed` is emitted to
        // PRIVATENOTE_SPLIT_CONFIRMED (138) instead.
        emits: None,
        owner: Owner::Reserved,
    },
    Event {
        konst: "PRIVATENOTE_FULLSET_STAKE_CANCELLED",
        id: 117,
        emits: None,
        owner: Owner::Reserved,
    },
    // ── PMP ───────────────────────────────────────────────────────────────
    Event {
        konst: "PMP_STAKE_ACCEPTED",
        id: 118,
        emits: Some("PMP.StakeAccepted"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PMP_APPROVED_BY_ORACLE",
        id: 119,
        emits: Some("PMP.ApprovedByOracle"),
        owner: Owner::Unit("B12"),
    },
    Event {
        konst: "PMP_RESOLVED",
        id: 120,
        emits: Some("PMP.Resolved"),
        owner: Owner::Unit("B15"),
    },
    Event {
        konst: "PMP_CLAIM_PROCESSED",
        id: 121,
        emits: Some("PMP.ClaimProcessed"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PMP_NETWORK_FEE_BURNED",
        id: 122,
        // Live as of v4.0.30: `setResultStart` burns the network fee on the
        // first approval and reports it. The matrix still records this id as
        // never emitted — that entry is stale, and B6 owns the per-account ECC
        // delta that says where the fee went.
        emits: Some("PMP.NetworkFeeBurned"),
        owner: Owner::Unit("B6"),
    },
    Event { konst: "PMP_STAKE_DEADLINE_SET", id: 123, emits: None, owner: Owner::Reserved },
    Event {
        konst: "PMP_SET_TIMINGS",
        id: 124,
        emits: Some("PMP.TimingsSet"),
        owner: Owner::Unit("B12"),
    },
    Event {
        konst: "PMP_NUM_OUTCOMES_SET",
        id: 125,
        // `_numOutcomes` is derived from `outcomeNames`; PMP.sol says outright
        // that it does not emit this.
        emits: None,
        owner: Owner::Reserved,
    },
    Event {
        konst: "PMP_EVENT_CANCELLED",
        id: 126,
        emits: Some("PMP.EventCancelled"),
        owner: Owner::Unit("B4"),
    },
    Event { konst: "PMP_ORACLE_CONFIRMED", id: 127, emits: None, owner: Owner::Reserved },
    Event { konst: "PMP_ALL_ORACLES_CONFIRMED", id: 128, emits: None, owner: Owner::Reserved },
    Event { konst: "PMP_INITIALIZED", id: 129, emits: None, owner: Owner::Reserved },
    Event {
        konst: "PMP_REJECTED_BY_ORACLE",
        id: 132,
        emits: Some("PMP.PMPRejected"),
        owner: Owner::Unit("B19"),
    },
    // ── OracleEventList, continued ────────────────────────────────────────
    Event {
        konst: "ORACLE_EVENT_ADDED",
        id: 133,
        emits: Some("OracleEventList.EventAdded"),
        owner: Owner::Unit("B19"),
    },
    Event {
        konst: "ORACLE_EVENT_PUBLISHED",
        id: 134,
        // `Oracle.EventPublished` is declared in Oracle.sol and emitted
        // nowhere, so this id has nothing to route.
        emits: None,
        owner: Owner::Reserved,
    },
    Event {
        konst: "ORACLE_RANGE_EVENT_ADDED",
        id: 162,
        emits: Some("OracleEventList.RangeEventAdded"),
        owner: Owner::Unit("B19"),
    },
    // ── Vault / RootOracle / creator fee ──────────────────────────────────
    Event {
        konst: "VAULT_voucher_GENERATED",
        id: 135,
        emits: Some("RootPN.VoucherGenerated"),
        owner: Owner::Unit("B9"),
    },
    Event {
        konst: "ROOTORACLE_ORACLE_DEPLOYED",
        id: 136,
        emits: Some("RootOracle.OracleDeployed"),
        owner: Owner::Unit("B28"),
    },
    Event {
        konst: "PMP_CREATOR_FEE_COLLECTED",
        id: 137,
        emits: Some("PMP.CreatorFeeCollected"),
        owner: Owner::Unit("B4"),
    },
    // ── split / merge ─────────────────────────────────────────────────────
    Event {
        konst: "PRIVATENOTE_SPLIT_CONFIRMED",
        id: 138,
        // Named Split, carries FullSetStakeConfirmed. See the module header.
        emits: Some("PrivateNote.FullSetStakeConfirmed"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PRIVATENOTE_MERGE_CONFIRMED",
        id: 139,
        emits: Some("PrivateNote.FullSetStakeCancelled"),
        owner: Owner::Unit("B10"),
    },
    Event {
        konst: "PMP_POOLS_FROZEN",
        id: 140,
        emits: Some("PMP.PoolsFrozen"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PMP_SPLIT_PROCESSED",
        id: 141,
        emits: Some("PMP.SplitProcessed"),
        owner: Owner::Unit("B4"),
    },
    Event {
        konst: "PMP_MERGE_PROCESSED",
        id: 142,
        emits: Some("PMP.MergeProcessed"),
        owner: Owner::Unit("B10"),
    },
    // ── OrderBook ─────────────────────────────────────────────────────────
    Event {
        konst: "OB_ORDER_PLACED",
        id: 143,
        emits: Some("OrderBook.OrderPlaced"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "OB_ORDER_CANCELLED",
        id: 144,
        emits: Some("OrderBook.OrderCancelled"),
        owner: Owner::Unit("B1"),
    },
    Event { konst: "OB_EPOCH_SETTLED", id: 145, emits: None, owner: Owner::Reserved },
    Event {
        konst: "OB_ORDER_FILLED",
        id: 146,
        emits: Some("OrderBook.OrderFilled"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "OB_PARTIAL_FILL",
        id: 157,
        emits: Some("OrderBook.PartialFill"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "OB_FULLY_FILLED",
        id: 158,
        emits: Some("OrderBook.FullyFilled"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "OB_QUEUED",
        id: 159,
        emits: Some("OrderBook.Queued"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "OB_REJECTED",
        id: 160,
        emits: Some("OrderBook.Rejected"),
        owner: Owner::Unit("B30"),
    },
    Event {
        konst: "OB_CALLBACK_BOUNCED",
        id: 161,
        emits: Some("OrderBook.CallbackBounced"),
        owner: Owner::Unit("B23"),
    },
    // ── PrivateNote order mirrors ─────────────────────────────────────────
    Event {
        konst: "PRIVATENOTE_ORDER_PLACED",
        id: 147,
        emits: Some("PrivateNote.OrderPlacedConfirmed"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "PRIVATENOTE_ORDER_FILLED",
        id: 148,
        emits: Some("PrivateNote.OrderFilledConfirmed"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "PRIVATENOTE_ORDER_SUBMITTED",
        id: 151,
        emits: Some("PrivateNote.OrderSubmitted"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "PRIVATENOTE_ORDER_CANCELLED",
        id: 152,
        emits: Some("PrivateNote.OrderCancelledConfirmed"),
        owner: Owner::Unit("B1"),
    },
    Event {
        konst: "PRIVATENOTE_ORDER_REJECTED",
        id: 153,
        emits: Some("PrivateNote.OrderPlaceRejected"),
        owner: Owner::Unit("B30"),
    },
    // ── transfers ─────────────────────────────────────────────────────────
    Event {
        konst: "PRIVATENOTE_TRANSFER_INITIATED",
        id: 149,
        emits: Some("PrivateNote.TransferInitiated"),
        owner: Owner::Unit("B6"),
    },
    Event {
        konst: "PRIVATENOTE_TRANSFER_CONFIRMED",
        id: 150,
        emits: Some("PrivateNote.TransferReceived"),
        owner: Owner::Unclaimed(
            "PN-T01: the receiving half of a transfer needs two live notes and taints both, and \
             the live pn_basic suite already covers the happy path — B6 covers only the bounce",
        ),
    },
];

// ─────────────────────────── contract-source facts ───────────────────────────

fn dex_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/dex")
}

/// Every `.sol` under `contracts/dex`, read at run time rather than baked in
/// with `include_str!` — a new contract file must be visible to the checks
/// below without anyone remembering to list it here.
fn dex_sources() -> Vec<(String, String)> {
    let dir = dex_dir();
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "sol"))
        .map(|p| {
            let stem = p.file_stem().expect("file stem").to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
            (stem, text)
        })
        .collect();
    assert!(!out.is_empty(), "no .sol files under {}", dir.display());
    out.sort();
    out
}

/// The declared event ids, in declaration order.
///
/// The block ends at `MIN_BALANCE`, the first constant that is not an event
/// id. Anything declared past that boundary is still caught, from the other
/// side, by [`every_emit_resolves_to_a_manifest_row`]: an id that something
/// emits to but the manifest does not list is a failure regardless of where it
/// was declared.
fn declared_event_ids() -> Vec<(String, u32)> {
    let path = dex_dir().join("modifiers/modifiers.sol");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let head = src
        .split_once("uint64 constant MIN_BALANCE")
        .unwrap_or_else(|| {
            panic!("MIN_BALANCE no longer marks the end of the event-id block in {path:?}")
        })
        .0;

    head.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("uint128 constant ")?;
            let (name, rest) = rest.split_once('=')?;
            let value = rest.split(';').next()?.trim().replace('_', "");
            let id = value.parse().unwrap_or_else(|e| panic!("cannot parse id from {line:?}: {e}"));
            Some((name.trim().to_string(), id))
        })
        .collect()
}

/// A single `emit` statement resolved to the id constant it routes to.
struct EmitSite {
    konst: String,
    /// `Kind.EventName`, the spelling the indexer uses for `event_type`.
    event_type: String,
}

/// Every `emit` in the suite, resolved to its routing constant.
///
/// Two spellings occur: the destination inlined into the `emit`, and the more
/// common one where a local `address` holds it. Both are handled; anything
/// else is returned as unresolved rather than skipped, because an emit this
/// cannot read is exactly the case the manifest would otherwise miss.
fn emit_sites() -> (Vec<EmitSite>, Vec<String>) {
    let mut sites = Vec::new();
    let mut unresolved = Vec::new();

    for (kind, text) in dex_sources() {
        let mut locals: BTreeMap<&str, &str> = BTreeMap::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some((var, konst)) = parse_extern_binding(trimmed) {
                locals.insert(var, konst);
            }
            let Some(rest) = trimmed.strip_prefix("emit ") else { continue };
            let Some((name, dest)) = parse_emit(rest) else {
                unresolved.push(format!("{kind}.sol: cannot read the destination of {trimmed:?}"));
                continue;
            };
            let konst = match extern_argument(dest) {
                Some(k) => Some(k),
                None => locals.get(dest.trim()).copied(),
            };
            match konst {
                Some(konst) => sites.push(EmitSite {
                    konst: konst.to_string(),
                    event_type: format!("{kind}.{name}"),
                }),
                None => unresolved.push(format!(
                    "{kind}.sol: `{dest}` is not an event-id destination we can resolve"
                )),
            }
        }
    }
    (sites, unresolved)
}

/// `address addrExtern = address.makeAddrExtern(CONST, bitCntAddress);`
fn parse_extern_binding(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("address ")?;
    let (var, rhs) = rest.split_once('=')?;
    Some((var.trim(), extern_argument(rhs)?))
}

/// The first argument of a `makeAddrExtern(...)` call anywhere in `expr`.
fn extern_argument(expr: &str) -> Option<&str> {
    let after = expr.split_once("makeAddrExtern(")?.1;
    let arg = after.split(',').next()?.trim();
    arg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_').then_some(arg)
}

/// `EventName{dest: <expr>}(args…)` — the tail of an `emit` statement.
fn parse_emit(rest: &str) -> Option<(&str, &str)> {
    let (name, tail) = rest.split_once('{')?;
    let dest = tail.split_once("dest:")?.1.split_once('}')?.0;
    Some((name.trim(), dest))
}

// ────────────────────────────────── checks ──────────────────────────────────

#[test]
fn the_manifest_lists_exactly_the_declared_event_ids() {
    let declared: Vec<(String, u32)> = declared_event_ids();
    let listed: Vec<(String, u32)> = MANIFEST.iter().map(|e| (e.konst.to_string(), e.id)).collect();

    // Order matters as well as membership: the manifest reads as the contract
    // file does, and a reader comparing them should not have to sort.
    assert_eq!(
        listed, declared,
        "the manifest and modifiers.sol disagree on the event ids. A new event needs a row here \
         naming what it carries and which matrix unit owns its payload; a renumbered one needs \
         its row corrected — and every consumer keyed on the old id re-checked, `dst` routing \
         included"
    );
}

#[test]
fn every_id_is_unique() {
    let mut seen: BTreeMap<u32, &str> = BTreeMap::new();
    for e in MANIFEST {
        if let Some(other) = seen.insert(e.id, e.konst) {
            panic!(
                "{} and {} share event id {} — their external `dst` is identical, so nothing \
                 downstream can tell the two apart",
                other, e.konst, e.id
            );
        }
    }
}

#[test]
fn emitted_events_match_their_emit_sites() {
    let (sites, unresolved) = emit_sites();
    assert!(unresolved.is_empty(), "unresolved emit destinations: {unresolved:#?}");

    // A constant may be emitted from several places — `Rejected` and `Queued`
    // each have three — but every one of them must carry the same event.
    let mut actual: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for site in sites {
        actual.entry(site.konst).or_default().insert(site.event_type);
    }
    for (konst, types) in &actual {
        assert_eq!(
            types.len(),
            1,
            "{konst} carries more than one event ({types:?}); the manifest assumes an id names \
             exactly one payload, and so does every consumer that routes on `dst`"
        );
    }

    let expected: BTreeMap<&str, Option<&str>> =
        MANIFEST.iter().map(|e| (e.konst, e.emits)).collect();
    for (konst, want) in expected {
        let got = actual.get(konst).and_then(|s| s.iter().next()).map(String::as_str);
        assert_eq!(
            got, want,
            "{konst}: the manifest says it carries {want:?}, the contracts emit {got:?}"
        );
    }
}

#[test]
fn reserved_means_nothing_emits_it() {
    let (sites, _) = emit_sites();
    let emitted: BTreeSet<String> = sites.into_iter().map(|s| s.konst).collect();

    for e in MANIFEST {
        let reserved = matches!(e.owner, Owner::Reserved);
        assert_eq!(
            reserved,
            !emitted.contains(e.konst),
            "{}: the manifest calls it {}, the contracts {} it. A reserved id that gains an emit \
             needs an owner; a live one that loses its last emit needs its consumers checked \
             before the row is downgraded",
            e.konst,
            if reserved { "reserved" } else { "live" },
            if emitted.contains(e.konst) { "emit" } else { "never emit" },
        );
    }
    // The classification is only worth having while it still separates
    // something: an all-live or all-reserved manifest means the parser stopped
    // reading emits rather than that the contracts changed that much.
    let live = MANIFEST.iter().filter(|e| e.emits.is_some()).count();
    assert!(live > 0 && live < MANIFEST.len(), "{live} of {} ids live", MANIFEST.len());
}

#[test]
fn every_emit_resolves_to_a_manifest_row() {
    let (sites, unresolved) = emit_sites();
    assert!(unresolved.is_empty(), "unresolved emit destinations: {unresolved:#?}");

    let known: BTreeSet<&str> = MANIFEST.iter().map(|e| e.konst).collect();
    let missing: BTreeSet<String> =
        sites.into_iter().map(|s| s.konst).filter(|k| !known.contains(k.as_str())).collect();
    assert!(
        missing.is_empty(),
        "these constants are emitted to but absent from the manifest: {missing:?} — they are \
         declared outside the event-id block, so add the row by hand"
    );
}

#[test]
fn every_live_event_names_an_owner() {
    let mut unclaimed: Vec<&str> = Vec::new();
    for e in MANIFEST {
        match e.owner {
            Owner::Unit(unit) => {
                assert!(e.emits.is_some(), "{} is owned by {unit} but nothing emits it", e.konst)
            }
            Owner::Unclaimed(reason) => {
                assert!(!reason.is_empty(), "{} is unclaimed without a reason", e.konst);
                unclaimed.push(e.konst);
            }
            Owner::Reserved => {}
        }
    }
    assert_eq!(
        unclaimed.len(),
        UNCLAIMED_BUDGET,
        "unclaimed events are now {unclaimed:?}. Claiming one means lowering UNCLAIMED_BUDGET in \
         the same commit; a new one means the matrix has an event no unit produces, which is a \
         decision to make deliberately rather than a number to raise"
    );
}

#[test]
fn dropped_events_agree_with_the_manifest() {
    // The ingest filter drops these by `dst` before anything is decoded, so a
    // wrong id here does not fail loudly — it silently drops a different
    // event, or stops dropping the intended one.
    for (event_type, id) in IGNORABLE_EVENT_IDS {
        let row = MANIFEST.iter().find(|e| e.id == id).unwrap_or_else(|| {
            panic!("{event_type} is dropped by id {id}, which no event declares")
        });
        assert_eq!(
            row.emits,
            Some(event_type),
            "the ingest filter drops id {id} as {event_type}, but {} carries {:?}",
            row.konst,
            row.emits
        );
    }
}

/// The prose catalogue of the same events: what each carries, when it fires,
/// and where to read it. The two checks below keep it from drifting away from
/// the contracts the way it had by v4.0.30, when every one of its 53 source
/// citations pointed at an unrelated line.
const ROUTING_DOC: &str = "docs/contract-specs/dex-events-routing.md";

fn routing_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(ROUTING_DOC);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

/// A table row of the routing doc: the event it documents, and the source
/// lines it points a reader at.
fn doc_rows(doc: &str) -> Vec<(String, Vec<(String, usize)>)> {
    doc.lines()
        .filter_map(|line| {
            let name = line.strip_prefix("| `")?.split_once('`')?.0;
            let cites: Vec<(String, usize)> = line
                .match_indices("](../../contracts/dex/")
                .filter_map(|(at, marker)| {
                    let rest = &line[at + marker.len()..];
                    let (cite, _) = rest.split_once(')')?;
                    let (file, no) = cite.split_once(':')?;
                    Some((file.to_string(), no.parse().ok()?))
                })
                .collect();
            (!cites.is_empty()).then(|| (name.to_string(), cites))
        })
        .collect()
}

/// Where `name` is emitted, or — for an event that is only declared — where it
/// is declared.
fn sites_of(name: &str) -> Vec<(String, usize)> {
    let starts_with_name = |rest: &str| {
        rest.strip_prefix(name).is_some_and(|t| t.starts_with('{') || t.starts_with('('))
    };
    let scan = |prefix: &str| -> Vec<(String, usize)> {
        dex_sources()
            .iter()
            .flat_map(|(stem, text)| {
                text.lines().enumerate().filter_map(move |(i, line)| {
                    let rest = line.trim().strip_prefix(prefix)?;
                    starts_with_name(rest).then(|| (format!("{stem}.sol"), i + 1))
                })
            })
            .collect()
    };
    let emitted = scan("emit ");
    if emitted.is_empty() {
        scan("event ")
    } else {
        emitted
    }
}

#[test]
fn the_routing_doc_cites_lines_that_still_say_what_it_claims() {
    let doc = routing_doc();
    let rows = doc_rows(&doc);
    assert!(rows.len() > 40, "only {} rows parsed out of the routing doc", rows.len());

    for (name, cited) in rows {
        assert_eq!(
            cited,
            sites_of(&name),
            "{ROUTING_DOC} points at the wrong lines for `{name}`. Line numbers move with every \
             contract bump, so this is a re-derivation, not a judgement call: cite every `emit \
             {name}` — or, for an event nothing emits, its declaration"
        );
    }
}

#[test]
fn the_routing_doc_and_the_manifest_agree_on_every_id() {
    let doc = routing_doc();
    let by_const: BTreeMap<&str, u32> = MANIFEST.iter().map(|e| (e.konst, e.id)).collect();

    let mut checked = 0;
    for line in doc.lines() {
        // Table rows only: the prose above them spells the same call with a
        // placeholder constant, to explain the scheme rather than to route.
        if !line.starts_with("| `") {
            continue;
        }
        let Some(rest) = line.split_once("makeAddrExtern(") else { continue };
        let Some((konst, _)) = rest.1.split_once(',') else { continue };
        // `…, bitCntAddress)` = `143`
        let documented: u32 = line
            .rsplit_once("= `")
            .and_then(|(_, t)| t.split_once('`'))
            .and_then(|(n, _)| n.parse().ok())
            .unwrap_or_else(|| panic!("no documented id in {line:?}"));
        let konst = konst.trim();
        let manifest = by_const
            .get(konst)
            .unwrap_or_else(|| panic!("{ROUTING_DOC} documents {konst}, which no event declares"));
        assert_eq!(
            *manifest, documented,
            "{ROUTING_DOC} routes {konst} to {documented}, modifiers.sol declares {manifest} — a \
             reader following the doc would watch the wrong `dst`"
        );
        checked += 1;
    }
    let live = MANIFEST.iter().filter(|e| e.emits.is_some()).count();
    assert_eq!(
        checked, live,
        "{ROUTING_DOC} documents {checked} of the {live} live events; it is the catalogue, so an \
         event missing from it is invisible to anyone who has not read modifiers.sol"
    );
}

#[test]
fn the_manifest_agrees_with_the_gateway_dst_format() {
    // One worked example end to end, against a dst recorded in production:
    // OrderBook.OrderPlaced, id 143 == 0x8f.
    let placed = MANIFEST
        .iter()
        .find(|e| e.emits == Some("OrderBook.OrderPlaced"))
        .expect("OrderBook.OrderPlaced is in the manifest");
    assert_eq!(
        event_type_dst(placed.id),
        ":000000000000000000000000000000000000000000000000000000000000008f"
    );
}
