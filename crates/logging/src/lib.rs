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

/// Tracing target for high-volume, low-value diagnostic lines (e.g. the
/// projector's "no handler for event type" warns). Routed to a dedicated
/// `<service>.noise.log` file when `LOG_DIR` is set, and kept out of stdout
/// and the main log file.
pub const EVENT_NOISE_TARGET: &str = "dodex::event_noise";

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn max_files() -> usize {
    env::var("LOG_MAX_FILES").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_FILES)
}

/// Build the daily rolling-file appender with the given filename `prefix`
/// under `dir`, creating the directory if needed. Errors out (rather than
/// panicking) so the caller can fall back to stdout-only.
fn file_appender(dir: &str, prefix: &str) -> anyhow::Result<RollingFileAppender> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix.to_string())
        .max_log_files(max_files())
        .build(dir)?;
    Ok(appender)
}

/// Install the global tracing subscriber for `service`.
///
/// Always logs to stdout (excluding noise-target lines when `LOG_DIR` is set).
/// If `LOG_DIR` is set and non-empty, also writes daily-rotated files:
/// `<service>.log.<date>` for normal lines and `<service>.noise.log.<date>`
/// for [`EVENT_NOISE_TARGET`] lines (kept out of stdout and the main log).
/// When `LOG_DIR` is absent (dev), everything including noise goes to stdout.
/// On a log-dir error, warns loudly and continues stdout-only.
///
/// Returns the appender guard(s). The caller MUST keep them alive for the
/// lifetime of the process (`let _guards = dodex_logging::init("api");`) — drop
/// them and the background file writer stops flushing.
#[must_use]
pub fn init(service: &str) -> Vec<WorkerGuard> {
    use tracing_subscriber::filter::FilterFn;
    use tracing_subscriber::Layer;

    let log_dir = env::var("LOG_DIR").unwrap_or_default();
    if log_dir.is_empty() {
        // No LOG_DIR: a single stdout sink carries everything, including the
        // noise-target lines — nothing is dropped in dev.
        let stdout_layer = fmt::layer().with_writer(std::io::stdout);
        tracing_subscriber::registry().with(env_filter()).with(stdout_layer).init();
        return Vec::new();
    }

    let main_appender = file_appender(&log_dir, &format!("{service}.log"));
    let noise_appender = file_appender(&log_dir, &format!("{service}.noise.log"));
    match (main_appender, noise_appender) {
        (Ok(main), Ok(noise)) => {
            let (main_writer, main_guard) = tracing_appender::non_blocking(main);
            let (noise_writer, noise_guard) = tracing_appender::non_blocking(noise);

            // Noise target -> dedicated file only; everything else -> stdout +
            // main file.
            let stdout_layer = fmt::layer()
                .with_writer(std::io::stdout)
                .with_filter(FilterFn::new(|m: &tracing::Metadata| {
                    m.target() != EVENT_NOISE_TARGET
                }));
            let main_layer = fmt::layer()
                .with_ansi(false)
                .with_writer(main_writer)
                .with_filter(FilterFn::new(|m: &tracing::Metadata| {
                    m.target() != EVENT_NOISE_TARGET
                }));
            let noise_layer = fmt::layer()
                .with_ansi(false)
                .with_writer(noise_writer)
                .with_filter(FilterFn::new(|m: &tracing::Metadata| {
                    m.target() == EVENT_NOISE_TARGET
                }));

            // `env_filter()` is applied at the registry level, i.e. BEFORE the
            // per-layer target filters. The default is `info`, so the WARN-level
            // noise lines reach the noise layer fine; but an aggressive
            // `RUST_LOG` (e.g. `error`, or a directive disabling this target)
            // suppresses them globally before routing — keep that in mind when
            // tuning `RUST_LOG`.
            tracing_subscriber::registry()
                .with(env_filter())
                .with(stdout_layer)
                .with(main_layer)
                .with(noise_layer)
                .init();
            vec![main_guard, noise_guard]
        }
        (main_res, noise_res) => {
            let stdout_layer = fmt::layer().with_writer(std::io::stdout);
            tracing_subscriber::registry().with(env_filter()).with(stdout_layer).init();
            let err = main_res.err().or(noise_res.err());
            tracing::warn!(
                log_dir = %log_dir,
                error = ?err,
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

        let appender = file_appender(path, "testsvc.log").expect("appender builds");
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

    #[test]
    fn noise_target_routes_to_separate_file() {
        use tracing_subscriber::filter::FilterFn;
        use tracing_subscriber::Layer;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 path");

        let main = file_appender(path, "rt.log").expect("main appender");
        let noise = file_appender(path, "rt.noise.log").expect("noise appender");
        let (mw, mg) = tracing_appender::non_blocking(main);
        let (nw, ng) = tracing_appender::non_blocking(noise);

        let subscriber = tracing_subscriber::registry()
            .with(
                fmt::layer().with_ansi(false).with_writer(mw).with_filter(FilterFn::new(
                    |m: &tracing::Metadata| m.target() != EVENT_NOISE_TARGET,
                )),
            )
            .with(
                fmt::layer().with_ansi(false).with_writer(nw).with_filter(FilterFn::new(
                    |m: &tracing::Metadata| m.target() == EVENT_NOISE_TARGET,
                )),
            );

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: EVENT_NOISE_TARGET, "noise-line");
            tracing::info!(target: "rt::normal", "normal-line");
        });
        drop(mg);
        drop(ng);

        let read = |prefix: &str| -> String {
            let entry = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .find(|e| e.file_name().to_string_lossy().starts_with(prefix))
                .expect("file exists");
            std::fs::read_to_string(entry.path()).unwrap()
        };
        let main_contents = read("rt.log");
        let noise_contents = read("rt.noise.log");

        assert!(main_contents.contains("normal-line"), "main has normal");
        assert!(!main_contents.contains("noise-line"), "main excludes noise");
        assert!(noise_contents.contains("noise-line"), "noise has noise");
        assert!(!noise_contents.contains("normal-line"), "noise excludes normal");
    }
}
