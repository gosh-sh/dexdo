// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Every `binary(…)` a nextest filter names must be a test target that exists.
//!
//! ## Why this is a test and not a convention
//!
//! A nextest filter expression that names no binary is an **error**, not an
//! empty match:
//!
//! ```text
//! error: operator didn't match any binary names
//!   1 │ not binary(e2e_inference_dispute)
//! ```
//!
//! So a filter written to EXCLUDE a suite does not quietly stop excluding
//! anything when that suite is deleted — it takes the whole run down with it,
//! before a single test executes. The exclusion and the thing excluded live in
//! different files, and only one of them is in the compiler's view.
//!
//! That is not hypothetical either. `-E 'not binary(e2e_inference_dispute)'`
//! sat in the shellnet workflow to skip a suite that waited out a ~1200s
//! window. When the v4.0.33 sync deleted that suite along with the contract
//! calls it drove, the filter stayed. The step then failed in about a second,
//! having run nothing — and the failure was invisible for weeks because an
//! earlier step was failing first and the job never reached it.
//!
//! ## What is checked
//!
//! Both places a filter can be written: `.config/nextest.toml` overrides and
//! the `-E` arguments in `.github/workflows`. Comment lines are skipped, so a
//! filter can be described in prose after it is deleted without resurrecting
//! the requirement to keep its binary alive.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("canonicalize root")
}

/// Every integration-test target in the repo, by the name nextest knows it by.
///
/// Both layouts cargo auto-discovers: `tests/<name>.rs`, and `tests/<name>/`
/// holding a `main.rs`. Crates are found rather than listed so a new one is
/// covered without an edit here.
fn test_targets() -> BTreeSet<String> {
    let root = repo_root();
    let mut out = BTreeSet::new();

    for area in ["crates", "services", "sdk"] {
        let area_dir = root.join(area);
        // `sdk` is a crate itself; `crates` and `services` hold crates.
        let crate_dirs: Vec<PathBuf> = if area_dir.join("tests").is_dir() {
            vec![area_dir.clone()]
        } else {
            let Ok(entries) = std::fs::read_dir(&area_dir) else { continue };
            entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect()
        };

        for dir in crate_dirs {
            let tests = dir.join("tests");
            let Ok(entries) = std::fs::read_dir(&tests) else { continue };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                    continue;
                };
                if path.is_dir() {
                    if path.join("main.rs").is_file() {
                        out.insert(stem);
                    }
                } else if path.extension().is_some_and(|x| x == "rs") {
                    out.insert(stem);
                }
            }
        }
    }

    assert!(out.len() > 20, "only {} test targets discovered — the scan is broken", out.len());
    out
}

/// `(file, binary name)` for every `binary(…)` outside a comment line.
fn referenced_binaries() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files: Vec<PathBuf> = vec![root.join(".config/nextest.toml")];
    if let Ok(entries) = std::fs::read_dir(root.join(".github/workflows")) {
        let mut yml: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
            .collect();
        yml.sort();
        files.extend(yml);
    }

    let mut out = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let label = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().into_owned();
        for line in text.lines() {
            // A commented-out or merely described filter is prose, not a filter.
            // Both TOML and YAML start a whole-line comment with `#`.
            if line.trim_start().starts_with('#') {
                continue;
            }
            let mut rest = line;
            while let Some((_, tail)) = rest.split_once("binary(") {
                let Some((name, after)) = tail.split_once(')') else { break };
                out.push((label.clone(), name.trim().to_string()));
                rest = after;
            }
        }
    }
    out
}

#[test]
fn every_filtered_binary_exists() {
    let targets = test_targets();
    let referenced = referenced_binaries();

    assert!(
        !referenced.is_empty(),
        "no `binary(…)` found in any nextest config or workflow — the scan stopped understanding \
         where filters are written, which hides the very drift this pins"
    );

    let missing: Vec<String> = referenced
        .iter()
        .filter(|(_, name)| !targets.contains(name))
        .map(|(file, name)| format!("{file} filters on `binary({name})`, which is not a target"))
        .collect();

    assert!(
        missing.is_empty(),
        "a nextest filter names a binary that does not exist. This is not a no-op: nextest \
         rejects the whole expression, so the run dies before any test starts.\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn a_deleted_binary_is_caught_rather_than_ignored() {
    // The check above only earns its place if an unknown name actually fails
    // it — the failure mode being guarded against is a filter that looks fine.
    let targets = test_targets();
    assert!(
        !targets.contains("e2e_inference_dispute"),
        "this suite was deleted with the v4.0.33 contract calls; if it is back, the example \
         below needs a name that is genuinely absent"
    );
    assert!(targets.contains("dex_event_manifest"), "a target the scan must find");
}
