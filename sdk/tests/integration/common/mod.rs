//! Shared helpers for integration tests. Each sub-module owns one
//! responsibility:
//!
//! - `context` — endpoint constants + `ClientContext` / `Dex` builders.
//! - `allocator` — `sweep_verdict`, the reuse judgment over a note's decoded
//!   storage, pinned against the full `PrivateNote` ABI field set; and
//!   `Allocator`/`LeasedPn`, the pool of pre-baked `PrivateNote`s tests rent
//!   from and release/taint back into, quarantining on anything but a
//!   verified-clean return.
//! - `chain_reader` — `ChainReader`, the single read path for on-chain
//!   account state (raw BOC, physical balance, decoded storage fields).
//! - `invariant` — B0 money check: exact per-currency conservation over a
//!   declared set of contracts, per-account physical (ECC) deltas, and the
//!   quiescence barriers a snapshot must be taken behind.
//! - `keys` — random keypair generation.
//! - `misc` — small utilities (time, account-active wait, balance read, GraphQL
//!   event-entry destructuring).
//! - `voucher` — `make_voucher_proof` driving the live halo2 pipeline.
//! - `pn` — `deploy_pn`, `fund_pn_gas`, `deploy_funded_pn`,
//!   `ensure_root_pn_funded`, `deploy_pn_with_keys`.
//! - `pmp` — `deploy_oracle_with_event`, `setup_pmp`, `PmpSetup`; and the
//!   two-phase split `prepare_oracle_event`/`deploy_pmp_with_deployer` with
//!   `OracleEventCtx`, for scenarios that need a snapshot between
//!   publishing the event and deploying the PMP.
//! - `preflight` — refuses to run against a stand that does not match the
//!   build-time contract manifest: deployed code hashes, the semantic hashes of
//!   the ABIs compiled into these binaries, and the zerostate invariants over
//!   the pre-baked notes.
//! - `ledger` — re-exported from the `dodex-e2e-harness` crate; the file-backed
//!   account registry shared across concurrent e2e test processes.
//! - `locks` — re-exported from the `dodex-e2e-harness` crate; the
//!   `ChainLockGuard` shared/exclusive protocol on `b0.lock`.
//!
//! Each test module imports the items it actually uses via
//! `use crate::common::<sub>::<item>;`.

pub mod allocator;
pub mod chain_reader;
pub mod context;
pub mod invariant;
pub mod keys;
pub mod misc;
pub mod ob_pool;
pub mod pmp;
pub mod pn;
pub mod pn_pool;
pub mod preflight;
pub mod voucher;

pub use dodex_e2e_harness::ledger;
pub use dodex_e2e_harness::locks;
