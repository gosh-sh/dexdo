// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//
// In-process cache of the two blocks behind `GET /api/v1/inference/stats`.
//
// Four independent slots — `volume24h` and the `14d` / `30d` / `all` histories —
// each with its own TTL; a history block is also stale once the UTC date has
// moved past its `asOf`. A request that finds its slot stale either joins the
// calculation already in flight for that slot or starts one. A flight is a
// detached task, so it lands and fills the slot even if every request waiting
// on it is dropped, and every request attached to it receives its outcome,
// success or failure. Nothing is stored on failure. A calculation is cut at
// `FLIGHT_TIMEOUT`: detached from its requests, it no longer falls under the
// request-timeout hoop. The repository bounds each statement in the database
// first (25 s); this cut is the backstop.
//
// The one invariant that keeps a slot from wedging: a flight is registered
// together with the `Flight` guard that owns it, and only that guard's `Drop`
// clears it — whether the calculation landed, panicked, or its task was
// dropped without ever running.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dodex_domain::DomainError;
use dodex_domain::InferenceStatsHistory;
use dodex_domain::InferenceStatsPeriod;
use dodex_domain::InferenceStatsVolume24h;
use tokio::sync::watch;
use tracing::error;
use tracing::info;

use crate::SharedInferenceRepo;

/// Wall clock, Unix milliseconds. Injected so tests drive TTLs and the UTC date.
#[doc(hidden)]
pub type ClockMs = Arc<dyn Fn() -> i64 + Send + Sync>;

pub(crate) fn system_clock_ms() -> ClockMs {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    })
}

pub(crate) const VOLUME_24H_TTL_MS: i64 = 600_000;
/// `14d` and `30d`.
pub(crate) const HISTORY_TTL_MS: i64 = 3_600_000;
pub(crate) const HISTORY_ALL_TTL_MS: i64 = 14_400_000;

/// Upper bound on one flight's repository call. Without it a calculation that never
/// returns would hold its slot for the life of the process, every later request
/// joining it and timing out. A cut calculation fails like any other:
/// `DomainError::RequestTimeout`, nothing stored, the next request starts afresh.
/// A backstop: the repository's 25 s `statement_timeout` cancels a stalled statement in
/// the database first; dropping the future here sends no cancel.
pub(crate) const FLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

const DAY_MS: i64 = 86_400_000;

/// UTC date of a Unix-ms instant, as days since the epoch.
fn utc_day(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// What a flight hands every request attached to it.
type Outcome<T> = Result<Arc<T>, DomainError>;

/// The channel a flight publishes on: `None` until it lands. A sender dropped
/// while the value is still `None` means the flight ended without an outcome.
type OutcomeSender<T> = watch::Sender<Option<Outcome<T>>>;
type OutcomeReceiver<T> = watch::Receiver<Option<Outcome<T>>>;

/// A block a slot can hold.
trait Block: Send + Sync + 'static {
    /// UTC date the block was computed for; `None` when it is not tied to a date.
    fn utc_day(&self) -> Option<i64>;
}

impl Block for InferenceStatsHistory {
    fn utc_day(&self) -> Option<i64> {
        Some(utc_day(self.as_of_ms))
    }
}

impl Block for InferenceStatsVolume24h {
    fn utc_day(&self) -> Option<i64> {
        None
    }
}

struct Stored<T> {
    value: Arc<T>,
    /// Clock reading taken when the calculation succeeded; reads never touch it.
    stored_at_ms: i64,
}

struct SlotState<T> {
    stored: Option<Stored<T>>,
    /// The calculation in the air, if any; joiners subscribe to it.
    flight: Option<OutcomeSender<T>>,
}

struct Slot<T> {
    /// For logs: `volume24h`, `14d`, `30d` or `all`.
    name: &'static str,
    ttl_ms: i64,
    /// Held only to inspect or update the state, never across an `.await`.
    state: Mutex<SlotState<T>>,
}

/// Where a request stands after looking at its slot.
enum Lookup<T> {
    Fresh(Arc<T>),
    /// Attached to the flight in the air, which this request may just have started.
    Wait(OutcomeReceiver<T>),
}

impl<T: Block> Slot<T> {
    fn new(name: &'static str, ttl_ms: i64) -> Arc<Self> {
        Arc::new(Self { name, ttl_ms, state: Mutex::new(SlotState { stored: None, flight: None }) })
    }

