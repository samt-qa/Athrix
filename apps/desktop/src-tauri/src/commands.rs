//! The Tauri commands of `docs/IPC-CONTRACT.md`: indexing, search and opening entries.
//!
//! Errors the UI does not expect are returned as English strings (the promise rejects).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::indexing::{LiveIndex, SnapshotStore, StatusListener};
use crate::model::{IndexState, IndexStatus, QueryError, ResultItem, SearchResponse};
use crate::query::compile_query;
use crate::results::{SearchFailure, SearchScope, decode_id, run_search};
use crate::roots::{
    IndexRoots, ROOTS_SETTING_FILE, RootsChoice, RootsSetting, normalize_roots,
    resolve as resolve_roots,
};
use crate::state::AppState;
use crate::volumes::{self, Volume};

/// Error returned for a result id from an index that is no longer current.
pub const STALE_ID_ERROR: &str = "this result is out of date; search again";

/// Folder under the app data dir holding the index snapshot.
pub const SNAPSHOT_DIR: &str = "index";

/// Event carrying an [`IndexStatus`] on every indexing state change.
pub const STATUS_EVENT: &str = "index://status";

/// Upper bound for `search`'s `limit`, whatever the UI asks.
pub const MAX_LIMIT: usize = 10_000;

/// Current indexing status.
#[tauri::command]
pub fn index_status(state: State<'_, AppState>) -> IndexStatus {
    state.indexer.status()
}

/// Starts indexing `roots` (default: the indexed-roots setting, see [`set_index_roots`]) on a
/// background thread and returns at once. A call made while a walk is running is queued
/// (latest wins). Explicit `roots` are indexed once and not saved.
#[tauri::command]
pub async fn start_indexing(app: AppHandle, roots: Option<Vec<String>>) -> Result<(), String> {
    let roots = match roots {
        Some(r) if !r.is_empty() => normalize_roots(&r)?,
        _ => {
            let choice = app.state::<AppState>().roots.choice();
            blocking(move || resolve_roots(&choice, &volumes::list_volumes())).await??
        }
    };
    let outcome = app
        .state::<AppState>()
        .indexer
        .start(roots, status_listener(&app));
    tracing::info!(?outcome, "indexing requested");
    Ok(())
}

/// The drives of this computer (sorted by path), for the drive picker and the search scope.
#[tauri::command]
pub async fn list_volumes() -> Result<Vec<Volume>, String> {
    blocking(volumes::list_volumes).await
}

/// The indexed-roots setting and the folders it indexes now.
#[tauri::command]
pub async fn get_index_roots(state: State<'_, AppState>) -> Result<IndexRoots, String> {
    let choice = state.roots.choice();
    let resolved = {
        let choice = choice.clone();
        blocking(move || resolve_roots(&choice, &volumes::list_volumes())).await??
    };
    Ok(index_roots(&choice, &resolved))
}

/// Sets what to index: `roots: null` (or absent) = the whole computer (every fixed drive),
/// otherwise exactly these drives / folders. Saves the choice for the next start, then
/// re-indexes if the folders changed (queued behind a running walk, latest wins). Returns the
/// new setting. Rejects (English) a relative path or an empty list; a save failure is logged
/// and the choice still applies until the app quits.
#[tauri::command]
pub async fn set_index_roots(
    app: AppHandle,
    roots: Option<Vec<String>>,
) -> Result<IndexRoots, String> {
    let choice = match roots {
        None => RootsChoice::WholeComputer,
        Some(r) => RootsChoice::Custom(normalize_roots(&r)?),
    };
    let resolved = {
        let choice = choice.clone();
        blocking(move || resolve_roots(&choice, &volumes::list_volumes())).await??
    };
    let state = app.state::<AppState>();
    if let Err(e) = state.roots.set(choice.clone()) {
        tracing::warn!(error = %e, "cannot save the indexed-roots setting");
    }
    let current = state.indexer.status();
    let wanted = root_strings(&resolved);
    let unchanged = current.roots == wanted
        && matches!(current.state, IndexState::Ready | IndexState::Indexing);
    if !unchanged {
        let outcome = state.indexer.start(resolved.clone(), status_listener(&app));
        tracing::info!(?outcome, roots = ?wanted, "indexed roots changed");
    }
    Ok(index_roots(&choice, &resolved))
}

fn index_roots(choice: &RootsChoice, resolved: &[PathBuf]) -> IndexRoots {
    IndexRoots {
        whole_computer: *choice == RootsChoice::WholeComputer,
        roots: root_strings(resolved),
    }
}

fn root_strings(roots: &[PathBuf]) -> Vec<String> {
    roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect()
}

/// Runs `f` on the blocking thread pool (volume listing can take a moment).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("background task failed: {e}"))
}

