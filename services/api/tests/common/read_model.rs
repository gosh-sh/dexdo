// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
//! Polling the read model through the production router brought up in-process.
//!
//! Two rules this module is built around.
//!
//! **Poll for presence, assert content.** `poll_read_with` returns a value as
//! soon as a row appears, and all field-level checks are the caller's job. A
//! loop that spun until VALUES matched would turn a wrong remaining order
//! quantity into an expired budget — a read-model defect would be masked as
//! a slow indexer.
//!
//! **Never panic.** Every outcome comes back as a `Result`: panicking
//! mid-scenario would leave a deposit stranded on a note, because order
//! settlement happens further down the test. The caller puts the `Err` into
//! its own `failures` and proceeds to settlement.

use std::time::Duration;
use std::time::Instant;

use salvo::http::StatusCode;
use salvo::test::ResponseExt;
use salvo::test::TestClient;
use salvo::Service;
use serde_json::Value;

/// The result of a single probe.
pub enum Probe<T> {
    /// The fact is in place. Checking its fields is the caller's job.
    Ready(T),
    /// The fact is not there yet. The string is what is visible RIGHT NOW: it
    /// lands in the expired-budget message, so it must name the observation,
    /// not the intent ("book is discovering", not "waiting for the book").
    Pending(String),
    /// Retrying is pointless: a test defect or a service refusal.
    Fatal(String),
}

/// Wait budget for the WHOLE binary, not per fact. Capture ticks once every
/// 3s, the inference reconciler once every 15s, book visibility is stamped
/// after a full sweep cycle; the matrix rates such a fact at "order of a
/// minute".
///
/// Why shared rather than per-fact: `e2e_inference` carries four polls on top
/// of 270s of chained on-chain waits, and the `ci-e2e` profile kills a test at
/// `60s × terminate-after 10` = 600s. A per-fact budget of 90s would give
/// 630s — and in the "read model is stuck" scenario, where every budget burns
/// out, nextest would kill the test BEFORE the final `assert!`, losing the
/// collected `failures`.
///
/// Chain waits and this budget share one clock, so the 240s bounds ELAPSED TEST
/// TIME rather than time spent polling: a slow chain eats into it before the
/// first read phase ever runs.
///
/// WHERE the clock starts differs by binary, two to one. `e2e_inference` and
/// `e2e_inference_clob` start it ahead of every chain wait, as decision E
/// describes — in clob the wait sits inside the `deploy_book` helper, which is
/// called after `ReadBudget::start()`, so reading the helper's own line number
/// as the order is a mistake this comment made once already.
/// `e2e_inference_orders` is the exception: it starts the clock after its
/// `wait_inference_book_live`, so its ceiling is
/// `max(chain-so-far, 240s) + trailing chain waits` rather than the plain sum —
/// with up to ~290s of chain before the first read phase there, the budget is
/// the smaller term. That lands near 380s, under nextest's 600s
/// `terminate-after`, with room. Aligning it with the other two is an open
/// decision that needs a stand run to settle — do not "fix" one side to match
/// the other from reading alone.
///
/// The worst case is therefore not the sum `chain + 240s`;
/// it is `max(chain-so-far, 240s)` at the point the last read phase starts,
/// plus whatever chain waits still follow it. For `e2e_inference` that is
/// ≈330s: 180s of chain (the book-live wait, then the order-surface loop)
/// before IX-SEQ-02 enters with `left() = 60s` and drains it, then a further
/// ~90s cancel-drain wait before IX-SEQ-01 and IX-GATE-17 enter with
/// `left() = 0` and return on their first probe. If it burns out on the
/// first phase, the remaining phases return `Err` immediately, and the
/// binary still reaches order settlement and its own `assert!`.
/// **Recalibrated 2026-08-24 from 240s, against measured runs rather than an
/// estimate.** The original figure came from "the matrix rates such a fact at
/// order of a minute" — a rating, not an observation. Pipelines #299 (green) and
/// #300 (read model dead) give the real numbers, and they are far apart:
///
/// | binary                    | green | with a dead read model |
/// |---------------------------|-------|------------------------|
/// | `e2e_inference`           |  19.1s|                 243.9s |
/// | `e2e_inference_clob`      |  18.9s|                 239.6s |
/// | `e2e_inference_orders`    |  28.8s|                 243.2s |
///
/// Two things follow. The dead-run durations land ON the budget — 243.9 ≈ 240 —
/// so this constant, not the chain, is what a stuck read model costs. And the
/// green totals are WHOLE-TEST times, chain waits included, so the read-model
/// facts themselves arrive in single-digit seconds: 240s was roughly twelve
/// times the entire runtime of the binaries it governs.
///
/// 90s keeps ~60s over the slowest of them. That is ~10× the observed latency of
/// the facts, against a stand whose chain waits repeat within ±2s across runs
/// (every test that passed in both #299 and #300 did so within two seconds of
/// its own previous time). The slack is for a slower stand, not for a broken one.
///
/// WHY THIS IS NOT MERELY TIDINESS. Six binaries burning 240s each is 24 minutes
/// added to a run that has a 120-minute ceiling; #300 finished at 126.8 and was
/// killed, which skipped `net_down`, which strands the host lease for its full
/// 90-minute TTL and kills every pipeline behind it. The budget is a term in
/// that sum.
///
/// Binaries whose own chain waits exceed this hold their own constant —
/// `SWEEP_READ_BUDGET`, `EXPIRY_READ_BUDGET`, `RANGE_READ_BUDGET`,
/// `STREAM_READ_BUDGET` — each derived the same way and carrying its green
/// measurement. Adding a binary with long chain waits means adding one, NOT
/// raising this: the three above are fast and would silently inherit the cost.
pub const DEFAULT_READ_BUDGET: Duration = Duration::from_secs(90);

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Lower bound on the time any single probe is given, and the upper bound on
/// how far one probe can overrun its phase's remaining allowance. A healthy
/// in-process probe answers in milliseconds; one that is still running after
/// this long has stalled (a DB lock, an exhausted pool) and is reported as an
/// observation instead of hanging the loop. Public so the polling tests can
/// assert the bound itself.
pub const PROBE_TIMEOUT_FLOOR: Duration = Duration::from_secs(5);

