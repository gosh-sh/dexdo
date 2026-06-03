// 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
//

//! Shared tracing setup for the dodex services.
//!
//! Every service logs to stdout, filtered by `RUST_LOG` (default `info`).
//! When the `LOG_DIR` environment variable is set and non-empty, each service
//! ALSO writes human-readable, daily-rotated files named `<service>.log.<date>`
//! into that directory, keeping at most `LOG_MAX_FILES` (default 14) of them.
//!
//! Lives in its own crate — free of the heavy `dodex-infrastructure`
//! dependency graph — so a standalone workspace can reuse it by path,
//! exactly like `dodex-chain`.

use std::env;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Daily log files retained before the oldest is pruned, when `LOG_MAX_FILES`
/// is unset or unparseable.
const DEFAULT_MAX_FILES: usize = 14;

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn max_files() -> usize {
    env::var("LOG_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_FILES)
}

/// Build the daily rolling-file appender for `service` under `dir`, creating
/// the directory if needed. Errors out (rather than panicking) so the caller
/// can fall back to stdout-only.
fn file_appender(dir: &str, service: &str) -> anyhow::Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(format!("{service}.log"))
        .max_log_files(max_files())
        .build(dir)?;
    Ok(appender)
}

/// Install the global tracing subscriber for `service`.
///
/// Always logs to stdout. If `LOG_DIR` is set and non-empty, also writes
/// daily-rotated `<service>.log.<date>` files there. On a log-dir error, warns
/// loudly to the (already-installed) stdout logger and continues stdout-only.
///
/// Returns the appender guard(s). The caller MUST keep them alive for the
/// lifetime of the process (`let _guards = dodex_logging::init("api");`) — drop
/// them and the background file writer stops flushing.
#[must_use]
pub fn init(service: &str) -> Vec<WorkerGuard> {
    let stdout_layer = fmt::layer().with_writer(std::io::stdout);

    let log_dir = env::var("LOG_DIR").unwrap_or_default();
    if log_dir.is_empty() {
        tracing_subscriber::registry().with(env_filter()).with(stdout_layer).init();
        return Vec::new();
    }

    match file_appender(&log_dir, service) {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = fmt::layer().with_ansi(false).with_writer(writer);
            tracing_subscriber::registry()
                .with(env_filter())
                .with(stdout_layer)
                .with(file_layer)
                .init();
            vec![guard]
        }
        Err(err) => {
            tracing_subscriber::registry().with(env_filter()).with(stdout_layer).init();
            tracing::warn!(
                log_dir = %log_dir,
                error = %err,
                "LOG_DIR set but file logging could not be initialised; continuing with stdout only"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_rotated_file_when_dir_is_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path");

        let appender = file_appender(path, "testsvc").expect("appender builds");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let subscriber =
            tracing_subscriber::registry().with(fmt::layer().with_ansi(false).with_writer(writer));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("hello-from-test");
        });

        // Dropping the guard flushes and joins the background writer thread.
        drop(guard);

        let log_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read tempdir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("testsvc.log"))
            .collect();
        assert_eq!(log_files.len(), 1, "exactly one rotated file is created");

        let contents = std::fs::read_to_string(log_files[0].path()).expect("read log file");
        assert!(contents.contains("hello-from-test"), "log line written, got: {contents}");
    }
}
