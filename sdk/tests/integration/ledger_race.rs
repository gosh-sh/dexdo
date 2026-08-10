//! Multiprocess contention on the shared ledger: real OS processes, not
//! threads, rent/release/quarantine the same small note pool concurrently.
//!
//! Threads in one address space would pass this test even with `Ledger`'s
//! file locking removed entirely — a shared `BTreeMap` behind nothing but
//! Rust's own memory model still gives every thread a consistent view once
//! `with_txn`'s closure runs to completion. What the ledger actually
//! promises is a *cross-process* mutual-exclusion protocol (`flock` on a
//! stable sidecar file), and the only way to exercise that honestly is to
//! spawn processes that do not share an address space. So this test
//! re-executes its own test binary: each child is invoked with
//! `LEDGER_RACE_WORKER=<dir>,<run_id>,<worker-index>` set, and the test
//! function below checks for that variable before doing anything else, runs
//! the worker loop in its place, and returns — it never spawns children of
//! its own.
//!
//! Not `#[ignore]`: this needs no chain and no seed file, only a temp
//! directory, so it runs in ordinary PR CI and is the only thing that will
//! ever catch a regression in the locking protocol before it reaches a
//! stand.

use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;

use crate::common::ledger::Ledger;
use crate::common::ledger::LedgerError;
use crate::common::ledger::NoteState;

/// Child processes racing for the pool. Real contention needs more than one,
/// and needs them overlapping — four is enough for a broken lock to show up
/// in practice without the test taking long to run.
const WORKERS: u32 = 4;

/// Rent/release-or-taint cycles each worker attempts. Not every cycle finds
/// a `Free` note (see [`worker`]), so this is an attempt budget, not a
/// guaranteed rent count.
const CYCLES: u32 = 20;

/// Notes registered before the race starts. Sized against `CYCLES` and
/// [`TAINT_EVERY`] below: quarantining removes a note from the pool
/// permanently, and a pool too small relative to how often quarantine fires
/// would run dry early, leaving the back half of the race with nothing to
/// contend over. Six notes shared by four workers keeps `rent` finding a
/// `Free` note through most of the run instead of every worker spending its
/// remaining cycles finding the pool empty.
const POOL_SIZE: u32 = 6;

/// Quarantine cadence: every fifth successful cycle taints instead of
/// releasing. `i % TAINT_EVERY == 0` would quarantine on each worker's very
/// first successful cycle — with all four workers starting close together,
/// that removes up to four of the six notes before the race has done
/// anything else. Deferring the first quarantine to a worker's fifth cycle
/// (`i % TAINT_EVERY == TAINT_EVERY - 1`) spreads the drain across the run
/// instead of front-loading it into the first few contended rents.
const TAINT_EVERY: u32 = 5;

