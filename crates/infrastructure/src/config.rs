// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    #[serde(flatten)]
    pub common: CommonSection,
    pub server: ServerSection,
    pub auth: AuthSection,
    /// Defaulting to an empty `gateway_endpoint` so YAML files written
    /// before this field existed still parse — `validate()` then
    /// surfaces the missing endpoint with a clear error. Live configs
    /// (api.local.yaml, stage, prod) populate it.
    #[serde(default)]
    pub chain: ChainSection,
    /// On-demand PrivateNote BOC reads for `/api/v1/account` and
    /// `/api/v1/account/balances`. Production may point this at the same
    /// gateway the indexer uses; we keep it as its own section so the
    /// two can diverge (e.g. a read replica for the API).
    pub graphql: GraphqlSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerConfig {
    #[serde(flatten)]
    pub common: CommonSection,
    pub graphql: GraphqlSection,
    pub indexer: IndexerSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonSection {
    pub app: AppSection,
    pub database: DatabaseSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSection {
    pub env: String,
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSection {
    pub host: String,
    pub port: u16,
    /// Per-request wall-clock budget. Enforced by the
    /// `timeout_hoop::enforce_request_timeout` hoop in `services/api`:
    /// a handler that hangs past this returns
    /// `-1007 / 504 RequestTimeout`. Must exceed
    /// `chain.place_order_timeout_ms` by enough slack to cover the
    /// path between the chain sender's own timeout firing and the
    /// handler shaping its response — otherwise the HTTP timeout can
    /// fire while a chain submission is still in flight (api.local.yaml
    /// ships 30s chain + 5s slack = 35s).
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSection {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_ms: u64,
}

/// `recvWindow` constants mandated by `docs/api-spec.md §Security
/// Types`: client-side default is 5 s, server-side ceiling is 60 s.
/// Operators may tighten either knob in config; loosening the ceiling
/// past `MAX_RECV_WINDOW_MS` fails validation.
const DEFAULT_RECV_WINDOW_MS: u64 = 5_000;
const MAX_RECV_WINDOW_MS: u64 = 60_000;

/// `kek_hex` is the at-rest encryption key (32 bytes / 64 hex chars).
/// Every environment supplies its own; `config/api.local.yaml` ships
/// a shared dev value, stage/prod configs are assembled by CI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    pub kek_hex: String,
    #[serde(default = "default_recv_window_ms")]
    pub default_recv_window_ms: u64,
    #[serde(default = "default_max_recv_window_ms")]
    pub max_recv_window_ms: u64,
    #[serde(default)]
    pub seed_accounts: bool,
    /// Path to the JSON notes file the seeder reads when `seed_accounts`
    /// is on. The file holds real note custody keys, so it is supplied out
    /// of band per environment and never committed (`config/seed_notes*.json`
    /// is gitignored). The only committed notes file is the cargo test
    /// fixture `services/api/tests/fixtures/seed_notes_dummy.json`. Required
    /// whenever `seed_accounts` is true.
    #[serde(default)]
    pub seed_accounts_path: Option<String>,
}

fn default_recv_window_ms() -> u64 {
    DEFAULT_RECV_WINDOW_MS
}

fn default_max_recv_window_ms() -> u64 {
    MAX_RECV_WINDOW_MS
}

/// Chain gateway settings used by `DexChainSender`. `gateway_endpoint`
/// is the Acki Nacki node URL the trading path POSTs external messages
/// to; the `*_timeout_ms` fields bound the per-request wait so a hung
/// gateway cannot indefinitely stall an HTTP caller. See
/// `docs/tech-specs/write-api.md §Chain submission` for the layering.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChainSection {
    #[serde(default)]
    pub gateway_endpoint: String,
    #[serde(default = "default_place_order_timeout_ms")]
    pub place_order_timeout_ms: u64,
    #[serde(default = "default_cancel_order_timeout_ms")]
    pub cancel_order_timeout_ms: u64,
    #[serde(default = "default_place_batch_timeout_ms")]
    pub place_batch_timeout_ms: u64,
    #[serde(default = "default_cancel_batch_timeout_ms")]
    pub cancel_batch_timeout_ms: u64,
    #[serde(default = "default_split_full_set_timeout_ms")]
    pub split_full_set_timeout_ms: u64,
    /// Batch-length cap for `POST`/`DELETE /api/v1/batchOrders`,
    /// advertised as `maxBatchSize` in `/api/v1/markets`. Manually
    /// mirrors the chain's compiled-in per-side `MAX_BATCH_SIZE`
    /// (`contracts/modifiers/modifiers.sol`, 10 today) — the chain
    /// exposes no getter for it. Must not exceed the chain value:
    /// a larger cap lets batches through only to be rejected on-chain
    /// with `ERR_BATCH_TOO_LARGE`, surfaced as 503 instead of 400.
    #[serde(default = "default_max_batch_size")]
    pub max_batch_size: u16,
}

/// 30 s — comfortable budget given typical chain round-trip is 1-3 s.
/// Tight enough that a partitioned gateway does not pin HTTP workers
/// indefinitely; loose enough that occasional slow ticks do not flake.
fn default_place_order_timeout_ms() -> u64 {
    30_000
}

/// Same 30 s budget as placement. `PrivateNote.cancelOrder` follows
/// the same chain-round-trip profile (busy-check + forward to
/// `OrderBook.executeBatch` internal message) so the chain-side wait
/// is comparable.
fn default_cancel_order_timeout_ms() -> u64 {
    30_000
}

/// Same 30 s budget as single-order placement. `PrivateNote.placeBatch`
/// runs more validation per call but the synchronous chain return
/// fires off the same external message as `placeOrder` — the wait is
/// bounded by network latency, not by per-item work. The symmetry is
/// conservative-pending-data: once we have shellnet `placeBatch`
/// latency measurements for batches at `max_batch_size`, this default
/// should be revisited rather than carrying the assumption forward.
fn default_place_batch_timeout_ms() -> u64 {
    30_000
}

/// Same 30 s budget as the other chain entry points. The batch cancel
/// dispatches the same `PrivateNote.placeBatch` (with an empty
/// placements side) and so shares the placement batch's busy-window
/// profile (one external message, `_pendingBatchActive` held until
/// `onBatchComplete` returns) — the synchronous wait is comparable.
fn default_cancel_batch_timeout_ms() -> u64 {
    30_000
}

/// Same 30 s budget. `PrivateNote.splitFullSet` shares the same
/// busy-window profile (one external message, `_busy` cleared by the
/// asynchronous `onSplitAccepted` callback) — the synchronous chain
/// return from the kit awaits only `splitFullSet`'s own execution, not
/// the callback, so the timeout bounds the same network round-trip as
/// placement.
fn default_split_full_set_timeout_ms() -> u64 {
    30_000
}

/// Mirrors the chain's `MAX_BATCH_SIZE` (contracts/modifiers/modifiers.sol).
fn default_max_batch_size() -> u16 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphqlSection {
    pub endpoint: String,
    /// Batch page size for paginated GraphQL queries (indexer path).
    /// Optional at the API tier, which does not paginate; defaults to 100.
    #[serde(default = "default_graphql_page_size")]
    pub page_size: u32,
    pub request_timeout_ms: u64,
}

fn default_graphql_page_size() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexerSection {
    pub polling_interval_ms: u64,
    pub depth_refresh_interval_ms: u64,
    pub reconciliation_interval_ms: u64,
    /// Cadence of the deferred-projection retry pass. Background loop scans
    /// `raw_events` rows whose `processed_at` is still null and re-runs the
    /// projector against stored `decoded` jsonb, in chain-arrival order.
    #[serde(default = "default_reprojection_interval_ms")]
    pub reprojection_interval_ms: u64,
    /// Maximum rows replayed per reprojection sweep. Bounded so a long idle
    /// backlog does not block the rest of the indexer for too long.
    #[serde(default = "default_reprojection_batch_size")]
    pub reprojection_batch_size: u32,
    /// Cadence of the OracleEventList reconciler. Calls `_events` getter on
    /// OEL contracts that still have child `oracle_events` rows with
    /// `describe is null` (the `EventAdded` event does not carry `describe`,
    /// `count`, or `trustAddr` — they live only in contract state).
    #[serde(default = "default_oel_reconciliation_interval_ms")]
    pub oracle_event_list_reconciliation_interval_ms: u64,
    /// Source addresses whose events must be skipped entirely:
    /// edges with matching `node.src` are dropped before raw_events insert
    /// and projector dispatch. Useful to silence well-known noise contracts
    /// (system / null-route addresses) without polluting the read-model.
    #[serde(default)]
    pub ignored_addresses: Vec<String>,
    /// Decoded `event_type` values to drop at ingest, before the `raw_events`
    /// insert and projection. The page cursor still advances past them. Used
    /// to shed observability-only floods (e.g. `OrderBook.Queued`).
    ///
    /// Only genuine no-op types belong here — those `projectors::project_event`
    /// maps to `ProjectionOutcome::Applied` without touching a read-model table
    /// AND that are not metric-critical
    /// (`OrderBook.FullyFilled`/`Queued`/`Rejected`/`CallbackBounced` — the
    /// exact set is [`IGNORABLE_EVENT_TYPES`]). `OrderBook.PartialFill` is also
    /// a projector no-op, but its `raw_events` rows back an OTLP counter (it is
    /// metric-critical), so it is excluded.
    /// `IndexerConfig::validate` rejects any entry outside
    /// [`IGNORABLE_EVENT_TYPES`] — metric-critical types, state-changing types
    /// (e.g. `OrderBook.OrderFilled`, which would corrupt `live_orders`), and
    /// typos all fail loudly at startup rather than silently doing nothing.
    #[serde(default)]
    pub ignored_event_types: Vec<String>,
}

fn default_reprojection_interval_ms() -> u64 {
    30_000
}

fn default_reprojection_batch_size() -> u32 {
    500
}

fn default_oel_reconciliation_interval_ms() -> u64 {
    60_000
}

impl ApiConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let cfg: Self = load_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        self.common.validate()?;
        anyhow::ensure!(!self.server.host.is_empty(), "server.host must not be empty");
        anyhow::ensure!(self.server.port > 0, "server.port must be non-zero");
        anyhow::ensure!(
            self.server.request_timeout_ms > 0,
            "server.request_timeout_ms must be > 0"
        );
        self.auth.validate()?;
        self.chain.validate()?;
        anyhow::ensure!(!self.graphql.endpoint.is_empty(), "graphql.endpoint must not be empty");
        anyhow::ensure!(
            self.graphql.request_timeout_ms > 0,
            "graphql.request_timeout_ms must be > 0",
        );
        // The HTTP request_timeout hoop must outlast each chain
        // sender timeout; otherwise an in-flight chain call would be
        // dropped while still running on chain — the client would see
        // a 504 and lose the request id for an op that eventually
        // lands.
        anyhow::ensure!(
            self.server.request_timeout_ms > self.chain.place_order_timeout_ms,
            "server.request_timeout_ms ({}) must exceed chain.place_order_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.chain.place_order_timeout_ms,
        );
        anyhow::ensure!(
            self.server.request_timeout_ms > self.chain.cancel_order_timeout_ms,
            "server.request_timeout_ms ({}) must exceed chain.cancel_order_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.chain.cancel_order_timeout_ms,
        );
        anyhow::ensure!(
            self.server.request_timeout_ms > self.chain.place_batch_timeout_ms,
            "server.request_timeout_ms ({}) must exceed chain.place_batch_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.chain.place_batch_timeout_ms,
        );
        anyhow::ensure!(
            self.server.request_timeout_ms > self.chain.cancel_batch_timeout_ms,
            "server.request_timeout_ms ({}) must exceed chain.cancel_batch_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.chain.cancel_batch_timeout_ms,
        );
        anyhow::ensure!(
            self.server.request_timeout_ms > self.chain.split_full_set_timeout_ms,
            "server.request_timeout_ms ({}) must exceed chain.split_full_set_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.chain.split_full_set_timeout_ms,
        );
        // The HTTP request_timeout hoop must also outlast each GraphQL
        // read — the PN BOC fetch for /account and /account/balances runs
        // inside the request budget. A graphql.request_timeout_ms that
        // equals or exceeds server.request_timeout_ms would cause the HTTP
        // hoop to fire while the GraphQL client is still waiting, surfacing
        // as a 504 with no useful breadcrumb instead of a clean 503 from the
        // chain-read error path.
        anyhow::ensure!(
            self.server.request_timeout_ms > self.graphql.request_timeout_ms,
            "server.request_timeout_ms ({}) must exceed graphql.request_timeout_ms ({})",
            self.server.request_timeout_ms,
            self.graphql.request_timeout_ms,
        );
        Ok(())
    }
}

