// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::error;
use tracing::info;

use crate::config::ReloadableConfig;

pub async fn run_config_reload_loop<C>(
    config_path: String,
    config_state: Arc<RwLock<C>>,
    service_name: &'static str,
) where
    C: ReloadableConfig,
{
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
            match C::load_from_path(&config_path) {
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
