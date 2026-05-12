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
    /// HMAC auth tuning. Optional in the YAML — the defaults match the
    /// public spec (5 s default window, 60 s ceiling). Operators can
    /// tighten either knob; loosening past the spec ceiling fails
    /// validation.
    #[serde(default)]
    pub auth: AuthSection,
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
    // TODO: not wired into the Salvo router yet — declared for forward
    // compatibility. When wiring it up, use a hoop that runs each handler
    // under `tokio::time::timeout` and responds 504 on elapsed.
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

/// HMAC validity window settings. Field names follow `api-spec.md`
/// semantics: `default_*` is what the middleware applies when a request
/// omits `recvWindow`; `max_*` is the clamp the middleware enforces on
/// any client-supplied `recvWindow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    #[serde(default = "default_recv_window_ms")]
    pub default_recv_window_ms: u64,
    #[serde(default = "default_max_recv_window_ms")]
    pub max_recv_window_ms: u64,
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            default_recv_window_ms: DEFAULT_RECV_WINDOW_MS,
            max_recv_window_ms: MAX_RECV_WINDOW_MS,
        }
    }
}

fn default_recv_window_ms() -> u64 {
    DEFAULT_RECV_WINDOW_MS
}

fn default_max_recv_window_ms() -> u64 {
    MAX_RECV_WINDOW_MS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphqlSection {
    pub endpoint: String,
    pub page_size: u32,
    pub request_timeout_ms: u64,
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
        Ok(())
    }
}

impl AuthSection {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.default_recv_window_ms > 0,
            "auth.default_recv_window_ms must be > 0"
        );
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
        Ok(())
    }
}

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
    fn api_config_does_not_require_indexer_sections() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
"
        );

        let cfg: ApiConfig = serde_yaml::from_str(&raw).unwrap();

        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.common.app.env, "local");
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
    fn api_config_rejects_indexer_sections() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
graphql:
  endpoint: https://graphql.example.invalid
  page_size: 100
  request_timeout_ms: 10000
"
        );

        let err = serde_yaml::from_str::<ApiConfig>(&raw).unwrap_err();

        assert!(err.to_string().contains("graphql"));
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

    #[test]
    fn api_config_defaults_auth_section_when_absent() {
        // The YAML may omit the `auth:` block entirely; the defaults
        // match the public spec (5 s / 60 s) so an upgraded operator
        // does not need to touch their config file.
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(cfg.auth.default_recv_window_ms, 5_000);
        assert_eq!(cfg.auth.max_recv_window_ms, 60_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn api_config_parses_explicit_auth_section() {
        let raw = format!(
            "{COMMON}
server:
  host: 0.0.0.0
  port: 8080
  request_timeout_ms: 5000
auth:
  default_recv_window_ms: 2000
  max_recv_window_ms: 30000
"
        );
        let cfg: ApiConfig = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(cfg.auth.default_recv_window_ms, 2_000);
        assert_eq!(cfg.auth.max_recv_window_ms, 30_000);
        cfg.validate().unwrap();
    }

    #[test]
    fn auth_validate_rejects_max_above_spec_ceiling() {
        let s = AuthSection { default_recv_window_ms: 5_000, max_recv_window_ms: 120_000 };
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("60000"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_default_above_max() {
        let s = AuthSection { default_recv_window_ms: 30_000, max_recv_window_ms: 10_000 };
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("must be <="), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_zero_default() {
        let s = AuthSection { default_recv_window_ms: 0, max_recv_window_ms: 60_000 };
        let err = s.validate().unwrap_err();
        assert!(err.to_string().contains("default_recv_window_ms"), "got: {err}");
    }

    #[test]
    fn auth_validate_rejects_zero_max() {
        let s = AuthSection { default_recv_window_ms: 0, max_recv_window_ms: 0 };
        let err = s.validate().unwrap_err();
        // `default == 0` is hit first by the order of checks, but the
        // important thing is that a zero-max config is rejected.
        assert!(err.to_string().contains("recv_window"), "got: {err}");
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
}
