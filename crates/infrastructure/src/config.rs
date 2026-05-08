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
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSection {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_ms: u64,
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
        anyhow::ensure!(self.server.port > 0, "server.port must be non-zero");
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
        anyhow::ensure!(self.graphql.page_size > 0, "graphql.page_size must be positive");
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
        anyhow::ensure!(
            self.database.max_connections >= self.database.min_connections,
            "database.max_connections must be >= database.min_connections"
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
}
