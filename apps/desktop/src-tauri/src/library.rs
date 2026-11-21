//! The search library: history, saved searches and favorites (`athrix-store`, in
//! `<app_data>/library.sqlite3`), and the commands of the "مكتبة البحث" section of
//! `docs/IPC-CONTRACT.md`.
//!
//! Paths never travel UI -> Rust except here, and only to name a favorite: a favorite is added
//! by result id, and `favorite_open` / `favorite_reveal` act on the stored path of a favorite,
//! never on an arbitrary path from the UI.

use std::sync::{Arc, OnceLock};

use athrix_store::{Favorite, HistoryEntry, SavedSearch, Store, StoreError};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::commands::STALE_ID_ERROR;
use crate::results::decode_id;
use crate::state::AppState;

/// File name of the library database in the app data folder.
pub const LIBRARY_FILE: &str = "library.sqlite3";

/// Default `limit` of `history_list`.
pub const DEFAULT_HISTORY_LIMIT: usize = 50;

/// Error of `favorite_open` / `favorite_reveal` for a path that is not a favorite.
pub const NOT_A_FAVORITE_ERROR: &str = "not a favorite";

/// Managed state: the store, once [`open`] ran (or why it could not be opened).
#[derive(Debug, Default)]
pub struct Library(OnceLock<Result<Arc<Store>, String>>);

impl Library {
    /// The store, or the error string the commands return.
    fn store(&self) -> Result<Arc<Store>, String> {
        match self.0.get() {
            Some(Ok(store)) => Ok(Arc::clone(store)),
            Some(Err(e)) => Err(e.clone()),
            None => Err("the library is not ready yet".to_owned()),
        }
    }
}

/// Opens the library store at start-up (called from `setup`). A failure is logged and kept:
/// the library commands then return it, the rest of the app works.
pub fn open(app: &AppHandle) {
    let opened = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data folder: {e}"))
        .and_then(|dir| Store::open(&dir.join(LIBRARY_FILE)).map_err(|e| e.to_string()));
    match &opened {
        Ok(store) => {
            if let Some(moved) = store.recovered_from() {
                tracing::warn!(moved = %moved.display(), "the library was corrupt; started a new one");
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot open the library"),
    }
    let _ = app.state::<Library>().0.set(opened.map(Arc::new));
}

/// Runs `f` with the store on the blocking pool (SQLite does file I/O).
async fn with_store<T: Send + 'static>(
    library: &Library,
    f: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, String> {
    let store = library.store()?;
    tauri::async_runtime::spawn_blocking(move || f(&store).map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("background task failed: {e}"))?
}

/// The most recent searches (default [`DEFAULT_HISTORY_LIMIT`]), newest first.
#[tauri::command]
pub async fn history_list(
    library: State<'_, Library>,
    limit: Option<usize>,
) -> Result<Vec<HistoryEntry>, String> {
    let limit = limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    with_store(&library, move |s| s.history_list(limit)).await
}

/// Records a search (empty queries are ignored; a repeat bumps the existing row).
#[tauri::command]
pub async fn history_add(
    library: State<'_, Library>,
    query: String,
    scope: Option<String>,
) -> Result<(), String> {
    with_store(&library, move |s| s.history_add(&query, scope.as_deref())).await
}

/// Removes one history row.
#[tauri::command]
pub async fn history_remove(library: State<'_, Library>, id: i64) -> Result<(), String> {
    with_store(&library, move |s| s.history_remove(id)).await
}

/// Clears the history.
#[tauri::command]
pub async fn history_clear(library: State<'_, Library>) -> Result<(), String> {
    with_store(&library, Store::history_clear).await
}

/// Every saved search, newest first.
#[tauri::command]
pub async fn saved_list(library: State<'_, Library>) -> Result<Vec<SavedSearch>, String> {
    with_store(&library, Store::saved_list).await
}

/// Saves a search under a name.
#[tauri::command]
pub async fn saved_create(
    library: State<'_, Library>,
    name: String,
    query: String,
    scope: Option<String>,
) -> Result<SavedSearch, String> {
    with_store(&library, move |s| {
        s.saved_create(&name, &query, scope.as_deref())
    })
    .await
}

/// Renames a saved search.
#[tauri::command]
pub async fn saved_rename(
    library: State<'_, Library>,
    id: i64,
    name: String,
) -> Result<SavedSearch, String> {
    with_store(&library, move |s| s.saved_rename(id, &name)).await
}

/// Deletes a saved search.
#[tauri::command]
pub async fn saved_delete(library: State<'_, Library>, id: i64) -> Result<(), String> {
    with_store(&library, move |s| s.saved_delete(id)).await
}

/// Every favorite, newest first, with `exists` checked now.
#[tauri::command]
pub async fn favorite_list(library: State<'_, Library>) -> Result<Vec<Favorite>, String> {
    with_store(&library, Store::favorite_list).await
}

/// Adds the entry of result `id` to the favorites (resolved like `open_entry`; a stale id
/// rejects with [`STALE_ID_ERROR`]).
#[tauri::command]
pub async fn favorite_add(
    state: State<'_, AppState>,
    library: State<'_, Library>,
    id: u64,
) -> Result<Favorite, String> {
    // Same resolution as `commands::resolve_entry`: only the id's own view, never another.
    if state.indexer.index().is_none() {
        return Err("the index is not ready yet".to_owned());
    }
    let (generation, entry) = decode_id(id).ok_or_else(|| format!("invalid result id {id}"))?;
    let (path, is_dir) = match state.indexer.view(generation) {
        Some(view) if view.has_entry(entry) => (view.path(entry), view.meta(entry).is_dir),
        _ => return Err(STALE_ID_ERROR.to_owned()),
    };
    with_store(&library, move |s| s.favorite_add(&path, is_dir)).await
}

/// Removes a favorite by path.
#[tauri::command]
pub async fn favorite_remove(library: State<'_, Library>, path: String) -> Result<(), String> {
    with_store(&library, move |s| s.favorite_remove(&path)).await
}

/// Opens a favorite with the system's default program (only a path in the favorites).
#[tauri::command]
pub async fn favorite_open(
    app: AppHandle,
    library: State<'_, Library>,
    path: String,
) -> Result<(), String> {
    let stored = with_store(&library, move |s| s.favorite_path(&path))
        .await?
        .ok_or_else(|| NOT_A_FAVORITE_ERROR.to_owned())?;
    let text = stored.to_string_lossy().into_owned();
    app.opener()
        .open_path(text.as_str(), None::<&str>)
        .map_err(|e| format!("cannot open {}: {e}", stored.display()))?;
    app.state::<AppState>().content.record_open(&text);
    Ok(())
}

/// Opens a favorite's folder in the file manager with it selected (only a path in the
/// favorites).
#[tauri::command]
pub async fn favorite_reveal(
    app: AppHandle,
    library: State<'_, Library>,
    path: String,
) -> Result<(), String> {
    let stored = with_store(&library, move |s| s.favorite_path(&path))
        .await?
        .ok_or_else(|| NOT_A_FAVORITE_ERROR.to_owned())?;
    app.opener()
        .reveal_item_in_dir(&stored)
        .map_err(|e| format!("cannot reveal {}: {e}", stored.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_fail_cleanly_before_or_without_a_store() {
        let library = Library::default();
        assert_eq!(library.store().unwrap_err(), "the library is not ready yet");
        let _ = library.0.set(Err("boom".to_owned()));
        assert_eq!(library.store().unwrap_err(), "boom");
    }
}
