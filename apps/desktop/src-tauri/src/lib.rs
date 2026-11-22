//! Athrix desktop app: Tauri setup and the commands the UI calls.
//!
//! The UI ⇄ Rust contract is `docs/IPC-CONTRACT.md`. Modules:
//! - [`model`]: the JSON types of the contract;
//! - [`indexing`]: background indexing, live updates from the file watcher and the shared index;
//! - [`content`]: the content index, its background indexing, content search and frecency;
//! - [`query`]: query compilation, error mapping, UTF-16 highlights;
//! - [`results`]: running a search (optionally scoped to a drive) and building result rows;
//! - [`dupes`]: `dupe:` searches (identical files, BLAKE3 hashes cached in SQLite);
//! - [`similar`]: `similar:` and "find similar images" (perceptual hashes cached in SQLite);
//! - [`preview`]: previews for the preview pane and the scheme serving previewed images;
//! - [`suggest`]: the "did you mean…" rewrite offered when a search finds nothing (asked
//!   separately, `search_suggest`);
//! - [`roots`]: which drives / folders are indexed, persisted for the next start;
//! - [`volumes`]: the drives of this computer;
//! - [`state`]: state shared by the commands;
//! - [`commands`]: the Tauri commands themselves;
//! - [`library`]: search history, saved searches and favorites (SQLite) and their commands;
//! - [`popup`]: the quick-search popup window and hide-to-tray of the main window;
//! - [`shortcut`]: the global shortcut that toggles the popup;
//! - [`tray`]: the system-tray icon and its menu;
//! - [`launch_args`]: `--search-in <folder>` from Explorer's context menu;
//! - [`updater`]: manual update checks / install and the update settings;
//! - [`mcp`]: the settings of the `athrix-mcp` server for AI assistants (off by default).

pub mod commands;
pub mod content;
pub mod drives;
pub mod dupes;
pub mod indexing;
pub mod interpret;
pub mod launch_args;
pub mod library;
pub mod mcp;
pub mod model;
pub mod pdf_worker;
pub mod popup;
pub mod preview;
pub mod query;
pub mod results;
pub mod roots;
pub mod shortcut;
pub mod similar;
pub mod state;
pub mod suggest;
pub mod tray;
pub mod updater;
pub mod volumes;

use serde::Serialize;
use tauri::Manager;
use tauri::http::{Request, Response, header};

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

/// Answers a request of the [`preview::SCHEME`] scheme: a previewed image, by token, only to
/// the main window and only for GET (see [`preview::PreviewAssets::serve`]).
fn serve_preview_asset(
    app: &tauri::AppHandle,
    webview: &str,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let served =
        if webview != preview::PREVIEW_WINDOW || request.method() != tauri::http::Method::GET {
            preview::Served::forbidden()
        } else {
            app.state::<state::AppState>()
                .preview_assets
                .serve(request.uri().path())
        };
    let mut response = Response::builder()
        .status(served.status)
        .header(header::CONTENT_TYPE, served.mime);
    for (name, value) in preview::SECURITY_HEADERS {
        response = response.header(name, value);
    }
    response.body(served.body).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cannot build a preview response");
        Response::default()
    })
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
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            tracing::info!("another launch of the app; showing the main window");
            // `--search-in <folder>` (Explorer context menu) shows the window itself.
            if !launch_args::handle_args(app, args, Some(std::path::Path::new(&cwd))) {
                popup::show_main(app);
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(launch_args::SearchInState::default())
        .manage(updater::UpdaterState::default())
        .manage(state::AppState::default())
        .manage(popup::PopupState::default())
        .manage(library::Library::default())
        .setup(|app| {
            commands::start_default_indexing(app.handle());
            library::open(app.handle());
            dupes::configure(app.handle());
            similar::configure(app.handle());
            shortcut::register(app.handle());
            if let Err(err) = tray::create(app.handle()) {
                tracing::warn!("couldn't create the tray icon: {err}");
            }
            updater::init(app.handle());
            let cwd = std::env::current_dir().ok();
            launch_args::handle_args(app.handle(), std::env::args_os(), cwd.as_deref());
            Ok(())
        })
        .on_window_event(popup::on_window_event)
        .register_asynchronous_uri_scheme_protocol(preview::SCHEME, |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let webview = ctx.webview_label().to_owned();
            // File reads stay off the webview's thread.
            drop(tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_preview_asset(&app, &webview, &request));
            }));
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            normalize_preview,
            commands::index_status,
            commands::start_indexing,
            commands::list_volumes,
            commands::get_index_roots,
            commands::set_index_roots,
            drives::get_drive_settings,
            drives::set_drive_settings,
            commands::search,
            commands::search_suggest,
            similar::find_similar,
            interpret::interpret_query,
            commands::open_entry,
            commands::reveal_entry,
            commands::entry_path,
            commands::preview_entry,
            commands::content_snippets,
            commands::content_status,
            commands::content_pause,
            commands::content_resume,
            commands::set_content_indexing,
            library::history_list,
            library::history_add,
            library::history_remove,
            library::history_clear,
            library::saved_list,
            library::saved_create,
            library::saved_rename,
            library::saved_delete,
            library::favorite_list,
            library::favorite_add,
            library::favorite_remove,
            library::favorite_open,
            library::favorite_reveal,
            launch_args::take_pending_search_in,
            updater::update_check,
            updater::update_install,
            updater::get_update_settings,
            updater::set_update_settings,
            mcp::get_mcp_settings,
            mcp::set_mcp_settings,
            mcp::mcp_server_path,
            mcp::mcp_pick_folder,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Athrix app")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // Commit what was extracted, join the indexing threads, save frecency.
                app.state::<state::AppState>().content.shutdown();
            }
        });
}
