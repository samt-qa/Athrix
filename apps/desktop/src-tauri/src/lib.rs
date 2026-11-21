//! Athrix desktop app: Tauri setup and the commands the UI calls.
//!
//! The UI ⇄ Rust contract is `docs/IPC-CONTRACT.md`. Modules:
//! - [`model`]: the JSON types of the contract;
//! - [`indexing`]: background indexing and the shared index;
//! - [`query`]: query compilation, error mapping, UTF-16 highlights;
//! - [`results`]: running a search and building result rows;
//! - [`state`]: state shared by the commands;
//! - [`commands`]: the Tauri commands themselves.

pub mod commands;
pub mod indexing;
pub mod model;
pub mod query;
pub mod results;
pub mod state;

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

/// How the name index will store `text`. Temporary: proves the UI ⇄ Rust bridge.
#[tauri::command]
fn normalize_preview(text: &str) -> String {
    athrix_text_ar::normalize_name(text)
}

/// Builds and runs the Tauri app. Indexing of the user's home folder starts right away.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    athrix_core::logging::init("info");
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state::AppState::default())
        .setup(|app| {
            commands::start_default_indexing(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            normalize_preview,
            commands::index_status,
            commands::start_indexing,
            commands::search,
            commands::open_entry,
            commands::reveal_entry,
            commands::entry_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Athrix app");
}
