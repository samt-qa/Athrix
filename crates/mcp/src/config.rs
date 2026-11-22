//! The MCP settings file (`<app_data>/mcp.json`) and where the app data folder is.
//!
//! Layout (camelCase, every field optional):
//!
//! ```json
//! { "enabled": false, "allowedFolders": [], "allowContentRead": false, "maxResults": 50 }
//! ```
//!
//! The desktop app writes it (Settings → MCP); the server only reads it, again on every tool
//! call, so a change applies without restarting the AI client. A missing, unreadable or
//! corrupt file means the defaults: **disabled**.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The desktop app's Tauri identifier: the name of its data folder.
pub const APP_IDENTIFIER: &str = "com.samt.athrix";
/// File name of the settings inside the app data folder.
pub const CONFIG_FILE: &str = "mcp.json";
/// Default for [`McpConfig::max_results`].
pub const DEFAULT_MAX_RESULTS: u32 = 50;
/// Highest accepted [`McpConfig::max_results`].
pub const MAX_RESULTS_CAP: u32 = 200;
/// Most allowed folders kept.
pub const MAX_FOLDERS: usize = 64;

/// The MCP settings. See the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpConfig {
    /// The server answers tool calls only when this is true (and a folder is allowed).
    pub enabled: bool,
    /// Absolute folders the assistant may see; everything else is invisible to it.
    pub allowed_folders: Vec<String>,
    /// Whether `read_text` may return the text of files.
    pub allow_content_read: bool,
    /// Most results per search (1..=[`MAX_RESULTS_CAP`]).
    pub max_results: u32,
}

impl Default for McpConfig {
    fn default() -> Self {
        McpConfig {
            enabled: false,
            allowed_folders: Vec::new(),
            allow_content_read: false,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

impl McpConfig {
    /// The settings in `file`; the defaults (disabled) when it is missing or unusable.
    pub fn load(file: &Path) -> McpConfig {
        match fs::read(file) {
            Ok(bytes) => serde_json::from_slice::<McpConfig>(&bytes)
                .map(McpConfig::normalized)
                .unwrap_or_default(),
            Err(_) => McpConfig::default(),
        }
    }

    /// Writes the settings to `file` atomically (temporary file, then rename).
    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let json =
            serde_json::to_vec_pretty(&self.clone().normalized()).map_err(io::Error::other)?;
        let tmp = file.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, file).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// `max_results` clamped, blank and duplicate folders dropped (at most [`MAX_FOLDERS`]).
    pub fn normalized(mut self) -> McpConfig {
        self.max_results = self.max_results.clamp(1, MAX_RESULTS_CAP);
        let mut seen: Vec<String> = Vec::new();
        self.allowed_folders.retain(|f| {
            let key = crate::paths::key(f.trim());
            if f.trim().is_empty() || seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        });
        for f in &mut self.allowed_folders {
            *f = f.trim().to_owned();
        }
        self.allowed_folders.truncate(MAX_FOLDERS);
        self
    }

    /// Whether tools may run: enabled with at least one allowed folder.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.allowed_folders.is_empty()
    }
}

/// The desktop app's data folder, as Tauri's `app_data_dir` computes it:
/// `%APPDATA%\com.samt.athrix` on Windows, `~/Library/Application Support/com.samt.athrix` on
/// macOS, `$XDG_DATA_HOME/com.samt.athrix` (default `~/.local/share/…`) elsewhere.
pub fn default_data_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env_dir("APPDATA")?
    } else if cfg!(target_os = "macos") {
        env_dir("HOME")?.join("Library").join("Application Support")
    } else {
        env_dir("XDG_DATA_HOME").or_else(|| Some(env_dir("HOME")?.join(".local").join("share")))?
    };
    Some(base.join(APP_IDENTIFIER))
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_and_when_the_file_is_bad() {
        let d = McpConfig::default();
        assert!(!d.enabled && !d.allow_content_read && d.allowed_folders.is_empty());
        assert!(!d.is_active());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE);
        assert_eq!(McpConfig::load(&file), d);
        fs::write(&file, "{oops").unwrap();
        assert_eq!(McpConfig::load(&file), d);
        fs::write(&file, r#"{"enabled":"yes"}"#).unwrap();
        assert_eq!(McpConfig::load(&file), d);
    }

    #[test]
    fn round_trips_and_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join(CONFIG_FILE);
        let c = McpConfig {
            enabled: true,
            allowed_folders: vec![" /a ".into(), "/a".into(), String::new(), "/b".into()],
            allow_content_read: true,
            max_results: 10_000,
        };
        c.save(&file).unwrap();
        let back = McpConfig::load(&file);
        assert_eq!(back.allowed_folders, vec!["/a".to_owned(), "/b".to_owned()]);
        assert_eq!(back.max_results, MAX_RESULTS_CAP);
        assert!(back.is_active());
        // Missing fields take their defaults.
        fs::write(&file, r#"{"enabled":true}"#).unwrap();
        let partial = McpConfig::load(&file);
        assert!(partial.enabled && !partial.is_active());
        assert_eq!(partial.max_results, DEFAULT_MAX_RESULTS);
    }
}
