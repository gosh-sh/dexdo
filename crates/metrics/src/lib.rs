// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! OTLP metrics for the dodex indexer. A small, standalone crate (like
//! `dodex-logging`) so a separate workspace / Docker layer can reuse it by
//! path, and so the OpenTelemetry dependency stays out of crates that don't
//! emit metrics.
//!
//! Exposes nineteen observable counters and gauges pushed over OTLP — ingestion,
//! the projection pipeline, the DB pool and the inference markets; the catalogue
//! with what each one measures is in `docs/tech-specs/indexer.md`. When no OTLP endpoint env var is
//! set, `init()` returns `None` and nothing is collected.

use std::env;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::metrics::ObservableCounter;
use opentelemetry::metrics::ObservableGauge;
use opentelemetry::Key;
use opentelemetry::KeyValue;
use opentelemetry::Value;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::metrics::PeriodicReader;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::Resource;

/// How often the indexer refreshes the DB-derived counter caches. Kept below
/// the OTLP reader interval so every export sees fresh values.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

const OTLP_READER_INTERVAL: Duration = Duration::from_secs(30);
const OTLP_READER_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_NAME: &str = "dodex-indexer";
/// What the SDK's resource detector reports when the environment configured no
/// name at all (neither `OTEL_SERVICE_NAME` nor a `service.name` in
/// `OTEL_RESOURCE_ATTRIBUTES`). Matched as a prefix because the specification
/// also allows an `unknown_service:<process>` form.
const SDK_UNKNOWN_SERVICE: &str = "unknown_service";
const SERVICE_NAME_KEY: &str = "service.name";

/// Owns the meter provider (held for the process lifetime) and the indexer
/// metric handles.
pub struct Metrics {
    _provider: SdkMeterProvider,
    pub indexer: IndexerMetrics,
}

/// Cloneable handle to the indexer's observable counters and gauges. Setter
/// calls update the values reported on the next OTLP collection.
#[derive(Clone)]
pub struct IndexerMetrics {
    orders_created: Arc<AtomicU64>,
    orders_partially_filled: Arc<AtomicU64>,
    projection_backlog: Arc<AtomicU64>,
    projection_lag_seconds: Arc<AtomicU64>,
    capture_cursor_age_seconds: Arc<AtomicU64>,
    pool_in_use: Arc<AtomicU64>,
    pool_idle: Arc<AtomicU64>,
    projection_fallbacks: Arc<AtomicU64>,
    inference_orphans_dropped: Arc<AtomicU64>,
    decode_errors: Arc<AtomicU64>,
    unknown_events: Arc<AtomicU64>,
    decode_ambiguous_collisions: Arc<AtomicU64>,
    inference_markets_discovering: Arc<AtomicU64>,
    inference_markets_visible: Arc<AtomicU64>,
    inference_markets_failing: Arc<AtomicU64>,
    inference_reference_price_lag_seconds: Arc<AtomicU64>,
    inference_sweep_lag_seconds: Arc<AtomicU64>,
    inference_orders_open: Arc<AtomicU64>,
    inference_orders_filled: Arc<AtomicU64>,
    inference_orders_cancelled: Arc<AtomicU64>,
    inference_orders_expired: Arc<AtomicU64>,
    inference_reconcile_failures: Arc<AtomicU64>,
    inference_wedged_books: Arc<AtomicU64>,
    metrics_refresh_failures: Arc<AtomicU64>,
    metrics_refresh_passes: Arc<AtomicU64>,
    // Retain the observable-counter and gauge handles for the lifetime of the
    // provider, mirroring the reference metrics setup. The observe callbacks
    // themselves are registered with the meter at `build()` time.
    // Underscore-prefixed because the handles are never read directly.
    _orders_created_counter: ObservableCounter<u64>,
    _orders_partially_filled_counter: ObservableCounter<u64>,
    _projection_backlog_gauge: ObservableGauge<u64>,
    _projection_lag_seconds_gauge: ObservableGauge<u64>,
    _capture_cursor_age_seconds_gauge: ObservableGauge<u64>,
    _db_pool_connections_gauge: ObservableGauge<u64>,
    _projection_fallbacks_counter: ObservableCounter<u64>,
    _inference_orphans_dropped_counter: ObservableCounter<u64>,
    _decode_errors_counter: ObservableCounter<u64>,
    _unknown_events_counter: ObservableCounter<u64>,
    _decode_ambiguous_collisions_counter: ObservableCounter<u64>,
    _inference_markets_gauge: ObservableGauge<u64>,
    _inference_reference_price_lag_gauge: ObservableGauge<u64>,
    _inference_sweep_lag_gauge: ObservableGauge<u64>,
    _inference_orders_gauge: ObservableGauge<u64>,
    _inference_reconcile_failures_counter: ObservableCounter<u64>,
    _inference_wedged_books_gauge: ObservableGauge<u64>,
    _metrics_refresh_failures_counter: ObservableCounter<u64>,
    _metrics_refresh_passes_counter: ObservableCounter<u64>,
}