impl ChainSection {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.gateway_endpoint.is_empty(),
            "chain.gateway_endpoint must not be empty"
        );
        anyhow::ensure!(
            self.place_order_timeout_ms > 0,
            "chain.place_order_timeout_ms must be > 0"
        );
        anyhow::ensure!(
            self.cancel_order_timeout_ms > 0,
            "chain.cancel_order_timeout_ms must be > 0"
        );
        anyhow::ensure!(
            self.place_batch_timeout_ms > 0,
            "chain.place_batch_timeout_ms must be > 0"
        );
        anyhow::ensure!(
            self.cancel_batch_timeout_ms > 0,
            "chain.cancel_batch_timeout_ms must be > 0"
        );
        anyhow::ensure!(
            self.split_full_set_timeout_ms > 0,
            "chain.split_full_set_timeout_ms must be > 0"
        );
        anyhow::ensure!(self.max_batch_size > 0, "chain.max_batch_size must be > 0");
        Ok(())
    }
}

impl AuthSection {
    fn validate(&self) -> anyhow::Result<()> {
        crate::crypto::Kek::from_hex(&self.kek_hex)
            .map_err(|err| anyhow::anyhow!("auth.kek_hex is not a valid KEK: {err}"))?;
        anyhow::ensure!(self.default_recv_window_ms > 0, "auth.default_recv_window_ms must be > 0");
        anyhow::ensure!(self.max_recv_window_ms > 0, "auth.max_recv_window_ms must be > 0");
        anyhow::ensure!(
            self.max_recv_window_ms <= MAX_RECV_WINDOW_MS,
            "auth.max_recv_window_ms must be <= {MAX_RECV_WINDOW_MS} (spec maximum)"
        );
        anyhow::ensure!(
            self.default_recv_window_ms <= self.max_recv_window_ms,
            "auth.default_recv_window_ms ({}) must be <= auth.max_recv_window_ms ({})",
            self.default_recv_window_ms,
            self.max_recv_window_ms,
        );
        anyhow::ensure!(
            !self.seed_accounts || self.seed_accounts_path.is_some(),
            "auth.seed_accounts=true requires auth.seed_accounts_path",
        );
        Ok(())
    }
}