    fn lock(&self) -> MutexGuard<'_, SlotState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_fresh(&self, stored: &Stored<T>, now_ms: i64) -> bool {
        now_ms - stored.stored_at_ms < self.ttl_ms
            && stored.value.utc_day().is_none_or(|day| day == utc_day(now_ms))
    }

    /// The slot's value when fresh; otherwise the outcome of the flight this request
    /// joins or starts. `calculate` runs only when a flight is started, with the
    /// flight's `as_of_ms`, and is cut after `bound`.
    async fn get<F, Fut>(
        self: &Arc<Self>,
        clock: &ClockMs,
        bound: Duration,
        calculate: F,
    ) -> Outcome<T>
    where
        F: FnOnce(i64) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, anyhow::Error>> + Send + 'static,
    {
        let mut receiver = match self.lookup(clock, bound, calculate) {
            Lookup::Fresh(value) => return Ok(value),
            Lookup::Wait(receiver) => receiver,
        };
        // The receiver was subscribed before the flight could land (both happen under
        // the slot lock) and a watch channel keeps its last value, so an outcome
        // published before this await starts is still seen.
        let published =
            receiver.wait_for(Option::is_some).await.ok().and_then(|outcome| outcome.clone());
        // Channel closed with no outcome: the flight's task ended without landing.
        published.unwrap_or(Err(DomainError::Unexpected))
    }

    fn lookup<F, Fut>(self: &Arc<Self>, clock: &ClockMs, bound: Duration, calculate: F) -> Lookup<T>
    where
        F: FnOnce(i64) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, anyhow::Error>> + Send + 'static,
    {
        let now_ms = clock();
        let receiver = {
            let mut state = self.lock();
            if let Some(stored) = &state.stored
                && self.is_fresh(stored, now_ms)
            {
                return Lookup::Fresh(Arc::clone(&stored.value));
            }
            if let Some(flight) = &state.flight {
                return Lookup::Wait(flight.subscribe());
            }
            let (sender, receiver) = watch::channel(None);
            state.flight = Some(sender);
            receiver
        };
        // No `.await` between registering the flight and handing it to its guard, so a
        // dropped request cannot leave a registered flight without one.
        let flight = Flight { slot: Arc::clone(self), landed: None };
        tokio::spawn(fly(flight, Arc::clone(clock), bound, calculate));
        Lookup::Wait(receiver)
    }
}

/// Owns a slot's flight. Its `Drop` is the only place a flight is cleared: it stores a
/// successful value, clears the flight and publishes the outcome to every joiner. Dropped
/// without an outcome — the calculation panicked, or the task was dropped before it ran —
/// it still clears the flight; joiners then see the channel closed with no outcome, and
/// the next request starts a fresh flight.
struct Flight<T: Block> {
    slot: Arc<Slot<T>>,
    landed: Option<Result<Stored<T>, DomainError>>,
}

impl<T: Block> Drop for Flight<T> {
    fn drop(&mut self) {
        let mut state = self.slot.lock();
        let sender = state.flight.take();
        let outcome = match self.landed.take() {
            Some(Ok(stored)) => {
                let value = Arc::clone(&stored.value);
                state.stored = Some(stored);
                Ok(value)
            }
            Some(Err(err)) => Err(err),
            None => {
                error!(
                    slot = self.slot.name,
                    "inference stats calculation ended without an outcome"
                );
                return;
            }
        };
        if let Some(sender) = sender {
            sender.send_replace(Some(outcome));
        }
    }
}

/// The detached calculation behind one flight, cut after `bound`. The flight lands when
/// `flight` drops.
async fn fly<T, F, Fut>(mut flight: Flight<T>, clock: ClockMs, bound: Duration, calculate: F)
where
    T: Block,
    F: FnOnce(i64) -> Fut,
    Fut: Future<Output = Result<T, anyhow::Error>>,
{
    let as_of_ms = clock();
    let started = Instant::now();
    flight.landed = Some(match tokio::time::timeout(bound, calculate(as_of_ms)).await {
        Ok(Ok(value)) => {
            // The production signal that a cold calculation drifts toward the frontend's
            // 4.5 s budget: the flight lands even when its requests have timed out.
            info!(
                slot = flight.slot.name,
                as_of_ms,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "inference stats block recalculated"
            );
            Ok(Stored { value: Arc::new(value), stored_at_ms: clock() })
        }
        Ok(Err(err)) => {
            error!(?err, slot = flight.slot.name, as_of_ms, "inference stats calculation failed");
            Err(err.downcast_ref::<DomainError>().copied().unwrap_or(DomainError::Unexpected))
        }
        Err(_elapsed) => {
            error!(
                slot = flight.slot.name,
                as_of_ms,
                bound_ms = bound.as_millis() as u64,
                "inference stats calculation did not finish within its bound and was cut"
            );
            Err(DomainError::RequestTimeout)
        }
    });
}

