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
//!
//! Each `mod` declaration here pulls in `tests/integration/<name>.rs` (or
//! `tests/integration/<name>/mod.rs` for the multi-file `common` module).

mod common;
mod discovery;
mod flows;
mod history;
mod multitoken;
mod oracle;
mod order_book;
mod pmp;
mod pn_basic;
