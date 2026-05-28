//! Layered halo2 voucher pipeline. Lifted out of `ackinacki-kit` so the
//! kit can stay a thin ABI layer (see `EXTRACT_HALO2_TO_BEWE.md`).
//!
//! - `cache` — `ProverCacheStorage` trait + `FilesystemCache` / `NoCache`
//!   impls. The prover persists `pk_cache.bin` / `break_points_cache.bin` /
//!   `vk_cache.bin` here.
//! - `proof` — thin wrappers around the witness-export library and the halo2
//!   prover, plus history-window math (`target_height_for_layer`).
//! - `sk_commit` — `skUCommit = poseidon([sk_u, 0])` reproduction of the
//!   stand-alone `sk-commit-tool`.
//! - `voucher_event` — GraphQL helpers to capture `RootPN.VoucherGenerated`
//!   ext-out messages and wait on chain height.
//! - `live` — high-level pipeline: send `RootPN.generateVoucher` via Giver →
//!   wait for the L-th history-proof window → export witness → run prover.

pub mod cache;
pub mod giver_voucher;
pub mod live;
pub mod paths;
pub mod proof;
pub mod sk_commit;
pub mod voucher_event;

pub use paths::Halo2Paths;
pub use paths::Halo2PathsError;
pub use paths::SRS_K;
