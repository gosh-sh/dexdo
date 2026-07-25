//! Reader/writer protocol for the one blockchain shared by every process in
//! an e2e run, encoded as an advisory lock on a stable sidecar file
//! (`dir/b0.lock`, alongside the ledger's own `ledger.json`/`ledger.lock`).
//!
//! Most scenarios only mutate their own accounts and can run concurrently
//! with each other — they take the lock SHARED. A scenario that asserts
//! global conservation of funds cannot tolerate any other process landing a
//! transfer inside its measurement window, so it takes the lock EXCLUSIVE,
//! which blocks until every shared holder has released.
//!
//! Helpers added in later tasks accept `&ChainLockGuard` and never call
//! `flock` themselves: a nested acquisition on the same file either
//! self-deadlocks (a second, independent open of the same path blocks
//! behind the first) or silently converts an exclusive hold into a shared
//! one (re-locking the same open file description with a different mode),
//! neither of which is what a caller wants.
//!
//! `flock` locks belong to the open file description, not the process, so
//! two independent `open()` calls conflict even from inside a single
//! process — [`ChainLockGuard::shared`] and [`ChainLockGuard::b0_exclusive`]
//! each open a fresh handle rather than reusing one, which is what lets the
//! test below exercise exclusion without spawning a second process.

use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Holds an advisory `flock` on `dir/b0.lock` for as long as it is alive.
/// Dropping it releases the lock via `LOCK_UN` (belt-and-braces — closing
/// the underlying fd already releases it, but the explicit call makes the
/// release point visible at the site a reader looks for it).
pub struct ChainLockGuard {
    file: fs::File,
}

impl ChainLockGuard {
    /// Blocking shared acquire — for chain-mutating scenarios, which may
    /// hold the lock concurrently with each other.
    pub fn shared(dir: &Path) -> io::Result<ChainLockGuard> {
        Self::acquire(dir, libc::LOCK_SH)
    }

    /// Blocking exclusive acquire — for a B0 (conservation) scenario, which
    /// must be the only process touching the chain. Blocks until every
    /// shared holder has released.
    pub fn b0_exclusive(dir: &Path) -> io::Result<ChainLockGuard> {
        Self::acquire(dir, libc::LOCK_EX)
    }

    /// Non-blocking shared acquire. Returns `Ok(None)` if the lock is
    /// currently held exclusively; any other error (e.g. the lock file is
    /// unreadable) is returned as `Err` rather than folded into `None`, so a
    /// real error cannot masquerade as "just busy" and hang a scenario
    /// forever.
    pub fn try_shared(dir: &Path) -> io::Result<Option<ChainLockGuard>> {
        Self::try_acquire(dir, libc::LOCK_SH)
    }

    /// Non-blocking exclusive acquire. Returns `Ok(None)` if any shared or
    /// exclusive holder is currently live; see [`ChainLockGuard::try_shared`]
    /// for the error-vs-busy distinction.
    pub fn try_b0_exclusive(dir: &Path) -> io::Result<Option<ChainLockGuard>> {
        Self::try_acquire(dir, libc::LOCK_EX)
    }

    fn acquire(dir: &Path, op: libc::c_int) -> io::Result<ChainLockGuard> {
        let file = open_lock_file(dir)?;
        flock(&file, op)?;
        Ok(ChainLockGuard { file })
    }

    fn try_acquire(dir: &Path, op: libc::c_int) -> io::Result<Option<ChainLockGuard>> {
        let file = open_lock_file(dir)?;
        match flock(&file, op | libc::LOCK_NB) {
            Ok(()) => Ok(Some(ChainLockGuard { file })),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for ChainLockGuard {
    fn drop(&mut self) {
        // Closing `self.file`'s fd right after this call would release the
        // lock on its own; the explicit LOCK_UN exists so the release point
        // is visible at the site a reader looks for it, not because it's
        // load-bearing. Best-effort: there is nothing actionable to do with
        // an error here.
        let _ = flock(&self.file, libc::LOCK_UN);
    }
}

fn open_lock_file(dir: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).write(true).open(dir.join("b0.lock"))
}

fn flock(file: &fs::File, op: libc::c_int) -> io::Result<()> {
    // SAFETY: `file` owns a valid, open fd for the duration of this call.
    let ret = unsafe { libc::flock(file.as_raw_fd(), op) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn exclusive_excludes_shared_and_vice_versa() {
        let d = TempDir::new().unwrap();
        let ex = ChainLockGuard::b0_exclusive(d.path()).unwrap();
        // второй handle в том же процессе: flock на другом fd конфликтует
        assert!(
            ChainLockGuard::try_shared(d.path()).unwrap().is_none(),
            "shared при живом exclusive"
        );
        drop(ex);
        let sh = ChainLockGuard::shared(d.path()).unwrap();
        assert!(
            ChainLockGuard::try_b0_exclusive(d.path()).unwrap().is_none(),
            "exclusive при живом shared"
        );
        drop(sh);
        assert!(
            ChainLockGuard::try_b0_exclusive(d.path()).unwrap().is_some(),
            "exclusive не смог взяться после освобождения shared"
        );
    }
}
