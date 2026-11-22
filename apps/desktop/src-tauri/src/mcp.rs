//! Settings → MCP (task 7.2): the switches of the `athrix-mcp` server that AI assistants
//! (Claude Desktop, Claude Code, …) start to search the user's files.
//!
//! The server is a separate program (the `athrix-mcp` sidecar, installed next to `Athrix.exe`);
//! the app only writes its settings file `<app_data>/mcp.json` ([`athrix_mcp::config`]), which
//! the server reads again on every tool call. Off by default.
//!
//! Commands:
//! - `get_mcp_settings` / `set_mcp_settings`: the [`McpConfig`]; `set` checks every allowed
//!   folder (absolute local path of an existing folder, no UNC / `\\?\` path) and saves.
//! - `mcp_server_path`: where `athrix-mcp[.exe]` is and whether it exists, for the "copy
//!   Claude Desktop config" button.
//! - `mcp_pick_folder`: the native folder picker (Rust side of `tauri-plugin-dialog`, so the
//!   web view gets no dialog permission); `None` when cancelled.

use std::path::PathBuf;

use athrix_mcp::config::{CONFIG_FILE, McpConfig};
use athrix_mcp::paths;
use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_dialog::DialogExt;

/// File name of the server executable.
pub const SERVER_BIN: &str = "athrix-mcp";

/// What `mcp_server_path` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerPath {
    /// Absolute path of `athrix-mcp[.exe]` next to the app executable.
    pub path: String,
    /// Whether the file is there (missing in a dev build until `npm run worker`).
    pub exists: bool,
}

fn config_file<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join(CONFIG_FILE))
        .map_err(|e| format!("no app data folder: {e}"))
}

/// Checks and cleans `settings` for saving: every folder must be an absolute local path of an
/// existing folder. English error naming the first bad folder.
pub fn validate(settings: McpConfig) -> Result<McpConfig, String> {
    let mut settings = settings.normalized();
    for folder in &mut settings.allowed_folders {
        let norm = paths::normalize(folder).map_err(|e| format!("{folder}: {e}"))?;
        if !norm.is_dir() {
            return Err(format!("{folder}: not an existing folder"));
        }
        *folder = norm.to_string_lossy().into_owned();
    }
    Ok(settings.normalized())
}

/// `get_mcp_settings`: the saved settings (defaults, i.e. off, when none are saved).
#[tauri::command]
pub fn get_mcp_settings(app: AppHandle) -> Result<McpConfig, String> {
    Ok(McpConfig::load(&config_file(&app)?))
}

/// `set_mcp_settings`: validates, saves and returns the settings. The server picks them up at
/// its next tool call.
#[tauri::command]
pub fn set_mcp_settings(app: AppHandle, settings: McpConfig) -> Result<McpConfig, String> {
    let settings = validate(settings)?;
    settings
        .save(&config_file(&app)?)
        .map_err(|e| format!("cannot save the MCP settings: {e}"))?;
    tracing::info!(
        enabled = settings.enabled,
        folders = settings.allowed_folders.len(),
        read = settings.allow_content_read,
        "MCP settings saved"
    );
    Ok(settings)
}

/// `mcp_server_path`: the server executable beside the app.
#[tauri::command]
pub fn mcp_server_path() -> Result<McpServerPath, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("the app executable has no folder")?;
    let path = dir.join(format!("{SERVER_BIN}{}", std::env::consts::EXE_SUFFIX));
    Ok(McpServerPath {
        exists: path.is_file(),
        path: path.to_string_lossy().into_owned(),
    })
}

/// `mcp_pick_folder`: the native "choose folder" dialog over the main window; `None` when
/// cancelled.
#[tauri::command]
pub async fn mcp_pick_folder(app: AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file();
        if let Some(window) = app.get_webview_window("main") {
            dialog = dialog.set_parent(&window);
        }
        dialog
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_keeps_existing_absolute_folders_only() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().to_string_lossy().into_owned();
        let good = McpConfig {
            enabled: true,
            allowed_folders: vec![ok.clone(), format!("{ok}{}", std::path::MAIN_SEPARATOR)],
            allow_content_read: false,
            max_results: 0,
        };
        let v = validate(good).unwrap();
        assert_eq!(v.allowed_folders.len(), 1, "duplicates merged");
        assert_eq!(v.max_results, 1);

        for bad in [
            "relative/folder".to_owned(),
            dir.path().join("missing").to_string_lossy().into_owned(),
            r"\\server\share".to_owned(),
        ] {
            let c = McpConfig {
                allowed_folders: vec![bad.clone()],
                ..McpConfig::default()
            };
            assert!(validate(c).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_path_is_beside_the_executable() {
        let p = mcp_server_path().unwrap();
        assert!(p.path.contains(SERVER_BIN));
    }
}
