// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! OTLP metrics for the dodex indexer. A small, standalone crate (like
//! `dodex-logging`) so a separate workspace / Docker layer can reuse it by
//! path, and so the OpenTelemetry dependency stays out of crates that don't
//! emit metrics.
//!
//! Exposes `orders_created_event_cnt` and `order_partially_filled_event_cnt`
//! as observable counters pushed over OTLP. When no OTLP endpoint env var is
//! set, `init()` returns `None` and nothing is collected.

use std::env;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::metrics::ObservableCounter;
use opentelemetry::KeyValue;
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

/// Owns the meter provider (held for the process lifetime) and the indexer
/// metric handles.
pub struct Metrics {
    _provider: SdkMeterProvider,
    pub indexer: IndexerMetrics,
}

/// Cloneable handle to the indexer's two observable counters. Setter calls
/// update the values reported on the next OTLP collection.
#[derive(Clone)]
pub struct IndexerMetrics {
    orders_created: Arc<AtomicU64>,
    orders_partially_filled: Arc<AtomicU64>,
    // Retain the observable-counter handles for the lifetime of the provider,
    // mirroring the reference metrics setup. The observe callbacks themselves
    // are registered with the meter at `build()` time. Underscore-prefixed
    // because the handles are never read directly.
    _orders_created_counter: ObservableCounter<u64>,
    _orders_partially_filled_counter: ObservableCounter<u64>,
}

impl IndexerMetrics {
    fn new(meter: &Meter) -> Self {
        let orders_created = Arc::new(AtomicU64::new(0));
        let orders_partially_filled = Arc::new(AtomicU64::new(0));

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

        Self {
            orders_created,
            orders_partially_filled,
            _orders_created_counter: orders_created_counter,
            _orders_partially_filled_counter: orders_partially_filled_counter,
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

/// Builds the OTLP resource. The explicit `service.name` is merged on the
/// right so it wins over the SDK default's `unknown_service` — `Resource::merge`
/// gives precedence to its argument — while still inheriting the SDK-detected
/// attributes carried by `Resource::default()`.
fn build_resource() -> Resource {
    Resource::default().merge(&Resource::new(vec![KeyValue::new("service.name", SERVICE_NAME)]))
}

/// Initialise OTLP metrics. Returns `None` when no OTLP endpoint env var is
/// set — the caller then runs without metrics. The returned `Metrics` owns the
/// meter provider and MUST be kept alive for the process lifetime. Must be
/// called from within a Tokio runtime (the OTLP exporter uses it).
#[must_use]
pub fn init() -> Option<Metrics> {
    // Gate on the endpoint; the OTLP exporter independently reads the same env
    // vars to configure its transport.
    metrics_endpoint()?;

    // Metrics are best-effort: a malformed endpoint must not take down the
    // indexer's core ingestion, so degrade to no metrics rather than panic.
    let exporter = match opentelemetry_otlp::MetricExporter::builder().with_tonic().build() {
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
    use opentelemetry_sdk::metrics::SdkMeterProvider;

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
    fn resource_carries_our_service_name() {
        let resource = super::build_resource();
        assert_eq!(
            resource.get(opentelemetry::Key::from_static_str("service.name")),
            Some(opentelemetry::Value::from(super::SERVICE_NAME))
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

        assert_eq!(metrics.orders_created.load(Ordering::Relaxed), 7);
        assert_eq!(metrics.orders_partially_filled.load(Ordering::Relaxed), 3);
    }
}
