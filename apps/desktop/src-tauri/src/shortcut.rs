//! The system-wide shortcut that toggles the quick-search popup.

use tauri::{AppHandle, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Default accelerator for the quick-search popup. The one place to change it (a settings UI can
/// later re-register another one). `Alt+Space` is avoided: Windows uses it for the window menu
/// and PowerToys Run claims it by default.
pub const QUICK_SEARCH_SHORTCUT: &str = "Ctrl+Shift+Space";

/// Registers [`QUICK_SEARCH_SHORTCUT`]. If another app already owns it, logs a warning and
/// carries on: the tray icon still opens the popup.
pub fn register<R: Runtime>(app: &AppHandle<R>) {
    let registered =
        app.global_shortcut()
            .on_shortcut(QUICK_SEARCH_SHORTCUT, |app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    crate::popup::toggle(app);
                }
            });
    match registered {
        Ok(()) => tracing::info!("global shortcut {QUICK_SEARCH_SHORTCUT} registered"),
        Err(err) => tracing::warn!(
            "couldn't register the global shortcut {QUICK_SEARCH_SHORTCUT} \
             (probably taken by another app); use the tray icon instead: {err}"
        ),
    }
}
