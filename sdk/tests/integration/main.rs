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
//! - `book_segments` — orders that would cross kept apart by epoch and by
//!   outcome, and the rules `placeBatch` enforces on the two lists it takes.
//! - `coupon_debt` — the free coupon a broke note can mint, the debt it comes
//!   with, and both of them across three markets.
//! - `forfeit_close` — a market that resolves to outcome 1 rather than 0, and
//!   the three stakes walked away from instead of claimed, the last of which
//!   closes it.
//! - `mm_cycle` — a maker's whole sequence: quote both sides in one batch,
//!   get taken on part of it, cancel by name and then wholesale, merge the
//!   inventory back, and settle.
//! - `multi_market` — one note staking and quoting in two markets at once:
//!   the order ids that collide, the locks that must not, and the claim gate
//!   that counts one market's orders rather than the note's.
//! - `oracle_quorum` — a market answering to three oracles: what one vote
//!   cannot do, what a repeated one does not add, and what a changed one
//!   moves.
//! - `parallel_setup` — two market setups against a live chain, run as a
//!   pair, proving each side takes a different note, derives a different
//!   nonce, and lands on a different market address.
//!
//! - `usdc_market` — a whole market denominated in a six-decimal token, and
//!   the fill so small the taker fee floors to nothing inside it.
//!
//! Each `mod` declaration here pulls in `tests/integration/<name>.rs` (or
//! `tests/integration/<name>/mod.rs` for the multi-file `common` module).

mod book_segments;
mod bounce_deploy;
mod bounce_recovery;
mod cancelled_event;
mod common;
mod coupon_debt;
mod discovery;
mod flows;
mod forfeit_close;
mod history;
mod ledger_race;
mod market_orders;
mod matching_ladder;
mod mm_cycle;
mod multi_market;
mod multitoken;
mod oracle;
mod oracle_quorum;
mod order_book;
mod parallel_setup;
mod pmp;
mod pn_basic;
mod price_above_par;
mod proof_money;
mod resting_orders;
mod shutdown_orders;
mod usdc_market;
mod usdc_release;