/// Whether this lane has an indexer filling the read model, and therefore
/// whether the read phases can prove anything.
///
/// Two different facts used to be conflated into one. `TEST_DATABASE_URL` says a
/// database is reachable; it does NOT say anything is writing to it. The
/// shellnet lane (`.github/workflows/e2e-shellnet.yml`) sets that variable for a
/// service-container Postgres and runs no indexer — its database exists for the
/// helpers that upsert a market row directly. Gating the read phases on the URL
/// alone made them run there and poll for facts only an indexer can produce: a
/// book in `/markets`, an order in `/orders`, a trade in `/trades`. Nothing ever
/// writes those, so every phase burned the shared budget to zero and pushed a
/// failure — the binary went red for the shape of the lane rather than for a
/// defect, and the whole 240s was spent proving it.
///
/// `E2E_READ_MODEL` is the second fact, set only where an indexer is running
/// (`.woodpecker/e2e.yml`). The gate is deliberately NOT inside `common::setup()`:
/// seventeen API test files call that, and the fourteen that are not these three
/// binaries seed their own rows — they need a database, not an indexer.
///
/// The two facts fail in opposite directions on purpose. `E2E_READ_MODEL` unset is
/// the ordinary case on every lane but one, and skips quietly. `E2E_READ_MODEL` set
/// with no reachable database is an instruction that cannot be carried out on the
/// only lane that could carry it out, so each caller pushes a failure rather than
/// printing a line onto a green run.
pub fn read_phases_enabled() -> bool {
    read_phases_enabled_from(std::env::var("E2E_READ_MODEL").ok().as_deref())
}

/// The decision behind [`read_phases_enabled`], split out so it can be tested
/// without mutating the process environment — the same shape `select_endpoint`
/// uses in `crates/metrics`. It earns a test despite its size: the binaries this
/// gate governs are all `#[ignore]`, so a gate that silently answered "yes"
/// everywhere would put the read phases back into the lane that cannot serve
/// them, and no CI run would notice.
pub fn read_phases_enabled_from(raw: Option<&str>) -> bool {
    matches!(raw, Some("1") | Some("true") | Some("TRUE"))
}

/// The remaining shared budget. Created once per binary; each phase gets
/// `left()`. Waiting for visibility is paid for once, not once per request.
pub struct ReadBudget {
    started: Instant,
    total: Duration,
}

impl ReadBudget {
    pub fn start() -> Self {
        Self::with_total(DEFAULT_READ_BUDGET)
    }

    pub fn with_total(total: Duration) -> Self {
        Self { started: Instant::now(), total }
    }

    /// How much is left. Zero is a legitimate value: `poll_read_with` with a
    /// zero budget still makes one probe (the probe runs BEFORE the deadline
    /// check) and returns `Err` — the phase reports rather than hangs, and
    /// does not skip a fact that has already arrived by that point.
    pub fn left(&self) -> Duration {
        self.total.saturating_sub(self.started.elapsed())
    }
}

