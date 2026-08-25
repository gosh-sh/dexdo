// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use dodex_chain::DEX_DAPP_ID;
use dodex_contracts::dex::root_pn::RootPn;
use dodex_infrastructure::config::IndexerConfig;
use dodex_infrastructure::database;
use dodex_infrastructure::decoder::Decoder;
use dodex_infrastructure::graphql::EventEdge;
use dodex_infrastructure::graphql::EventsPage;
use dodex_infrastructure::graphql::GraphqlClient;
use dodex_infrastructure::indexer_repo::IndexerRepository;
use dodex_infrastructure::indexer_repo::DAPP_CAPTURE_STREAM;
use dodex_infrastructure::indexer_repo::ROOT_PN_CAPTURE_STREAM;
use dodex_infrastructure::inference_reconciler::InferenceReconciler;
use dodex_infrastructure::oracle_event_list_reconciler::OracleEventListReconciler;
use dodex_infrastructure::reconciler::MarketReconciler;
use dodex_infrastructure::signal::run_config_reload_loop;
use tokio::sync::RwLock;
use tracing::error;
use tracing::info;
use tracing::warn;

mod metrics_refresh;

// Single source of truth shared with the repo, whose orphan dead-letter reads
// this stream's `at_head` — they must name the same cursor row.
const STREAM_NAME: &str = dodex_infrastructure::indexer_repo::CAPTURE_STREAM;
const MAX_PAGES_PER_TICK: u32 = 100;

#[derive(Clone, Copy, Debug)]
enum CaptureSource {
    DexDapp,
    RootPn,
}

impl CaptureSource {
    fn stream_name(self) -> &'static str {
        match self {
            Self::DexDapp => DAPP_CAPTURE_STREAM,
            Self::RootPn => ROOT_PN_CAPTURE_STREAM,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::DexDapp => "dex_dapp",
            Self::RootPn => "root_pn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CaptureBarrier {
    cursor: Option<String>,
    at_head: bool,
}

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
    // Ingest scope: the `dst` of every event our contracts emit. Static for the
    // process, so it is built once and matched per edge before decode.
    let scoped_event_dsts = dodex_infrastructure::config::scoped_event_dsts();
    info!(
        known_events = decoder.known_events(),
        scoped_dsts = scoped_event_dsts.len(),
        "abi decoder initialized"
    );

    // Spawn the market reconciler as an independent task. It keeps its own
    // GraphQL client and Decoder instance, so config-reload that swaps the
    // main-loop client does not disturb mid-run reconciliation.
    let reconciler_graphql = GraphqlClient::new(
        config.graphql.endpoint.clone(),
        Duration::from_millis(config.graphql.request_timeout_ms),
    )?
    .with_bearer_token(config.graphql.bearer_token.clone());
    let reconciler = MarketReconciler::new(pool.clone(), reconciler_graphql, decoder.clone());
    let reconciler_interval = Duration::from_millis(config.indexer.reconciliation_interval_ms);
    tokio::spawn(reconciler.run_loop(reconciler_interval));
    info!(interval_ms = config.indexer.reconciliation_interval_ms, "market reconciler started");

    // The old single-stream cursor is not a valid barrier for the new pair of
    // filtered streams. On the first dual-stream start, clear it before the
    // projector can apply rows; later restarts preserve the last synchronized
    // barrier but close `at_head` until both streams poll successfully again.
    repo.initialize_capture_barrier(&[DAPP_CAPTURE_STREAM, ROOT_PN_CAPTURE_STREAM]).await?;