pub(crate) struct InferenceStatsCache {
    repo: SharedInferenceRepo,
    clock: ClockMs,
    /// `FLIGHT_TIMEOUT`; only tests shorten it.
    flight_timeout: Duration,
    volume_24h: Arc<Slot<InferenceStatsVolume24h>>,
    history_14d: Arc<Slot<InferenceStatsHistory>>,
    history_30d: Arc<Slot<InferenceStatsHistory>>,
    history_all: Arc<Slot<InferenceStatsHistory>>,
}

impl InferenceStatsCache {
    pub(crate) fn new(repo: SharedInferenceRepo, clock: ClockMs) -> Self {
        Self {
            repo,
            clock,
            flight_timeout: FLIGHT_TIMEOUT,
            volume_24h: Slot::new("volume24h", VOLUME_24H_TTL_MS),
            history_14d: Slot::new(InferenceStatsPeriod::Days14.as_str(), HISTORY_TTL_MS),
            history_30d: Slot::new(InferenceStatsPeriod::Days30.as_str(), HISTORY_TTL_MS),
            history_all: Slot::new(InferenceStatsPeriod::All.as_str(), HISTORY_ALL_TTL_MS),
        }
    }

    /// Shortens the bound on each calculation, so a test can reach it.
    #[cfg(test)]
    pub(crate) fn with_flight_timeout(mut self, bound: Duration) -> Self {
        self.flight_timeout = bound;
        self
    }

    fn history_slot(&self, period: InferenceStatsPeriod) -> &Arc<Slot<InferenceStatsHistory>> {
        match period {
            InferenceStatsPeriod::Days14 => &self.history_14d,
            InferenceStatsPeriod::Days30 => &self.history_30d,
            InferenceStatsPeriod::All => &self.history_all,
        }
    }

    /// The history block for `period`, dated on the current UTC day.
    ///
    /// A flight started before midnight and received after it carries yesterday's
    /// block, so a block dated on another day is never returned: the request goes
    /// round once more and ends on a calculation made for the current day. Midnight
    /// cannot pass twice inside one request, so a second such block means the
    /// repository did not honour the cutoff it was given; retrying again would hammer
    /// the database, returning it would serve the wrong days — the request fails.
    pub(crate) async fn history(
        &self,
        period: InferenceStatsPeriod,
    ) -> Result<Arc<InferenceStatsHistory>, DomainError> {
        let block = self.history_round(period).await?;
        if utc_day(block.as_of_ms) == utc_day((self.clock)()) {
            return Ok(block);
        }
        let block = self.history_round(period).await?;
        let now_ms = (self.clock)();
        if utc_day(block.as_of_ms) == utc_day(now_ms) {
            return Ok(block);
        }
        error!(
            period = period.as_str(),
            block_as_of_ms = block.as_of_ms,
            now_ms,
            "inference stats history block is dated on another UTC day than now, twice in a row"
        );
        Err(DomainError::Unexpected)
    }

    async fn history_round(
        &self,
        period: InferenceStatsPeriod,
    ) -> Result<Arc<InferenceStatsHistory>, DomainError> {
        let repo = Arc::clone(&self.repo);
        self.history_slot(period)
            .get(&self.clock, self.flight_timeout, move |as_of_ms| async move {
                repo.inference_stats_history(period, as_of_ms).await
            })
            .await
    }

    pub(crate) async fn volume_24h(&self) -> Result<Arc<InferenceStatsVolume24h>, DomainError> {
        let repo = Arc::clone(&self.repo);
        self.volume_24h
            .get(&self.clock, self.flight_timeout, move |as_of_ms| async move {
                repo.inference_stats_volume_24h(as_of_ms).await
            })
            .await
    }