/// `raw_events.event_type` values whose row counts back OTLP metric counters
/// in the indexer's metrics-refresh loop (`services/indexer/src/metrics_refresh.rs`).
/// Dropping any of these at ingest would silently undercount
/// `orders_created_event_cnt` / `order_partially_filled_event_cnt`, so
/// [`IndexerConfig::validate`] refuses a config that lists one.
///
/// This set must stay a superset of the types `metrics_refresh` actually
/// tracks; a unit test there (`metric_critical_covers_tracked_types`) fails
/// loudly if a new tracked type is added without updating this list.
pub const METRIC_CRITICAL_EVENT_TYPES: [&str; 2] =
    ["OrderBook.OrderPlaced", "OrderBook.PartialFill"];

/// The only event types `indexer.ignored_event_types` may contain: projector
/// no-ops (`projectors::project_event` -> `ProjectionOutcome::Applied` without
/// touching a read-model table) that are NOT metric-critical. This is the
/// `OrderBook` observability-only arm of `projectors::project_event` minus
/// `OrderBook.PartialFill` (a no-op there, but it backs an OTLP counter — see
/// [`METRIC_CRITICAL_EVENT_TYPES`]). `validate` rejects any configured type
/// outside this set, so a typo or a state-changing type fails loudly at startup
/// instead of being a silent no-op (the ingest filter only fires post-decode,
/// so an unmatched name simply never drops anything). Keep in sync with the
/// no-op arm in `projectors::project_event`.
pub const IGNORABLE_EVENT_TYPES: [&str; 4] = [
    "OrderBook.FullyFilled",
    "OrderBook.Queued",
    "OrderBook.Rejected",
    "OrderBook.CallbackBounced",
];

