// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::env;
use std::sync::Arc;
use std::time::Duration;

use dodex_infrastructure::config::IndexerConfig;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventsPage;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_infrastructure::signal::run_config_reload_loop;
use tokio::sync::RwLock;
use tracing::error;
use tracing::info;
use tracing::warn;

const STREAM_NAME: &str = "blockchain_events";
const MAX_PAGES_PER_TICK: u32 = 100;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path =
        env::var("APP_CONFIG").unwrap_or_else(|_| "config/indexer.local.yaml".to_string());
    let config = IndexerConfig::load_from_path(&config_path)?;
    let config_state = Arc::new(RwLock::new(config.clone()));

    tokio::spawn(run_config_reload_loop(config_path.clone(), Arc::clone(&config_state), "indexer"));

    let pool = database::build_pool(&config.common.database).await?;
    database::run_migrations(&pool).await?;
    let repo = IndexerRepository::new(pool);
    let decoder = Decoder::new()?;
    info!(known_events = decoder.known_events(), "abi decoder initialized");

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
            match drain_events(client, &repo, &decoder, cfg.graphql.page_size, &mut cursor).await {
                Ok(stats) => {
                    info!(
                        edges = stats.edges,
                        inserted = stats.inserted,
                        skipped = stats.skipped,
                        decoded = stats.decoded,
                        undecoded = stats.undecoded,
                        projected = stats.projected,
                        projection_deferred = stats.projection_deferred,
                        projection_failed = stats.projection_failed,
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
    inserted: u64,
    skipped: u64,
    decoded: u64,
    undecoded: u64,
    projected: u64,
    projection_deferred: u64,
    projection_failed: u64,
    pages: u32,
}

async fn drain_events(
    client: &GraphqlClient,
    repo: &IndexerRepository,
    decoder: &Decoder,
    page_size: u32,
    cursor: &mut Option<String>,
) -> anyhow::Result<DrainStats> {
    let mut stats = DrainStats::default();

    while stats.pages < MAX_PAGES_PER_TICK {
        let page: EventsPage = client.fetch_events(page_size, cursor.as_deref()).await?;
        stats.pages += 1;
        stats.edges += page.edges.len();

        let end_cursor = page.page_info.end_cursor.as_deref();
        let persisted = repo.persist_page(STREAM_NAME, &page.edges, end_cursor, decoder).await?;
        stats.inserted += persisted.inserted;
        stats.skipped += persisted.skipped;
        stats.decoded += persisted.decoded;
        stats.undecoded += persisted.undecoded;
        stats.projected += persisted.projected;
        stats.projection_deferred += persisted.projection_deferred;
        stats.projection_failed += persisted.projection_failed;

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
