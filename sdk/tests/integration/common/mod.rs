//! Shared helpers for integration tests. Each sub-module owns one
//! responsibility:
//!
//! - `context` — endpoint constants + `ClientContext` / `Dex` builders.
//! - `keys` — random keypair generation.
//! - `misc` — small utilities (time, account-active wait, balance read, GraphQL
//!   event-entry destructuring).
//! - `voucher` — `make_voucher_proof` driving the live halo2 pipeline.
//! - `pn` — `deploy_pn`, `fund_pn_gas`, `deploy_funded_pn`,
//!   `ensure_root_pn_funded`, `deploy_pn_with_keys`.
//! - `pmp` — `deploy_oracle_with_event`, `setup_pmp`, `PmpSetup`.
//!
//! Each test module imports the items it actually uses via
//! `use crate::common::<sub>::<item>;`.

pub mod context;
pub mod keys;
pub mod misc;
pub mod ob_pool;
pub mod pmp;
pub mod pn;
pub mod pn_pool;
pub mod voucher;
