// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Pins the three contract constants the indexer copies in order to recover a
//! deal's `deposit`, which no event carries.
//!
//! The book computes the escrow it sends a deal as
//! `debit = ticks * (p + p * PLATFORM_FEE_BPS / BPS_DENOMINATOR) + bond`, where
//! `bond` is `2 * clearingPrice` for a `FLAG_SUBSCRIPTION` placement and zero
//! otherwise (`InferenceOrderBook.sol`, `_match`). Every input to that expression
//! is emitted; the result is not. So the read model reproduces the arithmetic —
//! and a copy of a constant is only as good as the thing that notices it drifting.
//!
//! Re-derived from the Solidity source on every run rather than trusted, the same
//! way `dodex-infrastructure/tests/ingest_scope.rs` re-derives the ingest scope.
//! A fee change that lands in the contracts and not here would otherwise make
//! every recovered deposit quietly wrong: no error, no warning, just a number
//! that is off by the fee.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use dodex_contracts::airegistry::inference_order_book_events::BPS_DENOMINATOR;
use dodex_contracts::airegistry::inference_order_book_events::FLAG_SUBSCRIPTION;
use dodex_contracts::airegistry::inference_order_book_events::PLATFORM_FEE_BPS;

fn contracts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts")
}

/// `<name> = <value>` for one integer constant declared anywhere in `path`.
/// Hex (`0x40`) and decimal with underscores (`10_000`) both occur.
fn declared_constant(path: &Path, name: &str) -> u64 {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for line in text.lines() {
        let Some((decl, value)) = line.split_once('=') else { continue };
        if !decl.contains("constant") {
            continue;
        }
        if decl.split_whitespace().last() != Some(name) {
            continue;
        }
        let raw = value.trim().trim_end_matches(';').split_whitespace().next().unwrap_or("");
        let raw = raw.trim_end_matches(';').replace('_', "");
        let parsed = raw
            .strip_prefix("0x")
            .map(|hex| u64::from_str_radix(hex, 16))
            .unwrap_or_else(|| raw.parse::<u64>())
            .unwrap_or_else(|e| panic!("parse {name} = `{raw}`: {e}"));
        return parsed;
    }
    panic!("{name} is not declared in {}", path.display());
}

#[test]
fn the_platform_fee_matches_the_contract_that_charges_it() {
    let modifiers = contracts_dir().join("airegistry/modifiers/modifiers.sol");
    assert_eq!(
        declared_constant(&modifiers, "PLATFORM_FEE_BPS"),
        u64::from(PLATFORM_FEE_BPS),
        "PLATFORM_FEE_BPS drifted: the read model would recover every deposit off by the fee"
    );
    assert_eq!(
        declared_constant(&modifiers, "BPS_DENOMINATOR"),
        u64::from(BPS_DENOMINATOR),
        "BPS_DENOMINATOR drifted: same consequence as the fee itself"
    );
}

#[test]
fn the_subscription_flag_matches_the_book_and_the_deal() {
    // Declared TWICE on the chain side — the book reads it off the placement and
    // the deal reads it off the flags the book forwards — so both are checked. A
    // change to one and not the other is a contract bug this test also catches.
    let book = contracts_dir().join("airegistry/InferenceOrderBook.sol");
    let deal = contracts_dir().join("airegistry/TokenContract.sol");
    assert_eq!(
        declared_constant(&book, "FLAG_SUBSCRIPTION"),
        u64::from(FLAG_SUBSCRIPTION),
        "FLAG_SUBSCRIPTION drifted in InferenceOrderBook: subscriptions would be read as ordinary \
         deals and their bond omitted from the recovered deposit"
    );
    assert_eq!(
        declared_constant(&deal, "FLAG_SUBSCRIPTION"),
        u64::from(FLAG_SUBSCRIPTION),
        "FLAG_SUBSCRIPTION drifted in TokenContract"
    );
}