#[test]
fn ledger_survives_multiprocess_contention() {
    if let Ok(spec) = std::env::var("LEDGER_RACE_WORKER") {
        worker(&spec);
        return;
    }

    let dir = TempDir::new().expect("create temp ledger directory");
    Ledger::bootstrap(dir.path(), "race-run", None).expect("bootstrap ledger generation");
    seed_pool(dir.path());

    let exe = std::env::current_exe().expect("resolve this test binary's own path");
    let children: Vec<_> = (0..WORKERS)
        .map(|i| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "ledger_race::ledger_survives_multiprocess_contention",
                    "--nocapture",
                ])
                .env("LEDGER_RACE_WORKER", format!("{},race-run,{i}", dir.path().display()))
                .spawn()
                .unwrap_or_else(|e| panic!("spawn worker {i}: {e}"))
        })
        .collect();

    for (i, mut child) in children.into_iter().enumerate() {
        let status = child.wait().unwrap_or_else(|e| panic!("wait for worker {i}: {e}"));
        assert!(status.success(), "worker {i} exited with {status}");
    }

    // Every worker completed its whole cycle range without panicking, and
    // every cycle either releases or taints the note it rented before
    // moving on — so no note should still be `Leased` once all four
    // processes have exited cleanly. A `Leased` survivor here would mean a
    // worker rented a note and then died (or raced) between the rent and
    // its release/taint.
    let led = Ledger::open(dir.path(), "race-run");
    led.with_txn(|f| {
        assert!(
            !f.notes.values().any(|s| matches!(s, NoteState::Leased { .. })),
            "dangling lease: a note is still Leased after every worker exited cleanly"
        );
        assert!(
            f.notes.values().any(|s| matches!(s, NoteState::Quarantined { .. })),
            "quarantines lost: no note ended up Quarantined across the whole race"
        );
    })
    .expect("read final ledger state");

    // Per-worker counters, written to a stats file because a worker is a
    // separate process and cannot hand its counts back through memory. Every
    // one of the three operations has to have fired at least once somewhere
    // across the whole race — a run where rents, releases or taints never
    // happened exercised less of the protocol than it appears to.
    let (mut total_rents, mut total_releases, mut total_taints) = (0u32, 0u32, 0u32);
    for i in 0..WORKERS {
        let raw = std::fs::read_to_string(dir.path().join(format!("stats_{i}")))
            .unwrap_or_else(|e| panic!("read stats file for worker {i}: {e}"));
        let counts: Vec<u32> = raw
            .split_whitespace()
            .map(|s| s.parse().unwrap_or_else(|e| panic!("parse stats `{s}` for worker {i}: {e}")))
            .collect();
        assert_eq!(counts.len(), 3, "stats file for worker {i} malformed: `{raw}`");
        total_rents += counts[0];
        total_releases += counts[1];
        total_taints += counts[2];
    }
    assert!(
        total_rents > 0 && total_releases > 0 && total_taints > 0,
        "every operation must occur at least once: rent={total_rents} release={total_releases} \
         taint={total_taints}"
    );
}

/// Registers [`POOL_SIZE`] synthetic `Free` notes before any worker starts.
/// `Ledger::bootstrap` starts a generation with an empty `notes` map, and
/// without this no worker's `rent` would ever find anything to lease — this
/// test has no seed file or real `PrivateNote` addresses, only the ledger's
/// own bookkeeping, so a made-up address is exactly as good as a real one.
fn seed_pool(dir: &Path) {
    let led = Ledger::open(dir, "race-run");
    led.with_txn(|f| {
        for i in 0..POOL_SIZE {
            f.notes.insert(
                format!("0:race-note-{i:02}"),
                NoteState::Free { ecc_shell_remaining: None, balances: Default::default() },
            );
        }
    })
    .expect("register the race pool");
}

/// One worker's whole run: parses `spec` as `<dir>,<run_id>,<worker-index>`,
/// then performs [`CYCLES`] rent attempts, each followed by a release or a
/// [`TAINT_EVERY`]-cadence taint, and finally writes its operation counts to
/// `<dir>/stats_<index>` for the parent to aggregate.
fn worker(spec: &str) {
    let mut parts = spec.splitn(3, ',');
    let dir = parts.next().expect("worker spec missing dir");
    let run_id = parts.next().expect("worker spec missing run_id");
    let idx = parts.next().expect("worker spec missing worker index");

    let dir = Path::new(dir);
    let led = Ledger::open(dir, run_id);
    let held_dir = dir.join("held");
    std::fs::create_dir_all(&held_dir).expect("create held/ marker directory");

    let (mut rents, mut releases, mut taints) = (0u32, 0u32, 0u32);

    for i in 0..CYCLES {
        // Rent: find the first `Free` note and mark it `Leased`, atomically —
        // both the read and the write happen inside one `with_txn`, so two
        // concurrent workers can never be handed the same address.
        let rented = led
            .with_txn(|f| {
                f.notes.iter_mut().find(|(_, st)| matches!(st, NoteState::Free { .. })).map(
                    |(addr, st)| {
                        *st = NoteState::Leased { pid: std::process::id(), test: idx.to_string() };
                        addr.clone()
                    },
                )
            })
            .expect("rent transaction");
        let Some(addr) = rented else {
            // Every remaining note is `Leased` by a peer or already
            // `Quarantined`. Not a failure — the pool is sized so this
            // resolves as peers release theirs.
            continue;
        };

        // Double-lease detection has to be atomic itself, or it proves
        // nothing: checking `marker.exists()` and then creating the file is
        // two syscalls, and two workers that (wrongly) both hold the same
        // note as `Leased` could each observe "absent" in the gap between
        // them and both proceed as if they were first. `create_new` folds
        // the check and the create into the one syscall that actually
        // enforces exclusivity — only one of two double-leasing workers can
        // win it.
        let marker = held_dir.join(addr.replace(':', "_"));
        std::fs::OpenOptions::new().write(true).create_new(true).open(&marker).unwrap_or_else(
            |e| {
                panic!(
                    "DOUBLE LEASE on {addr}: marker already exists ({e}) — two workers held the \
                     same note as Leased at once"
                )
            },
        );
        std::thread::sleep(Duration::from_millis(5));
        std::fs::remove_file(&marker).expect("remove held marker");

        let taint = i % TAINT_EVERY == TAINT_EVERY - 1;
        led.with_txn(|f| {
            let st = f.notes.get_mut(&addr).expect("leased note must still be present");
            *st = if taint {
                NoteState::Quarantined { reason: format!("worker {idx}") }
            } else {
                NoteState::Free { ecc_shell_remaining: None, balances: Default::default() }
            };
        })
        .expect("release/taint transaction");

        if taint {
            taints += 1;
        } else {
            releases += 1;
        }
        rents += 1;
    }

    std::fs::write(dir.join(format!("stats_{idx}")), format!("{rents} {releases} {taints}"))
        .expect("write stats file");
}

