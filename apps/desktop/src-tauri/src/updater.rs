//! Updates (task 5.7) through `tauri-plugin-updater`, under the "no internet by default" rule
//! (docs/01 §11): Athrix never contacts the update server unless the user asks.
//!
//! - `update_check` (manual): asks the endpoint in `tauri.conf.json` (`plugins.updater`) for a
//!   newer, signed release; returns an [`UpdateInfo`].
//! - `update_install`: checks again, downloads the newer release, verifies
//!   its signature, installs it and restarts. On Windows the plugin starts the NSIS installer in
//!   passive mode and exits the process; the content index is flushed first.
//! - The setting `auto_update_check` (default **false**, `<app_data>/update-settings.json`):
//!   when on, one check runs in the background at start-up and, if a release is found,
//!   [`UPDATE_AVAILABLE_EVENT`] is emitted. Nothing is ever installed without the user.
//!
//! File layout: `{"version":1,"autoUpdateCheck":false}`. A missing, unreadable, corrupt or
//! other-version file means the default (off).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::state::AppState;

/// File name of the update settings inside the app data folder.
pub const UPDATE_SETTINGS_FILE: &str = "update-settings.json";
/// Event emitted (to every window) with an [`UpdateInfo`] when the start-up check finds a
/// release. Only sent when `auto_update_check` is on.
pub const UPDATE_AVAILABLE_EVENT: &str = "app://update-available";

const SETTINGS_FILE_VERSION: u32 = 1;

/// What `get_update_settings` returns and `set_update_settings` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettings {
    /// Check for updates once at start-up. Off by default (no internet unless asked).
    pub auto_update_check: bool,
}

/// What `update_check` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// A newer release exists.
    pub available: bool,
    /// Its version (only when `available`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Its release notes (only when `available` and the release has notes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl UpdateInfo {
    fn from_update(update: Option<&Update>) -> Self {
        match update {
            Some(u) => UpdateInfo {
                available: true,
                version: Some(u.version.clone()),
                notes: u.body.clone().filter(|b| !b.trim().is_empty()),
            },
            None => UpdateInfo {
                available: false,
                version: None,
                notes: None,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsFile {
    version: u32,
    auto_update_check: bool,
}

/// Update state managed by Tauri: the persisted settings.
#[derive(Debug, Default)]
pub struct UpdaterState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Unset: not persisted (no app data folder, or in tests).
    file: Option<PathBuf>,
    settings: UpdateSettings,
}

impl UpdaterState {
    /// Points the settings at `file` and loads them (see the module docs for fallbacks).
    pub fn load_from(&self, file: PathBuf) {
        let settings = read_settings(&file);
        let mut inner = self.lock();
        inner.settings = settings;
        inner.file = Some(file);
    }

    /// The current settings.
    pub fn settings(&self) -> UpdateSettings {
        self.lock().settings
    }

    /// Replaces the settings and writes them, if a file is set. The new value is kept even if
    /// writing fails (the error is returned).
    pub fn set(&self, settings: UpdateSettings) -> io::Result<()> {
        let mut inner = self.lock();
        inner.settings = settings;
        match &inner.file {
            Some(file) => write_settings(file, settings),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn read_settings(file: &Path) -> UpdateSettings {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return UpdateSettings::default(),
        Err(e) => {
            tracing::warn!(error = %e, file = %file.display(), "cannot read the update settings");
            return UpdateSettings::default();
        }
    };
    match serde_json::from_str::<SettingsFile>(&text) {
        Ok(f) if f.version == SETTINGS_FILE_VERSION => UpdateSettings {
            auto_update_check: f.auto_update_check,
        },
        Ok(f) => {
            tracing::warn!(
                version = f.version,
                "unknown update settings version; defaults"
            );
            UpdateSettings::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "corrupt update settings; defaults");
            UpdateSettings::default()
        }
    }
}

fn write_settings(file: &Path, settings: UpdateSettings) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let body = SettingsFile {
        version: SETTINGS_FILE_VERSION,
        auto_update_check: settings.auto_update_check,
    };
    let json = serde_json::to_string(&body).map_err(io::Error::other)?;
    // Write then rename, so a crash never leaves a half-written file.
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, file)
}

/// Called once from `setup`: loads the settings from the app data folder and, only when
/// `auto_update_check` is on, starts one background check.
pub fn init<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<UpdaterState>();
    match app.path().app_data_dir() {
        Ok(dir) => state.load_from(dir.join(UPDATE_SETTINGS_FILE)),
        Err(e) => tracing::warn!(error = %e, "no app data folder; update settings not saved"),
    }
    if !state.settings().auto_update_check {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match check(&app).await {
            Ok(info) if info.available => {
                if let Err(err) = app.emit(UPDATE_AVAILABLE_EVENT, &info) {
                    tracing::warn!("couldn't emit {UPDATE_AVAILABLE_EVENT}: {err}");
                }
            }
            Ok(_) => tracing::info!("start-up update check: up to date"),
            Err(err) => tracing::warn!("start-up update check failed: {err}"),
        }
    });
}