impl IndexerConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let cfg: Self = load_yaml(path)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        self.common.validate()?;
        anyhow::ensure!(!self.graphql.endpoint.is_empty(), "graphql.endpoint must not be empty");
        anyhow::ensure!(self.graphql.page_size > 0, "graphql.page_size must be positive");
        anyhow::ensure!(
            self.graphql.request_timeout_ms > 0,
            "graphql.request_timeout_ms must be > 0"
        );
        let i = &self.indexer;
        anyhow::ensure!(i.polling_interval_ms > 0, "indexer.polling_interval_ms must be > 0");
        anyhow::ensure!(
            i.depth_refresh_interval_ms > 0,
            "indexer.depth_refresh_interval_ms must be > 0"
        );
        anyhow::ensure!(
            i.reconciliation_interval_ms > 0,
            "indexer.reconciliation_interval_ms must be > 0"
        );
        anyhow::ensure!(
            i.reprojection_interval_ms > 0,
            "indexer.reprojection_interval_ms must be > 0"
        );
        anyhow::ensure!(
            i.reprojection_batch_size > 0,
            "indexer.reprojection_batch_size must be > 0"
        );
        anyhow::ensure!(
            i.oracle_event_list_reconciliation_interval_ms > 0,
            "indexer.oracle_event_list_reconciliation_interval_ms must be > 0"
        );
        // Every configured ignored type must be a known droppable no-op
        // (`IGNORABLE_EVENT_TYPES`). This rejects, at startup: metric-critical
        // types (dropping them silently undercounts the OTLP counters their
        // raw_events back), state-changing types (dropping them corrupts the
        // read model), and typos — which would otherwise be a silent no-op,
        // since the ingest filter only fires post-decode and an unmatched name
        // never drops anything. Metric-critical types are checked first so they
        // get the more specific message.
        for t in &i.ignored_event_types {
            let t = t.as_str();
            anyhow::ensure!(
                !METRIC_CRITICAL_EVENT_TYPES.contains(&t),
                "indexer.ignored_event_types must not contain metric-critical type {t}"
            );
            anyhow::ensure!(
                IGNORABLE_EVENT_TYPES.contains(&t),
                "indexer.ignored_event_types contains {t}, which is not a droppable no-op \
                 event type (allowed: {:?}); check for a typo or a state-changing type",
                IGNORABLE_EVENT_TYPES
            );
        }
        Ok(())
    }
}