    // Spawn the projection loop. It is the SOLE projector: it drains the
    // raw_events rows the capture loop writes (processed_at IS NULL) in
    // chain_order and projects each into the read-model, retrying Deferred
    // rows once their parent lands. Continuous-drain with an idle pause of
    // polling_interval_ms — no point polling for pending rows faster than
    // capture produces them.
    let projector = repo.clone().with_inference_orphan_cutoff(Duration::from_millis(
        config.indexer.inference_orphan_cutoff_ms,
    ));
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
    )?
    .with_bearer_token(config.graphql.bearer_token.clone());
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
    )?
    .with_bearer_token(config.graphql.bearer_token.clone());
    let inference_reconciler = InferenceReconciler::new(
        pool.clone(),
        inf_graphql,
        decoder.clone(),
        Duration::from_millis(config.indexer.inference_reference_price_refresh_ms),
        Duration::from_millis(config.indexer.inference_sweep_interval_ms),
    )
    .with_failure_counter(repo.inference_reconcile_failures_handle());
    let inf_interval = Duration::from_millis(config.indexer.inference_reconciliation_interval_ms);
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

    let mut dapp_cursor = repo.load_cursor(DAPP_CAPTURE_STREAM).await?;
    let mut root_pn_cursor = repo.load_cursor(ROOT_PN_CAPTURE_STREAM).await?;
    let mut published_barrier_cursor = repo.load_cursor(STREAM_NAME).await?;
    let mut pending_head_barrier: Option<CaptureBarrier> = None;
    log_resume_cursor(CaptureSource::DexDapp, dapp_cursor.as_deref());
    log_resume_cursor(CaptureSource::RootPn, root_pn_cursor.as_deref());

    let mut current_endpoint = String::new();
    let mut current_timeout_ms: u64 = 0;
    let mut current_bearer_token: Option<String> = None;
    let mut client: Option<GraphqlClient> = None;

    loop {
        let cfg = config_state.read().await.clone();

        if client.is_none()
            || current_endpoint != cfg.graphql.endpoint
            || current_timeout_ms != cfg.graphql.request_timeout_ms
            || current_bearer_token != cfg.graphql.bearer_token
        {
            match GraphqlClient::new(
                cfg.graphql.endpoint.clone(),
                Duration::from_millis(cfg.graphql.request_timeout_ms),
            ) {
                Ok(new_client) => {
                    let new_client = new_client.with_bearer_token(cfg.graphql.bearer_token.clone());
                    info!(endpoint = %cfg.graphql.endpoint, "graphql client (re)built");
                    client = Some(new_client);
                    current_endpoint = cfg.graphql.endpoint.clone();
                    current_timeout_ms = cfg.graphql.request_timeout_ms;
                    current_bearer_token = cfg.graphql.bearer_token.clone();
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

            let dapp_drain = drain_events(
                CaptureSource::DexDapp,
                client,
                &repo,
                &decoder,
                cfg.graphql.page_size,
                &scoped_event_dsts,
                &ignored,
                &ignored_event_dsts,
                &mut dapp_cursor,
            );
            let root_pn_drain = drain_events(
                CaptureSource::RootPn,
                client,
                &repo,
                &decoder,
                cfg.graphql.page_size,
                &scoped_event_dsts,
                &ignored,
                &ignored_event_dsts,
                &mut root_pn_cursor,
            );
            let (dapp_result, root_pn_result) = tokio::join!(dapp_drain, root_pn_drain);

            report_drain_result(CaptureSource::DexDapp, &dapp_result, dapp_cursor.as_deref());
            report_drain_result(CaptureSource::RootPn, &root_pn_result, root_pn_cursor.as_deref());

            if let (Ok(dapp), Ok(root_pn)) = (&dapp_result, &root_pn_result) {
                let barrier = synchronized_capture_barrier(&[
                    (dapp_cursor.as_deref(), dapp.at_head),
                    (root_pn_cursor.as_deref(), root_pn.at_head),
                ]);
                let publishable = stabilized_capture_barrier(
                    barrier,
                    published_barrier_cursor.as_deref(),
                    &mut pending_head_barrier,
                );
                match repo
                    .set_capture_barrier(publishable.cursor.as_deref(), publishable.at_head)
                    .await
                {
                    Ok(()) => published_barrier_cursor = publishable.cursor,
                    Err(err) => error!(?err, "failed to persist synchronized capture barrier"),
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(cfg.indexer.polling_interval_ms)).await;
    }
}

fn log_resume_cursor(source: CaptureSource, cursor: Option<&str>) {
    match cursor {
        Some(cursor) if !cursor.is_empty() => {
            info!(source = source.label(), cursor, "capture stream resumed from cursor")
        }
        Some(_) => warn!(
            source = source.label(),
            "stored capture cursor is empty; restarting from the earliest retained event"
        ),
        None => info!(
            source = source.label(),
            "capture stream cold start; capturing from the earliest retained event"
        ),
    }
}

fn synchronized_capture_barrier(states: &[(Option<&str>, bool)]) -> CaptureBarrier {
    let at_head = states.iter().all(|(_, at_head)| *at_head);
    let cursor = if at_head {
        states.iter().filter_map(|(cursor, _)| *cursor).max().map(str::to_owned)
    } else {
        let limiting: Vec<Option<&str>> =
            states.iter().filter(|(_, at_head)| !*at_head).map(|(cursor, _)| *cursor).collect();
        if limiting.iter().any(Option::is_none) {
            None
        } else {
            limiting.into_iter().flatten().min().map(str::to_owned)
        }
    };
    CaptureBarrier { cursor, at_head }
}

fn stabilized_capture_barrier(
    candidate: CaptureBarrier,
    published_cursor: Option<&str>,
    pending_head: &mut Option<CaptureBarrier>,
) -> CaptureBarrier {
    if !candidate.at_head {
        *pending_head = None;
        return candidate;
    }

    let previous = pending_head.replace(candidate);
    CaptureBarrier {
        cursor: previous
            .and_then(|barrier| barrier.cursor)
            .or_else(|| published_cursor.map(str::to_owned)),
        at_head: true,
    }
}

fn report_drain_result(
    source: CaptureSource,
    result: &anyhow::Result<DrainStats>,
    cursor: Option<&str>,
) {
    match result {
        Ok(stats) => {
            info!(
                source = source.label(),
                edges = stats.edges,
                ignored = stats.ignored,
                out_of_scope = stats.out_of_scope,
                dst_missing = stats.dst_missing,
                inserted = stats.inserted,
                skipped = stats.skipped,
                decoded = stats.decoded,
                undecoded = stats.undecoded,
                type_ignored = stats.type_ignored,
                pages = stats.pages,
                at_head = stats.at_head,
                cursor = cursor.unwrap_or(""),
                "capture stream tick"
            );
            if stats.dst_missing > 0 {
                warn!(
                    source = source.label(),
                    dst_missing = stats.dst_missing,
                    edges = stats.edges,
                    "scoped GraphQL edges arrived with no `dst` and could not be decoded"
                );
            }
        }
        Err(err) => error!(source = source.label(), ?err, "graphql fetch / persist failed"),
    }
}

#[derive(Debug, Default)]
struct DrainStats {
    edges: usize,
    ignored: u64,
    out_of_scope: u64,
    dst_missing: u64,
    inserted: u64,
    skipped: u64,
    decoded: u64,
    undecoded: u64,
    type_ignored: u64,
    pages: u32,
    at_head: bool,
}

#[derive(Debug, Default)]
struct FilterStats {
    ignored: u64,
    out_of_scope: u64,
    dst_missing: u64,
    type_ignored: u64,
}

/// Whether an edge is a configured no-op event to drop, matched by its external
/// `dst`. Edges with no `dst` are kept.
fn edge_is_ignored_noop(edge: &EventEdge, ignored_dsts: &HashSet<String>) -> bool {
    match edge.node.dst.as_deref() {
        Some(dst) => ignored_dsts.contains(dst),
        None => false,
    }
}

fn apply_ingest_filters(
    mut edges: Vec<EventEdge>,
    scoped_event_dsts: &HashSet<String>,
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

    // The GraphQL query has already selected either the DEX dApp or RootPN.
    // Keep the existing ABI destination check as a cheap, local discriminator
    // before decode, so unrelated/no-op messages from those sources never reach
    // the decoder or raw_events.
    //
    // `dst` is a 1:1 discriminator of event type readable from the message header,
    // so this costs no decode. An edge with no `dst` is dropped: every event we emit
    // is routed to `makeAddrExtern(EVENT_ID, 256)`, so a missing `dst` cannot be ours.
    let before = edges.len();
    let mut dst_missing = 0u64;
    edges.retain(|edge| match edge.node.dst.as_deref() {
        Some(dst) => scoped_event_dsts.contains(dst),
        // Counted apart from ordinary out-of-route traffic. An external event
        // without a `dst` should not exist; if the gateway stops reporting the
        // field, losing our events must be loud rather than silently decoded.
        None => {
            dst_missing += 1;
            false
        }
    });
    stats.out_of_scope += (before - edges.len()) as u64;
    stats.dst_missing += dst_missing;

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

/// What one drained page did to the resume cursor. Returned rather than logged
/// so the decision stays pure and a unit can pin it (IX-CAP-09); the caller owns
/// the `warn!` because only it knows which source stream to name.
#[derive(Debug, PartialEq, Eq)]
enum CursorMove {
    /// The cursor moved to the page's `endCursor`.
    Advanced,
    /// The gateway sent an empty `endCursor`. It is not a position anything can
    /// resume from, so storing it would silently restart the next tick at the
    /// oldest retained event and re-ingest the whole window.
    EmptyEndCursor,
    /// The page carried edges but no `endCursor` at all.
    MissingEndCursor,
    /// Nothing to do: no `endCursor` and nothing retained.
    Idle,
}

/// The cursor decision for one drained page: the cursor advances to the page's
/// `endCursor` whenever a usable one is present — `endCursor` comes from
/// `page_info`, not from the edges, so a page whose every edge the ingest filters
/// dropped still advances past the noise. Without one the cursor stays;
/// `retained_edges` is the POST-filter count, so only a page that still carries
/// edges earns a warn — a fully filtered page with no `endCursor` is a silent
/// no-op, deliberately: recurring noise pages would otherwise flood the log.
fn advance_cursor(
    cursor: &mut Option<String>,
    end_cursor: Option<&str>,
    retained_edges: usize,
) -> CursorMove {
    match end_cursor {
        Some(end) if !end.is_empty() => {
            *cursor = Some(end.to_string());
            CursorMove::Advanced
        }
        Some(_) => CursorMove::EmptyEndCursor,
        None if retained_edges > 0 => CursorMove::MissingEndCursor,
        None => CursorMove::Idle,
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_events(
    source: CaptureSource,
    client: &GraphqlClient,
    repo: &IndexerRepository,
    decoder: &Decoder,
    page_size: u32,
    scoped_event_dsts: &HashSet<String>,
    ignored_src: &HashSet<&str>,
    ignored_event_dsts: &HashSet<String>,
    cursor: &mut Option<String>,
) -> anyhow::Result<DrainStats> {
    let mut stats = DrainStats::default();

    while stats.pages < MAX_PAGES_PER_TICK {
        let mut page: EventsPage = match source {
            CaptureSource::DexDapp => {
                client.fetch_dapp_events(DEX_DAPP_ID, page_size, cursor.as_deref()).await?
            }
            CaptureSource::RootPn => {
                let account_id = RootPn::DEFAULT_ADDRESS
                    .strip_prefix("0:")
                    .expect("RootPN default address has a workchain prefix");
                client
                    .fetch_account_events(account_id, DEX_DAPP_ID, page_size, cursor.as_deref())
                    .await?
            }
        };
        stats.pages += 1;
        let edges_seen = page.edges.len();
        stats.edges += edges_seen;

        let (retained, filter_stats) =
            apply_ingest_filters(page.edges, scoped_event_dsts, ignored_src, ignored_event_dsts);
        page.edges = retained;
        stats.ignored += filter_stats.ignored;
        stats.out_of_scope += filter_stats.out_of_scope;
        stats.dst_missing += filter_stats.dst_missing;
        stats.type_ignored += filter_stats.type_ignored;

        let at_head = !page.page_info.has_next_page;
        stats.at_head = at_head;
        let raw_end_cursor = page.page_info.end_cursor.as_deref();
        let end_cursor = raw_end_cursor.filter(|end| !end.is_empty());
        let persisted = repo
            .persist_page(source.stream_name(), &page.edges, end_cursor, decoder, at_head)
            .await?;
        stats.inserted += persisted.inserted;
        stats.skipped += persisted.skipped;
        stats.decoded += persisted.decoded;
        stats.undecoded += persisted.undecoded;

        match advance_cursor(cursor, raw_end_cursor, page.edges.len()) {
            CursorMove::Advanced | CursorMove::Idle => {}
            CursorMove::EmptyEndCursor => warn!(
                source = source.label(),
                "graphql page returned an empty endCursor; cursor not advanced"
            ),
            CursorMove::MissingEndCursor => warn!(
                source = source.label(),
                "graphql page has edges but missing endCursor; cursor not advanced"
            ),
        }

        if !page.page_info.has_next_page {
            break;
        }
    }

    if stats.pages >= MAX_PAGES_PER_TICK {
        warn!(
            source = source.label(),
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

    fn edge_with(dst: Option<&str>) -> EventEdge {
        edge_with_all(None, None, dst)
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
    fn edge_is_ignored_noop_matches_exact_dst_only() {
        use std::collections::HashSet;
        let queued = dodex_infrastructure::config::event_type_dst(159);
        let ignored: HashSet<String> = [queued.clone()].into_iter().collect();

        assert!(edge_is_ignored_noop(&edge_with(Some(&queued)), &ignored));
        // a different dst (OrderPlaced, 143) is not dropped
        let placed = dodex_infrastructure::config::event_type_dst(143);
        assert!(!edge_is_ignored_noop(&edge_with(Some(&placed)), &ignored));
        // no dst -> kept
        assert!(!edge_is_ignored_noop(&edge_with(None), &ignored));
    }

    #[test]
    fn a_fully_filtered_page_still_advances_the_cursor() {
        // IX-CAP-09: `endCursor` is read from `page_info`, never derived from
        // the edges, so a page the ingest filters emptied entirely must still
        // move the cursor — otherwise the capture loop would re-fetch the same
        // noise page forever.
        let mut cursor = Some("c1".to_string());
        let moved = advance_cursor(&mut cursor, Some("c2"), 0);
        assert_eq!(cursor.as_deref(), Some("c2"), "the dropped page must be passed, not re-read");
        assert_eq!(moved, CursorMove::Advanced, "advancing is the healthy path — no warn");
    }

    #[test]
    fn a_fully_filtered_page_without_end_cursor_stays_silent() {
        // The current silence is pinned DELIBERATELY: the warn keys on the
        // post-filter edge count, so a page with edges but no endCursor warns,
        // while a fully filtered one without endCursor says nothing and leaves
        // the cursor alone. A gateway stuck emitting such pages is visible only
        // through the cursor-age gauge, not the log — recorded here so a future
        // reader finds a decision, not an accident.
        let mut cursor = Some("c1".to_string());
        let moved = advance_cursor(&mut cursor, None, 0);
        assert_eq!(cursor.as_deref(), Some("c1"), "no endCursor — the cursor must not move");
        assert_eq!(
            moved,
            CursorMove::Idle,
            "a fully filtered page without endCursor is a silent no-op"
        );

        // Contrast case, same function: edges retained + no endCursor => warn.
        let mut cursor2 = Some("c1".to_string());
        assert_eq!(advance_cursor(&mut cursor2, None, 3), CursorMove::MissingEndCursor);
        assert_eq!(cursor2.as_deref(), Some("c1"));
    }

    #[test]
    fn an_empty_end_cursor_is_refused_rather_than_stored() {
        // An empty string is not a position the gateway can resume from. Storing
        // it would silently restart the next tick at the oldest retained event
        // and re-ingest the whole window — the cursor must stay where it was.
        let mut cursor = Some("c1".to_string());
        let moved = advance_cursor(&mut cursor, Some(""), 3);
        assert_eq!(cursor.as_deref(), Some("c1"), "an empty endCursor must not move the cursor");
        assert_eq!(moved, CursorMove::EmptyEndCursor);
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

        // Both dsts are real DEXDO event ids, so the scope filter passes them all
        // through and each drop below is attributable to the filter under test.
        let known: HashSet<String> = [queued.clone(), placed.clone()].into_iter().collect();

        let (retained, stats) =
            apply_ingest_filters(edges, &known, &ignored_src, &ignored_event_dsts);

        assert_eq!(retained.len(), 2);
        assert_eq!(stats.ignored, 1);
        assert_eq!(stats.out_of_scope, 0);
        assert_eq!(stats.type_ignored, 2);
    }

    #[test]
    fn apply_ingest_filters_keeps_only_dsts_our_abis_declare() {
        use std::collections::HashSet;

        // 143 is OrderBook.OrderPlaced; 5 is an id no DEXDO contract emits — it is
        // one of the ids actually observed on mainnet, where such traffic outnumbers
        // ours by orders of magnitude.
        let placed = dodex_infrastructure::config::event_type_dst(143);
        let foreign = dodex_infrastructure::config::event_type_dst(5);
        let known: HashSet<String> = [placed.clone()].into_iter().collect();

        let edges = vec![
            edge_with_all(Some("own"), None, Some(&placed)),
            edge_with_all(Some("someone-else"), None, Some(&foreign)),
            edge_with_all(Some("someone-else"), None, None),
        ];

        let (retained, stats) =
            apply_ingest_filters(edges, &known, &HashSet::new(), &HashSet::new());

        assert_eq!(retained.len(), 1, "only the event our ABI declares survives");
        assert_eq!(retained[0].node.dst.as_deref(), Some(placed.as_str()));
        // A missing dst is dropped too — every event we emit carries one — but it
        // is counted apart so a gateway that stops reporting the field is loud
        // rather than indistinguishable from ordinary foreign traffic.
        assert_eq!(stats.out_of_scope, 2);
        assert_eq!(stats.dst_missing, 1);
    }

    #[test]
    fn apply_ingest_filters_drops_token_contract_routes_before_decode() {
        use std::collections::HashSet;

        let order_placed = dodex_infrastructure::config::event_type_dst(1000);
        let token_stream_funded = dodex_infrastructure::config::event_type_dst(720);
        let edges = vec![
            edge_with_all(Some("orderbook"), Some(DEX_DAPP_ID), Some(&order_placed)),
            edge_with_all(Some("token-contract"), Some(DEX_DAPP_ID), Some(&token_stream_funded)),
        ];

        let (retained, stats) = apply_ingest_filters(
            edges,
            &dodex_infrastructure::config::scoped_event_dsts(),
            &HashSet::new(),
            &HashSet::new(),
        );

        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].node.dst.as_deref(), Some(order_placed.as_str()));
        assert_eq!(stats.out_of_scope, 1);
    }

    #[test]
    fn synchronized_barrier_uses_highest_cursor_when_both_streams_are_at_head() {
        assert_eq!(
            synchronized_capture_barrier(&[(Some("10"), true), (Some("20"), true)]),
            CaptureBarrier { cursor: Some("20".to_string()), at_head: true }
        );
    }

    #[test]
    fn synchronized_barrier_is_limited_only_by_streams_still_backfilling() {
        assert_eq!(
            synchronized_capture_barrier(&[(Some("10"), false), (Some("20"), true)]),
            CaptureBarrier { cursor: Some("10".to_string()), at_head: false }
        );
        assert_eq!(
            synchronized_capture_barrier(&[(Some("30"), false), (Some("20"), false)]),
            CaptureBarrier { cursor: Some("20".to_string()), at_head: false }
        );
    }

    #[test]
    fn empty_head_stream_does_not_block_but_empty_backfill_stream_does() {
        assert_eq!(
            synchronized_capture_barrier(&[(Some("10"), false), (None, true)]),
            CaptureBarrier { cursor: Some("10".to_string()), at_head: false }
        );
        assert_eq!(
            synchronized_capture_barrier(&[(Some("10"), true), (None, false)]),
            CaptureBarrier { cursor: None, at_head: false }
        );
    }

    #[test]
    fn head_barrier_is_published_only_after_the_next_successful_poll() {
        let mut pending = None;
        let first = stabilized_capture_barrier(
            CaptureBarrier { cursor: Some("20".to_string()), at_head: true },
            Some("10"),
            &mut pending,
        );
        assert_eq!(
            first,
            CaptureBarrier { cursor: Some("10".to_string()), at_head: true },
            "the first head observation keeps the previously proven barrier"
        );

        let second = stabilized_capture_barrier(
            CaptureBarrier { cursor: Some("30".to_string()), at_head: true },
            first.cursor.as_deref(),
            &mut pending,
        );
        assert_eq!(
            second,
            CaptureBarrier { cursor: Some("20".to_string()), at_head: true },
            "the next poll proves the prior candidate safe even if a newer event arrived"
        );
    }

    #[test]
    fn backfill_barrier_is_immediate_and_resets_head_stabilization() {
        let mut pending = Some(CaptureBarrier { cursor: Some("20".to_string()), at_head: true });
        let barrier = stabilized_capture_barrier(
            CaptureBarrier { cursor: Some("25".to_string()), at_head: false },
            Some("10"),
            &mut pending,
        );
        assert_eq!(barrier, CaptureBarrier { cursor: Some("25".to_string()), at_head: false });
        assert!(pending.is_none());
    }
}