    /// How many requests are attached to the flight of `period`'s slot; 0 when none is
    /// in the air.
    #[cfg(test)]
    pub(crate) fn history_joiners(&self, period: InferenceStatsPeriod) -> usize {
        self.history_slot(period).lock().flight.as_ref().map_or(0, |sender| sender.receiver_count())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use dodex_application::InferenceMarketsRequest;
    use dodex_application::InferenceOrdersPage;
    use dodex_application::InferenceOrdersQuery;
    use dodex_application::InferenceReadRepository;
    use dodex_domain::DomainError;
    use dodex_domain::InferenceDepthSnapshot;
    use dodex_domain::InferenceMarketsPage;
    use dodex_domain::InferenceStatsHistory;
    use dodex_domain::InferenceStatsPeriod;
    use dodex_domain::InferenceStatsTotals;
    use dodex_domain::InferenceStatsVolume24h;
    use dodex_domain::Trade;
    use tokio::sync::Semaphore;
    use tokio::task::JoinHandle;

    use super::ClockMs;
    use super::InferenceStatsCache;

    const DAY: i64 = 86_400_000;
    /// 2026-10-23T12:00:00Z.
    const T0: i64 = 1_792_756_800_000;
    /// 2026-10-23T00:00:01Z.
    const OCT_23_00_00_01: i64 = 1_792_713_601_000;
    /// 2026-10-23T23:59:00Z.
    const OCT_23_23_59_00: i64 = 1_792_799_940_000;
    /// 2026-10-23T23:59:59Z.
    const OCT_23_23_59_59: i64 = 1_792_799_999_000;
    /// 2026-10-24T00:00:01Z.
    const OCT_24_00_00_01: i64 = 1_792_800_001_000;
    /// 2026-10-01T12:00:00Z.
    const OCT_01_12_00_00: i64 = T0 - 22 * DAY;

    /// Repository double: counts calls, records the cutoff each one received, and can
    /// fail, panic, hold every calculation at a gate, or answer with a fixed `as_of_ms`.
    #[derive(Default)]
    struct StubRepo {
        history_calls: AtomicUsize,
        volume_calls: AtomicUsize,
        received_as_of: Mutex<Vec<i64>>,
        fixed_as_of: Option<i64>,
        fail: AtomicBool,
        panic: AtomicBool,
        gate: Option<Semaphore>,
    }

    impl StubRepo {
        fn gated() -> Self {
            Self { gate: Some(Semaphore::new(0)), ..Self::default() }
        }

        /// Lets the held calculation and every later one through.
        fn open_gate(&self) {
            self.gate.as_ref().expect("stub has a gate").add_permits(1_000);
        }

        fn received_as_of(&self) -> Vec<i64> {
            self.received_as_of.lock().unwrap().clone()
        }

        /// Common body of both stats methods; returns the `as_of_ms` to echo.
        async fn calculate(&self, calls: &AtomicUsize, as_of_ms: i64) -> anyhow::Result<i64> {
            calls.fetch_add(1, Ordering::SeqCst);
            self.received_as_of.lock().unwrap().push(as_of_ms);
            if let Some(gate) = &self.gate {
                let _permit = gate.acquire().await.expect("the gate is never closed");
            }
            if self.panic.load(Ordering::SeqCst) {
                panic!("stub calculation panicked");
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(anyhow!(DomainError::MarketInconsistent));
            }
            Ok(self.fixed_as_of.unwrap_or(as_of_ms))
        }
    }

    fn zero_totals() -> InferenceStatsTotals {
        InferenceStatsTotals { ticks: "0".into(), quote: "0".into(), fills: 0 }
    }

    #[async_trait]
    impl InferenceReadRepository for StubRepo {
        async fn list_inference_markets(
            &self,
            _request: &InferenceMarketsRequest,
        ) -> Result<InferenceMarketsPage, anyhow::Error> {
            unimplemented!()
        }

        async fn get_inference_depth(
            &self,
            _orderbook_address: &str,
            _limit: u16,
            _now: i64,
        ) -> Result<InferenceDepthSnapshot, anyhow::Error> {
            unimplemented!()
        }

        async fn list_inference_orders(
            &self,
            _query: &InferenceOrdersQuery,
        ) -> Result<InferenceOrdersPage, anyhow::Error> {
            unimplemented!()
        }

        async fn list_inference_trades(
            &self,
            _orderbook_address: &str,
            _limit: u16,
        ) -> Result<Vec<Trade>, anyhow::Error> {
            unimplemented!()
        }

        async fn inference_stats_history(
            &self,
            _period: InferenceStatsPeriod,
            as_of_ms: i64,
        ) -> Result<InferenceStatsHistory, anyhow::Error> {
            let as_of_ms = self.calculate(&self.history_calls, as_of_ms).await?;
            Ok(InferenceStatsHistory {
                as_of_ms,
                per_day: vec![],
                totals: zero_totals(),
                previous_period: None,
            })
        }

        async fn inference_stats_volume_24h(
            &self,
            as_of_ms: i64,
        ) -> Result<InferenceStatsVolume24h, anyhow::Error> {
            let as_of_ms = self.calculate(&self.volume_calls, as_of_ms).await?;
            Ok(InferenceStatsVolume24h { as_of_ms, totals: zero_totals() })
        }
    }

    struct Fixture {
        cache: Arc<InferenceStatsCache>,
        repo: Arc<StubRepo>,
        now_ms: Arc<AtomicI64>,
    }

    impl Fixture {
        fn new(repo: StubRepo, start_ms: i64) -> Self {
            Self::build(repo, start_ms, None)
        }

        fn with_flight_timeout(repo: StubRepo, start_ms: i64, bound: Duration) -> Self {
            Self::build(repo, start_ms, Some(bound))
        }

        fn build(repo: StubRepo, start_ms: i64, flight_timeout: Option<Duration>) -> Self {
            let repo = Arc::new(repo);
            let now_ms = Arc::new(AtomicI64::new(start_ms));
            let read = Arc::clone(&now_ms);
            let clock: ClockMs = Arc::new(move || read.load(Ordering::SeqCst));
            let mut cache = InferenceStatsCache::new(Arc::clone(&repo) as _, clock);
            if let Some(bound) = flight_timeout {
                cache = cache.with_flight_timeout(bound);
            }
            Self { cache: Arc::new(cache), repo, now_ms }
        }

        fn set_clock(&self, ms: i64) {
            self.now_ms.store(ms, Ordering::SeqCst);
        }

        fn advance_clock(&self, ms: i64) {
            self.now_ms.fetch_add(ms, Ordering::SeqCst);
        }

        fn history_calls(&self) -> usize {
            self.repo.history_calls.load(Ordering::SeqCst)
        }

        fn volume_calls(&self) -> usize {
            self.repo.volume_calls.load(Ordering::SeqCst)
        }

        fn joiners(&self, period: InferenceStatsPeriod) -> usize {
            self.cache.history_joiners(period)
        }

        fn spawn_history(
            &self,
            period: InferenceStatsPeriod,
        ) -> JoinHandle<Result<Arc<InferenceStatsHistory>, DomainError>> {
            let cache = Arc::clone(&self.cache);
            tokio::spawn(async move { cache.history(period).await })
        }

        fn spawn_volume(&self) -> JoinHandle<Result<Arc<InferenceStatsVolume24h>, DomainError>> {
            let cache = Arc::clone(&self.cache);
            tokio::spawn(async move { cache.volume_24h().await })
        }
    }

    /// Yields to the runtime until `condition` holds; fails the test instead of hanging.
    async fn wait_until(mut condition: impl FnMut() -> bool) {
        const MAX_YIELDS: usize = 100_000;
        for _ in 0..MAX_YIELDS {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition still false after {MAX_YIELDS} yields");
    }

    #[tokio::test]
    async fn a_hit_does_not_recompute() {
        let f = Fixture::new(StubRepo::default(), T0);
        let first = f.cache.volume_24h().await.unwrap();
        let second = f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 1);
        assert_eq!(second.as_of_ms, first.as_of_ms);
    }

    #[tokio::test]
    async fn ttl_expiry_recomputes_and_reads_do_not_extend_it() {
        let f = Fixture::new(StubRepo::default(), T0);
        f.cache.volume_24h().await.unwrap();

        // A hit just inside the TTL must not push the expiry out.
        f.set_clock(T0 + 599_999);
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 1);

        f.set_clock(T0 + 600_000);
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 2);
    }

    #[tokio::test]
    async fn history_ttls_are_one_hour_and_four_hours() {
        // Start one second after midnight so four hours stay inside one UTC date.
        let f = Fixture::new(StubRepo::default(), OCT_23_00_00_01);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        f.set_clock(OCT_23_00_00_01 + 3_599_999);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        assert_eq!(f.history_calls(), 1);
        f.set_clock(OCT_23_00_00_01 + 3_600_000);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        assert_eq!(f.history_calls(), 2);

        let f = Fixture::new(StubRepo::default(), OCT_23_00_00_01);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        f.set_clock(OCT_23_00_00_01 + 14_399_999);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        assert_eq!(f.history_calls(), 1);
        f.set_clock(OCT_23_00_00_01 + 14_400_000);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        assert_eq!(f.history_calls(), 2);
    }

    #[tokio::test]
    async fn ttl_starts_when_the_calculation_succeeds() {
        let f = Fixture::new(StubRepo::gated(), T0);
        let request = f.spawn_volume();
        wait_until(|| f.volume_calls() == 1).await;
        f.advance_clock(30_000);
        f.repo.open_gate();
        request.await.unwrap().unwrap();

        f.set_clock(T0 + 30_000 + 599_999);
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 1);

        f.set_clock(T0 + 30_000 + 600_000);
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 2);
    }

    #[tokio::test]
    async fn each_history_period_has_its_own_slot() {
        let f = Fixture::new(StubRepo::default(), T0);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        f.cache.history(InferenceStatsPeriod::Days30).await.unwrap();
        assert_eq!(f.history_calls(), 2);
        f.cache.history(InferenceStatsPeriod::Days30).await.unwrap();
        assert_eq!(f.history_calls(), 2);
        // Nor does `30d` share the `all` slot.
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        assert_eq!(f.history_calls(), 3);
    }

    #[tokio::test]
    async fn utc_date_rollover_invalidates_history_but_not_volume() {
        let f = Fixture::new(StubRepo::default(), OCT_23_23_59_00);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.history_calls(), 2);
        assert_eq!(f.volume_calls(), 1);

        // Now 2026-10-24T00:01:00Z.
        f.advance_clock(120_000);
        f.cache.history(InferenceStatsPeriod::Days14).await.unwrap();
        // The second `14d` call.
        assert_eq!(f.history_calls(), 3);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        // The second `all` call.
        assert_eq!(f.history_calls(), 4);
        f.cache.volume_24h().await.unwrap();
        assert_eq!(f.volume_calls(), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_successful_calculation() {
        let f = Fixture::new(StubRepo::gated(), T0);
        let requests: Vec<_> =
            (0..5).map(|_| f.spawn_history(InferenceStatsPeriod::Days14)).collect();
        // A request that arrived after the flight landed would be served from the cache
        // and prove nothing about sharing: let the calculation finish only once all five
        // are attached to it.
        wait_until(|| f.joiners(InferenceStatsPeriod::Days14) == 5).await;
        f.repo.open_gate();

        let mut as_of = Vec::new();
        for request in requests {
            as_of.push(request.await.unwrap().unwrap().as_of_ms);
        }
        assert_eq!(as_of, vec![T0; 5]);
        assert_eq!(f.history_calls(), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_failure_and_the_next_request_retries() {
        let f = Fixture::new(StubRepo::gated(), T0);
        f.repo.fail.store(true, Ordering::SeqCst);
        let requests: Vec<_> =
            (0..5).map(|_| f.spawn_history(InferenceStatsPeriod::Days14)).collect();
        // Without this wait the flight could fail before the others start; they would
        // then rightly start flights of their own.
        wait_until(|| f.joiners(InferenceStatsPeriod::Days14) == 5).await;
        f.repo.open_gate();

        for request in requests {
            assert_eq!(request.await.unwrap(), Err(DomainError::MarketInconsistent));
        }
        assert_eq!(f.history_calls(), 1);

        f.repo.fail.store(false, Ordering::SeqCst);
        assert!(f.cache.history(InferenceStatsPeriod::Days14).await.is_ok());
        assert_eq!(f.history_calls(), 2);
    }

    #[tokio::test]
    async fn a_request_after_midnight_does_not_get_yesterdays_block() {
        let f = Fixture::new(StubRepo::gated(), OCT_23_23_59_59);
        let a = f.spawn_history(InferenceStatsPeriod::Days14);
        // `history_calls == 1` means the detached task has already read the old clock for
        // its cutoff; a joiner alone does not show that, and moving the clock before that
        // read would simply make the first calculation for the new day.
        wait_until(|| f.joiners(InferenceStatsPeriod::Days14) == 1 && f.history_calls() == 1).await;
        assert_eq!(f.repo.received_as_of()[0], OCT_23_23_59_59);

        f.set_clock(OCT_24_00_00_01);
        let b = f.spawn_history(InferenceStatsPeriod::Days14);
        wait_until(|| f.joiners(InferenceStatsPeriod::Days14) == 2).await;
        f.repo.open_gate();

        assert_eq!(a.await.unwrap().unwrap().as_of_ms, OCT_24_00_00_01);
        assert_eq!(b.await.unwrap().unwrap().as_of_ms, OCT_24_00_00_01);
        assert_eq!(f.history_calls(), 2);
    }

    #[tokio::test]
    async fn a_block_dated_on_another_day_is_retried_once_then_fails() {
        let repo = StubRepo { fixed_as_of: Some(OCT_01_12_00_00), ..StubRepo::default() };
        let f = Fixture::new(repo, T0);
        assert_eq!(
            f.cache.history(InferenceStatsPeriod::Days14).await,
            Err(DomainError::Unexpected)
        );
        // One extra round, then stop.
        assert_eq!(f.history_calls(), 2);
        // The stale block is never handed out, also not from the slot.
        assert_eq!(
            f.cache.history(InferenceStatsPeriod::Days14).await,
            Err(DomainError::Unexpected)
        );
    }

    #[tokio::test]
    async fn a_failure_in_one_slot_leaves_the_others_cached() {
        let f = Fixture::new(StubRepo::default(), T0);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        f.cache.volume_24h().await.unwrap();

        f.advance_clock(600_000);
        f.repo.fail.store(true, Ordering::SeqCst);
        assert_eq!(f.cache.volume_24h().await, Err(DomainError::MarketInconsistent));
        assert!(f.cache.history(InferenceStatsPeriod::All).await.is_ok());
        assert_eq!(f.history_calls(), 1);
    }

    #[tokio::test]
    async fn a_calculation_finishes_after_its_requester_is_dropped() {
        let f = Fixture::new(StubRepo::gated(), T0);
        let h = f.spawn_history(InferenceStatsPeriod::Days14);
        wait_until(|| f.joiners(InferenceStatsPeriod::Days14) == 1).await;
        h.abort();
        // The requester is gone while its calculation is still held at the gate.
        assert!(h.await.unwrap_err().is_cancelled());
        assert_eq!(f.joiners(InferenceStatsPeriod::Days14), 0);
        f.repo.open_gate();

        assert!(f.cache.history(InferenceStatsPeriod::Days14).await.is_ok());
        assert_eq!(f.history_calls(), 1);
    }

    #[tokio::test]
    async fn all_reuses_history_while_volume_refreshes() {
        let f = Fixture::new(StubRepo::default(), T0);
        f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        f.cache.volume_24h().await.unwrap();

        f.set_clock(T0 + 660_000);
        let history = f.cache.history(InferenceStatsPeriod::All).await.unwrap();
        assert_eq!(history.as_of_ms, T0);
        assert_eq!(f.history_calls(), 1);
        let volume = f.cache.volume_24h().await.unwrap();
        assert_eq!(volume.as_of_ms, T0 + 660_000);
        assert_eq!(f.volume_calls(), 2);
    }

    #[tokio::test]
    async fn a_panicked_calculation_does_not_wedge_the_slot() {
        let f = Fixture::new(StubRepo::default(), T0);
        f.repo.panic.store(true, Ordering::SeqCst);
        assert_eq!(
            f.cache.history(InferenceStatsPeriod::Days14).await,
            Err(DomainError::Unexpected)
        );

        f.repo.panic.store(false, Ordering::SeqCst);
        assert!(f.cache.history(InferenceStatsPeriod::Days14).await.is_ok());
        assert_eq!(f.history_calls(), 2);
    }

    #[tokio::test]
    async fn a_calculation_that_never_returns_is_cut_and_the_slot_recovers() {
        // The only test that waits on real time: ~20 ms for the bound to fire. The held
        // calculation can end no other way, and the second one finds the gate already
        // open, so it completes on its first poll — before its own bound is consulted.
        let f = Fixture::with_flight_timeout(StubRepo::gated(), T0, Duration::from_millis(20));
        assert_eq!(
            f.cache.history(InferenceStatsPeriod::Days14).await,
            Err(DomainError::RequestTimeout)
        );
        assert_eq!(f.history_calls(), 1);
        assert_eq!(f.joiners(InferenceStatsPeriod::Days14), 0);

        f.repo.open_gate();
        assert!(f.cache.history(InferenceStatsPeriod::Days14).await.is_ok());
        assert_eq!(f.history_calls(), 2);
    }
}
