// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use dodex_infrastructure::config::IndexerConfig;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventsPage;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::indexer_repo::IndexerRepository;
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

    // Spawn the deferred-projection retry pass. Replays raw_events rows whose
    // projector returned `Deferred` (or failed) once their parent records show
    // up. Idempotent — projectors are upserts.
    let reprojector = repo.clone();
    let reprojection_interval = Duration::from_millis(config.indexer.reprojection_interval_ms);
    let reprojection_batch_size = config.indexer.reprojection_batch_size;
    tokio::spawn(reprojector.run_reprojection_loop(reprojection_interval, reprojection_batch_size));
    info!(
        interval_ms = config.indexer.reprojection_interval_ms,
        batch_size = reprojection_batch_size,
        "reprojection loop started"
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
            let ignored_event_types: HashSet<&str> =
                cfg.indexer.ignored_event_types.iter().map(String::as_str).collect();
            match drain_events(
                client,
                &repo,
                &decoder,
                cfg.graphql.page_size,
                &ignored,
                &ignored_event_types,
                &mut cursor,
            )
            .await
            {
                Ok(stats) => {
                    info!(
                        edges = stats.edges,
                        ignored = stats.ignored,
                        inserted = stats.inserted,
                        skipped = stats.skipped,
                        decoded = stats.decoded,
                        undecoded = stats.undecoded,
                        projected = stats.projected,
                        projection_deferred = stats.projection_deferred,
                        projection_failed = stats.projection_failed,
                        type_ignored = stats.type_ignored,
                        pages = stats.pages,
                        cursor = cursor.as_deref().unwrap_or(""),
                        "indexer tick"
                    );
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
    inserted: u64,
    skipped: u64,
    decoded: u64,
    undecoded: u64,
    projected: u64,
    projection_deferred: u64,
    projection_failed: u64,
    type_ignored: u64,
    pages: u32,
}

async fn drain_events(
    client: &GraphqlClient,
    repo: &IndexerRepository,
    decoder: &Decoder,
    page_size: u32,
    ignored_src: &HashSet<&str>,
    ignored_event_types: &HashSet<&str>,
    cursor: &mut Option<String>,
) -> anyhow::Result<DrainStats> {
    let mut stats = DrainStats::default();

    while stats.pages < MAX_PAGES_PER_TICK {
        let mut page: EventsPage = client.fetch_events(page_size, cursor.as_deref()).await?;
        stats.pages += 1;
        let edges_seen = page.edges.len();
        stats.edges += edges_seen;

        // Filter on src only: events are outbound externals from a contract.
        // Cursor still advances on the original page, so we will not re-fetch
        // the noise on the next tick.
        if !ignored_src.is_empty() {
            page.edges.retain(|edge| match edge.node.src.as_deref() {
                Some(src) => !ignored_src.contains(src),
                None => true,
            });
        }
        stats.ignored += (edges_seen - page.edges.len()) as u64;

        let end_cursor = page.page_info.end_cursor.as_deref();
        let persisted = repo
            .persist_page(STREAM_NAME, &page.edges, end_cursor, decoder, ignored_event_types)
            .await?;
        stats.inserted += persisted.inserted;
        stats.skipped += persisted.skipped;
        stats.decoded += persisted.decoded;
        stats.undecoded += persisted.undecoded;
        stats.projected += persisted.projected;
        stats.projection_deferred += persisted.projection_deferred;
        stats.projection_failed += persisted.projection_failed;
        stats.type_ignored += persisted.type_ignored;

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
