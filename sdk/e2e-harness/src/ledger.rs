//! File-backed ledger shared by every process in one e2e run.
//!
//! One JSON file (`ledger.json`) tracks pre-baked-account leases and a
//! rendezvous mailbox for the run; a stable sidecar (`ledger.lock`) is the
//! only thing ever flocked. `ledger.json` itself is replaced by tmp+rename
//! under that lock on every transaction, so a lock held across a rename
//! would protect nothing — the sidecar never moves, only the data file does.
//!
//! Every write goes through a single generation check: the file's `run_id`
//! must match the caller's. A process from a stale generation gets
//! `LedgerError::StaleRun` before anything is read into its closure or
//! written back, so a leftover process from a previous CI run can never
//! mutate state belonging to the run that superseded it. Only
//! [`Ledger::bootstrap`] may start a new generation.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fd_lock::RwLock as FileLock;
use serde::{Deserialize, Serialize};

/// Handle to the ledger for one run. Cheap to construct — [`Ledger::open`]
/// does not touch the filesystem; every real access happens inside
/// [`Ledger::with_txn`].
pub struct Ledger {
    path: PathBuf,
    lock_path: PathBuf,
    run_id: String,
}

/// Errors from ledger operations.
#[derive(Debug)]
pub enum LedgerError {
    /// The ledger belongs to a different generation than this `Ledger`
    /// handle. Returned before any read is applied or any write happens.
    StaleRun { ledger: String, mine: String },
    Io(io::Error),
    /// The ledger file could not be parsed, or a wait operation timed out.
    Corrupt(String),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::StaleRun { ledger, mine } => write!(
                f,
                "stale run: ledger is generation {ledger}, this process is generation {mine}"
            ),
            LedgerError::Io(e) => write!(f, "ledger io error: {e}"),
            LedgerError::Corrupt(msg) => write!(f, "ledger corrupt: {msg}"),
        }
    }
}

impl std::error::Error for LedgerError {}

impl From<io::Error> for LedgerError {
    fn from(e: io::Error) -> Self {
        LedgerError::Io(e)
    }
}

#[derive(Serialize, Deserialize)]
pub struct LedgerFile {
    pub run_id: String,
    /// sha256 of the manifest the bootstrapper deployed against.
    /// Provenance metadata only — never compared or enforced.
    pub manifest_hash: Option<String>,
    pub next_nonce: u64,
    /// Pre-baked account address -> current state.
    pub notes: BTreeMap<String, NoteState>,
    /// Rendezvous/result mailbox, keyed by slot or by an explicit key
    /// (see [`Ledger::rendezvous`], [`Ledger::wait_entry`], [`Ledger::put_entry`]).
    pub rendezvous: BTreeMap<String, RendezvousMark>,
}

