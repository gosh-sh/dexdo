// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Pins the indexer's ingest scope — [`config::SCOPED_EVENT_IDS`] — against the
//! contract sources it claims to mirror, `TokenContract.*` included since
//! contracts 4.0.36 put the per-deal settlement contract in the DEX dApp.
//!
//! Capture keeps an edge only when its `dst` is one of those ids, so the list is
//! load-bearing in both directions and fails silently either way:
//!
//! - an indexed id **missing** from it is dropped at ingest, never reaches
//!   `raw_events`, and cannot be recovered by reprojection — the gateway's event
//!   window is finite;
//! - an id **stale** in it admits a route the indexer does not intend to store.
//!
//! Neither shows up as an error at runtime, so the set is re-derived here from
//! `contracts/**` on every run rather than trusted. The derivation is the same
//! one the event manifests use: an event's routing destination is
//! `makeAddrExtern(<CONST>, bitCntAddress)`, so the emitted ids are exactly the
//! constants appearing as that call's first argument.
//!
//! Note the two different "event id" numbers in this codebase: the EVENT_ID
//! constant below, which forms the `dst` and is what routing depends on, and the
//! ABI's signature-hash id that `Decoder`'s index is keyed by. They are not the
//! same number, so the scope list cannot be derived from the ABI bundle.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use dodex_infrastructure::config::SCOPED_EVENT_IDS;

fn contracts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts")
}

/// Every `.sol` file under `contracts/dex` and `contracts/airegistry`, including
/// the `modifiers/` subdirectories that declare the constants.
fn solidity_sources() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for group in ["dex", "airegistry"] {
        let root = contracts_dir().join(group);
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            let entries = fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
                .filter_map(Result::ok);
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "sol") {
                    let text = fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                    out.push((path, text));
                }
            }
        }
    }
    assert!(!out.is_empty(), "no solidity sources found under {}", contracts_dir().display());
    out
}

/// `<name> -> <value>` for every integer constant declared in the sources.
/// Both naming styles occur — `SCREAMING_SNAKE` in `contracts/dex`, `CamelCase`
/// with an `Emit` suffix in `contracts/airegistry` — so the name is matched as a
/// plain identifier rather than by shape.
fn declared_constants(sources: &[(PathBuf, String)]) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for (_, text) in sources {
        for line in text.lines() {
            let Some((decl, value)) = line.split_once('=') else { continue };
            let Some(name) = decl.split_whitespace().last() else { continue };
            if !decl.contains("constant") {
                continue;
            }
            let digits: String =
                value.trim().chars().take_while(char::is_ascii_digit).collect::<String>();
            if digits.is_empty() {
                continue;
            }
            if let Ok(v) = digits.parse::<u32>() {
                out.insert(name.to_string(), v);
            }
        }
    }
    out
}

/// EVENT_IDs routed to an external address, optionally restricted by source.
fn emitted_event_ids_matching(include_source: impl Fn(&Path) -> bool) -> BTreeSet<u32> {
    let sources = solidity_sources();
    let constants = declared_constants(&sources);
    let mut ids = BTreeSet::new();

    for (path, text) in &sources {
        if !include_source(path) {
            continue;
        }
        for (offset, _) in text.match_indices("makeAddrExtern(") {
            let rest = &text[offset + "makeAddrExtern(".len()..];
            let Some((arg, _)) = rest.split_once(',') else { continue };
            let name = arg.trim();
            let id = constants.get(name).copied().unwrap_or_else(|| {
                panic!(
                    "{}: makeAddrExtern({name}, ..) names a constant declared nowhere under \
                     contracts/ — the ingest scope cannot be derived while an emit is unreadable",
                    path.display()
                )
            });
            ids.insert(id);
        }
    }

    assert!(!ids.is_empty(), "found no makeAddrExtern call sites; the derivation is broken");
    ids
}

#[test]
fn scoped_event_ids_match_the_contracts() {
    let derived = emitted_event_ids_matching(|_| true);
    let declared: BTreeSet<u32> = SCOPED_EVENT_IDS.iter().copied().collect();

    let missing: Vec<u32> = derived.difference(&declared).copied().collect();
    let stale: Vec<u32> = declared.difference(&derived).copied().collect();

    assert!(
        missing.is_empty(),
        "indexed contracts emit event ids the indexer does not capture: {missing:?}. Add them to \
         config::SCOPED_EVENT_IDS — until then those events are dropped at ingest and are NOT \
         recoverable by reprojection."
    );
    assert!(
        stale.is_empty(),
        "config::SCOPED_EVENT_IDS carries ids outside the indexed contract set: {stale:?}. \
         Remove them — each one admits traffic under a route we do not index."
    );
}

/// The settlement block, asserted as a block rather than left to the set
/// comparison above.
///
/// `scoped_event_ids_match_the_contracts` would already fail if one of these
/// went missing, but it would fail as "a number is absent" — and this is the
/// one group in the list whose presence is a decision rather than a
/// consequence. It was deliberately excluded for as long as a deal was
/// deployed by an external message and lived in a dApp of its own, where no
/// amount of scoping could have reached it. Naming the group keeps the
/// deal-side count visible: drop `deployDeal` and put the deal back outside
/// the DEX dApp, and this is the test that says what has to be reconsidered.
#[test]
fn token_contract_event_ids_are_all_in_scope() {
    let token_contract_ids = emitted_event_ids_matching(|path| {
        path.file_name().and_then(|name| name.to_str()) == Some("TokenContract.sol")
    });
    let declared: BTreeSet<u32> = SCOPED_EVENT_IDS.iter().copied().collect();
    let dropped: Vec<u32> = token_contract_ids.difference(&declared).copied().collect();

    assert!(!token_contract_ids.is_empty(), "found no TokenContract event routes");
    assert!(
        dropped.is_empty(),
        "TokenContract emits event ids the indexer does not capture: {dropped:?}. Since contracts \
         4.0.36 the deal lives in the DEX dApp, so these arrive on the stream we already drain and \
         a missing id is dropped at ingest — not recoverable by reprojection."
    );
}

#[test]
fn scoped_event_ids_has_no_duplicates() {
    let unique: BTreeSet<u32> = SCOPED_EVENT_IDS.iter().copied().collect();
    assert_eq!(
        unique.len(),
        SCOPED_EVENT_IDS.len(),
        "config::SCOPED_EVENT_IDS repeats an id; the array length is part of the type, so a \
         duplicate hides a missing one"
    );
}

#[test]
fn every_ignorable_event_type_is_in_scope() {
    // The no-op deny-list drops by `dst` *after* the scope filter, so an ignorable
    // type outside the scope set would already be gone and its `type_ignored` count
    // would silently read zero — the same "configured but never matches" failure the
    // startup allow-list guard exists to prevent.
    let declared: BTreeSet<u32> = SCOPED_EVENT_IDS.iter().copied().collect();
    for (name, id) in dodex_infrastructure::config::IGNORABLE_EVENT_IDS {
        assert!(declared.contains(&id), "{name} (id {id}) is droppable but not in ingest scope");
    }
}
