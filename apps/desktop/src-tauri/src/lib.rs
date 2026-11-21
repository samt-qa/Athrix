//! Athrix desktop app: Tauri setup and the commands the UI calls.

use serde::Serialize;

/// Name and version, shown in the UI.
#[derive(Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: athrix_core::APP_NAME,
        version: athrix_core::VERSION,
    }
}

/// How the name index will store `text`. Temporary: proves the UI ⇄ Rust bridge until the
/// search commands arrive (task 1.6).
#[tauri::command]
fn normalize_preview(text: &str) -> String {
    athrix_text_ar::normalize_name(text)
}

/// Builds and runs the Tauri app.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    athrix_core::logging::init("info");
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![app_info, normalize_preview])
        .run(tauri::generate_context!())
        .expect("error while running the Athrix app");
}
