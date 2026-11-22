//! The call log, `<app_data>/logs/mcp.log`: one line per tool call with the tool, the query
//! or path, the number of results and the time taken. Never file contents or snippets.
//!
//! When the file passes [`MAX_LOG_BYTES`] it is renamed to `mcp.log.1` (replacing the older
//! one) and a new file starts, so at most about twice that is kept. Logging failures are
//! ignored: they must never break a tool call.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Folder of the log inside the app data folder.
pub const LOG_DIR: &str = "logs";
/// Log file name.
pub const LOG_FILE: &str = "mcp.log";
/// Size at which the log is rotated.
pub const MAX_LOG_BYTES: u64 = 512 * 1024;

/// Appends tool-call lines to a log file.
#[derive(Debug, Clone)]
pub struct CallLog {
    file: Option<PathBuf>,
}

impl CallLog {
    /// A log writing to `file`; `None` logs nothing.
    pub fn new(file: Option<PathBuf>) -> Self {
        CallLog { file }
    }

    /// The default log file of `data_dir`.
    pub fn default_file(data_dir: &Path) -> PathBuf {
        data_dir.join(LOG_DIR).join(LOG_FILE)
    }

    /// Records one call. `subject` is the query or path (cut to 200 characters, one line).
    pub fn record(&self, tool: &str, subject: &str, outcome: &str, millis: u128) {
        let Some(file) = &self.file else { return };
        let subject: String = subject
            .chars()
            .take(200)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let time = crate::tools::iso_time(SystemTime::now()).unwrap_or_default();
        let line = format!("{time} tool={tool} subject={subject:?} {outcome} ms={millis}\n");
        let _ = append(file, &line);
    }
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(file).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let mut old = file.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::remove_file(&old);
        fs::rename(file, &old)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_lines_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("logs").join(LOG_FILE);
        let log = CallLog::new(Some(file.clone()));
        log.record("search_files", "تقرير\nx", "results=3", 5);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("tool=search_files") && text.contains("results=3"));
        assert_eq!(text.lines().count(), 1);
        fs::write(&file, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        log.record("file_info", "/a", "results=1", 1);
        assert!(dir.path().join("logs").join("mcp.log.1").exists());
        assert!(fs::metadata(&file).unwrap().len() < 1000);
    }
}
