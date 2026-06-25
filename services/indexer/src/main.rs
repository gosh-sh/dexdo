// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use dodex_infrastructure::config::IndexerConfig;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventEdge;
use dodex_infrastructure::graphql::EventsPage;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_infrastructure::inference_reconciler::InferenceReconciler;
use dodex_infrastructure::oracle_event_list_reconciler::OracleEventListReconciler;
use dodex_infrastructure::reconciler::MarketReconciler;
use dodex_infrastructure::signal::run_config_reload_loop;
use tokio::sync::RwLock;
use tracing::error;
use tracing::info;
use tracing::warn;

mod metrics_refresh;

const STREAM_NAME: &str = "blockchain_events";
const MAX_PAGES_PER_TICK: u32 = 100;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // When LOG_DIR is set, these guards keep the background file-log writer
    // alive for the lifetime of the process; the indexer loops until shutdown.
    let _guards = dodex_logging::init("indexer");

    let config_path =
        env::var("APP_CONFIG").unwrap_or_else(|_| "config/indexer.local.yaml".to_string());
    let config = IndexerConfig::load_from_path(&config_path)?;
    let config_state = Arc::new(RwLock::new(config.clone()));

    tokio::spawn(run_config_reload_loop(config_path.clone(), Arc::clone(&config_state), "indexer"));

    let pool = database::build_pool(&config.common.database).await?;
    database::run_migrations(&pool).await?;
    let repo = IndexerRepository::new(pool.clone());
    let decoder = Decoder::new()?;
    info!(known_events = decoder.known_events(), "abi decoder initialized");

    // Spawn the market reconciler as an independent task. It keeps its own
    // GraphQL client and Decoder instance, so config-reload that swaps the
    // main-loop client does not disturb mid-run reconciliation.
    let reconciler_graphql = GraphqlClient::new(
        config.graphql.endpoint.clone(),
        Duration::from_millis(config.graphql.request_timeout_ms),
    )?;
    let reconciler = MarketReconciler::new(pool.clone(), reconciler_graphql, decoder.clone());
    let reconciler_interval = Duration::from_millis(config.indexer.reconciliation_interval_ms);
    tokio::spawn(reconciler.run_loop(reconciler_interval));
    info!(interval_ms = config.indexer.reconciliation_interval_ms, "market reconciler started");

    // Spawn the projection loop. It is the SOLE projector: it drains the
    // raw_events rows the capture loop writes (processed_at IS NULL) in
    // chain_order and projects each into the read-model, retrying Deferred
    // rows once their parent lands. Continuous-drain with an idle pause of
    // polling_interval_ms — no point polling for pending rows faster than
    // capture produces them.
    let projector = repo.clone()
        .with_inference_orphan_cutoff(Duration::from_millis(config.indexer.inference_orphan_cutoff_ms));
    let projection_idle_interval = Duration::from_millis(config.indexer.polling_interval_ms);
    let projection_batch_size = config.indexer.reprojection_batch_size;
    tokio::spawn(projector.run_reprojection_loop(projection_idle_interval, projection_batch_size));
    info!(
        idle_interval_ms = config.indexer.polling_interval_ms,
        batch_size = projection_batch_size,
        "projection loop started"
    );

    // Spawn the OracleEventList reconciler. Fills `oracle_events.describe`
    // (and `trust_addr`) by calling the OEL `_events` getter — these fields
    // live in contract state but are not carried by the `EventAdded` event.
    let oel_graphql = GraphqlClient::new(
        config.graphql.endpoint.clone(),
        Duration::from_millis(config.graphql.request_timeout_ms),
    )?;
    let oel_reconciler = OracleEventListReconciler::new(pool.clone(), oel_graphql, decoder.clone());
    let oel_interval =
        Duration::from_millis(config.indexer.oracle_event_list_reconciliation_interval_ms);
    tokio::spawn(oel_reconciler.run_loop(oel_interval));
    info!(
        interval_ms = config.indexer.oracle_event_list_reconciliation_interval_ms,
        "oracle event list reconciler started"
    );

    let inf_graphql = GraphqlClient::new(
        config.graphql.endpoint.clone(),
        Duration::from_millis(config.graphql.request_timeout_ms),
    )?;
    let inference_reconciler = InferenceReconciler::new(
        pool.clone(),
        inf_graphql,
        decoder.clone(),
        Duration::from_millis(config.indexer.inference_reference_price_refresh_ms),
        Duration::from_millis(config.indexer.inference_sweep_interval_ms),
    );
    let inf_interval =
        Duration::from_millis(config.indexer.inference_reconciliation_interval_ms);
    tokio::spawn(inference_reconciler.run_loop(inf_interval));
    info!(
        interval_ms = config.indexer.inference_reconciliation_interval_ms,
        "inference reconciler started"
    );

    // OTLP metrics. `init()` returns `None` when no OTEL endpoint env var is
    // set, in which case nothing is collected. `_metrics` owns the meter
    // provider and must outlive the loop — bound here for the process
    // lifetime, exactly like `_guards` above.
    let _metrics = dodex_metrics::init();
    match _metrics.as_ref() {
        Some(m) => {
            tokio::spawn(metrics_refresh::run_refresh_loop(
                repo.clone(),
                dodex_metrics::REFRESH_INTERVAL,
                m.indexer.clone(),
                STREAM_NAME,
            ));
            info!(
                interval_s = dodex_metrics::REFRESH_INTERVAL.as_secs(),
                "metrics refresh loop started"
            );
        }
        None => info!("no OTLP endpoint configured; metrics not collected"),
    }

    let mut cursor = repo.load_cursor(STREAM_NAME).await?;
    info!(cursor = cursor.as_deref().unwrap_or(""), "indexer resumed from cursor");

    let mut current_endpoint = String::new();
    let mut current_timeout_ms: u64 = 0;
    let mut client: Option<GraphqlClient> = None;

    loop {
        let cfg = config_state.read().await.clone();

        if client.is_none()
            || current_endpoint != cfg.graphql.endpoint
            || current_timeout_ms != cfg.graphql.request_timeout_ms
        {
            match GraphqlClient::new(
                cfg.graphql.endpoint.clone(),
                Duration::from_millis(cfg.graphql.request_timeout_ms),
            ) {
                Ok(new_client) => {
                    info!(endpoint = %cfg.graphql.endpoint, "graphql client (re)built");
                    client = Some(new_client);
                    current_endpoint = cfg.graphql.endpoint.clone();
                    current_timeout_ms = cfg.graphql.request_timeout_ms;
                }
                Err(err) => {
                    error!(?err, "failed to build graphql client; will retry next tick");
                }
            }
        }

        if let Some(client) = client.as_ref() {
            let ignored: HashSet<&str> =
                cfg.indexer.ignored_addresses.iter().map(String::as_str).collect();
            let ignored_event_dsts =
                dodex_infrastructure::config::ignored_event_dsts(&cfg.indexer.ignored_event_types);
            let dapp_id = cfg.indexer.dapp_id.as_deref();
            match drain_events(
                client,
                &repo,
                &decoder,
                cfg.graphql.page_size,
                dapp_id,
                &ignored,
                &ignored_event_dsts,
                &mut cursor,
            )
            .await
            {
                Ok(stats) => {
                    info!(
                        edges = stats.edges,
                        ignored = stats.ignored,
                        foreign_skipped = stats.foreign_skipped,
                        inserted = stats.inserted,
                        skipped = stats.skipped,
                        decoded = stats.decoded,
                        undecoded = stats.undecoded,
                        type_ignored = stats.type_ignored,
                        pages = stats.pages,
                        cursor = cursor.as_deref().unwrap_or(""),
                        "capture tick"
                    );
                    if let Some(drop_rate) =
                        foreign_drop_warning(stats.edges, stats.foreign_skipped)
                    {
                        warn!(
                            edges = stats.edges,
                            foreign_skipped = stats.foreign_skipped,
                            drop_rate,
                            "indexer dropped foreign-dapp edges"
                        );
                    }
                    if let Some(drop_rate) =
                        ignored_type_drop_warning(stats.edges, stats.type_ignored)
                    {
                        warn!(
                            edges = stats.edges,
                            type_ignored = stats.type_ignored,
                            drop_rate,
                            "indexer dropped high share of ignored event-type edges"
                        );
                    }
                }
                Err(err) => {
                    error!(?err, "graphql fetch / persist failed");
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(cfg.indexer.polling_interval_ms)).await;
    }
}

#[derive(Debug, Default)]
struct DrainStats {
    edges: usize,
    ignored: u64,
    foreign_skipped: u64,
    inserted: u64,
    skipped: u64,
    decoded: u64,
    undecoded: u64,
    type_ignored: u64,
    pages: u32,
}

#[derive(Debug, Default)]
struct FilterStats {
    ignored: u64,
    foreign_skipped: u64,
    type_ignored: u64,
}

/// Whether an event edge belongs to the configured DEXDO dapp. Edges with no
/// `src_dapp_id` are kept (treated as in-scope) so a gateway that omits the
/// field never costs us our own events.
fn edge_in_scope(edge: &EventEdge, dapp_id: &str) -> bool {
    match edge.node.src_dapp_id.as_deref() {
        Some(d) => d == dapp_id,
        None => true,
    }
}

/// Whether an edge is a configured no-op event to drop, matched by its external
/// `dst`. Edges with no `dst` are kept.
fn edge_is_ignored_noop(edge: &EventEdge, ignored_dsts: &HashSet<String>) -> bool {
    match edge.node.dst.as_deref() {
        Some(dst) => ignored_dsts.contains(dst),
        None => false,
    }
}

fn drop_rate(edges: usize, dropped: u64) -> Option<f64> {
    if edges == 0 || dropped == 0 {
        return None;
    }
    Some(dropped as f64 / edges as f64)
}

fn foreign_drop_warning(edges: usize, dropped: u64) -> Option<f64> {
    drop_rate(edges, dropped)
}

fn ignored_type_drop_warning(_edges: usize, _dropped: u64) -> Option<f64> {
    // Configured no-op event types are expected to be dropped during normal
    // OrderBook floods; the per-tick info log still carries the count.
    None
}

fn apply_ingest_filters(
    mut edges: Vec<EventEdge>,
    dapp_id: Option<&str>,
    ignored_src: &HashSet<&str>,
    ignored_event_dsts: &HashSet<String>,
) -> (Vec<EventEdge>, FilterStats) {
    let mut stats = FilterStats::default();

    // Filter on src only: events are outbound externals from a contract.
    // Cursor still advances on the original page, so we will not re-fetch
    // the noise on the next tick.
    if !ignored_src.is_empty() {
        let before = edges.len();
        edges.retain(|edge| match edge.node.src.as_deref() {
            Some(src) => !ignored_src.contains(src),
            None => true,
        });
        stats.ignored += (before - edges.len()) as u64;
    }

    // Scope to the DEXDO dapp: drop foreign chain traffic before decode.
    // Inert when no dapp_id is configured.
    if let Some(dapp_id) = dapp_id {
        let before = edges.len();
        edges.retain(|edge| edge_in_scope(edge, dapp_id));
        stats.foreign_skipped += (before - edges.len()) as u64;
    }

    // Drop configured no-op event types by dst before decode. dst is in the
    // message header, so this costs no decode. PartialFill is excluded by
    // the startup guard, so its dst is never here and its metric stays.
    if !ignored_event_dsts.is_empty() {
        let before = edges.len();
        edges.retain(|edge| !edge_is_ignored_noop(edge, ignored_event_dsts));
        stats.type_ignored += (before - edges.len()) as u64;
    }

    (edges, stats)
}

#[allow(clippy::too_many_arguments)]
async fn drain_events(
    client: &GraphqlClient,
    repo: &IndexerRepository,
    decoder: &Decoder,
    page_size: u32,
    dapp_id: Option<&str>,
    ignored_src: &HashSet<&str>,
    ignored_event_dsts: &HashSet<String>,
    cursor: &mut Option<String>,
) -> anyhow::Result<DrainStats> {
    let mut stats = DrainStats::default();

    while stats.pages < MAX_PAGES_PER_TICK {
        let mut page: EventsPage = client.fetch_events(page_size, cursor.as_deref()).await?;
        stats.pages += 1;
        let edges_seen = page.edges.len();
        stats.edges += edges_seen;

        let (retained, filter_stats) =
            apply_ingest_filters(page.edges, dapp_id, ignored_src, ignored_event_dsts);
        page.edges = retained;
        stats.ignored += filter_stats.ignored;
        stats.foreign_skipped += filter_stats.foreign_skipped;
        stats.type_ignored += filter_stats.type_ignored;

        let at_head = !page.page_info.has_next_page;
        let end_cursor = page.page_info.end_cursor.as_deref();
        let persisted =
            repo.persist_page(STREAM_NAME, &page.edges, end_cursor, decoder, at_head).await?;
        stats.inserted += persisted.inserted;
        stats.skipped += persisted.skipped;
        stats.decoded += persisted.decoded;
        stats.undecoded += persisted.undecoded;

        if let Some(end) = page.page_info.end_cursor.clone() {
            *cursor = Some(end);
        } else if !page.edges.is_empty() {
            warn!("graphql page has edges but missing endCursor; cursor not advanced");
        }

        if !page.page_info.has_next_page {
            break;
        }
    }

    if stats.pages >= MAX_PAGES_PER_TICK {
        warn!(
            pages = stats.pages,
            "graphql drain hit MAX_PAGES_PER_TICK; will continue on next tick"
        );
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use dodex_infrastructure::graphql::EventEdge;
    use dodex_infrastructure::graphql::EventNode;

    use super::*;

    fn edge_with(src_dapp_id: Option<&str>, dst: Option<&str>) -> EventEdge {
        edge_with_all(None, src_dapp_id, dst)
    }

    fn edge_with_all(src: Option<&str>, src_dapp_id: Option<&str>, dst: Option<&str>) -> EventEdge {
        EventEdge {
            cursor: "c".to_string(),
            node: EventNode {
                msg_id: "m".to_string(),
                msg_chain_order: Some("m".to_string()),
                src: src.map(str::to_string),
                src_dapp_id: src_dapp_id.map(str::to_string),
                dst: dst.map(str::to_string),
                body: None,
                created_at: None,
            },
        }
    }

    #[test]
    fn edge_in_scope_keeps_own_dapp_and_null_drops_foreign() {
        assert!(edge_in_scope(&edge_with(Some("dexdo"), None), "dexdo"));
        assert!(!edge_in_scope(&edge_with(Some("other"), None), "dexdo"));
        assert!(edge_in_scope(&edge_with(None, None), "dexdo"), "null dapp is kept");
    }

    #[test]
    fn edge_is_ignored_noop_matches_exact_dst_only() {
        use std::collections::HashSet;
        let queued = dodex_infrastructure::config::event_type_dst(159);
        let ignored: HashSet<String> = [queued.clone()].into_iter().collect();

        assert!(edge_is_ignored_noop(&edge_with(None, Some(&queued)), &ignored));
        // a different dst (OrderPlaced, 143) is not dropped
        let placed = dodex_infrastructure::config::event_type_dst(143);
        assert!(!edge_is_ignored_noop(&edge_with(None, Some(&placed)), &ignored));
        // no dst -> kept
        assert!(!edge_is_ignored_noop(&edge_with(None, None), &ignored));
    }

    #[test]
    fn foreign_drop_warning_trips_on_any_nonzero_drop() {
        assert!(foreign_drop_warning(10, 1).is_some());
        assert!(foreign_drop_warning(10, 4).is_some());
    }

    #[test]
    fn foreign_drop_warning_stays_quiet_for_empty_or_zero_drop_ticks() {
        assert!(foreign_drop_warning(0, 1).is_none());
        assert!(foreign_drop_warning(10, 0).is_none());
    }

    #[test]
    fn ignored_type_drop_warning_is_disabled_for_expected_noop_floods() {
        assert!(ignored_type_drop_warning(10, 10).is_none());
    }

    #[test]
    fn apply_ingest_filters_attributes_each_drop_to_the_filter_that_removed_it() {
        use std::collections::HashSet;

        let queued = dodex_infrastructure::config::event_type_dst(159);
        let placed = dodex_infrastructure::config::event_type_dst(143);
        let ignored_src: HashSet<&str> = ["noise"].into_iter().collect();
        let ignored_event_dsts: HashSet<String> = [queued.clone()].into_iter().collect();

        let edges = vec![
            edge_with_all(Some("noise"), Some("dexdo"), Some(&queued)),
            edge_with_all(Some("own"), Some("other"), Some(&queued)),
            edge_with_all(Some("own"), Some("dexdo"), Some(&queued)),
            edge_with_all(Some("own"), Some("dexdo"), Some(&placed)),
            edge_with_all(Some("own"), None, Some(&placed)),
        ];

        let (retained, stats) =
            apply_ingest_filters(edges, Some("dexdo"), &ignored_src, &ignored_event_dsts);

        assert_eq!(retained.len(), 2);
        assert_eq!(stats.ignored, 1);
        assert_eq!(stats.foreign_skipped, 1);
        assert_eq!(stats.type_ignored, 1);
    }
}