/// Called once at app start-up: sets the snapshot store (`<app_data>/index`) and loads the
/// indexed-roots setting (`<app_data>/indexed-roots.json`), then starts indexing those roots
/// (by default every fixed drive). The indexing thread first loads the snapshot of those
/// roots, if valid, so searching works at once, then walks to refresh it.
pub fn start_default_indexing(app: &AppHandle) {
    let state = app.state::<AppState>();
    let indexer = &state.indexer;
    match app.path().app_data_dir() {
        Ok(dir) => {
            indexer.set_snapshot_store(SnapshotStore::new(dir.join(SNAPSHOT_DIR)));
            state.roots.load_from(dir.join(ROOTS_SETTING_FILE));
        }
        Err(e) => tracing::warn!(
            error = %e,
            "no app data folder; index snapshots and the roots setting are not saved"
        ),
    }
    match startup_roots(&state.roots) {
        Ok(roots) => {
            tracing::info!(?roots, "start-up indexing");
            let outcome = indexer.start_from_snapshot(roots, status_listener(app));
            tracing::info!(?outcome, "start-up indexing requested");
        }
        Err(e) => tracing::error!(error = %e, "cannot start indexing"),
    }
}

/// The roots to index at start-up, from the setting and the drives present now.
fn startup_roots(setting: &RootsSetting) -> Result<Vec<PathBuf>, String> {
    resolve_roots(&setting.choice(), &volumes::list_volumes())
}

/// Emits every status change as [`STATUS_EVENT`].
fn status_listener(app: &AppHandle) -> StatusListener {
    let handle = app.clone();
    Arc::new(move |status: &IndexStatus| {
        if let Err(e) = handle.emit(STATUS_EVENT, status) {
            tracing::warn!(error = %e, "cannot emit the index status");
        }
    })
}

/// Searches the name index, only under `scope` if given (a drive such as `D:\` or a folder;
/// blank = everywhere; a relative path rejects). Each call cancels the previous one; the work
/// runs on a blocking thread pool, never on the UI thread.
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: u32,
    request_id: u64,
    scope: Option<String>,
) -> Result<SearchResponse, String> {
    let started = Instant::now();
    let scope = SearchScope::parse(scope.as_deref())?;
    let cancel = state.begin_search();
    let index = state.indexer.index();
    let limit = (limit as usize).min(MAX_LIMIT);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        execute_search(index, &query, limit, scope.as_ref(), &cancel)
    })
    .await
    .map_err(|e| format!("search task failed: {e}"))?;
    let mut response = SearchResponse {
        request_id,
        items: Vec::new(),
        truncated: false,
        elapsed_ms: 0.0,
        error: None,
        cancelled: false,
    };
    match outcome {
        SearchOutcome::Hits { items, truncated } => {
            response.items = items;
            response.truncated = truncated;
        }
        SearchOutcome::Empty => {}
        SearchOutcome::Invalid(e) => response.error = Some(e),
        SearchOutcome::Cancelled => response.cancelled = true,
        SearchOutcome::Failed(e) => return Err(e),
    }
    response.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(response)
}

/// Result of [`execute_search`], before timing and the request id are added.
#[derive(Debug)]
enum SearchOutcome {
    Hits {
        items: Vec<ResultItem>,
        truncated: bool,
    },
    /// Empty query, or no index yet.
    Empty,
    Invalid(QueryError),
    Cancelled,
    Failed(String),
}