pub trait ReloadableConfig: Sized + Send + Sync + 'static {
    fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self>;
}

impl ReloadableConfig for ApiConfig {
    fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::load_from_path(path)
    }
}

impl ReloadableConfig for IndexerConfig {
    fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::load_from_path(path)
    }
}

impl CommonSection {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.app.env.is_empty(), "app.env must not be empty");
        anyhow::ensure!(!self.app.log_level.is_empty(), "app.log_level must not be empty");
        anyhow::ensure!(!self.database.url.is_empty(), "database.url must not be empty");
        anyhow::ensure!(self.database.max_connections > 0, "database.max_connections must be > 0");
        anyhow::ensure!(
            self.database.max_connections >= self.database.min_connections,
            "database.max_connections must be >= database.min_connections"
        );
        anyhow::ensure!(
            self.database.connect_timeout_ms > 0,
            "database.connect_timeout_ms must be > 0"
        );
        Ok(())
    }
}

fn load_yaml<T>(path: impl AsRef<Path>) -> anyhow::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let raw = fs::read_to_string(path)?;
    Ok(serde_yaml::from_str(&raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMON: &str = r#"
app:
  env: local
  log_level: info
database:
  url: postgres://postgres:postgres@localhost:5432/dodex
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000
"#;

    #[test]
    fn api_config_requires_graphql_section() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
"
        );
        let err = serde_yaml::from_str::<ApiConfig>(&raw).unwrap_err();
        assert!(err.to_string().contains("graphql"), "got: {err}");
    }

    #[test]
    fn indexer_config_does_not_require_server_section() {
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
"
        );

        let cfg: IndexerConfig = serde_yaml::from_str(&raw).unwrap();

        assert_eq!(cfg.graphql.page_size, 100);
        assert_eq!(cfg.indexer.polling_interval_ms, 3000);
    }

    #[test]
    fn api_config_accepts_graphql_section() {
        // The API owns its own `graphql` section for on-demand PN BOC reads.
        // page_size is optional at the API tier (defaults to 100) and may be
        // omitted — this verifies the default kicks in.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 35000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: https://graphql.example.invalid
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        assert_eq!(cfg.graphql.endpoint, "https://graphql.example.invalid");
        assert_eq!(cfg.graphql.page_size, 100);
        cfg.validate().expect("validate");
    }

    #[test]
    fn api_validate_rejects_empty_graphql_endpoint() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 35000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: \"\"
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("graphql.endpoint"), "got: {err}");
    }

    #[test]
    fn indexer_config_parses_ignored_addresses() {
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
  ignored_addresses:
    - \"0:1111111111111111111111111111111111111111111111111111111111111111\"
    - \"0:11111111111111111111111111111111111111111111111111111111111111ff\"
"
        );

        let cfg: IndexerConfig = serde_yaml::from_str(&raw).unwrap();

        assert_eq!(cfg.indexer.ignored_addresses.len(), 2);
        assert!(cfg.indexer.ignored_addresses.iter().any(|a| a.ends_with("11ff")));
    }

    #[test]
    fn indexer_config_defaults_ignored_addresses_to_empty() {
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
"
        );

        let cfg: IndexerConfig = serde_yaml::from_str(&raw).unwrap();

        assert!(cfg.indexer.ignored_addresses.is_empty());
    }

    #[test]
    fn indexer_config_rejects_server_section() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 18080
  request_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
