// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! The manifest of every external event the `contracts/airegistry` suite can
//! emit: the routing id and what rides on it.
//!
//! ## Why a second manifest
//!
//! `dex_event_manifest.rs` covers `contracts/dex` and says so; the airegistry
//! books number their events in a 700/1000-block of their own and had nothing.
//! What that cost is on record. Three events were emitted to a neighbour's id
//! for a release each — `TokenContract.TicksClaimed` on 722, the book's
//! `InferenceOrderRejected` on 1009, and the note's `InferenceOrderRejectedMirror`
//! on 165 — and only the last was caught, because only the last was in a
//! manifest. The dex one failed on it deliberately until the id arrived. The two
//! airegistry ones were found by reading a diff.
//!
//! 722 is the reason this file is worth its length. Both bodies were
//! `(uint128, uint128)`, so a positional decode SUCCEEDED and handed back two
//! plausible numbers that mean different things — `TicksClaimed(trusted, claimed)`
//! counts ticks, `TickFinalized(finalizedOwed, deposit)` counts money. A
//! collision that breaks loudly gets fixed. That one only ever produced a wrong
//! figure in somebody's report.
//!
//! ## What is checked
//!
//! Everything except the rows themselves is re-derived on every run:
//!
//! - the row set equals the declared constants, in declaration order, so a new
//!   or renumbered event fails here before it can reach a consumer;
//! - `emits` equals the events each id is actually emitted with, so a
//!   re-pointed emit fails;
//! - every id carries exactly ONE event, except the collisions pinned in
//!   [`KNOWN_SHARED`] — a ratchet, so the known ones stay known and a new one
//!   is a failure;
//! - every emit statement resolves to a row, so an emit the parser cannot read
//!   is itself a failure rather than a silent gap.
//!
//! Unlike the dex manifest there is no `owner` column: the e2e matrix assigns
//! its units over the dex suite, and inventing labels here would be a claim
//! about coverage rather than a record of it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// One external event id.
struct Event {
    /// The `modifiers.sol` constant naming the id.
    konst: &'static str,
    /// The routing id itself: what the gateway reports as `dst`.
    id: u32,
    /// Every `Kind.EventName` emitted to this id, sorted. More than one entry
    /// means a collision, and a collision must appear in [`KNOWN_SHARED`].
    emits: &'static [&'static str],
}

/// Ids that carry more than one event, and why that is tolerated rather than
/// fixed. Pinned as a set: closing one is an edit here, opening a new one is a
/// test failure.
///
/// `ContractDeployedEmit` is the one entry, and it is the worst-shaped of the
/// collisions this file exists because of. `RootModel.ContractDeployed` and
/// `TokenContract.ContractDeployed` share the id, the NAME, and the body
/// `(address self)`. The others could at least be told apart by decoding the
/// body and reading the event name out of it — the SDK enums do exactly that.
/// Here that does not work either: nothing in the message distinguishes the two.
/// A consumer on 703 learns only that something was deployed, and must ask the
/// chain what now lives at the address to find out what.
///
/// Both `RootModelEvent` and `TokenContractEvent` map 703 to their own
/// `ContractDeployed`, so each SDK enum is right about its own contract and
/// neither can be right about a stream carrying both.
const KNOWN_SHARED: &[&str] = &["ContractDeployedEmit"];

/// Every external event id `contracts/airegistry` declares, in declaration
/// order.
const MANIFEST: &[Event] = &[
    // ── registry ─────────────────────────────────────────────────────────
    Event { konst: "RootRegisteredEmit", id: 700, emits: &["SuperRoot.RootRegistered"] },
    Event {
        konst: "TokenContractRegisteredEmit",
        id: 702,
        emits: &["RootModel.TokenContractRegistered"],
    },
    Event {
        konst: "ContractDeployedEmit",
        id: 703,
        // Two contracts, one id, one name, one body shape. See `KNOWN_SHARED`.
        emits: &["RootModel.ContractDeployed", "TokenContract.ContractDeployed"],
    },
    // ── the deal contract's own lifecycle ────────────────────────────────
    Event { konst: "ContractDestroyedEmit", id: 709, emits: &["TokenContract.ContractDestroyed"] },
    Event { konst: "ShellWithdrawnEmit", id: 710, emits: &["TokenContract.ShellWithdrawn"] },
    Event { konst: "StreamFundedEmit", id: 720, emits: &["TokenContract.StreamFunded"] },
    Event { konst: "StreamOpenedEmit", id: 721, emits: &["TokenContract.StreamOpened"] },
    // Money: what the deal owes and what backs it.
    Event { konst: "TickFinalizedEmit", id: 722, emits: &["TokenContract.TickFinalized"] },
    Event { konst: "StreamStoppedEmit", id: 723, emits: &["TokenContract.StreamStopped"] },
    Event { konst: "StreamDisputedEmit", id: 724, emits: &["TokenContract.StreamDisputed"] },
    Event { konst: "DisputeResolvedEmit", id: 725, emits: &["TokenContract.DisputeResolved"] },
    Event { konst: "SellerBondFundedEmit", id: 727, emits: &["TokenContract.SellerBondFunded"] },
    Event { konst: "ProbeAcceptedEmit", id: 728, emits: &["TokenContract.ProbeAccepted"] },
    Event { konst: "ProbeBurnedEmit", id: 729, emits: &["TokenContract.ProbeBurned"] },
    // Ticks: what the seller has produced. Shared 722 with the money above
    // until it was given an id of its own.
    Event { konst: "TicksClaimedEmit", id: 730, emits: &["TokenContract.TicksClaimed"] },
    // ── InferenceOrderBook ───────────────────────────────────────────────
    Event {
        konst: "OfferPlacedEmit",
        id: 1000,
        emits: &["InferenceOrderBook.InferenceOrderPlaced"],
    },
    Event {
        konst: "OfferCancelledEmit",
        id: 1001,
        emits: &["InferenceOrderBook.InferenceOrderCancelled"],
    },
    Event { konst: "BuyUnmatchedEmit", id: 1002, emits: &["InferenceOrderBook.InferenceRefunded"] },
    Event { konst: "MatchedEmit", id: 1003, emits: &["InferenceOrderBook.InferenceFilled"] },
    Event { konst: "ExecutedEmit", id: 1004, emits: &["InferenceOrderBook.InferenceExecuted"] },
    Event {
        konst: "InferenceOBDeployedEmit",
        id: 1008,
        emits: &["InferenceOrderBook.InferenceOrderBookDeployed"],
    },
    // A refused cancellation.
    Event {
        konst: "OfferCancelRejectedEmit",
        id: 1009,
        emits: &["InferenceOrderBook.InferenceOrderCancelRejected"],
    },
    Event {
        konst: "OrderExpiredEmit",
        id: 1010,
        emits: &["InferenceOrderBook.InferenceOrderExpired"],
    },
    // A refused PLACEMENT — a different thing, and it shared 1009 with the
    // cancellation above until this id was added.
    Event {
        konst: "OrderRejectedEmit",
        id: 1011,
        emits: &["InferenceOrderBook.InferenceOrderRejected"],
    },
];

