// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! The observer: end-to-end indexer invariants over the WHOLE e2e run.
//!
//! It creates no traffic — it reads the state the scenarios left behind. Hence it
//! runs as the pipeline's last step and with `status: [success, failure]`: a run
//! in which a scenario failed needs the diagnosis all the more.
//!
//! Two properties make it usable as a BLOCKING step; without them it would fail
//! for the wrong reason regularly and for the right one almost never:
//!
//! * it POLLS to convergence with a deadline instead of taking a snapshot.
//!   Capture ticks every 3 s, the reconciler every 15 s, and visibility is stamped
//!   after a full sweep cycle; a book seeded seconds before the tail is
//!   legitimately `discovering`;
//! * every claim is bounded by the run's WINDOW. The stand's Postgres outlives
//!   pipelines, so a book left behind by an aborted run would otherwise fail the
//!   next one for a foreign reason.
//!
//! There is no SQL of its own here: every predicate lives in `IndexerRepository`,
//! because `WEDGED_BOOKS_WHERE` and `PENDING_PROJECTION_WHERE` are the only
//! sources of the corresponding gauges, and IX-MET-03 requires the check to match
//! the gauge.
//!
//! TWO ANCHORS, one per half of the ingest scope. `config::SCOPED_EVENT_IDS` is a
//! single list covering `contracts/dex` and `contracts/airegistry`, and a single
//! capture loop feeds both — so an inference anchor alone cannot answer for the
//! DEX side, and an edit that drops the DEX ids would leave it green. Nothing
//! else in the suite would notice either: the DEX scenarios verify placement by
//! polling the CONTRACT (`getOrdersByOwner`), never the read model, so a run in
//! which DEX capture is dark passes them all.
//!
//! `#[ignore]` as on the other e2e binaries: a local run does not touch them, CI
//! calls them with `--run-ignored only`.

use std::env;
use std::time::Duration;
use std::time::Instant;

use dodex_infrastructure::database;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use sqlx::postgres::PgPoolOptions;

/// `None` means "the step ran outside the stand": there is nothing to assert, the
/// test prints the reason and exits. On the host the variable comes from a
/// pipeline secret, and its absence there is caught not here but by the
/// empty-result guard in the script.
async fn observer_repo() -> Option<IndexerRepository> {
    let _ = dotenvy::dotenv();
    let url = match env::var("TEST_DATABASE_URL") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            eprintln!("observer: TEST_DATABASE_URL not set — nothing to observe");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("observer: connect to TEST_DATABASE_URL");
    database::run_migrations(&pool).await.expect("observer: run migrations");
    Some(IndexerRepository::new(pool))
}

/// The run's start, in unix seconds. The host script computes it as
/// `now_on_host - elapsed`, where `elapsed` is measured entirely against the CI
/// runner's clock, so any clock offset cancels out.
///
/// A missing variable is not a silent relaxation: the window falls back to a day
/// and that is printed. A day is not "almost always" but an honest bound: on a
/// stand with a persistent database, an unbounded anchor would be satisfied by the
/// run before last.
fn run_window() -> i64 {
    match env::var("E2E_STARTED_AT").ok().and_then(|v| v.parse::<i64>().ok()) {
        Some(t) => t,
        None => {
            let fallback = chrono::Utc::now().timestamp() - 86_400;
            eprintln!(
                "observer: E2E_STARTED_AT not set — falling back to a 24h window. \
                 Assertions still hold, but residue from a run inside that window \
                 is indistinguishable from this run's own work"
            );
            fallback
        }
    }
}

/// The convergence deadline for the two tests that wait on the RECONCILER. The
/// value lives HERE and nowhere else: the host script only forwards the variable
/// and carries no default of its own. A second source of the same quantity would
/// drift silently — the script would go on honestly printing an untruth about the
/// worst case after only the Rust side was edited.
const DEFAULT_DEADLINE_SECS: u64 = 240;

