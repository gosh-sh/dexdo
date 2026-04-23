use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub app: AppSection,
    pub server: ServerSection,
    pub database: DatabaseSection,
    pub graphql: GraphqlSection,
    pub indexer: IndexerSection,
    pub features: FeatureSection,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSection {
    pub stub_mode: bool,
}

impl AppConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)?;
        let cfg: Self = serde_yaml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.app.env.is_empty(), "app.env must not be empty");
        anyhow::ensure!(self.server.port > 0, "server.port must be non-zero");
        anyhow::ensure!(
            self.database.max_connections >= self.database.min_connections,
            "database.max_connections must be >= database.min_connections"
        );
        anyhow::ensure!(self.graphql.page_size > 0, "graphql.page_size must be positive");
        Ok(())
    }
}