fn airegistry_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/airegistry")
}

/// Every `.sol` under `contracts/airegistry`, read at run time rather than
/// baked in with `include_str!` — a new contract file must be visible to the
/// checks below without anyone remembering to list it here.
fn airegistry_sources() -> Vec<(String, String)> {
    let dir = airegistry_dir();
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
/// Matched by the `…Emit` naming convention rather than by position, because
/// the block is not delimited — `SUB_TICKS_PER_WEEK` is a `uint128 constant`
/// too, and a computed one. An event id that breaks the convention is still
/// caught from the other side by [`every_emit_resolves_to_a_manifest_row`]: an
/// id something emits to but the manifest does not list is a failure however
/// it was spelled.
fn declared_event_ids() -> Vec<(String, u32)> {
    let path = airegistry_dir().join("modifiers/modifiers.sol");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));

    src.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("uint128 constant ")?;
            let (name, rest) = rest.split_once('=')?;
            let name = name.trim();
            if !name.ends_with("Emit") {
                return None;
            }
            let value = rest.split(';').next()?.trim().replace('_', "");
            let id = value.parse().unwrap_or_else(|e| panic!("cannot parse id from {line:?}: {e}"));
            Some((name.to_string(), id))
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
/// Two spellings occur: the destination inlined into the `emit`, and the one
/// where a local `address` holds it. Both are handled; anything else is
/// returned as unresolved rather than skipped, because an emit this cannot
/// read is exactly the case the manifest would otherwise miss.
fn emit_sites() -> (Vec<EmitSite>, Vec<String>) {
    let mut sites = Vec::new();
    let mut unresolved = Vec::new();

    for (kind, text) in airegistry_sources() {
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
        "the manifest and airegistry/modifiers.sol disagree on the event ids. A new event needs a \
         row here naming what it carries; a renumbered one needs its row corrected — and every \
         consumer keyed on the old id re-checked, the SDK event enums included"
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

    let mut actual: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for site in sites {
        actual.entry(site.konst).or_default().insert(site.event_type);
    }
    let expected: BTreeMap<String, BTreeSet<String>> = MANIFEST
        .iter()
        .filter(|e| !e.emits.is_empty())
        .map(|e| (e.konst.to_string(), e.emits.iter().map(|s| s.to_string()).collect()))
        .collect();

    assert_eq!(
        actual, expected,
        "an emit routes somewhere the manifest does not say. Whatever reads that `dst` is now \
         being handed a different payload than it was written for"
    );
}

#[test]
fn every_id_carries_one_payload() {
    let shared: BTreeSet<&str> = KNOWN_SHARED.iter().copied().collect();

    let offenders: Vec<String> = MANIFEST
        .iter()
        .filter(|e| e.emits.len() > 1 && !shared.contains(e.konst))
        .map(|e| format!("{} ({}) carries {:?}", e.konst, e.id, e.emits))
        .collect();
    assert!(
        offenders.is_empty(),
        "one id, one payload — `dst` routing rests on it, and a consumer filtering on a shared id \
         decodes one event as another. Give the newcomer an id of its own:\n  {}",
        offenders.join("\n  ")
    );

    // The ratchet in the other direction: a collision that gets fixed must
    // leave `KNOWN_SHARED`, or the set stops meaning what it says.
    let stale: Vec<&str> = KNOWN_SHARED
        .iter()
        .copied()
        .filter(|k| !MANIFEST.iter().any(|e| e.konst == *k && e.emits.len() > 1))
        .collect();
    assert!(
        stale.is_empty(),
        "these are listed as known collisions but no longer collide — drop them from \
         KNOWN_SHARED: {stale:?}"
    );
}

#[test]
fn every_emit_resolves_to_a_manifest_row() {
    let (sites, unresolved) = emit_sites();
    assert!(unresolved.is_empty(), "unresolved emit destinations: {unresolved:#?}");

    let known: BTreeSet<&str> = MANIFEST.iter().map(|e| e.konst).collect();
    let missing: BTreeSet<String> =
        sites.iter().map(|s| s.konst.clone()).filter(|k| !known.contains(k.as_str())).collect();

    assert!(
        missing.is_empty(),
        "these constants are emitted to but absent from the manifest — either they are new, or \
         they are declared in a form `declared_event_ids` does not recognise: {missing:?}"
    );
}