"
        );

        let err = serde_yaml::from_str::<IndexerConfig>(&raw).unwrap_err();

        assert!(err.to_string().contains("server"));
    }

    fn api_config_with(database_url: &str, request_timeout_ms: u64) -> ApiConfig {
        let raw = format!(
            r#"
app:
  env: local
  log_level: info
database:
  url: "{database_url}"
  max_connections: 10
  min_connections: 1
  connect_timeout_ms: 3000
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: {request_timeout_ms}
auth:
  kek_hex: "{TEST_KEK_HEX}"
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"#
        );
        serde_yaml::from_str(&raw).expect("parse")
    }

    #[test]
    fn api_validate_rejects_empty_database_url() {
        let cfg = api_config_with("", 5000);
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("database.url"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_zero_request_timeout() {
        let cfg = api_config_with("postgres://x", 0);
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("server.request_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_chain_timeout() {
        // `chain.place_order_timeout_ms` defaults to 30 000. Without
        // a strict ordering, the HTTP timeout could fire while the
        // chain submission is still in flight — the client would see
        // 504 and lose the `clientOrderId` for an order that eventually
        // lands.
        let cfg = api_config_with("postgres://x", 30_000);
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("place_order_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn api_validate_accepts_request_timeout_just_above_chain_timeout() {
        // Boundary: 1 ms of slack is the minimum the strict check
        // requires; production yaml ships ~5 s of slack.
        let cfg = api_config_with("postgres://x", 30_001);
        cfg.validate().expect("1 ms above chain timeout must validate");
    }

    #[test]
    fn indexer_validate_rejects_zero_intervals() {
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 0
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
"
        );
        let cfg: IndexerConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("polling_interval_ms"), "got: {err}");
    }

    /// Any 64-char hex; the bytes are irrelevant for config-shape tests.
    const TEST_KEK_HEX: &str = "abababababababababababababababababababababababababababababababab";

    fn valid_auth_section(default_ms: u64, max_ms: u64) -> AuthSection {
        AuthSection {
            kek_hex: TEST_KEK_HEX.to_string(),
            default_recv_window_ms: default_ms,
            max_recv_window_ms: max_ms,
            seed_accounts: false,
            seed_accounts_path: None,
        }
    }

    #[test]
    fn api_config_rejects_yaml_without_auth_section() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
"
        );
        let err = serde_yaml::from_str::<ApiConfig>(&raw).unwrap_err();
        assert!(err.to_string().contains("auth"), "got: {err}");
    }

    #[test]
    fn api_config_parses_explicit_auth_section() {
        // request_timeout_ms must exceed chain.place_order_timeout_ms
        // (default 30_000) per the ApiConfig invariant.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 35000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
  default_recv_window_ms: 2000
  max_recv_window_ms: 30000
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(cfg.auth.default_recv_window_ms, 2_000);
        assert_eq!(cfg.auth.max_recv_window_ms, 30_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn api_config_defaults_recv_window_when_only_kek_given() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 35000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(cfg.auth.default_recv_window_ms, 5_000);
        assert_eq!(cfg.auth.max_recv_window_ms, 60_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn auth_validate_rejects_malformed_kek() {
        let s = AuthSection {
            kek_hex: "not hex".to_string(),
            default_recv_window_ms: 5_000,
            max_recv_window_ms: 60_000,
            seed_accounts: false,
            seed_accounts_path: None,
        };
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("kek_hex"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_wrong_length_kek() {
        let s = AuthSection {
            // 30 bytes, not 32.
            kek_hex: "ab".repeat(30),
            default_recv_window_ms: 5_000,
            max_recv_window_ms: 60_000,
            seed_accounts: false,
            seed_accounts_path: None,
        };
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("kek_hex"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_seed_accounts_without_path() {
        // A misconfig must fail at config load, not later when the seeder
        // aborts after migrations have already run.
        let mut s = valid_auth_section(5_000, 60_000);
        s.seed_accounts = true;
        s.seed_accounts_path = None;
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("seed_accounts_path"), "got: {err}");
    }

    #[test]
    fn auth_validate_accepts_seed_accounts_with_path() {
        let mut s = valid_auth_section(5_000, 60_000);
        s.seed_accounts = true;
        s.seed_accounts_path = Some("/etc/dodex/seed_notes.json".to_string());
        s.validate().unwrap();
    }

    #[test]
    fn auth_validate_rejects_max_above_spec_ceiling() {
        let s = valid_auth_section(5_000, 120_000);
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("60000"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_default_above_max() {
        let s = valid_auth_section(30_000, 10_000);
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("must be <="), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_zero_default() {
        let s = valid_auth_section(0, 60_000);
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("default_recv_window_ms"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_zero_max() {
        // Use a non-zero default so the validator does not short-circuit
        // on the default check before reaching the max check.
        let s = valid_auth_section(5_000, 0);
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("max_recv_window_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_empty_chain_endpoint() {
        // The handler hits `chain.gateway_endpoint` on every order
        // submission. An empty value silently means "POST /api/v1/order
        // 500s on every request" — the validator MUST catch this at
        // boot rather than at the first trade.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: \"\"
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("chain.gateway_endpoint"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_zero_place_order_timeout() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("place_order_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_config_parses_chain_section_with_explicit_timeout() {
        // request_timeout_ms must exceed every chain timeout.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 20000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 15000
  cancel_order_timeout_ms: 15000
  place_batch_timeout_ms: 15000
  cancel_batch_timeout_ms: 15000
  split_full_set_timeout_ms: 15000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        assert_eq!(cfg.chain.gateway_endpoint, "shellnet.ackinacki.org");
        assert_eq!(cfg.chain.place_order_timeout_ms, 15_000);
        assert_eq!(cfg.chain.cancel_order_timeout_ms, 15_000);
        assert_eq!(cfg.chain.place_batch_timeout_ms, 15_000);
        assert_eq!(cfg.chain.cancel_batch_timeout_ms, 15_000);
        assert_eq!(cfg.chain.split_full_set_timeout_ms, 15_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn api_config_defaults_place_order_timeout_when_omitted() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        assert_eq!(cfg.chain.place_order_timeout_ms, 30_000);
        assert_eq!(cfg.chain.cancel_order_timeout_ms, 30_000);
        assert_eq!(cfg.chain.place_batch_timeout_ms, 30_000);
        assert_eq!(cfg.chain.cancel_batch_timeout_ms, 30_000);
        assert_eq!(cfg.chain.split_full_set_timeout_ms, 30_000);
        assert_eq!(cfg.chain.max_batch_size, 10);
    }

    #[test]
    fn api_validate_rejects_zero_place_batch_timeout() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 4000
  cancel_order_timeout_ms: 4000
  place_batch_timeout_ms: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("place_batch_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_zero_max_batch_size() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 60000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  max_batch_size: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("max_batch_size"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_batch_timeout() {
        // The HTTP request_timeout hoop must outlast every chain
        // timeout — POST /batchOrders is no exception. Otherwise an
        // in-flight placeBatch gets dropped while still running on
        // chain and the client loses the ids for orders that
        // eventually land.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 1000
  place_batch_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("place_batch_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn api_validate_rejects_zero_cancel_order_timeout() {
        // Same invariant as place_order_timeout — a zero budget would
        // collapse every DELETE /order to an immediate RequestTimeout.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 4000
  cancel_order_timeout_ms: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("cancel_order_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_cancel_timeout() {
        // The HTTP request_timeout must outlast cancel_order_timeout
        // for the same reason it must outlast place_order_timeout:
        // otherwise an in-flight cancelOrder gets dropped while still
        // running on chain — the client would see 504 and lose the
        // orderId of an op that eventually lands.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("cancel_order_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn api_validate_rejects_zero_cancel_batch_timeout() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 4000
  cancel_order_timeout_ms: 4000
  place_batch_timeout_ms: 4000
  cancel_batch_timeout_ms: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 4000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("cancel_batch_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_cancel_batch_timeout() {
        // Same invariant as the other chain timeouts — request_timeout
        // must outlast cancel_batch_timeout, else an in-flight
        // cancelBatch gets dropped while still running on chain and the
        // client loses the orderIds of an op that eventually lands.
        // `5000 == 5000` hits the `==` boundary specifically (not just
        // `<`), so a future relaxation from `>` to `>=` in the validate
        // arm would fail here, not slip through.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 1000
  place_batch_timeout_ms: 1000
  cancel_batch_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 4000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("cancel_batch_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn api_validate_accepts_request_timeout_just_above_cancel_batch_timeout() {
        // Boundary peer of
        // `api_validate_rejects_request_timeout_not_exceeding_cancel_batch_timeout`:
        // `5001 > 5000` must pass. Without this, a regression that
        // tightened the comparison to `>=` would silently break every
        // deployment that ran with equal timeouts during a config
        // migration.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5001
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 1000
  place_batch_timeout_ms: 1000
  cancel_batch_timeout_ms: 5000
  split_full_set_timeout_ms: 1000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 4000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        cfg.validate().expect("1 ms above cancel_batch_timeout must validate");
    }

    #[test]
    fn api_validate_rejects_zero_split_full_set_timeout() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 4000
  cancel_order_timeout_ms: 4000
  place_batch_timeout_ms: 4000
  cancel_batch_timeout_ms: 4000
  split_full_set_timeout_ms: 0
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 4000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("split_full_set_timeout_ms"), "got: {err}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_split_full_set_timeout() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 1000
  place_batch_timeout_ms: 1000
  cancel_batch_timeout_ms: 1000
  split_full_set_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 4000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("split_full_set_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn api_validate_rejects_request_timeout_not_exceeding_graphql_timeout() {
        // server.request_timeout_ms must exceed graphql.request_timeout_ms
        // so the HTTP hoop does not fire while the PN BOC fetch for
        // /account and /account/balances is still in flight.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 35000
auth:
  kek_hex: \"{TEST_KEK_HEX}\"
chain:
  gateway_endpoint: shellnet.ackinacki.org
  place_order_timeout_ms: 1000
  cancel_order_timeout_ms: 1000
  place_batch_timeout_ms: 1000
  cancel_batch_timeout_ms: 1000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 35000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("request_timeout_ms"), "got: {msg}");
        assert!(msg.contains("graphql.request_timeout_ms"), "got: {msg}");
    }

    #[test]
    fn indexer_validate_rejects_empty_graphql_endpoint() {
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: \"\"
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
"
        );
        let cfg: IndexerConfig = serde_yaml::from_str(&raw).expect("parse");
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("graphql.endpoint"), "got: {err}");
    }

    fn indexer_cfg_with_ignored(types: &[&str]) -> IndexerConfig {
        let list: String = types.iter().map(|t| format!("    - \"{t}\"\n")).collect();
        let raw = format!(
            "{COMMON}
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
  ignored_event_types:
{list}"
        );
        serde_yaml::from_str(&raw).expect("parse")
    }

    #[test]
    fn indexer_validate_rejects_metric_critical_ignored_event_type() {
        // Both metric-critical types — not just PartialFill — must be rejected
        // with the metric-critical message; dropping either undercounts an
        // OTLP counter its raw_events back.
        for t in METRIC_CRITICAL_EVENT_TYPES {
            let err = indexer_cfg_with_ignored(&[t]).validate().unwrap_err();
            assert!(err.to_string().contains("metric-critical"), "{t}: {err}");
        }
    }

    #[test]
    fn indexer_validate_rejects_unknown_or_state_changing_ignored_event_type() {
        // A typo and a state-changing type both fail loudly. The latter would,
        // before the allow-list guard, pass validation and corrupt the read
        // model; the former is the silent no-op this guard exists to surface.
        for t in ["OrderBook.Quued", "OrderBook.OrderFilled"] {
            let err = indexer_cfg_with_ignored(&[t]).validate().unwrap_err();
            assert!(err.to_string().contains("not a droppable no-op"), "{t}: {err}");
        }
    }

    #[test]
    fn indexer_validate_accepts_every_ignorable_event_type() {
        let cfg = indexer_cfg_with_ignored(&IGNORABLE_EVENT_TYPES);
        cfg.validate().expect("all ignorable no-op types must validate");
        assert_eq!(cfg.indexer.ignored_event_types.len(), IGNORABLE_EVENT_TYPES.len());
    }

    /// `IGNORABLE_EVENT_TYPES` is, by construction, the projector no-op arm
    /// minus the metric-critical types. If the two ever overlap, a type would
    /// be both shed-able and metric-backing — pin the disjointness.
    #[test]
    fn ignorable_and_metric_critical_are_disjoint() {
        for t in IGNORABLE_EVENT_TYPES {
            assert!(
                !METRIC_CRITICAL_EVENT_TYPES.contains(&t),
                "{t} is both ignorable and metric-critical"
            );
        }
    }
}