impl IndexerMetrics {
    fn new(meter: &Meter) -> Self {
        let orders_created = Arc::new(AtomicU64::new(0));
        let orders_partially_filled = Arc::new(AtomicU64::new(0));
        let projection_backlog = Arc::new(AtomicU64::new(0));
        let projection_lag_seconds = Arc::new(AtomicU64::new(0));
        let capture_cursor_age_seconds = Arc::new(AtomicU64::new(0));
        let pool_in_use = Arc::new(AtomicU64::new(0));
        let pool_idle = Arc::new(AtomicU64::new(0));
        let projection_fallbacks = Arc::new(AtomicU64::new(0));
        let inference_orphans_dropped = Arc::new(AtomicU64::new(0));
        let decode_errors = Arc::new(AtomicU64::new(0));
        let unknown_events = Arc::new(AtomicU64::new(0));
        let decode_ambiguous_collisions = Arc::new(AtomicU64::new(0));
        let inference_markets_discovering = Arc::new(AtomicU64::new(0));
        let inference_markets_visible = Arc::new(AtomicU64::new(0));
        let inference_markets_failing = Arc::new(AtomicU64::new(0));
        let inference_reference_price_lag_seconds = Arc::new(AtomicU64::new(0));
        let inference_sweep_lag_seconds = Arc::new(AtomicU64::new(0));
        let inference_orders_open = Arc::new(AtomicU64::new(0));
        let inference_orders_filled = Arc::new(AtomicU64::new(0));
        let inference_orders_cancelled = Arc::new(AtomicU64::new(0));
        let inference_orders_expired = Arc::new(AtomicU64::new(0));
        let inference_reconcile_failures = Arc::new(AtomicU64::new(0));
        let inference_wedged_books = Arc::new(AtomicU64::new(0));
        let metrics_refresh_failures = Arc::new(AtomicU64::new(0));
        let metrics_refresh_passes = Arc::new(AtomicU64::new(0));

        let created_cache = Arc::clone(&orders_created);
        let orders_created_counter = meter
            .u64_observable_counter("orders_created_event_cnt")
            .with_description("Total OrderBook.OrderPlaced events across all markets and users")
            .with_callback(move |observer| {
                observer.observe(created_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let partial_cache = Arc::clone(&orders_partially_filled);
        let orders_partially_filled_counter = meter
            .u64_observable_counter("order_partially_filled_event_cnt")
            .with_description("Total OrderBook.PartialFill events across all markets and users")
            .with_callback(move |observer| {
                observer.observe(partial_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let backlog_cache = Arc::clone(&projection_backlog);
        let projection_backlog_gauge = meter
            .u64_observable_gauge("indexer_projection_backlog")
            .with_description(
                "raw_events rows waiting for the projection loop (typed + decoded, processed_at NULL)",
            )
            .with_callback(move |observer| {
                observer.observe(backlog_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let lag_cache = Arc::clone(&projection_lag_seconds);
        let projection_lag_seconds_gauge = meter
            .u64_observable_gauge("indexer_projection_lag_seconds")
            .with_description(
                "Wall-clock age in seconds of the oldest eligible-but-unprojected raw_events row",
            )
            .with_callback(move |observer| {
                observer.observe(lag_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let cursor_age_cache = Arc::clone(&capture_cursor_age_seconds);
        let capture_cursor_age_seconds_gauge = meter
            .u64_observable_gauge("indexer_capture_cursor_age_seconds")
            .with_description(
                "Seconds since the capture cursor last advanced; 0 when capture has not started",
            )
            .with_callback(move |observer| {
                observer.observe(cursor_age_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let in_use_cache = Arc::clone(&pool_in_use);
        let idle_cache = Arc::clone(&pool_idle);
        let db_pool_connections_gauge = meter
            .u64_observable_gauge("indexer_db_pool_connections")
            .with_description("sqlx DB pool connections by state (in_use or idle)")
            .with_callback(move |observer| {
                observer.observe(
                    in_use_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("state", "in_use")],
                );
                observer
                    .observe(idle_cache.load(Ordering::Relaxed), &[KeyValue::new("state", "idle")]);
            })
            .build();

        let fallbacks_cache = Arc::clone(&projection_fallbacks);
        let projection_fallbacks_counter = meter
            .u64_observable_counter("indexer_projection_fallbacks")
            .with_description(
                "Projection batches that aborted the optimistic pass and replayed with per-row savepoints",
            )
            .with_callback(move |observer| {
                observer.observe(fallbacks_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let orphans_cache = Arc::clone(&inference_orphans_dropped);
        let inference_orphans_dropped_counter = meter
            .u64_observable_counter("indexer_inference_orphans_dropped")
            .with_description(
                "Orphan events dead-lettered after their parent never arrived within the cutoff: the four inference events (InferenceFilled, InferenceOrderCancelled, InferenceOrderExpired, InferenceRefunded) plus OracleEventList.RangeEventAdded, which is counted here rather than left uncounted because a dropped row is a dropped row whichever contract emitted it. Each loses something different: a Filled first decrements any present resting leg (so book depth stays correct) and what goes unrecorded is the missing counterparty; a cancel or an expiry is lost outright, so the order never reaches its terminal status; a refund cannot be attributed to any order; and a RangeEventAdded loses the range-to-book linkage",
            )
            .with_callback(move |observer| {
                observer.observe(orphans_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let decode_errors_cache = Arc::clone(&decode_errors);
        let decode_errors_counter = meter
            .u64_observable_counter("indexer_decode_errors")
            .with_description(
                "Event bodies that failed to decode (decode_output/detokenize error) and were stored undecoded — distinct from an unknown/ambiguous event id, which is not an error",
            )
            .with_callback(move |observer| {
                observer.observe(decode_errors_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let unknown_events_cache = Arc::clone(&unknown_events);
        let unknown_events_counter = meter
            .u64_observable_counter("indexer_unknown_events")
            .with_description(
                "Decoded rows that matched no projector arm. Unknown marks the row processed and never retries it, so a rising count means events are being dropped for good — a new contract event without an arm, or an arm removed while its type still arrives. Distinct from indexer_decode_errors (the body failed to decode) and indexer_decode_ambiguous_collisions (a shared id with no dst route): those rows keep their payload and stay replayable, these do not",
            )
            .with_callback(move |observer| {
                observer.observe(unknown_events_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let ambiguous_collisions_cache = Arc::clone(&decode_ambiguous_collisions);
        let decode_ambiguous_collisions_counter = meter
            .u64_observable_counter("indexer_decode_ambiguous_collisions")
            .with_description(
                "Event bodies left undecoded because their id collides across ABIs and no dst route disambiguated it — a colliding-ABI-without-route regression; distinct from a benign unknown id and from a hard decode error",
            )
            .with_callback(move |observer| {
                observer.observe(ambiguous_collisions_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let disc_cache = Arc::clone(&inference_markets_discovering);
        let vis_cache = Arc::clone(&inference_markets_visible);
        let fail_cache = Arc::clone(&inference_markets_failing);
        let inference_markets_gauge = meter
            .u64_observable_gauge("indexer_inference_markets")
            .with_description(
                "Inference order-book markets by lifecycle state: discovering (seeded, not yet visible), visible (served by the API), failing (invisible with a recorded reconcile failure)",
            )
            .with_callback(move |observer| {
                observer.observe(
                    disc_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("state", "discovering")],
                );
                observer.observe(
                    vis_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("state", "visible")],
                );
                observer.observe(
                    fail_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("state", "failing")],
                );
            })
            .build();

        let price_lag_cache = Arc::clone(&inference_reference_price_lag_seconds);
        let inference_reference_price_lag_gauge = meter
            .u64_observable_gauge("indexer_inference_reference_price_lag_seconds")
            .with_description(
                "Age in seconds of the most stale reference_price_at across visible inference markets; 0 when no market is visible",
            )
            .with_callback(move |observer| {
                observer.observe(price_lag_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let sweep_lag_cache = Arc::clone(&inference_sweep_lag_seconds);
        let inference_sweep_lag_gauge = meter
            .u64_observable_gauge("indexer_inference_sweep_lag_seconds")
            .with_description(
                "Age in seconds of the most stale last_swept_at across visible inference markets; 0 when no market is visible",
            )
            .with_callback(move |observer| {
                observer.observe(sweep_lag_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let open_cache = Arc::clone(&inference_orders_open);
        let filled_cache = Arc::clone(&inference_orders_filled);
        let cancelled_cache = Arc::clone(&inference_orders_cancelled);
        let expired_cache = Arc::clone(&inference_orders_expired);
        let inference_orders_gauge = meter
            .u64_observable_gauge("indexer_inference_orders")
            .with_description(
                "Resting inference orders by status: open (live depth), filled, cancelled, expired",
            )
            .with_callback(move |observer| {
                observer.observe(
                    open_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("status", "open")],
                );
                observer.observe(
                    filled_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("status", "filled")],
                );
                observer.observe(
                    cancelled_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("status", "cancelled")],
                );
                observer.observe(
                    expired_cache.load(Ordering::Relaxed),
                    &[KeyValue::new("status", "expired")],
                );
            })
            .build();

        let reconcile_failures_cache = Arc::clone(&inference_reconcile_failures);
        let inference_reconcile_failures_counter = meter
            .u64_observable_counter("indexer_inference_reconcile_failures")
            .with_description(
                "Hard inference reconcile failures (Err outcomes from discovery or refresh; excludes the benign NoBoc skip)",
            )
            .with_callback(move |observer| {
                observer.observe(reconcile_failures_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let wedged_books_cache = Arc::clone(&inference_wedged_books);
        let inference_wedged_books_gauge = meter
            .u64_observable_gauge("indexer_inference_wedged_books")
            .with_description(
                "Visible inference order-book markets currently wedged by an unprojected raw_events row under their address — the read gate's arm-2 condition (MarketInconsistent/503) that an ABI-lagging contract upgrade can trigger indefinitely",
            )
            .with_callback(move |observer| {
                observer.observe(wedged_books_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let metrics_refresh_failures_cache = Arc::clone(&metrics_refresh_failures);
        let metrics_refresh_failures_counter = meter
            .u64_observable_counter("indexer_metrics_refresh_failures")
            .with_description(
                "Metrics-refresh-loop DB query failures; a rising rate means the refresh loop cannot read the DB, so the gauges it feeds are stale — pair any gauge alert (e.g. indexer_inference_wedged_books) with this counter or the underlying error log",
            )
            .with_callback(move |observer| {
                observer.observe(metrics_refresh_failures_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        let metrics_refresh_passes_cache = Arc::clone(&metrics_refresh_passes);
        let metrics_refresh_passes_counter = meter
            .u64_observable_counter("indexer_metrics_refresh_passes")
            .with_description(
                "Completed passes of the metrics refresh loop. A counter that stops growing is the only signal a dead refresh task gives: a panicked task freezes every gauge it feeds AND stops indexer_metrics_refresh_failures, so the failure counter cannot report its own death. Alert on the absence of growth, not on a value",
            )
            .with_callback(move |observer| {
                observer.observe(metrics_refresh_passes_cache.load(Ordering::Relaxed), &[]);
            })
            .build();

        Self {
            orders_created,
            orders_partially_filled,
            projection_backlog,
            projection_lag_seconds,
            capture_cursor_age_seconds,
            pool_in_use,
            pool_idle,
            projection_fallbacks,
            inference_orphans_dropped,
            decode_errors,
            unknown_events,
            decode_ambiguous_collisions,
            inference_markets_discovering,
            inference_markets_visible,
            inference_markets_failing,
            inference_reference_price_lag_seconds,
            inference_sweep_lag_seconds,
            inference_orders_open,
            inference_orders_filled,
            inference_orders_cancelled,
            inference_orders_expired,
            inference_reconcile_failures,
            inference_wedged_books,
            metrics_refresh_failures,
            metrics_refresh_passes,
            _orders_created_counter: orders_created_counter,
            _orders_partially_filled_counter: orders_partially_filled_counter,
            _projection_backlog_gauge: projection_backlog_gauge,
            _projection_lag_seconds_gauge: projection_lag_seconds_gauge,
            _capture_cursor_age_seconds_gauge: capture_cursor_age_seconds_gauge,
            _db_pool_connections_gauge: db_pool_connections_gauge,
            _projection_fallbacks_counter: projection_fallbacks_counter,
            _inference_orphans_dropped_counter: inference_orphans_dropped_counter,
            _decode_errors_counter: decode_errors_counter,
            _unknown_events_counter: unknown_events_counter,
            _decode_ambiguous_collisions_counter: decode_ambiguous_collisions_counter,
            _inference_markets_gauge: inference_markets_gauge,
            _inference_reference_price_lag_gauge: inference_reference_price_lag_gauge,
            _inference_sweep_lag_gauge: inference_sweep_lag_gauge,
            _inference_orders_gauge: inference_orders_gauge,
            _inference_reconcile_failures_counter: inference_reconcile_failures_counter,
            _inference_wedged_books_gauge: inference_wedged_books_gauge,
            _metrics_refresh_failures_counter: metrics_refresh_failures_counter,
            _metrics_refresh_passes_counter: metrics_refresh_passes_counter,
        }
    }

    /// Set the value reported by `orders_created_event_cnt` on the next push.
    pub fn set_orders_created(&self, value: u64) {
        self.orders_created.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `order_partially_filled_event_cnt`.
    pub fn set_orders_partially_filled(&self, value: u64) {
        self.orders_partially_filled.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_projection_backlog`.
    pub fn set_projection_backlog(&self, value: u64) {
        self.projection_backlog.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_projection_lag_seconds`.
    pub fn set_projection_lag_seconds(&self, value: u64) {
        self.projection_lag_seconds.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_capture_cursor_age_seconds`.
    pub fn set_capture_cursor_age_seconds(&self, value: u64) {
        self.capture_cursor_age_seconds.store(value, Ordering::Relaxed);
    }

    /// Set the values reported by `indexer_db_pool_connections`.
    pub fn set_pool_connections(&self, in_use: u64, idle: u64) {
        self.pool_in_use.store(in_use, Ordering::Relaxed);
        self.pool_idle.store(idle, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_projection_fallbacks` — the
    /// running total of projection batches that fell back to per-row savepoints.
    pub fn set_projection_fallbacks(&self, value: u64) {
        self.projection_fallbacks.store(value, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_inference_orphans_dropped` — the
    /// running total of inference depth updates permanently dropped because their
    /// parent OrderPlaced was never captured (orphan dead-letter).
    pub fn set_inference_orphans_dropped(&self, value: u64) {
        self.inference_orphans_dropped.store(value, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_decode_errors` — the running
    /// total of event bodies that failed to decode and were stored undecoded.
    pub fn set_decode_errors(&self, value: u64) {
        self.decode_errors.store(value, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_unknown_events` — the running
    /// total of decoded rows dropped by the projector's `Unknown` arm.
    pub fn set_unknown_events(&self, value: u64) {
        self.unknown_events.store(value, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_decode_ambiguous_collisions`
    /// — bodies left undecoded due to a colliding event id with no dst route.
    pub fn set_decode_ambiguous_collisions(&self, value: u64) {
        self.decode_ambiguous_collisions.store(value, Ordering::Relaxed);
    }

    /// Set the values reported by `indexer_inference_markets` — the count of
    /// inference order-book markets in each lifecycle state.
    pub fn set_inference_market_states(&self, discovering: u64, visible: u64, failing: u64) {
        self.inference_markets_discovering.store(discovering, Ordering::Relaxed);
        self.inference_markets_visible.store(visible, Ordering::Relaxed);
        self.inference_markets_failing.store(failing, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_inference_reference_price_lag_seconds`.
    pub fn set_inference_reference_price_lag_seconds(&self, value: u64) {
        self.inference_reference_price_lag_seconds.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_inference_sweep_lag_seconds`.
    pub fn set_inference_sweep_lag_seconds(&self, value: u64) {
        self.inference_sweep_lag_seconds.store(value, Ordering::Relaxed);
    }

    /// Set the values reported by `indexer_inference_orders` — the count of
    /// resting inference orders in each status.
    pub fn set_inference_order_counts(&self, open: u64, filled: u64, cancelled: u64, expired: u64) {
        self.inference_orders_open.store(open, Ordering::Relaxed);
        self.inference_orders_filled.store(filled, Ordering::Relaxed);
        self.inference_orders_cancelled.store(cancelled, Ordering::Relaxed);
        self.inference_orders_expired.store(expired, Ordering::Relaxed);
    }

    /// Set the cumulative value reported by `indexer_inference_reconcile_failures`.
    pub fn set_inference_reconcile_failures(&self, value: u64) {
        self.inference_reconcile_failures.store(value, Ordering::Relaxed);
    }

    /// Set the value reported by `indexer_inference_wedged_books` — the count of
    /// visible inference books currently wedged by an unprojected `raw_events` row.
    pub fn set_inference_wedged_books(&self, value: u64) {
        self.inference_wedged_books.store(value, Ordering::Relaxed);
    }

    /// Increment the cumulative value reported by `indexer_metrics_refresh_passes`.
    /// Called at the END of a refresh pass, so what it counts is a pass that
    /// reached the end. A panic anywhere in the pass leaves it flat, which is
    /// exactly the signal: the failure counter cannot report its own task's death.
    pub fn inc_metrics_refresh_passes(&self) {
        self.metrics_refresh_passes.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the cumulative value reported by `indexer_metrics_refresh_failures`.
    /// Called by the metrics-refresh loop itself on every failed DB query so a
    /// DB outage that freezes the other gauges (skipped `set_` on `Err`) still
    /// shows up as a rising counter rather than reading as healthy.
    pub fn inc_metrics_refresh_failures(&self) {
        self.metrics_refresh_failures.fetch_add(1, Ordering::Relaxed);
    }

    // Test-only snapshot getters: plain atomic reads of the caches the indexer's
    // refresh tests assert on (its sentinels are unreadable from another crate
    // otherwise). NOT every cache — seven have no getter because nothing asserts
    // on them yet: the two pool gauges, `projection_fallbacks`, `decode_errors`,
    // `decode_ambiguous_collisions`, `inference_orphans_dropped` and
    // `inference_reconcile_failures`. Of the thirteen that exist, ten mirror the
    // eight fallible DB-backed sections (two sections fill two caches each); the
    // other three are not DB-backed at all — `metrics_refresh_failures_count` is
    // the loop's own error counter, `metrics_refresh_passes_count` its heartbeat,
    // and `unknown_events_value` an in-process count the loop only copies across.
    // Same precedent as `IndexerRepository::decode_errors_count`: `#[doc(hidden)]`,
    // no features or cfg bridges. Tuple getters mirror the tuple setters.

    #[doc(hidden)]
    pub fn orders_created_value(&self) -> u64 {
        self.orders_created.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn orders_partially_filled_value(&self) -> u64 {
        self.orders_partially_filled.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn projection_backlog_value(&self) -> u64 {
        self.projection_backlog.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn projection_lag_seconds_value(&self) -> u64 {
        self.projection_lag_seconds.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn capture_cursor_age_seconds_value(&self) -> u64 {
        self.capture_cursor_age_seconds.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn inference_market_states_value(&self) -> (u64, u64, u64) {
        (
            self.inference_markets_discovering.load(Ordering::Relaxed),
            self.inference_markets_visible.load(Ordering::Relaxed),
            self.inference_markets_failing.load(Ordering::Relaxed),
        )
    }

    #[doc(hidden)]
    pub fn inference_reference_price_lag_seconds_value(&self) -> u64 {
        self.inference_reference_price_lag_seconds.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn inference_sweep_lag_seconds_value(&self) -> u64 {
        self.inference_sweep_lag_seconds.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn inference_order_counts_value(&self) -> (u64, u64, u64, u64) {
        (
            self.inference_orders_open.load(Ordering::Relaxed),
            self.inference_orders_filled.load(Ordering::Relaxed),
            self.inference_orders_cancelled.load(Ordering::Relaxed),
            self.inference_orders_expired.load(Ordering::Relaxed),
        )
    }

    #[doc(hidden)]
    pub fn inference_wedged_books_value(&self) -> u64 {
        self.inference_wedged_books.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn unknown_events_value(&self) -> u64 {
        self.unknown_events.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn metrics_refresh_passes_count(&self) -> u64 {
        self.metrics_refresh_passes.load(Ordering::Relaxed)
    }

    #[doc(hidden)]
    pub fn metrics_refresh_failures_count(&self) -> u64 {
        self.metrics_refresh_failures.load(Ordering::Relaxed)
    }

    /// Test-only factory: an `IndexerMetrics` on a reader-less in-memory
    /// provider ("exports nowhere", the same shape as the in-crate setter
    /// test). Exists because a dependent crate's test (the indexer's
    /// refresh-failure test) has no opentelemetry dependency to build a
    /// `Meter` with, and `init()` without an OTLP endpoint returns `None`.
    /// The provider is returned as owner — dropping it kills the instruments,
    /// so hold it for the metrics' lifetime.
    #[doc(hidden)]
    pub fn new_in_memory_for_tests() -> (SdkMeterProvider, IndexerMetrics) {
        let provider = SdkMeterProvider::builder().build();
        let meter = provider.meter("dodex-metrics-test");
        let metrics = IndexerMetrics::new(&meter);
        (provider, metrics)
    }
}

/// Picks the OTLP endpoint with the standard precedence: the metrics-specific
/// var wins over the generic one. Empty strings count as unset, so an empty
/// metrics var falls through to the generic one rather than swallowing it.
fn select_endpoint(metrics: Option<String>, fallback: Option<String>) -> Option<String> {
    metrics.filter(|s| !s.is_empty()).or_else(|| fallback.filter(|s| !s.is_empty()))
}

/// Reads the OTLP endpoint from env: `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`
/// then `OTEL_EXPORTER_OTLP_ENDPOINT`.
fn metrics_endpoint() -> Option<String> {
    select_endpoint(
        env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").ok(),
        env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
    )
}

/// Picks the exported `service.name` from what the SDK detected: a name the
/// environment configured wins, and ours only fills in the gap the detector
/// signals with `unknown_service`. Taking the detected value rather than the
/// raw env keeps the precedence rules (`OTEL_SERVICE_NAME` over a `service.name`
/// in `OTEL_RESOURCE_ATTRIBUTES`) in one place — the SDK's.
fn select_service_name(detected: Option<Value>) -> String {
    match detected {
        Some(name) if !name.as_str().starts_with(SDK_UNKNOWN_SERVICE) => name.as_str().into_owned(),
        _ => SERVICE_NAME.to_string(),
    }
}

/// Builds the OTLP resource. The chosen `service.name` is merged on the right so
/// it wins over the detected one — `Resource::merge` gives precedence to its
/// argument — while still inheriting the other SDK-detected attributes carried
/// by `Resource::default()`.
fn build_resource() -> Resource {
    let detected = Resource::default();
    let name = select_service_name(detected.get(Key::from_static_str(SERVICE_NAME_KEY)));
    detected.merge(&Resource::new(vec![KeyValue::new(SERVICE_NAME_KEY, name)]))
}

/// Initialise OTLP metrics. Returns `None` when no OTLP endpoint env var is
/// set — the caller then runs without metrics. The returned `Metrics` owns the
/// meter provider and MUST be kept alive for the process lifetime. Must be
/// called from within a Tokio runtime (the OTLP exporter uses it).
#[must_use]
pub fn init() -> Option<Metrics> {
    // Resolve the endpoint ourselves and hand it to the exporter, rather than
    // letting the exporter re-read env: the SDK gives the signal-specific var
    // precedence even when it is empty, which would defeat the empty-string
    // fallback in `select_endpoint`. Passing the resolved value makes our
    // resolution the single source of truth.
    let endpoint = metrics_endpoint()?;

    // Metrics are best-effort: a malformed endpoint must not take down the
    // indexer's core ingestion, so degrade to no metrics rather than panic.
    let exporter = match opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
    {
        Ok(exporter) => exporter,
        Err(err) => {
            tracing::warn!(?err, "failed to build OTLP metric exporter; metrics disabled");
            return None;
        }
    };

    let reader = PeriodicReader::builder(exporter, Tokio)
        .with_interval(OTLP_READER_INTERVAL)
        .with_timeout(OTLP_READER_TIMEOUT)
        .build();

    let provider =
        SdkMeterProvider::builder().with_reader(reader).with_resource(build_resource()).build();

    let meter = provider.meter(SERVICE_NAME);
    let indexer = IndexerMetrics::new(&meter);

    Some(Metrics { _provider: provider, indexer })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::Gauge;
    use opentelemetry_sdk::metrics::PeriodicReader;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::runtime;
    use opentelemetry_sdk::testing::metrics::InMemoryMetricExporter;

    use super::select_endpoint;
    use super::IndexerMetrics;

    #[test]
    fn metrics_endpoint_takes_precedence_over_generic() {
        assert_eq!(
            select_endpoint(Some("metrics".to_string()), Some("generic".to_string())),
            Some("metrics".to_string())
        );
    }

    #[test]
    fn falls_back_to_generic_endpoint() {
        assert_eq!(select_endpoint(None, Some("generic".to_string())), Some("generic".to_string()));
    }

    #[test]
    fn none_when_unset_or_empty() {
        assert_eq!(select_endpoint(None, None), None);
        assert_eq!(select_endpoint(Some(String::new()), None), None);
    }

    #[test]
    fn empty_metrics_var_falls_through_to_generic() {
        assert_eq!(
            select_endpoint(Some(String::new()), Some("generic".to_string())),
            Some("generic".to_string())
        );
    }

    #[test]
    fn env_configured_service_name_wins_over_ours() {
        assert_eq!(
            super::select_service_name(Some(opentelemetry::Value::from("dodex-indexer-stage"))),
            "dodex-indexer-stage"
        );
    }

    #[test]
    fn our_service_name_fills_in_when_env_configured_none() {
        assert_eq!(super::select_service_name(None), super::SERVICE_NAME);
        assert_eq!(
            super::select_service_name(Some(opentelemetry::Value::from("unknown_service"))),
            super::SERVICE_NAME
        );
        assert_eq!(
            super::select_service_name(Some(opentelemetry::Value::from("unknown_service:indexer"))),
            super::SERVICE_NAME
        );
    }

    #[test]
    fn resource_carries_the_chosen_service_name() {
        let resource = super::build_resource();
        let name = resource
            .get(opentelemetry::Key::from_static_str("service.name"))
            .expect("the SDK detector always sets service.name");
        assert!(
            !name.as_str().starts_with(super::SDK_UNKNOWN_SERVICE),
            "exported the SDK placeholder instead of a real name: {name:?}"
        );
    }

    #[test]
    fn setters_update_cached_values() {
        // A provider with no reader needs no runtime and exports nowhere — it
        // just hands out a meter so we can register the counters and exercise
        // the setters.
        let provider = SdkMeterProvider::builder().build();
        let meter = provider.meter("test");
        let metrics = IndexerMetrics::new(&meter);

        metrics.set_orders_created(7);
        metrics.set_orders_partially_filled(3);
        metrics.set_projection_backlog(42);
        metrics.set_projection_lag_seconds(120);
        metrics.set_capture_cursor_age_seconds(5);
        metrics.set_pool_connections(3, 7);
        metrics.set_projection_fallbacks(9);
        metrics.set_inference_orphans_dropped(4);
        metrics.set_decode_errors(6);
        metrics.set_decode_ambiguous_collisions(2);
        metrics.set_inference_market_states(11, 22, 33);
        metrics.set_inference_reference_price_lag_seconds(444);
        metrics.set_inference_sweep_lag_seconds(555);
        metrics.set_inference_order_counts(60, 70, 80, 90);
        metrics.set_inference_reconcile_failures(99);
        metrics.set_inference_wedged_books(13);
        for _ in 0..5 {
            metrics.inc_metrics_refresh_failures();
        }

        assert_eq!(metrics.orders_created.load(Ordering::Relaxed), 7);
        assert_eq!(metrics.orders_partially_filled.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.projection_backlog.load(Ordering::Relaxed), 42);
        assert_eq!(metrics.projection_lag_seconds.load(Ordering::Relaxed), 120);
        assert_eq!(metrics.capture_cursor_age_seconds.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.pool_in_use.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.pool_idle.load(Ordering::Relaxed), 7);
        assert_eq!(metrics.projection_fallbacks.load(Ordering::Relaxed), 9);
        assert_eq!(metrics.inference_orphans_dropped.load(Ordering::Relaxed), 4);
        assert_eq!(metrics.decode_errors.load(Ordering::Relaxed), 6);
        assert_eq!(metrics.decode_ambiguous_collisions.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.inference_markets_discovering.load(Ordering::Relaxed), 11);
        assert_eq!(metrics.inference_markets_visible.load(Ordering::Relaxed), 22);
        assert_eq!(metrics.inference_markets_failing.load(Ordering::Relaxed), 33);
        assert_eq!(metrics.inference_reference_price_lag_seconds.load(Ordering::Relaxed), 444);
        assert_eq!(metrics.inference_sweep_lag_seconds.load(Ordering::Relaxed), 555);
        assert_eq!(metrics.inference_orders_open.load(Ordering::Relaxed), 60);
        assert_eq!(metrics.inference_orders_filled.load(Ordering::Relaxed), 70);
        assert_eq!(metrics.inference_orders_cancelled.load(Ordering::Relaxed), 80);
        assert_eq!(metrics.inference_orders_expired.load(Ordering::Relaxed), 90);
        assert_eq!(metrics.inference_reconcile_failures.load(Ordering::Relaxed), 99);
        assert_eq!(metrics.inference_wedged_books.load(Ordering::Relaxed), 13);
        assert_eq!(metrics.metrics_refresh_failures.load(Ordering::Relaxed), 5);
    }

    // `setters_update_cached_values` above reads the private atomic caches
    // directly, so it stays green even if the gauge callback forgot to call
    // `observer.observe(..., "expired")` — the cache would still hold 40, but
    // the exported series would silently drop the fourth data point. This
    // test builds a real reader so the callback actually runs, and asserts on
    // what the OTLP export path produces.
    // `PeriodicReader::builder(_, runtime::Tokio)` spawns its background export
    // task via `tokio::spawn` and `force_flush` blocks the calling thread on a
    // reply from that task. On a current-thread runtime (the `#[tokio::test]`
    // default) that spawned task can never run while the calling thread is
    // parked in `force_flush`, so the flush — and the test — hangs forever;
    // `flavor = "multi_thread"` gives the spawned task its own thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn exported_series_carries_all_four_status_buckets() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone(), runtime::Tokio).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter("test");
        let metrics = IndexerMetrics::new(&meter);

        metrics.set_inference_order_counts(10, 20, 30, 40);
        provider.force_flush().expect("force flush");

        let resource_metrics = exporter.get_finished_metrics().expect("finished metrics");
        let metric = resource_metrics
            .iter()
            .flat_map(|rm| rm.scope_metrics.iter())
            .flat_map(|sm| sm.metrics.iter())
            .find(|m| m.name == "indexer_inference_orders")
            .expect("indexer_inference_orders was exported");
        let gauge = metric
            .data
            .as_any()
            .downcast_ref::<Gauge<u64>>()
            .expect("indexer_inference_orders is a gauge");
        assert_eq!(gauge.data_points.len(), 4, "one data point per status bucket");

        let value_for = |status: &str| {
            gauge
                .data_points
                .iter()
                .find(|dp| {
                    dp.attributes
                        .iter()
                        .any(|kv| kv.key.as_str() == "status" && kv.value.as_str() == status)
                })
                .unwrap_or_else(|| panic!("no data point for status={status}"))
                .value
        };
        assert_eq!(value_for("open"), 10);
        assert_eq!(value_for("filled"), 20);
        assert_eq!(value_for("cancelled"), 30);
        assert_eq!(value_for("expired"), 40);
    }
}
