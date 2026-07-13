//! Explicit configuration for halo2 on-disk artifacts.
//!
//! `prove_voucher_for_event` needs two writable directories on disk:
//!
//! 1. **Prover cache** — the Hermez KZG SRS (`hermez_kzg_srs_k19.bin`, ~64 MB,
//!    downloaded once from GOSH on first proof — see `proof::HERMEZ_KZG_SRS_URL`)
//!    plus the keygen artefacts `pk_cache.bin` (~464 MB) / `vk_cache.bin` /
//!    `break_points_cache.bin`. Read-write, persistent. A lost cache re-downloads
//!    the SRS and re-runs one ~5-minute keygen on the next call. (The dodex
//!    `api` / `indexer` never use halo2, so none of this applies to deploying
//!    the backend.)
//! 2. **Fixture dir** — `dex_fixture_live_L{layer}_H{height}_T{nanos}.json`
//!    witness dumps written per-proof. Read-write, ephemeral; safe to wipe
//!    between runs.
//!
//! `srs_dir` remains on the struct only for the legacy `ensure_srs` /
//! `gen_srs` path (insecure, unused by the live prover); see `ensure_srs`.
//!
//! On a host machine env vars + `./params/...` defaults are fine, but
//! mobile sandboxes (iOS / Android Tauri builds) need the host to pick
//! these paths at runtime — that's what this module enables. Configure
//! `Halo2Paths` once at boot, validate it, then thread it through every
//! `prove_voucher_for_event` call.

use std::path::Path;
use std::path::PathBuf;

/// SRS file degree we proof against (`2^K = 524288` rows). The SRS file
/// on disk must be named `kzg_bn254_{K}.srs`.
pub const SRS_K: u32 = 19;

/// Default sub-dirs used when the host doesn't override them. They mirror
/// the historical env-var defaults so existing tests / CLI keep working.
const DEFAULT_SRS_DIR: &str = "params";
const DEFAULT_PROVER_CACHE_DIR: &str = "params/halo2_cache";
const DEFAULT_FIXTURE_DIR: &str = "target/halo2_fixtures";

const ENV_SRS_DIR: &str = "PARAMS_DIR";
const ENV_PROVER_CACHE_DIR: &str = "HALO2_PK_CACHE";
const ENV_FIXTURE_DIR: &str = "HALO2_FIXTURE_DIR";

/// Filesystem layout for halo2 prover artifacts. Cheap to clone; hold one
/// instance for the lifetime of the host process.
#[derive(Debug, Clone)]
pub struct Halo2Paths {
    /// Directory containing `kzg_bn254_{SRS_K}.srs`. Read-only at runtime.
    pub srs_dir: PathBuf,
    /// Directory for `pk_cache.bin` / `vk_cache.bin` /
    /// `break_points_cache.bin`. Read-write, persistent.
    pub prover_cache_dir: PathBuf,
    /// Directory for ephemeral witness fixture JSONs.
    /// Read-write, safe to wipe.
    pub fixture_dir: PathBuf,
}

impl Halo2Paths {
    /// Construct from env vars with the existing project defaults
    /// (`PARAMS_DIR=./params`, `HALO2_PK_CACHE=./params/halo2_cache`,
    /// `HALO2_FIXTURE_DIR=./target/halo2_fixtures`). Suitable for tests
    /// and CLI; mobile / packaged hosts should build the struct
    /// explicitly from `app_data_dir` / `cache_dir` instead.
    pub fn from_env() -> Self {
        Self {
            srs_dir: env_path(ENV_SRS_DIR, DEFAULT_SRS_DIR),
            prover_cache_dir: env_path(ENV_PROVER_CACHE_DIR, DEFAULT_PROVER_CACHE_DIR),
            fixture_dir: env_path(ENV_FIXTURE_DIR, DEFAULT_FIXTURE_DIR),
        }
    }

    /// Verify the writable directories can be created. Call once at boot —
    /// turns deep halo2-internal panics (unwritable cache) into an upfront
    /// `Result` the host can surface to the user. The SRS is no longer a
    /// precondition here: the prover downloads the Hermez KZG SRS into
    /// `prover_cache_dir` on first proof (see `proof::HERMEZ_KZG_SRS_URL`).
    pub fn validate(&self) -> Result<(), Halo2PathsError> {
        ensure_writable_dir(&self.prover_cache_dir).map_err(|source| {
            Halo2PathsError::ProverCacheDirNotWritable {
                path: self.prover_cache_dir.clone(),
                source,
            }
        })?;
        ensure_writable_dir(&self.fixture_dir).map_err(|source| {
            Halo2PathsError::FixtureDirNotWritable { path: self.fixture_dir.clone(), source }
        })?;
        Ok(())
    }

    /// Absolute path of the SRS file (`{srs_dir}/kzg_bn254_{SRS_K}.srs`).
    pub fn srs_path(&self) -> PathBuf {
        self.srs_dir.join(format!("kzg_bn254_{SRS_K}.srs"))
    }

    /// Whether the SRS file is already on disk.
    pub fn srs_exists(&self) -> bool {
        self.srs_path().is_file()
    }

    /// **LEGACY / INSECURE — not used by the live prover.** Generates a
    /// self-seeded `gen_srs` SRS whose trapdoor is knowable; proofs built
    /// against it do NOT verify on the Hermez-anchored on-chain verifier.
    /// The prover now sources the real SRS over the network
    /// (`proof::HERMEZ_KZG_SRS_URL`), so nothing in this repo calls this.
    /// Kept only as an escape hatch for the deprecated `Prover::new` path.
    pub fn ensure_srs(&self) {
        if self.srs_exists() {
            return;
        }
        self.install_env();
        let _ = halo2_base::utils::fs::gen_srs(SRS_K);
    }

    /// Apply this configuration to the global env so the third-party
    /// halo2 lib (which reads `PARAMS_DIR` directly inside `gen_srs`)
    /// finds the SRS at the host-supplied location.
    ///
    /// Safety: mutating the process env is not thread-safe. Call once at
    /// boot from a single thread, before any halo2 work starts. The
    /// `unsafe` is `std::env::set_var`'s in Rust 2024+; we don't add
    /// extra invariants beyond "no concurrent env mutation".
    pub fn install_env(&self) {
        let dir_str = self.srs_dir.to_string_lossy().into_owned();
        // SAFETY: documented contract — single-threaded boot-time call.
        unsafe {
            std::env::set_var(ENV_SRS_DIR, dir_str);
        }
    }
}

impl Default for Halo2Paths {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug)]
pub enum Halo2PathsError {
    SrsNotFound { path: PathBuf },
    ProverCacheDirNotWritable { path: PathBuf, source: std::io::Error },
    FixtureDirNotWritable { path: PathBuf, source: std::io::Error },
}

impl std::fmt::Display for Halo2PathsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SrsNotFound { path } => {
                write!(f, "SRS file not found at {}", path.display())
            }
            Self::ProverCacheDirNotWritable { path, source } => {
                write!(f, "prover cache dir {} not writable: {source}", path.display())
            }
            Self::FixtureDirNotWritable { path, source } => {
                write!(f, "fixture dir {} not writable: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for Halo2PathsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SrsNotFound { .. } => None,
            Self::ProverCacheDirNotWritable { source, .. }
            | Self::FixtureDirNotWritable { source, .. } => Some(source),
        }
    }
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
}

fn ensure_writable_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    let probe = path.join(".halo2_paths_probe");
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)?;
    Ok(())
}
