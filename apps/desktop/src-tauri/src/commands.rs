//! The Tauri commands of `docs/IPC-CONTRACT.md`: indexing, search and opening entries.
//!
//! Errors the UI does not expect are returned as English strings (the promise rejects).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use athrix_fs_index::{EntryId, FileIndex};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::indexing::{StatusListener, default_roots};
use crate::model::{IndexStatus, QueryError, ResultItem, SearchResponse};
use crate::query::compile_query;
use crate::results::{SearchFailure, run_search};
use crate::state::AppState;

/// Event carrying an [`IndexStatus`] on every indexing state change.
pub const STATUS_EVENT: &str = "index://status";

/// Upper bound for `search`'s `limit`, whatever the UI asks.
pub const MAX_LIMIT: usize = 10_000;

/// Current indexing status.
#[tauri::command]
pub fn index_status(state: State<'_, AppState>) -> IndexStatus {
    state.indexer.status()
}

/// Starts indexing `roots` (default: the user's home folder) on a background thread and
/// returns at once. A call made while a walk is running is queued (latest wins).
#[tauri::command]
pub fn start_indexing(app: AppHandle, roots: Option<Vec<String>>) -> Result<(), String> {
    let roots = match roots {
        Some(r) if !r.is_empty() => r.into_iter().map(PathBuf::from).collect(),
        _ => default_roots()?,
    };
    start(&app, roots);
    Ok(())
}

/// Starts indexing the default roots; called once at app start-up.
pub fn start_default_indexing(app: &AppHandle) {
    match default_roots() {
        Ok(roots) => start(app, roots),
        Err(e) => tracing::error!(error = %e, "cannot start indexing"),
    }
}

fn start(app: &AppHandle, roots: Vec<PathBuf>) {
    let handle = app.clone();
    let listener: StatusListener = Arc::new(move |status: &IndexStatus| {
        if let Err(e) = handle.emit(STATUS_EVENT, status) {
            tracing::warn!(error = %e, "cannot emit the index status");
        }
    });
    let outcome = app.state::<AppState>().indexer.start(roots, listener);
    tracing::info!(?outcome, "indexing requested");
}

/// Searches the name index. Each call cancels the previous one; the work runs on a blocking
/// thread pool, never on the UI thread.
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: u32,
    request_id: u64,
) -> Result<SearchResponse, String> {
    let started = Instant::now();
    let cancel = state.begin_search();
    let index = state.indexer.index();
    let limit = (limit as usize).min(MAX_LIMIT);
    let outcome =
        tauri::async_runtime::spawn_blocking(move || execute_search(index, &query, limit, &cancel))
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
    index: Option<Arc<FileIndex>>,
    text: &str,
    limit: usize,
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
    match run_search(&index, &query, limit, cancel) {
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
pub async fn open_entry(app: AppHandle, state: State<'_, AppState>, id: u32) -> Result<(), String> {
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
    id: u32,
) -> Result<(), String> {
    let path = resolve(state.indexer.index(), id)?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("cannot reveal {}: {e}", path.display()))
}

/// The entry's full path, for copying to the clipboard.
#[tauri::command]
pub fn entry_path(state: State<'_, AppState>, id: u32) -> Result<String, String> {
    resolve(state.indexer.index(), id).map(|p| p.to_string_lossy().into_owned())
}

/// Resolves an entry id to its path on the Rust side (the UI never sends paths).
fn resolve(index: Option<Arc<FileIndex>>, id: EntryId) -> Result<PathBuf, String> {
    let index = index.ok_or_else(|| "the index is not ready yet".to_owned())?;
    if (id as usize) >= index.len() {
        return Err(format!(
            "entry {id} no longer exists (the index was rebuilt); search again"
        ));
    }
    Ok(index.path(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_fs_index::{EntryMeta, IndexBuilder};

    fn sample() -> Arc<FileIndex> {
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
        Arc::new(b.finish())
    }

    #[test]
    fn empty_query_and_missing_index_give_no_items() {
        let no = AtomicBool::new(false);
        assert!(matches!(
            execute_search(Some(sample()), "   ", 10, &no),
            SearchOutcome::Empty
        ));
        assert!(matches!(
            execute_search(None, "فاتورة", 10, &no),
            SearchOutcome::Empty
        ));
    }

    #[test]
    fn invalid_query_reports_an_error_even_without_an_index() {
        let no = AtomicBool::new(false);
        match execute_search(None, "szie:1", 10, &no) {
            SearchOutcome::Invalid(e) => assert_eq!(e.kind, "UnknownFilter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn finds_and_cancels() {
        let ix = sample();
        let no = AtomicBool::new(false);
        match execute_search(Some(Arc::clone(&ix)), "فاتورة 123", 10, &no) {
            SearchOutcome::Hits { items, truncated } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].id, 1);
                assert!(!truncated);
                assert_eq!(items[0].highlights.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        let yes = AtomicBool::new(true);
        assert!(matches!(
            execute_search(Some(ix), "فاتورة", 10, &yes),
            SearchOutcome::Cancelled
        ));
    }

    #[test]
    fn resolve_rejects_stale_ids() {
        let ix = sample();
        let expected: PathBuf = ["/data", "فاتورة ١٢٣.pdf"].iter().collect();
        assert_eq!(resolve(Some(Arc::clone(&ix)), 1).unwrap(), expected);
        assert!(
            resolve(Some(ix), 2)
                .unwrap_err()
                .contains("no longer exists")
        );
        assert!(resolve(None, 0).unwrap_err().contains("not ready"));
    }
}
