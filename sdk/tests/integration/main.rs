//! `dodex_sdk` live integration tests against shellnet.
//!
//! Single test binary, modular layout:
//!
//! - `common/` — shared helpers (network context, key generation, voucher
//!   pipeline, PN deploy/gas, oracle/PMP setup).
//! - `pn_basic` — deploy / change-owner / transfer / withdraw / full lifecycle.
//! - `pmp` — Prediction Market Position scenarios (happy/cancel/losing/
//!   two-stakers/delete/external/multi-stake/address-verification).
//! - `oracle` — fee withdrawal + oracle/market discovery.
//! - `discovery` — PN discovery by pubkey + aggregated balance.
//! - `history` — PN event history + pagination.
//! - `flows` — multi-step user flows (recovery, gas top-up,
//!   change-owner+stake).
//! - `multitoken` — one PN per token type (NACKL/SHELL/USDC).
//! - `proof_money` — one market's whole lifecycle against a from-scratch local
//!   stand, with an exact per-currency conservation assertion after every
//!   phase.
//! - `ledger_race` — hermetic multiprocess contention on the shared ledger:
//!   real OS processes rent/release/quarantine the same note pool
//!   concurrently, and a worker from a superseded generation gets
//!   `StaleRun`. No chain, not `#[ignore]`d — runs on every PR.
//! - `parallel_setup` — two market setups against a live chain, run as a
//!   pair, proving each side takes a different note, derives a different
//!   nonce, and lands on a different market address.
//!
//! Each `mod` declaration here pulls in `tests/integration/<name>.rs` (or
//! `tests/integration/<name>/mod.rs` for the multi-file `common` module).

mod common;
mod discovery;
mod flows;
mod history;
mod ledger_race;
mod multitoken;
mod oracle;
mod order_book;
mod parallel_setup;
mod pmp;
mod pn_basic;
mod proof_money;
mod usdc_release;
