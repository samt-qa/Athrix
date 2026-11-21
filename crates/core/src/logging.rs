//! Logging setup for Athrix binaries (the desktop app, the daemon and the CLI).

use tracing_subscriber::{EnvFilter, fmt};

/// Installs a global `tracing` subscriber that writes to stderr.
///
/// The filter comes from the `ATHRIX_LOG` environment variable (for example
/// `ATHRIX_LOG=athrix_fs_index=debug`), or `default_filter` when it is not set.
/// Calling it more than once is harmless: later calls do nothing.
pub fn init(default_filter: &str) {
    let filter =
        EnvFilter::try_from_env("ATHRIX_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter));
    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}
