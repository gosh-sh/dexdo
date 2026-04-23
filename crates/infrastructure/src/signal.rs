use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::error;
use tracing::info;

use crate::config::AppConfig;

pub async fn run_config_reload_loop(
    config_path: String,
    config_state: Arc<RwLock<AppConfig>>,
    service_name: &'static str,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::signal;
        use tokio::signal::unix::SignalKind;

        let mut stream = match signal(SignalKind::user_defined1()) {
            Ok(stream) => stream,
            Err(err) => {
                error!(service = service_name, ?err, "failed to register SIGUSR1 handler");
                return;
            }
        };

        while stream.recv().await.is_some() {
            match AppConfig::load_from_path(&config_path) {
                Ok(new_config) => {
                    *config_state.write().await = new_config;
                    info!(service = service_name, path = %config_path, "config reloaded");
                }
                Err(err) => {
                    error!(service = service_name, path = %config_path, ?err, "config reload failed");
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (config_path, config_state, service_name);
    }
}