/// The blocking part of `search`: parse, run, build items.
fn execute_search(
    index: Option<Arc<LiveIndex>>,
    text: &str,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> SearchOutcome {
    if text.trim().is_empty() {
        return SearchOutcome::Empty;
    }
    let query = match compile_query(text) {
        Ok(q) => q,
        Err(e) => return SearchOutcome::Invalid(e),
    };
    let Some(index) = index else {
        return SearchOutcome::Empty;
    };
    if cancel.load(Ordering::Relaxed) {
        return SearchOutcome::Cancelled;
    }
    match run_search(&index, &query, limit, scope, cancel) {
        Ok(hits) => SearchOutcome::Hits {
            items: hits.items,
            truncated: hits.truncated,
        },
        Err(SearchFailure::Cancelled) => SearchOutcome::Cancelled,
        Err(SearchFailure::Failed(e)) => SearchOutcome::Failed(e),
    }
}

/// Opens the entry with the system's default program.
#[tauri::command]
pub async fn open_entry(app: AppHandle, state: State<'_, AppState>, id: u64) -> Result<(), String> {
    let path = resolve(state.indexer.index(), id)?;
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

/// Opens the entry's folder in the file manager with the entry selected.
#[tauri::command]
pub async fn reveal_entry(
    app: AppHandle,
    state: State<'_, AppState>,
    id: u64,
) -> Result<(), String> {
    let path = resolve(state.indexer.index(), id)?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("cannot reveal {}: {e}", path.display()))
}

/// The entry's full path, for copying to the clipboard.
#[tauri::command]
pub fn entry_path(state: State<'_, AppState>, id: u64) -> Result<String, String> {
    resolve(state.indexer.index(), id).map(|p| p.to_string_lossy().into_owned())
}

/// Resolves a result id to its path on the Rust side (the UI never sends paths). An id from
/// an older index generation is rejected with [`STALE_ID_ERROR`], never resolved against the
/// current index.
fn resolve(index: Option<Arc<LiveIndex>>, id: u64) -> Result<PathBuf, String> {
    let index = index.ok_or_else(|| "the index is not ready yet".to_owned())?;
    let (generation, entry) = decode_id(id).ok_or_else(|| format!("invalid result id {id}"))?;
    if generation != index.generation() || (entry as usize) >= index.len() {
        return Err(STALE_ID_ERROR.to_owned());
    }
    Ok(index.path(entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::encode_id;
    use athrix_fs_index::{EntryMeta, FileIndex, IndexBuilder};

    fn sample() -> Arc<LiveIndex> {
        sample_gen(3)
    }

    fn sample_gen(generation: u32) -> Arc<LiveIndex> {
        Arc::new(LiveIndex::new(generation, sample_index()))
    }

    fn sample_index() -> FileIndex {
        let mut b = IndexBuilder::new();
        let root = b.push(
            None,
            "/data",
            EntryMeta {
                is_dir: true,
                ..Default::default()
            },
        );
        b.push(Some(root), "فاتورة ١٢٣.pdf", EntryMeta::default());
        b.finish()
    }

    #[test]
    fn empty_query_and_missing_index_give_no_items() {
        let no = AtomicBool::new(false);
        assert!(matches!(
            execute_search(Some(sample()), "   ", 10, None, &no),
            SearchOutcome::Empty
        ));
        assert!(matches!(
            execute_search(None, "فاتورة", 10, None, &no),
            SearchOutcome::Empty
        ));
    }

    #[test]
    fn invalid_query_reports_an_error_even_without_an_index() {
        let no = AtomicBool::new(false);
        match execute_search(None, "szie:1", 10, None, &no) {
            SearchOutcome::Invalid(e) => assert_eq!(e.kind, "UnknownFilter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn finds_and_cancels() {
        let ix = sample();
        let no = AtomicBool::new(false);
        match execute_search(Some(Arc::clone(&ix)), "فاتورة 123", 10, None, &no) {
            SearchOutcome::Hits { items, truncated } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, encode_id(3, 1));
                assert!(!truncated);
                assert_eq!(items[0].highlights.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        let yes = AtomicBool::new(true);
        assert!(matches!(
            execute_search(Some(ix), "فاتورة", 10, None, &yes),
            SearchOutcome::Cancelled
        ));
    }

    #[test]
    fn resolve_accepts_current_ids_and_rejects_stale_ones() {
        let ix = sample();
        let expected: PathBuf = ["/data", "فاتورة ١٢٣.pdf"].iter().collect();
        assert_eq!(
            resolve(Some(Arc::clone(&ix)), encode_id(3, 1)).unwrap(),
            expected
        );
        // Same entry number, older generation: rejected, not resolved to another file.
        assert_eq!(
            resolve(Some(Arc::clone(&ix)), encode_id(2, 1)).unwrap_err(),
            STALE_ID_ERROR
        );
        // A raw entry index (generation 0) from before generations is stale too.
        assert_eq!(
            resolve(Some(Arc::clone(&ix)), 1).unwrap_err(),
            STALE_ID_ERROR
        );
        // Current generation, entry out of range.
        assert_eq!(
            resolve(Some(Arc::clone(&ix)), encode_id(3, 2)).unwrap_err(),
            STALE_ID_ERROR
        );
        assert!(resolve(Some(ix), u64::MAX).unwrap_err().contains("invalid"));
        assert!(resolve(None, 0).unwrap_err().contains("not ready"));
    }

    #[test]
    fn an_id_from_a_search_is_rejected_after_a_reindex() {
        let no = AtomicBool::new(false);
        let SearchOutcome::Hits { items, .. } =
            execute_search(Some(sample_gen(1)), "فاتورة", 10, None, &no)
        else {
            panic!("expected hits");
        };
        let id = items[0].id;
        assert!(resolve(Some(sample_gen(1)), id).is_ok());
        assert_eq!(
            resolve(Some(sample_gen(2)), id).unwrap_err(),
            STALE_ID_ERROR
        );
    }
}
