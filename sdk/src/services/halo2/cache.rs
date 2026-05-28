//! Storage abstraction for the halo2 prover's PK / break-points / VK cache.
//!
//! The `halo2-proover` library writes three files into a directory the first
//! time it generates a proof and reads them on every subsequent call:
//! `pk_cache.bin`, `break_points_cache.bin`, `vk_cache.bin`. Tests pass a
//! local directory; library users embedding the prover into their own service
//! (e.g. mobile, where filesystem access is sandboxed) implement the trait
//! on top of any storage they like.

use std::path::PathBuf;

/// Provides the directory in which the halo2 prover persists its keygen
/// artefacts. Returning `None` disables caching (every proof triggers a
/// fresh keygen — slow).
pub trait ProverCacheStorage {
    fn pk_dir(&self) -> Option<PathBuf>;
}

/// Default file-system backed cache. Creates the directory on first use.
#[derive(Debug, Clone)]
pub struct FilesystemCache(pub PathBuf);

impl FilesystemCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

impl ProverCacheStorage for FilesystemCache {
    fn pk_dir(&self) -> Option<PathBuf> {
        if let Err(e) = std::fs::create_dir_all(&self.0) {
            tracing::warn!(
                error = %e,
                path = %self.0.display(),
                "halo2 prover cache directory could not be created; caching disabled"
            );
            return None;
        }
        Some(self.0.clone())
    }
}

/// Marker cache that disables persistence — every proof runs keygen.
#[derive(Debug, Clone, Default)]
pub struct NoCache;

impl ProverCacheStorage for NoCache {
    fn pk_dir(&self) -> Option<PathBuf> {
        None
    }
}
