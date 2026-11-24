//! Logging setup for Athrix binaries (the desktop app, the daemon and the CLI).

use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::{EnvFilter, fmt};

/// A log file larger than this at start-up is renamed to `<name>.old.log` (replacing the
/// previous one), so the logs never take more than about twice this much disk.
pub const MAX_LOG_BYTES: u64 = 5 << 20;

fn filter(default_filter: &str) -> EnvFilter {
    EnvFilter::try_from_env("ATHRIX_LOG").unwrap_or_else(|_| EnvFilter::new(default_filter))
}

/// Installs a global `tracing` subscriber that writes to stderr.
///
/// The filter comes from the `ATHRIX_LOG` environment variable (for example
/// `ATHRIX_LOG=athrix_fs_index=debug`), or `default_filter` when it is not set.
/// Calling it more than once is harmless: later calls do nothing.
pub fn init(default_filter: &str) {
    let _ = fmt()
        .with_env_filter(filter(default_filter))
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Like [`init`], also appending to the file `log` (its folder is created). A file over
/// [`MAX_LOG_BYTES`] is first moved to `<stem>.old.log`. If the file cannot be opened, logs go
/// to stderr only. A release build of a GUI app has no console, so the file is the only place
/// its logs can be read.
pub fn init_with_file(default_filter: &str, log: &Path) {
    let Some(file) = open_log(log) else {
        init(default_filter);
        return;
    };
    let _ = fmt()
        .with_env_filter(filter(default_filter))
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr.and(Mutex::new(file)))
        .try_init();
}

fn open_log(log: &Path) -> Option<File> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).ok()?;
    }
    if fs::metadata(log).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let old = log.with_extension("old.log");
        let _ = fs::rename(log, old);
    }
    OpenOptions::new().create(true).append(true).open(log).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_log_is_rotated_and_a_new_one_opened() {
        let dir = std::env::temp_dir().join(format!("athrix-log-test-{}", std::process::id()));
        let log = dir.join("logs").join("athrix.log");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        let file = open_log(&log).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert!(log.with_extension("old.log").exists());
        drop(file);
        fs::remove_dir_all(&dir).unwrap();
    }
}