/// The capture anchor's deadline, an order of magnitude below
/// [`DEFAULT_DEADLINE_SECS`] because it waits on something else entirely.
///
/// The other two poll because a fact becomes true LATER: a book is stamped
/// visible only after a full reconciler sweep, so a book seeded seconds before
/// the tail is legitimately still `discovering`. Capture has no such stage —
/// a row is ingested within a tick (3s) of the event and projected by the next
/// drain of the loop — and the DEX orders this anchor is taken over are placed
/// by scenarios that finished minutes before this step starts. On a working
/// stand the answer is already in the table at the first probe; the polling is
/// only so a projection loop caught mid-batch is not read as a dead one.
///
/// Deliberately NOT overridable by `OBSERVER_DEADLINE_SECS`. That variable
/// exists to loosen CONVERGENCE on a slow stand, and stretching a check that
/// converges instantly buys nothing while spending the step's budget: #300 ran
/// 126.8 minutes against a 120-minute pipeline ceiling and was killed, which
/// skipped `net_down` and stranded the host lease for its full TTL.
const CAPTURE_DEADLINE_SECS: u64 = 30;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The reconciler deadline actually in force, override applied.
fn reconciler_deadline_secs() -> u64 {
    env::var("OBSERVER_DEADLINE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DEADLINE_SECS)
}

/// Announces this test's own deadline together with the step's worst case, and
/// returns it.
///
/// Printed by every test, because the three no longer share one number. The worst
/// case is their SUM — the script calls the binary with `--test-threads 1` — and
/// it is computed here from the same values the tests are handed rather than
/// written down a second time. The pipeline budget is tight enough that this
/// total must be visible in the output instead of derived by the reader from
/// source.
fn announce(secs: u64) -> Duration {
    let worst = reconciler_deadline_secs() * 2 + CAPTURE_DEADLINE_SECS;
    eprintln!(
        "observer: deadline {secs}s for this test; the binary holds three `#[ignore]` tests \
         run with --test-threads 1, so a step where none of them converges costs about \
         {worst}s plus compilation"
    );
    Duration::from_secs(secs)
}

fn deadline() -> Duration {
    announce(reconciler_deadline_secs())
}

fn capture_deadline() -> Duration {
    announce(CAPTURE_DEADLINE_SECS)
}

/// One snapshot: `Ok(())` — everything converged, `Err(text)` — what has not yet.
async fn snapshot(repo: &IndexerRepository, since: i64) -> anyhow::Result<Result<(), String>> {
    let mut off: Vec<String> = Vec::new();

    let pending = repo.pending_projection_since(since).await?;
    if !pending.is_empty() {
        let total: i64 = pending.iter().map(|(_, n)| *n).sum();
        off.push(format!(
            "the backlog did not converge: {total} rows ingested in the window and not projected — {pending:?}"
        ));
    }

    let books = repo.inference_books_with_events_since(since).await?;
    let without = repo.inference_books_without_verdict(&books).await?;
    if !without.is_empty() {
        off.push(format!(
            "books with no verdict (neither visible, nor superseded, nor failing WITH A REASON): {without:?}"
        ));
    }

    let wedged = repo.inference_wedged_book_addresses(&books).await?;
    if !wedged.is_empty() {
        off.push(format!("visible books hold unprocessed events: {wedged:?}"));
    }

    Ok(if off.is_empty() { Ok(()) } else { Err(off.join("\n  ")) })
}

/// Printed on BOTH outcomes, and that is not symmetry for its own sake: the
/// distribution of causes is needed most on a failed run, and the `panic!` sits
/// inside the loop and never reaches the tail of the test. Without it, "the reason
/// is named" is indistinguishable from "something was written".
///
/// Query errors are never propagated from here: on the red path the diagnostics have
/// no right to displace the real cause of the failure. How each is absorbed differs by
/// what its default would claim — the book list defaults to empty and the undecodable
/// count to the `-1` sentinel, while a failed failing-books query is rendered inline,
/// because there its default (`[]`) reads as the verdict "nobody is failing".
async fn print_diagnostics(repo: &IndexerRepository, since: i64, elapsed: Duration) {
    let books = repo.inference_books_with_events_since(since).await.unwrap_or_default();
    let undecodable = repo.count_undecodable_since(since).await.unwrap_or(-1);
    // Rendered, not defaulted: an empty list and a failed query both print as
    // `[]` under `unwrap_or_default`, and "nobody is failing" is the single most
    // reassuring line this diagnostic emits. Same reason `count_undecodable_since`
    // above carries a `-1` sentinel.
    //
    // The cover is partial and the limit is worth knowing: it catches THIS query
    // failing. If `inference_books_with_events_since` failed instead, `books` is
    // `[]`, this query honestly returns `Ok([])` for an empty scope, and the same
    // reassuring line appears — with `books in window 0` beside it as the only
    // tell.
    let failing = match repo.inference_failing_books(&books).await {
        Ok(rows) => format!("{rows:?}"),
        Err(err) => format!("<query failed: {err}>"),
    };
    eprintln!(
        "observer: {}s; books in window {}; undecodable rows {undecodable} \
         (diagnostic, not a failure; -1 means the query itself failed); \
         failing with a reason: {failing}",
        elapsed.as_secs(),
        books.len()
    );
}

#[tokio::test]
#[ignore = "e2e: reads the stand database at the tail of a run"]
async fn the_run_converged_and_every_book_of_it_has_a_verdict() {
    let Some(repo) = observer_repo().await else { return };
    let since = run_window();
    let limit = deadline();

    let started = Instant::now();
    loop {
        match snapshot(&repo, since).await.expect("observer: snapshot query failed") {
            Ok(()) => break,
            Err(why) => {
                if started.elapsed() >= limit {
                    print_diagnostics(&repo, since, started.elapsed()).await;
                    panic!(
                        "observer: invariants did not converge within {}s:\n  {why}\n\
                         The deadline is overridden by OBSERVER_DEADLINE_SECS. Polling, not a \
                         snapshot, because capture ticks every 3s, the reconciler every 15s, \
                         and visibility is stamped after a full sweep cycle",
                        limit.as_secs()
                    );
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }

    print_diagnostics(&repo, since, started.elapsed()).await;
}

#[tokio::test]
#[ignore = "e2e: reads the stand database at the tail of a run"]
async fn at_least_one_visible_book_carries_an_order_and_events_from_this_run() {
    let Some(repo) = observer_repo().await else { return };
    let since = run_window();
    let limit = deadline();

    // Polling for the same reason as the diagnostics: the last scenario's book
    // becomes visible only after a reconciler cycle, and a snapshot would catch it
    // in a legitimate `discovering`.
    let started = Instant::now();
    loop {
        let anchored = repo
            .inference_anchored_books_since(since)
            .await
            .expect("observer: anchor query failed");
        if !anchored.is_empty() {
            eprintln!(
                "observer: anchor — {} visible books with orders: {anchored:?}",
                anchored.len()
            );
            break;
        }
        if started.elapsed() >= limit {
            // Printing matters more here than in the diagnostics. The assert text
            // below covers TWO different diagnoses: traffic never reached the indexer
            // at all (a `dapp_id` or `dst`-filter mistake — exactly the hole the
            // matrix separates the anchor from the diagnostics for), or it did arrive
            // but nothing became visible (the reconciler stalled). "books in window"
            // tells them apart: zero versus non-zero. The neighbouring test may never
            // print its own line under `--test-threads 1` — it could have failed
            // earlier.
            print_diagnostics(&repo, since, started.elapsed()).await;
            panic!(
                "observer: within {}s not a single visible book was found with a projected \
                 order and events from this run. Look at \"books in window\" in the line \
                 above: zero means traffic never reached the indexer; non-zero means it \
                 arrived but nothing became visible. The diagnostic step considers such a \
                 run perfect — an empty database passes all of its claims — which is \
                 exactly why the anchor exists separately",
                limit.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
#[ignore = "e2e: reads the stand database at the tail of a run"]
async fn the_dex_side_of_this_run_was_captured_and_projected() {
    let Some(repo) = observer_repo().await else { return };
    let since = run_window();
    let limit = capture_deadline();

    let started = Instant::now();
    loop {
        let progress = repo
            .dex_capture_progress_since(since)
            .await
            .expect("observer: dex capture query failed");
        let captured: i64 = progress.iter().map(|(_, c, _)| *c).sum();
        let projected: i64 = progress.iter().map(|(_, _, p)| *p).sum();
        if projected > 0 {
            eprintln!(
                "observer: DEX anchor — {captured} OrderBook.* rows ingested in the window, \
                 {projected} of them projected: {progress:?}"
            );
            break;
        }
        if started.elapsed() >= limit {
            // The undecodable count is fetched only on the red path, and only
            // here: it is what separates two of the three diagnoses below, and
            // on the green path it would be noise. `-1` is the query's own
            // failure sentinel, as in `print_diagnostics`.
            let undecodable = repo.count_undecodable_since(since).await.unwrap_or(-1);
            eprintln!(
                "observer: DEX anchor — {}s; OrderBook.* rows in window {captured}, projected \
                 {projected}; undecodable rows in window {undecodable} (any contract, -1 means \
                 the query itself failed); per type: {progress:?}",
                started.elapsed().as_secs()
            );
            panic!(
                "observer: within {}s no DEX order-book event from this run reached the read \
                 model. Read the line above as three cases. Rows 0 and undecodable 0 — nothing \
                 our order book emitted reached the indexer AT ALL: the capture query or the \
                 `dst` allow-list, not projection. Rows 0 and undecodable non-zero — events \
                 arrived and none decoded: ABI drift. Rows non-zero and projected 0 — captured \
                 and decoded, but the projection loop never drained them. One case this is NOT: \
                 a run in which every DEX order scenario failed on chain emits no OrderPlaced to \
                 begin with, so check the scenario results before the indexer. This anchor \
                 exists because the inference one cannot answer for this half — a single ingest \
                 scope feeds both, and an edit that drops the DEX ids leaves the inference \
                 anchor green",
                limit.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