/// Asks the update endpoint for a newer release.
async fn check<R: Runtime>(app: &AppHandle<R>) -> Result<UpdateInfo, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;
    let info = UpdateInfo::from_update(update.as_ref());
    tracing::info!(?info, "update check");
    Ok(info)
}

/// Checks again, then downloads, verifies and installs the release and restarts.
async fn install<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let flush = app.clone();
    let updater = app
        .updater_builder()
        // Replaces the plugin's default hook (cleanup_before_exit) on Windows, where the
        // installer is started and the process exits without the normal exit path.
        .on_before_exit(move || {
            flush.state::<AppState>().content.shutdown();
            flush.cleanup_before_exit();
        })
        .build()
        .map_err(|e| e.to_string())?;
    // A fresh check with this updater: the hook above travels with the update it returns.
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("no update available".to_owned());
    };
    tracing::info!(version = %update.version, "installing update");
    update
        .download_and_install(|_, _| {}, || tracing::info!("update downloaded"))
        .await
        .map_err(|e| e.to_string())?;
    // Not reached on Windows (the plugin exits); elsewhere restart into the new version.
    app.request_restart();
    Ok(())
}

/// `update_check`: manual check. Errors (English) when offline, the endpoint is unreachable
/// (e.g. private release assets) or the response is invalid.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateInfo, String> {
    check(&app).await
}

/// `update_install`: downloads, verifies and installs the latest release, then restarts.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    install(&app).await
}

/// `get_update_settings`.
#[tauri::command]
pub fn get_update_settings(state: tauri::State<'_, UpdaterState>) -> UpdateSettings {
    state.settings()
}

/// `set_update_settings`: saves and returns the settings. Takes effect at the next start-up.
#[tauri::command]
pub fn set_update_settings(
    state: tauri::State<'_, UpdaterState>,
    settings: UpdateSettings,
) -> Result<UpdateSettings, String> {
    state
        .set(settings)
        .map_err(|e| format!("cannot save the update settings: {e}"))?;
    Ok(state.settings())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_when_the_file_is_missing_or_bad() {
        assert!(!UpdateSettings::default().auto_update_check);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(UPDATE_SETTINGS_FILE);
        assert_eq!(read_settings(&file), UpdateSettings::default());
        fs::write(&file, "{not json").unwrap();
        assert_eq!(read_settings(&file), UpdateSettings::default());
        fs::write(&file, r#"{"version":9,"autoUpdateCheck":true}"#).unwrap();
        assert_eq!(read_settings(&file), UpdateSettings::default());
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join(UPDATE_SETTINGS_FILE);
        let state = UpdaterState::default();
        state.load_from(file.clone());
        let on = UpdateSettings {
            auto_update_check: true,
        };
        state.set(on).unwrap();
        assert_eq!(state.settings(), on);
        let again = UpdaterState::default();
        again.load_from(file);
        assert_eq!(again.settings(), on);
    }

    #[test]
    fn json_shapes_match_the_contract() {
        let none = UpdateInfo::from_update(None);
        assert_eq!(
            serde_json::to_string(&none).unwrap(),
            r#"{"available":false}"#
        );
        let s: UpdateSettings = serde_json::from_str(r#"{"autoUpdateCheck":true}"#).unwrap();
        assert!(s.auto_update_check);
    }
}
