// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! DB-derived refresh loop for the indexer's OTLP metric counters. Every
//! `interval`, counts the tracked `raw_events` types and pushes the totals
//! into the metric caches the OTLP reader exports. Lives in the indexer (not
//! `dodex-infrastructure`) so the API service does not transitively depend on
//! opentelemetry / tonic.

use std::time::Duration;

use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_metrics::IndexerMetrics;
use tracing::debug;
use tracing::error;

/// `raw_events.event_type` whose total backs `orders_created_event_cnt`.
const ORDERS_CREATED_EVENT: &str = "OrderBook.OrderPlaced";
/// `raw_events.event_type` whose total backs `order_partially_filled_event_cnt`.
const ORDER_PARTIALLY_FILLED_EVENT: &str = "OrderBook.PartialFill";

/// Maps the per-type count rows to `(orders_created, orders_partially_filled)`.
/// Missing types default to 0; an (impossible) negative count clamps to 0;
/// untracked types are ignored.
fn resolve_counts(rows: &[(String, i64)]) -> (u64, u64) {
    let mut created = 0u64;
    let mut partially = 0u64;
    for (event_type, count) in rows {
        let count = (*count).max(0) as u64;
        match event_type.as_str() {
            ORDERS_CREATED_EVENT => created = count,
            ORDER_PARTIALLY_FILLED_EVENT => partially = count,
            _ => {}
        }
    }
    (created, partially)
}

/// Forever loop: refresh the metric caches every `interval`. Errors are logged
/// and the loop continues. Spawned only when OTLP metrics are enabled.
pub async fn run_refresh_loop(
    repo: IndexerRepository,
    interval: Duration,
    metrics: IndexerMetrics,
    cursor_stream: &'static str,
) {
    loop {
        match repo.count_events_by_type(&[ORDERS_CREATED_EVENT, ORDER_PARTIALLY_FILLED_EVENT]).await
        {
            Ok(rows) => {
                let (created, partially) = resolve_counts(&rows);
                metrics.set_orders_created(created);
                metrics.set_orders_partially_filled(partially);
                debug!(created, partially, "metrics refresh");
            }
            Err(err) => error!(?err, "metrics refresh failed"),
        }
        match repo.count_pending_projection().await {
            Ok(n) => metrics.set_projection_backlog(n.max(0) as u64),
            Err(err) => error!(?err, "projection backlog metric refresh failed"),
        }
        match repo.projection_lag_seconds().await {
            Ok(s) => metrics.set_projection_lag_seconds(s.max(0) as u64),
            Err(err) => error!(?err, "projection lag metric refresh failed"),
        }
        match repo.cursor_age_seconds(cursor_stream).await {
            Ok(age) => metrics.set_capture_cursor_age_seconds(age.unwrap_or(0).max(0) as u64),
            Err(err) => error!(?err, "cursor age metric refresh failed"),
        }
        let (in_use, idle) = repo.pool_connection_stats();
        metrics.set_pool_connections(in_use, idle);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use dodex_infrastructure::config::METRIC_CRITICAL_EVENT_TYPES;

    use super::resolve_counts;
    use super::ORDERS_CREATED_EVENT;
    use super::ORDER_PARTIALLY_FILLED_EVENT;

    /// The config startup guard refuses to drop a metric-critical type, but it
    /// only knows the types listed in `METRIC_CRITICAL_EVENT_TYPES`. If a third
    /// metric-backed type is tracked here without being added there, the guard
    /// silently stops protecting it. Fail loudly instead of drifting.
    #[test]
    fn metric_critical_covers_tracked_types() {
        for tracked in [ORDERS_CREATED_EVENT, ORDER_PARTIALLY_FILLED_EVENT] {
            assert!(
                METRIC_CRITICAL_EVENT_TYPES.contains(&tracked),
                "metrics_refresh tracks {tracked:?} but config::METRIC_CRITICAL_EVENT_TYPES \
                 does not list it — the ignored_event_types guard would not protect it; \
                 add it to METRIC_CRITICAL_EVENT_TYPES"
            );
        }
    }

    #[test]
    fn maps_tracked_types() {
        let rows = vec![
            ("OrderBook.OrderPlaced".to_string(), 5),
            ("OrderBook.PartialFill".to_string(), 2),
            ("OrderBook.OrderFilled".to_string(), 99), // untracked — ignored
        ];
        assert_eq!(resolve_counts(&rows), (5, 2));
    }

    #[test]
    fn defaults_missing_to_zero() {
        let rows = vec![("OrderBook.OrderPlaced".to_string(), 4)];
        assert_eq!(resolve_counts(&rows), (4, 0));
    }

    #[test]
    fn clamps_negative() {
        let rows = vec![("OrderBook.PartialFill".to_string(), -1)];
        assert_eq!(resolve_counts(&rows), (0, 0));
    }

    #[test]
    fn ignores_untracked_types() {
        let rows = vec![("OrderBook.OrderFilled".to_string(), 7)];
        assert_eq!(resolve_counts(&rows), (0, 0));
    }
}