/// The single entry point: the budget is passed explicitly, and that is
/// deliberate. There is no "take the default" wrapper here — it would smuggle
/// a per-fact budget back in through the back door, exactly what decision E
/// moves away from.
pub async fn poll_read_with<T, F, Fut>(
    label: &str,
    budget: Duration,
    mut probe: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Probe<T>>,
{
    let started = Instant::now();
    let mut last: String;
    loop {
        // Bound the probe itself: an in-process request that stalls (a DB
        // lock, an exhausted pool) would otherwise hang this await past every
        // budget and hand the scenario to nextest's 600s kill before cleanup —
        // the exact outcome the shared budget exists to prevent. The bound is
        // the remaining allowance, floored at PROBE_TIMEOUT_FLOOR so the
        // single probe of an exhausted budget keeps a bounded-but-fair chance
        // instead of an unbounded await. A timed-out probe is an observation,
        // not a verdict: it feeds the same deadline logic below, so the
        // branch wordings and the "single probe" contract are unchanged.
        let allowance = budget.saturating_sub(started.elapsed()).max(PROBE_TIMEOUT_FLOOR);
        match tokio::time::timeout(allowance, probe()).await {
            Ok(Probe::Ready(v)) => return Ok(v),
            Ok(Probe::Fatal(why)) => return Err(format!("{label}: terminal refusal: {why}")),
            Ok(Probe::Pending(why)) => last = why,
            Err(_) => {
                last = format!(
                    "the probe itself was still running after {}s — a stalled in-process \
                     request (DB lock or pool exhaustion), not a missing fact",
                    allowance.as_secs()
                )
            }
        }
        // Exit when the NEXT round would not fit, not when the budget has
        // already expired. The difference shows up in the window `0 < budget
        // < POLL_INTERVAL`: with a check of `elapsed >= budget`, the first
        // probe would not exceed the remainder, the loop would go to sleep
        // for the full interval, and the phase would make a second probe,
        // overrunning its remainder. The message below about a "single
        // probe" would then be a lie exactly in that window.
        if started.elapsed() + POLL_INTERVAL >= budget {
            // The budget did not stretch to even one interval — not the same
            // as expired: it means the binary's shared budget was spent
            // EARLIER, including on chained waits (they share a clock,
            // decision E). Phrasing this as "did not arrive within 0s" would
            // send the reader looking for an instant check that does not
            // exist.
            if budget <= POLL_INTERVAL {
                return Err(format!(
                    "{label}: no retries happened — the binary's shared budget was spent \
                     earlier (chain waits run on the same clock). Observation from the single \
                     probe: {last}"
                ));
            }
            return Err(format!(
                "{label}: the fact did not reach the read model within {}s. Last observation: \
                 {last}. \"book not visible\" — check the reconciler; \"empty\" — check capture \
                 and projection",
                budget.as_secs()
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Full URL for a public inference endpoint.
pub fn api(path: &str) -> String {
    format!("http://test/api/v1/inference/{path}")
}

/// Full URL for a public PREDICTION endpoint — a different tree of the same
/// router, not a different service.
///
/// It earns its own function rather than a parameter because exactly one fact
/// crosses between the two: a range market is a prediction market that settles
/// from an inference book, so the scene that proves the link is the only one
/// that reads both. Everything else stays on [`api`], and a caller that
/// reaches for this one is saying which side of the link it is asking about.
pub fn api_prediction(path: &str) -> String {
    format!("http://test/api/v1/prediction/{path}")
}

/// The outcome of a single GET through the production router.
pub enum GetOutcome {
    Ok(Value),
    /// A legitimate answer from a healthy system: retry.
    Retry(String),
    /// Not retryable.
    Fatal(String),
}

/// GET with response classification.
///
/// `404` with the production `ErrorBody` code `-1121` — the book is not
/// `visible` yet; `503` (`-1500`) — the read path refused fail-closed. Both
/// are legitimate and retryable. Everything else is terminal: `400` means a
/// bad URL in the test itself, and a `404` WITHOUT the `-1121` envelope is
/// Salvo's own router miss — a misspelled path, which must fail fast rather
/// than retry until the shared budget expires.
pub async fn get_json(service: &Service, url: &str) -> GetOutcome {
    let mut resp = TestClient::get(url).send(service).await;
    match resp.status_code {
        Some(StatusCode::OK) => match resp.take_json::<Value>().await {
            Ok(v) => GetOutcome::Ok(v),
            Err(e) => GetOutcome::Fatal(format!("{url}: the 200 body does not parse as JSON: {e}")),
        },
        Some(StatusCode::SERVICE_UNAVAILABLE) => {
            GetOutcome::Retry(format!("503 (-1500) on {url} — legitimate refusal, retrying"))
        }
        Some(StatusCode::NOT_FOUND) => match resp.take_string().await {
            Ok(body) if error_body_code(&body) == Some(-1121) => {
                GetOutcome::Retry(format!("404 (-1121) on {url} — book not visible yet, retrying"))
            }
            Ok(body) => GetOutcome::Fatal(format!(
                "{url}: 404 without the -1121 envelope is the router's own miss (a misspelled \
                 path), not \"book not visible\"; body: {body:?}"
            )),
            Err(e) => GetOutcome::Fatal(format!("{url}: 404 with an unreadable body: {e}")),
        },
        other => GetOutcome::Fatal(format!(
            "{url}: status {other:?} is not \"not there yet\" — 400 is a test defect, any 5xx \
             other than 503 is a service refusal"
        )),
    }
}

/// The `code` of a production `ErrorBody`, when the body is one. A router-level
/// 404 carries no such envelope, which is exactly what tells it apart.
fn error_body_code(body: &str) -> Option<i64> {
    serde_json::from_str::<Value>(body).ok()?.get("code")?.as_i64()
}
