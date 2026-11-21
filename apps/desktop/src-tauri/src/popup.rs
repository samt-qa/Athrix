//! The quick-search popup (design §9.1) and the main window's hide-to-tray behavior.
//!
//! The popup is the second window of `tauri.conf.json` (label [`QUICK_LABEL`]): frameless,
//! transparent, always on top, no taskbar entry, created hidden at launch so it opens instantly.
//! It loads the same frontend as the main window; the frontend picks its layout from the window
//! label. Both windows call the same commands, so they share one backend state.
//!
//! Behavior:
//! - [`toggle`] (global shortcut, tray left-click) shows it centered on the monitor under the
//!   cursor and focuses it, or hides it when it is already visible;
//! - it hides itself when it loses focus (here) and on Esc / after opening a result (frontend);
//! - closing the main window hides it instead of quitting; Quit in the tray menu exits.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, PhysicalPosition, Runtime, WebviewWindow, Window, WindowEvent};

/// Label of the popup window in `tauri.conf.json` (the frontend checks the same label).
pub const QUICK_LABEL: &str = "quick";
/// Label of the main window.
pub const MAIN_LABEL: &str = "main";

/// Logical size the popup window can grow to: the 720 × 520 panel plus the transparent margin
/// that holds its shadow (keep in sync with `.quick-frame` in `styles/app.css`).
const MAX_LOGICAL_HEIGHT: f64 = 520.0 + 24.0 + 52.0;

/// A toggle this soon after a focus-loss hide is treated as "hide": clicking the tray icon
/// first takes focus from the popup (hiding it), then delivers the click.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// When the popup was last hidden because it lost focus.
#[derive(Default)]
pub struct PopupState {
    blur_hidden_at: Mutex<Option<Instant>>,
}

fn popup<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    let window = app.get_webview_window(QUICK_LABEL);
    if window.is_none() {
        tracing::warn!("quick-search window `{QUICK_LABEL}` is missing");
    }
    window
}

/// Shows the popup if hidden, hides it if visible.
pub fn toggle<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = popup(app) else { return };
    if window.is_visible().unwrap_or(false) {
        hide(&window);
        return;
    }
    let recently_hidden = app
        .state::<PopupState>()
        .blur_hidden_at
        .lock()
        .ok()
        .and_then(|at| *at)
        .is_some_and(|at| at.elapsed() < REOPEN_GUARD);
    if !recently_hidden {
        show(app);
    }
}

/// Shows and focuses the popup, centered on the monitor under the cursor.
pub fn show<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = popup(app) else { return };
    place(app, &window);
    if let Err(err) = window.show().and_then(|()| window.set_focus()) {
        tracing::warn!("couldn't show the quick-search window: {err}");
    }
}

fn hide<R: Runtime>(window: &WebviewWindow<R>) {
    if let Err(err) = window.hide() {
        tracing::warn!("couldn't hide the quick-search window: {err}");
    }
}

/// Centers the popup horizontally on the monitor under the cursor, and vertically as if it were
/// at its full height, so it grows downwards from a stable top edge.
fn place<R: Runtime>(app: &AppHandle<R>, window: &WebviewWindow<R>) {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let (Some(monitor), Ok(size)) = (monitor, window.outer_size()) else {
        let _ = window.center();
        return;
    };
    let area = monitor.work_area();
    let max_height = (MAX_LOGICAL_HEIGHT * monitor.scale_factor()).round() as i32;
    let x = area.position.x + (area.size.width as i32 - size.width as i32) / 2;
    let y = area.position.y + ((area.size.height as i32 - max_height) / 2).max(0);
    if let Err(err) = window.set_position(PhysicalPosition::new(x, y)) {
        tracing::warn!("couldn't place the quick-search window: {err}");
    }
}

/// Shows, unminimizes and focuses the main window.
pub fn show_main<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window(MAIN_LABEL) else {
        tracing::warn!("main window `{MAIN_LABEL}` is missing");
        return;
    };
    let shown = window
        .show()
        .and_then(|()| window.unminimize())
        .and_then(|()| window.set_focus());
    if let Err(err) = shown {
        tracing::warn!("couldn't show the main window: {err}");
    }
}

/// Window events of both windows (`Builder::on_window_event`).
pub fn on_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    match (window.label(), event) {
        // The popup is transient: it goes away as soon as focus moves elsewhere.
        (QUICK_LABEL, WindowEvent::Focused(false)) if window.is_visible().unwrap_or(false) => {
            if let Ok(mut at) = window.state::<PopupState>().blur_hidden_at.lock() {
                *at = Some(Instant::now());
            }
            if let Err(err) = window.hide() {
                tracing::warn!("couldn't hide the quick-search window: {err}");
            }
        }
        // Neither window quits the app: the main window hides to the tray, the popup just hides.
        (MAIN_LABEL | QUICK_LABEL, WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            if let Err(err) = window.hide() {
                tracing::warn!("couldn't hide window `{}`: {err}", window.label());
            }
        }
        _ => {}
    }
}
