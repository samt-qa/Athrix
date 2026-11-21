//! Athrix desktop app: Tauri setup and the commands the UI calls.
//!
//! The UI ⇄ Rust contract is `docs/IPC-CONTRACT.md`. Modules:
//! - [`model`]: the JSON types of the contract;
//! - [`indexing`]: background indexing, live updates from the file watcher and the shared index;
//! - [`query`]: query compilation, error mapping, UTF-16 highlights;
//! - [`results`]: running a search (optionally scoped to a drive) and building result rows;
//! - [`roots`]: which drives / folders are indexed, persisted for the next start;
//! - [`volumes`]: the drives of this computer;
//! - [`state`]: state shared by the commands;
//! - [`commands`]: the Tauri commands themselves;
//! - [`popup`]: the quick-search popup window and hide-to-tray of the main window;
//! - [`shortcut`]: the global shortcut that toggles the popup;
//! - [`tray`]: the system-tray icon and its menu.

pub mod commands;
pub mod indexing;
pub mod model;
pub mod popup;
pub mod query;
pub mod results;
pub mod roots;
pub mod shortcut;
pub mod state;
pub mod tray;
pub mod volumes;

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

/// Builds and runs the Tauri app. Indexing of the chosen drives (by default every fixed drive)
/// starts right away.
///
/// The app lives in the tray: closing the main window hides it, and only Quit in the tray menu
/// exits. The quick-search popup opens with [`shortcut::QUICK_SEARCH_SHORTCUT`] or a tray click.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    athrix_core::logging::init("info");
    tauri::Builder::default()
        // First, per the plugin's docs: a second launch exits right away and this instance
        // shows its main window instead (no second tray icon, no failed shortcut).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("another launch of the app; showing the main window");
            popup::show_main(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(state::AppState::default())
        .manage(popup::PopupState::default())
        .setup(|app| {
            commands::start_default_indexing(app.handle());
            shortcut::register(app.handle());
            if let Err(err) = tray::create(app.handle()) {
                tracing::warn!("couldn't create the tray icon: {err}");
            }
            Ok(())
        })
        .on_window_event(popup::on_window_event)
        .invoke_handler(tauri::generate_handler![
            app_info,
            normalize_preview,
            commands::index_status,
            commands::start_indexing,
            commands::list_volumes,
            commands::get_index_roots,
            commands::set_index_roots,
            commands::search,
            commands::open_entry,
            commands::reveal_entry,
            commands::entry_path,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Athrix app");
}