/// A worker whose `run_id` names a generation the ledger has already moved
/// past — exactly what a process left running from a previous CI run would
/// find if it tried to touch the run that superseded it. `Ledger::with_txn`
/// checks the generation before applying or writing anything, so this must
/// report `LedgerError::StaleRun` specifically, not merely fail: a different
/// bug (a corrupt read, an I/O error) would also make the worker exit
/// non-zero, and only checking the exit code would let that bug wear this
/// test's green checkmark.
#[test]
fn stale_generation_worker_gets_stale_run_not_corrupt() {
    if let Ok(spec) = std::env::var("LEDGER_RACE_STALE_WORKER") {
        stale_worker(&spec);
        return;
    }

    let dir = TempDir::new().expect("create temp ledger directory");
    Ledger::bootstrap(dir.path(), "gen-1", None).expect("bootstrap first generation");
    // A new generation supersedes "gen-1", the same way a fresh CI run's
    // bootstrap supersedes whatever a leftover process from the previous run
    // still believes it belongs to.
    Ledger::bootstrap(dir.path(), "gen-2", None).expect("bootstrap second generation");

    let exe = std::env::current_exe().expect("resolve this test binary's own path");
    let status = std::process::Command::new(&exe)
        .args([
            "--exact",
            "ledger_race::stale_generation_worker_gets_stale_run_not_corrupt",
            "--nocapture",
        ])
        .env("LEDGER_RACE_STALE_WORKER", format!("{},gen-1", dir.path().display()))
        .status()
        .expect("run stale-generation worker process");
    assert!(status.success(), "stale-generation worker did not observe StaleRun as expected");
}

/// Attempts one write against a generation this process does not belong to,
/// asserts the error is exactly [`LedgerError::StaleRun`], and asserts
/// `ledger.json` was not modified — `with_txn` rejects a stale generation
/// before reading the closure's result into anything or writing it back, so
/// the file's bytes must be identical before and after the attempt.
fn stale_worker(spec: &str) {
    let mut parts = spec.splitn(2, ',');
    let dir = parts.next().expect("stale worker spec missing dir");
    let run_id = parts.next().expect("stale worker spec missing run_id");
    let dir = Path::new(dir);
    let ledger_json = dir.join("ledger.json");

    let before = std::fs::read(&ledger_json).expect("read ledger.json before the stale write");

    let led = Ledger::open(dir, run_id);
    let err = led
        .with_txn(|f| {
            f.notes.clear();
        })
        .expect_err("a write from a superseded generation must be rejected");
    assert!(matches!(err, LedgerError::StaleRun { .. }), "expected StaleRun, got {err:?}");

    let after = std::fs::read(&ledger_json).expect("read ledger.json after the stale write");
    assert_eq!(before, after, "a rejected stale-generation write must not touch ledger.json");
}
