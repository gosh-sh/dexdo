// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::env;
use std::sync::Arc;
use std::time::Duration;

use dodex_infrastructure::config::AppConfig;
use dodex_infrastructure::signal::run_config_reload_loop;
use tokio::sync::RwLock;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = env::var("APP_CONFIG").unwrap_or_else(|_| "config/indexer.local.yaml".to_string());
    let config = AppConfig::load_from_path(&config_path)?;
    let config_state = Arc::new(RwLock::new(config));

    tokio::spawn(run_config_reload_loop(config_path.clone(), Arc::clone(&config_state), "indexer"));

    loop {
        let cfg = config_state.read().await.clone();
        info!(
            graphql_endpoint = %cfg.graphql.endpoint,
            page_size = cfg.graphql.page_size,
            stub_mode = cfg.features.stub_mode,
            "indexer tick"
        );
        tokio::time::sleep(Duration::from_millis(cfg.indexer.polling_interval_ms)).await;
    }
}
