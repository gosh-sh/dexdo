// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Every contract function a wrapper names must still exist in that contract's
//! ABI.
//!
//! ## The gap this fills
//!
//! The `*_params_match_abi` tests next door compare a `Params*` struct against
//! `abi_input_names(func)`, so they pin the SHAPE of a call. What they cannot
//! pin is that the call has a target at all: the function name is a string
//! literal handed to `CallSet`, and nothing type-checks a string. A wrapper
//! whose function was renamed in a contract sync therefore compiles, passes the
//! shape tests it appears in, and fails only when it reaches a node.
//!
//! That is not hypothetical. The v4.0.34 sync renamed `SuperRoot.registerRoot`
//! to `deployRootModel`; `super_root.rs` kept sending `"registerRoot"` and
//! `super_root_params_match_abi` stayed green, because a params test only
//! covers the params structs it happens to list and no struct existed for that
//! call. A rename is the easy case — the loud one. The quiet case is a function
//! deleted outright, where there is no new name to notice.
//!
//! ## What is checked
//!
//! Both ways a wrapper can name a function:
//!
//! - `function_name: "…"` — the `CallSet` of a state-changing call;
//! - `call_get_method::<T>("…")` — a getter run locally.
//!
//! Names are read out of the source text rather than the type system because
//! that is where they live. The file-to-ABI map below is explicit: a wrapper
//! module added without a line here is itself a failure, so the sweep cannot
//! quietly stop covering something.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

/// `(wrapper module, the ABI it calls into)`, relative to the repo root.
///
/// `*_events.rs` modules are deliberately absent: they decode bodies rather
/// than call functions, and the one `function_name` in them is a `None`.
const WRAPPERS: &[(&str, &str)] = &[
    ("crates/contracts/src/dex/nullifier.rs", "contracts/dex/Nullifier.abi.json"),
    ("crates/contracts/src/dex/oracle.rs", "contracts/dex/Oracle.abi.json"),
    ("crates/contracts/src/dex/oracle_event_list.rs", "contracts/dex/OracleEventList.abi.json"),
    ("crates/contracts/src/dex/order_book.rs", "contracts/dex/OrderBook.abi.json"),
    ("crates/contracts/src/dex/pmp.rs", "contracts/dex/PMP.abi.json"),
    ("crates/contracts/src/dex/private_note.rs", "contracts/dex/PrivateNote.abi.json"),
    ("crates/contracts/src/dex/root_oracle.rs", "contracts/dex/RootOracle.abi.json"),
    ("crates/contracts/src/dex/root_pn.rs", "contracts/dex/RootPN.abi.json"),
    (
        "crates/contracts/src/airegistry/inference_order_book.rs",
        "contracts/airegistry/InferenceOrderBook.abi.json",
    ),
    ("crates/contracts/src/airegistry/root_model.rs", "contracts/airegistry/RootModel.abi.json"),
    ("crates/contracts/src/airegistry/super_root.rs", "contracts/airegistry/SuperRoot.abi.json"),
    (
        "crates/contracts/src/airegistry/token_contract.rs",
        "contracts/airegistry/TokenContract.abi.json",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("canonicalize root")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

/// The first double-quoted run in `s`, or `None` if it has none.
fn first_quoted(s: &str) -> Option<&str> {
    s.split_once('"').and_then(|(_, rest)| rest.split_once('"')).map(|(name, _)| name)
}

/// Every contract function `src` names, by either convention.
fn called_functions(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in src.lines() {
        for marker in ["function_name:", "call_get_method"] {
            let Some((_, rest)) = line.split_once(marker) else { continue };
            // `function_name: None` and any other non-literal form: nothing to check.
            let Some(name) = first_quoted(rest) else { continue };
            out.insert(name.to_string());
        }
    }
    out
}

/// Every function the ABI declares.
fn abi_functions(abi: &str) -> BTreeSet<String> {
    let v: Value = serde_json::from_str(abi).expect("parse ABI");
    v["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .map(|f| f["name"].as_str().expect("function name").to_string())
        .collect()
}

#[test]
fn every_function_a_wrapper_calls_exists_in_its_abi() {
    let mut missing: Vec<String> = Vec::new();
    let mut checked = 0;

    for (wrapper, abi) in WRAPPERS {
        let declared = abi_functions(&read(abi));
        for name in called_functions(&read(wrapper)) {
            checked += 1;
            if !declared.contains(&name) {
                missing.push(format!("{wrapper} calls `{name}`, absent from {abi}"));
            }
        }
    }

    assert!(
        checked > 80,
        "only {checked} wrapper calls found — the source scan stopped understanding the call \
         convention, which is a hole rather than a clean sweep"
    );
    assert!(
        missing.is_empty(),
        "wrappers naming functions their contract no longer has. These compile and fail on \
         chain, so nothing else catches them:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_wrapper_module_is_covered() {
    let root = repo_root();
    let listed: BTreeSet<&str> = WRAPPERS.iter().map(|(w, _)| *w).collect();

    let mut uncovered: Vec<String> = Vec::new();
    for area in ["dex", "airegistry"] {
        let dir = root.join("crates/contracts/src").join(area);
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}")) {
            let path = entry.expect("dir entry").path();
            let stem = path.file_stem().expect("stem").to_string_lossy().into_owned();
            if path.extension().is_none_or(|x| x != "rs")
                || stem.ends_with("_events")
                || stem == "mod"
                || stem == "tests"
            {
                continue;
            }
            let rel = format!("crates/contracts/src/{area}/{stem}.rs");
            if !listed.contains(rel.as_str()) {
                uncovered.push(rel);
            }
        }
    }

    assert!(
        uncovered.is_empty(),
        "wrapper modules absent from WRAPPERS, so the call sweep does not see them: {uncovered:?}"
    );
}