#[derive(Serialize, Deserialize)]
pub enum NoteState {
    /// The account is available for lease. `balances` records the logical
    /// remaining balance per token type as observed when it was released —
    /// drift from what a test actually left behind is recorded here, not
    /// quarantined.
    Free {
        ecc_shell_remaining: Option<u128>,
        balances: BTreeMap<u32, u128>,
    },
    /// Held by a running test process.
    Leased { pid: u32, test: String },
    /// Removed from the free pool; will not be leased again this generation.
    Quarantined { reason: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RendezvousMark {
    pub run_id: String,
    pub pid: u32,
    pub test: String,
}

impl Ledger {
    /// One-sided generation reset: creates or overwrites `ledger.json` with
    /// an empty ledger stamped with `run_id`. Only the bootstrapper calls
    /// this; every other process only ever opens an existing generation.
    pub fn bootstrap(dir: &Path, run_id: &str, manifest_hash: Option<&str>) -> Result<(), LedgerError> {
        let lock_path = dir.join("ledger.lock");
        let mut fl = FileLock::new(open_rw(&lock_path)?);
        let _guard = fl.write()?;
        let fresh = LedgerFile {
            run_id: run_id.to_string(),
            manifest_hash: manifest_hash.map(str::to_string),
            next_nonce: 1,
            notes: BTreeMap::new(),
            rendezvous: BTreeMap::new(),
        };
        write_atomic(&dir.join("ledger.json"), &fresh)?;
        Ok(())
    }

    /// Opens a handle for an existing generation. Does not touch the
    /// filesystem; the generation check happens on first [`Ledger::with_txn`].
    pub fn open(dir: &Path, run_id: &str) -> Ledger {
        Ledger {
            path: dir.join("ledger.json"),
            lock_path: dir.join("ledger.lock"),
            run_id: run_id.to_string(),
        }
    }

    /// Runs `f` under an exclusive lock on `ledger.lock`: flock, read
    /// `ledger.json`, verify `run_id` (mismatch is `StaleRun` and nothing
    /// is written), apply `f`, write the result back via tmp+rename, unlock.
    pub fn with_txn<T>(&self, f: impl FnOnce(&mut LedgerFile) -> T) -> Result<T, LedgerError> {
        let mut fl = FileLock::new(open_rw(&self.lock_path)?);
        let _guard = fl.write()?;
        let mut lf: LedgerFile = read_json(&self.path)?;
        if lf.run_id != self.run_id {
            return Err(LedgerError::StaleRun {
                ledger: lf.run_id,
                mine: self.run_id.clone(),
            });
        }
        let out = f(&mut lf);
        write_atomic(&self.path, &lf)?;
        Ok(out)
    }

    /// Allocates and returns the next unique nonce for this generation.
    pub fn next_nonce(&self) -> Result<u64, LedgerError> {
        self.with_txn(|f| {
            let n = f.next_nonce;
            f.next_nonce += 1;
            n
        })
    }

    /// Marks `me` present at `slot` and blocks until `peer` shows up at the
    /// same slot (or `timeout` elapses). Used to synchronize two test
    /// processes that both need to be running before either proceeds.
    pub fn rendezvous(
        &self,
        slot: &str,
        me: &str,
        peer: &str,
        timeout: Duration,
    ) -> Result<RendezvousMark, LedgerError> {
        self.with_txn(|f| {
            f.rendezvous.insert(
                format!("{slot}/{me}"),
                RendezvousMark {
                    run_id: self.run_id.clone(),
                    pid: std::process::id(),
                    test: me.to_string(),
                },
            );
        })?;

        let peer_key = format!("{slot}/{peer}");
        let deadline = Instant::now() + timeout;
        loop {
            let found = self.with_txn(|f| f.rendezvous.get(&peer_key).cloned())?;
            if let Some(mark) = found {
                if mark.run_id != self.run_id {
                    return Err(LedgerError::StaleRun {
                        ledger: mark.run_id,
                        mine: self.run_id.clone(),
                    });
                }
                return Ok(mark);
            }
            if Instant::now() >= deadline {
                return Err(LedgerError::Corrupt(format!(
                    "rendezvous timeout on {slot}: peer {peer} absent"
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Read-only wait for an arbitrary mailbox entry: polls for `key` until
    /// it appears (checked against this generation) or `timeout` elapses.
    /// Never writes — used to read back a peer's result without touching the
    /// key the peer is waiting on.
    pub fn wait_entry(&self, key: &str, timeout: Duration) -> Result<RendezvousMark, LedgerError> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = self.with_txn(|f| f.rendezvous.get(key).cloned())?;
            if let Some(mark) = found {
                if mark.run_id != self.run_id {
                    return Err(LedgerError::StaleRun {
                        ledger: mark.run_id,
                        mine: self.run_id.clone(),
                    });
                }
                return Ok(mark);
            }
            if Instant::now() >= deadline {
                return Err(LedgerError::Corrupt(format!(
                    "wait_entry timeout: {key} absent"
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Write-only: records `payload` as a result at exactly `key`. Does not
    /// compose the key from a slot/caller pair — the caller passes the full
    /// key so a second call can never overwrite a key a peer is polling on
    /// with [`Ledger::wait_entry`].
    pub fn put_entry(&self, key: &str, payload: &str) -> Result<(), LedgerError> {
        self.with_txn(|f| {
            f.rendezvous.insert(
                key.to_string(),
                RendezvousMark {
                    run_id: self.run_id.clone(),
                    pid: std::process::id(),
                    test: payload.to_string(),
                },
            );
        })
    }
}

fn open_rw(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).write(true).open(path)
}

fn read_json(path: &Path) -> Result<LedgerFile, LedgerError> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| LedgerError::Corrupt(e.to_string()))
}

/// Serializes `value` to `path.with_extension("json.tmp")` (same directory
/// as `path`, so the following rename is atomic) and renames it onto `path`.
fn write_atomic(path: &Path, value: &LedgerFile) -> Result<(), LedgerError> {
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| LedgerError::Corrupt(e.to_string()))?;
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bootstrap_then_txn_roundtrip() {
        let d = TempDir::new().unwrap();
        Ledger::bootstrap(d.path(), "run-1", Some("mh")).unwrap();
        let led = Ledger::open(d.path(), "run-1");
        let n1 = led.next_nonce().unwrap();
        let n2 = led.next_nonce().unwrap();
        assert!(n2 > n1);
    }

    #[test]
    fn stale_run_reads_nothing_writes_nothing() {
        let d = TempDir::new().unwrap();
        Ledger::bootstrap(d.path(), "run-2", None).unwrap();
        // помечаем ноту, затем процесс "старого запуска" пытается писать
        let led2 = Ledger::open(d.path(), "run-2");
        led2.with_txn(|f| { f.notes.insert("0:aa".into(), NoteState::Leased { pid: 1, test: "t".into() }); }).unwrap();
        let stale = Ledger::open(d.path(), "run-1"); // старый RUN_ID
        let err = stale.with_txn(|f| { f.notes.clear(); }).unwrap_err();
        assert!(matches!(err, LedgerError::StaleRun { .. }));
        // и ledger НЕ изменился
        led2.with_txn(|f| assert!(matches!(f.notes.get("0:aa"), Some(NoteState::Leased { .. })))).unwrap();
    }

    #[test]
    fn bootstrap_resets_generation_one_sided() {
        let d = TempDir::new().unwrap();
        Ledger::bootstrap(d.path(), "run-1", None).unwrap();
        Ledger::open(d.path(), "run-1")
            .with_txn(|f| { f.notes.insert("0:aa".into(), NoteState::Quarantined { reason: "x".into() }); }).unwrap();
        Ledger::bootstrap(d.path(), "run-2", None).unwrap(); // новое поколение
        Ledger::open(d.path(), "run-2")
            .with_txn(|f| assert!(f.notes.is_empty(), "старые записи не переживают поколение")).unwrap();
    }

    #[test]
    fn rendezvous_stale_mark_is_not_a_peer() {
        let d = TempDir::new().unwrap();
        Ledger::bootstrap(d.path(), "run-2", None).unwrap();
        // метка чужого поколения, вписанная вручную (симулируем выжившего)
        Ledger::open(d.path(), "run-2").with_txn(|f| {
            // ключ = "{slot}/{peer}" — ровно тот, что читает реализация
            f.rendezvous.insert("pair/peer".into(), RendezvousMark { run_id: "run-1".into(), pid: 9, test: "peer".into() });
        }).unwrap();
        let led = Ledger::open(d.path(), "run-2");
        let err = led.rendezvous("pair", "me", "peer", Duration::from_millis(300)).unwrap_err();
        // строго StaleRun — таймаут означал бы, что ветка не упражнялась
        assert!(matches!(err, LedgerError::StaleRun { .. }));
    }
}
