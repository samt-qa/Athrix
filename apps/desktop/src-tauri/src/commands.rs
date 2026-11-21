//! The Tauri commands of `docs/IPC-CONTRACT.md`: indexing, search and opening entries.
//!
//! Errors the UI does not expect are returned as English strings (the promise rejects).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use athrix_fs_index::EntryId;

use athrix_query::{PlanOptions, SearchPlan};

use crate::content::{ContentService, StatusSink};
use crate::indexing::{IndexObserver, IndexView, Indexer, SnapshotStore, StatusListener};
use crate::model::{
    ContentStatus, IndexState, IndexStatus, PreviewResponse, QueryError, ResultItem,
    SearchResponse, SearchSuggestion,
};
use crate::preview::{DEFAULT_MAX_CHARS, build_preview};
use crate::query::{CompiledQuery, compile_ast, map_parse_error, map_plan_error};
use crate::results::{SearchFailure, SearchScope, decode_id, run_search};
use crate::roots::{
    IndexRoots, ROOTS_SETTING_FILE, RootsChoice, RootsSetting, normalize_roots,
    resolve as resolve_roots,
};
use crate::state::AppState;
use crate::suggest::suggest;
use crate::volumes::{self, Volume};

/// Error returned for a result id from an index view that is no longer kept (see
/// [`crate::indexing::RECENT_VIEWS`]).
pub const STALE_ID_ERROR: &str = "this result is out of date; search again";

/// Folder under the app data dir holding the index snapshot.
pub const SNAPSHOT_DIR: &str = "index";

/// Event carrying an [`IndexStatus`] on every indexing state change, and (at most once per
/// [`crate::indexing::LIVE_STATUS_INTERVAL`]) when watched changes alter the entry count.
pub const STATUS_EVENT: &str = "index://status";

/// Event carrying a [`ContentStatus`], at most once per [`crate::content::STATUS_INTERVAL`].
pub const CONTENT_STATUS_EVENT: &str = "content://status";

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
///
/// Content indexing (`<app_data>/content`, see [`crate::content`]) follows the name index: it
/// starts once that index is ready.
pub fn start_default_indexing(app: &AppHandle) {
    let state = app.state::<AppState>();
    let indexer = &state.indexer;
    match app.path().app_data_dir() {
        Ok(dir) => {
            indexer.set_snapshot_store(SnapshotStore::new(dir.join(SNAPSHOT_DIR)));
            state.roots.load_from(dir.join(ROOTS_SETTING_FILE));
            state.content.configure(&dir);
        }
        Err(e) => tracing::warn!(
            error = %e,
            "no app data folder; index snapshots, the roots setting and the content index are \
             not saved"
        ),
    }
    state.content.set_status_sink(content_status_sink(app));
    let observer: Arc<dyn IndexObserver> = state.content.clone();
    indexer.set_observer(observer);
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

/// Emits content statuses as [`CONTENT_STATUS_EVENT`] (the service throttles them).
fn content_status_sink(app: &AppHandle) -> StatusSink {
    let handle = app.clone();
    Arc::new(move |status: &ContentStatus| {
        if let Err(e) = handle.emit(CONTENT_STATUS_EVENT, status) {
            tracing::warn!(error = %e, "cannot emit the content status");
        }
    })
}

/// Searches the name index, only under `scope` if given (a drive such as `D:\` or a folder;
/// blank = everywhere; a relative path rejects). `content:` filters search the content index
/// too, and `searchContent: true` makes plain words search names and contents (see
/// [`crate::content`]). Each call cancels the previous one; the work runs on a blocking thread
/// pool, never on the UI thread.
#[tauri::command]
pub async fn search(
    state: State<'_, AppState>,
    query: String,
    limit: u32,
    request_id: u64,
    scope: Option<String>,
    search_content: Option<bool>,
) -> Result<SearchResponse, String> {
    let started = Instant::now();
    let scope = SearchScope::parse(scope.as_deref())?;
    let cancel = state.begin_search();
    let index = state.indexer.index();
    let content = Arc::clone(&state.content);
    let request = SearchRequest {
        limit: (limit as usize).min(MAX_LIMIT),
        scope,
        search_content: search_content.unwrap_or(false),
    };
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        execute_search(index, &content, &query, &request, &cancel)
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
        suggestion: None,
    };
    match outcome {
        SearchOutcome::Hits {
            items,
            truncated,
            suggestion,
        } => {
            response.items = items;
            response.truncated = truncated;
            response.suggestion = suggestion;
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
        /// Only when `items` is empty (see [`crate::suggest`]).
        suggestion: Option<SearchSuggestion>,
    },
    /// Empty query, or no index yet.
    Empty,
    Invalid(QueryError),
    Cancelled,
    Failed(String),
}

/// The options of one `search` call.
#[derive(Debug, Default)]
struct SearchRequest {
    limit: usize,
    scope: Option<SearchScope>,
    /// Plain words search contents too (`searchContent`).
    search_content: bool,
}

/// The blocking part of `search`: parse and plan, then either the name-index fast path (with a
/// "did you mean" rewrite when nothing is found) or a content search.
fn execute_search(
    index: Option<Arc<IndexView>>,
    content: &ContentService,
    text: &str,
    request: &SearchRequest,
    cancel: &AtomicBool,
) -> SearchOutcome {
    if text.trim().is_empty() {
        return SearchOutcome::Empty;
    }
    let ast = match athrix_query::parse(text) {
        Ok(ast) => ast,
        Err(e) => return SearchOutcome::Invalid(map_parse_error(text, &e)),
    };
    let options = PlanOptions {
        plain_words_search_content: request.search_content,
    };
    let plan = match athrix_query::plan(&ast, &options) {
        Ok(plan) => plan,
        Err(e) => return SearchOutcome::Invalid(map_plan_error(text, &e)),
    };
    let (limit, scope) = (request.limit, request.scope.as_ref());
    if plan.uses_content() {
        return content_search(index, content, text, &plan, limit, scope, cancel);
    }
    let query = match compile_ast(text, &ast) {
        Ok(q) => q,
        Err(e) => return SearchOutcome::Invalid(e),
    };
    let Some(index) = index else {
        return SearchOutcome::Empty;
    };
    if cancel.load(Ordering::Relaxed) {
        return SearchOutcome::Cancelled;
    }
    let hits = match run_search(&index, &query, limit, scope, cancel) {
        Ok(hits) => hits,
        Err(SearchFailure::Cancelled) => return SearchOutcome::Cancelled,
        Err(SearchFailure::Failed(e)) => return SearchOutcome::Failed(e),
    };
    let suggestion = if hits.items.is_empty() && limit > 0 {
        match suggest(&index, text, &query, limit, scope, cancel) {
            Ok(s) => s,
            Err(SearchFailure::Cancelled) => return SearchOutcome::Cancelled,
            // The search itself worked: a failed probe only means no suggestion.
            Err(SearchFailure::Failed(e)) => {
                tracing::warn!(error = %e, "suggestion search failed");
                None
            }
        }
    } else {
        None
    };
    SearchOutcome::Hits {
        items: hits.items,
        truncated: hits.truncated,
        suggestion,
    }
}

/// A plan that needs the content index: compiles its name parts (reporting what the name
/// index cannot run, like the fast path), then runs it. No "did you mean" here.
fn content_search(
    index: Option<Arc<IndexView>>,
    content: &ContentService,
    text: &str,
    plan: &SearchPlan,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> SearchOutcome {
    let (name, filters) = match plan {
        SearchPlan::NameAndContent { name, .. } => (name.as_ref(), None),
        SearchPlan::NameOrContent { name, filters, .. } => (Some(name), filters.as_ref()),
        SearchPlan::Name(_) | SearchPlan::Empty => (None, None),
    };
    let name_query: Option<CompiledQuery> = match name.map(|q| compile_ast(text, q)).transpose() {
        Ok(q) => q,
        Err(e) => return SearchOutcome::Invalid(e),
    };
    if let Some(Err(e)) = filters.map(|q| compile_ast(text, q)) {
        return SearchOutcome::Invalid(e);
    }
    let Some(index) = index else {
        return SearchOutcome::Empty;
    };
    if cancel.load(Ordering::Relaxed) {
        return SearchOutcome::Cancelled;
    }
    match content.search(&index, plan, name_query.as_ref(), limit, scope, cancel) {
        Ok(hits) => SearchOutcome::Hits {
            items: hits.items,
            truncated: hits.truncated,
            suggestion: None,
        },
        Err(SearchFailure::Cancelled) => SearchOutcome::Cancelled,
        Err(SearchFailure::Failed(e)) => SearchOutcome::Failed(e),
    }
}

/// Opens the entry with the system's default program, and records the open in the frecency
/// store (which boosts it in later content searches).
#[tauri::command]
pub async fn open_entry(app: AppHandle, state: State<'_, AppState>, id: u64) -> Result<(), String> {
    let path = resolve(&state.indexer, id)?;
    let text = path.to_string_lossy().into_owned();
    app.opener()
        .open_path(text.as_str(), None::<&str>)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    state.content.record_open(&text);
    Ok(())
}

/// The state of background content indexing.
#[tauri::command]
pub fn content_status(state: State<'_, AppState>) -> ContentStatus {
    state.content.status()
}

/// Pauses content indexing (user request).
#[tauri::command]
pub fn content_pause(state: State<'_, AppState>) {
    state.content.pause();
}

/// Ends a user pause of content indexing.
#[tauri::command]
pub fn content_resume(state: State<'_, AppState>) {
    state.content.resume();
}

/// Turns content indexing on or off; saved in `<app_data>/content-indexing.json`. Off stops
/// the background work (what is indexed stays searchable) and the status says `disabled`.
#[tauri::command]
pub async fn set_content_indexing(app: AppHandle, enabled: bool) -> Result<(), String> {
    // Stopping waits for the workers to finish their file; starting queues the whole index.
    blocking(move || {
        let state = app.state::<AppState>();
        let current = state.indexer.index();
        state.content.set_enabled(enabled, current);
    })
    .await
}

/// Opens the entry's folder in the file manager with the entry selected.
#[tauri::command]
pub async fn reveal_entry(
    app: AppHandle,
    state: State<'_, AppState>,
    id: u64,
) -> Result<(), String> {
    let path = resolve(&state.indexer, id)?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| format!("cannot reveal {}: {e}", path.display()))
}

/// The entry's full path, for copying to the clipboard.
#[tauri::command]
pub fn entry_path(state: State<'_, AppState>, id: u64) -> Result<String, String> {
    resolve(&state.indexer, id).map(|p| p.to_string_lossy().into_owned())
}

/// Resolves a result id to its path on the Rust side (the UI never sends paths), with the
/// view of the id's generation (the current one or one of the few before it, see
/// [`crate::indexing::RECENT_VIEWS`]). An id whose view is gone is rejected with
/// [`STALE_ID_ERROR`], never resolved against another view.
fn resolve(indexer: &Indexer, id: u64) -> Result<PathBuf, String> {
    resolve_entry(indexer, id).map(|(view, entry)| view.path(entry))
}

/// [`resolve`], keeping the view and the entry (for previews, which read metadata too).
fn resolve_entry(indexer: &Indexer, id: u64) -> Result<(Arc<IndexView>, EntryId), String> {
    if indexer.index().is_none() {
        return Err("the index is not ready yet".to_owned());
    }
    let (generation, entry) = decode_id(id).ok_or_else(|| format!("invalid result id {id}"))?;
    match indexer.view(generation) {
        Some(view) if view.has_entry(entry) => Ok((view, entry)),
        _ => Err(STALE_ID_ERROR.to_owned()),
    }
}

/// A preview of the entry for the preview pane (see [`crate::preview`]): text or code (the
/// first `max_chars` characters, default [`DEFAULT_MAX_CHARS`]), document text, an image URL,
/// a folder listing, or why there is none. Ids resolve like `open_entry` (stale ids reject with
/// [`STALE_ID_ERROR`]). Each call cancels the previous one; previews run one at a time on the
/// blocking pool, and a cancelled one answers `cancelled: true`.
#[tauri::command]
pub async fn preview_entry(
    state: State<'_, AppState>,
    id: u64,
    request_id: u64,
    max_chars: Option<u32>,
) -> Result<PreviewResponse, String> {
    let cancel = state.begin_preview();
    let (view, entry) = resolve_entry(&state.indexer, id)?;
    let gate = Arc::clone(&state.preview_gate);
    let assets = Arc::clone(&state.preview_assets);
    let max_chars = max_chars.map_or(DEFAULT_MAX_CHARS, |m| m as usize);
    let preview = tauri::async_runtime::spawn_blocking(move || {
        let _turn = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let preview = build_preview(&view, entry, max_chars, &cancel, &assets).ok()?;
        (!cancel.load(Ordering::Relaxed)).then_some(preview)
    })
    .await
    .map_err(|e| format!("preview task failed: {e}"))?;
    Ok(PreviewResponse {
        request_id,
        cancelled: preview.is_none(),
        preview,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::encode_id;
    use athrix_fs_index::{EntryMeta, FileIndex, IndexBuilder};

    /// `execute_search` as the UI calls it without `searchContent`.
    fn name_search(
        index: Option<Arc<IndexView>>,
        text: &str,
        limit: usize,
        scope: Option<SearchScope>,
        cancel: &AtomicBool,
    ) -> SearchOutcome {
        let content = ContentService::new(crate::content::ContentOptions::default());
        let request = SearchRequest {
            limit,
            scope,
            search_content: false,
        };
        execute_search(index, &content, text, &request, cancel)
    }

    fn sample() -> Arc<IndexView> {
        Arc::new(IndexView::from_index(3, sample_index()))
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

    /// An indexer whose current view (generation 1) is the sample index.
    fn indexer() -> Arc<Indexer> {
        let indexer = Arc::new(Indexer::new());
        let view = indexer
            .install(sample_index(), &[PathBuf::from("/data")])
            .unwrap();
        assert_eq!(view.generation(), 1);
        indexer
    }

    #[test]
    fn empty_query_and_missing_index_give_no_items() {
        let no = AtomicBool::new(false);
        assert!(matches!(
            name_search(Some(sample()), "   ", 10, None, &no),
            SearchOutcome::Empty
        ));
        assert!(matches!(
            name_search(None, "فاتورة", 10, None, &no),
            SearchOutcome::Empty
        ));
    }

    #[test]
    fn invalid_query_reports_an_error_even_without_an_index() {
        let no = AtomicBool::new(false);
        match name_search(None, "szie:1", 10, None, &no) {
            SearchOutcome::Invalid(e) => assert_eq!(e.kind, "UnknownFilter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn finds_and_cancels() {
        let ix = sample();
        let no = AtomicBool::new(false);
        match name_search(Some(Arc::clone(&ix)), "فاتورة 123", 10, None, &no) {
            SearchOutcome::Hits {
                items,
                truncated,
                suggestion,
            } => {
                assert_eq!(items.len(), 1);
                assert_eq!(suggestion, None);
                assert_eq!(items[0].id, encode_id(3, 1));
                assert!(!truncated);
                assert_eq!(items[0].highlights.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        let yes = AtomicBool::new(true);
        assert!(matches!(
            name_search(Some(ix), "فاتورة", 10, None, &yes),
            SearchOutcome::Cancelled
        ));
    }

    #[test]
    fn an_empty_result_carries_a_suggestion() {
        use crate::model::SuggestionKind;
        let ix = sample();
        let no = AtomicBool::new(false);
        // `فاتورة` typed with the English layout active, plus a filter kept as is.
        let s = match name_search(Some(Arc::clone(&ix)), "thj,vm ext:pdf", 10, None, &no) {
            SearchOutcome::Hits {
                items, suggestion, ..
            } => {
                assert!(items.is_empty());
                suggestion.expect("a layout suggestion")
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(s.kind, SuggestionKind::Layout);
        assert_eq!(s.query, "فاتورة ext:pdf");
        assert_eq!(s.count, 1);
        // A typo: the fuzzy rewrite.
        match name_search(Some(Arc::clone(&ix)), "فتورة", 10, None, &no) {
            SearchOutcome::Hits {
                items, suggestion, ..
            } => {
                assert!(items.is_empty());
                let s = suggestion.expect("a fuzzy suggestion");
                assert_eq!(
                    (s.kind, s.query.as_str()),
                    (SuggestionKind::Fuzzy, "fuzzy: فتورة")
                );
            }
            other => panic!("{other:?}"),
        }
        // Nothing close: no suggestion.
        assert!(matches!(
            name_search(Some(ix), "zzzzqqqq", 10, None, &no),
            SearchOutcome::Hits {
                suggestion: None,
                ..
            }
        ));
    }

    #[test]
    fn resolve_accepts_current_ids_and_rejects_stale_ones() {
        assert!(
            resolve(&Indexer::new(), 0)
                .unwrap_err()
                .contains("not ready")
        );
        let ix = indexer();
        let expected: PathBuf = ["/data", "فاتورة ١٢٣.pdf"].iter().collect();
        assert_eq!(resolve(&ix, encode_id(1, 1)).unwrap(), expected);
        // Same entry number, unknown generation: rejected, not resolved to another file.
        assert_eq!(resolve(&ix, encode_id(2, 1)).unwrap_err(), STALE_ID_ERROR);
        // A raw entry index (generation 0) from before generations is stale too.
        assert_eq!(resolve(&ix, 1).unwrap_err(), STALE_ID_ERROR);
        // Current generation, entry out of range.
        assert_eq!(resolve(&ix, encode_id(1, 2)).unwrap_err(), STALE_ID_ERROR);
        assert!(resolve(&ix, u64::MAX).unwrap_err().contains("invalid"));
    }

    #[test]
    fn an_id_from_a_search_is_rejected_after_a_reindex() {
        let ix = indexer();
        let no = AtomicBool::new(false);
        let SearchOutcome::Hits { items, .. } = name_search(ix.index(), "فاتورة", 10, None, &no)
        else {
            panic!("expected hits");
        };
        let id = items[0].id;
        assert!(resolve(&ix, id).is_ok());
        // A new walk (new base) makes every earlier id stale.
        ix.install(sample_index(), &[PathBuf::from("/data")])
            .unwrap();
        assert_eq!(resolve(&ix, id).unwrap_err(), STALE_ID_ERROR);
    }
}

/// End-to-end content search: real files in temp folders, the real walk, the real scheduler
/// and content index, through [`execute_search`] as the `search` command runs it.
#[cfg(test)]
mod content_search_tests {
    use super::*;
    use crate::content::test_options;
    use crate::indexing::walk_options;
    use crate::model::Matched;
    use crate::results::decode_id;
    use athrix_fs_index::{LiveIndex, WatchBatch, walk};
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::Path;
    use std::time::Duration;

    const LONG: Duration = Duration::from_secs(60);

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("athrix-search-")
            .tempdir()
            .unwrap()
    }

    /// A minimal Word document whose body is `text`.
    fn docx(text: &str) -> Vec<u8> {
        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
        );
        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#;
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in [
            ("[Content_Types].xml", types),
            ("_rels/.rels", rels),
            ("word/document.xml", document.as_str()),
        ] {
            w.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    struct Fixture {
        dirs: Vec<tempfile::TempDir>,
        _app: tempfile::TempDir,
        roots: Vec<PathBuf>,
        live: LiveIndex,
        content: Arc<ContentService>,
        view: Arc<IndexView>,
    }

    impl Fixture {
        /// One temp root per entry of `roots`, each with its `(name, bytes)` files; name and
        /// content indexed.
        fn new(roots: &[&[(&str, Vec<u8>)]]) -> Self {
            let dirs: Vec<_> = roots.iter().map(|_| tmp()).collect();
            for (dir, files) in dirs.iter().zip(roots) {
                for (name, bytes) in files.iter() {
                    fs::write(dir.path().join(name), bytes).unwrap();
                }
            }
            let root_paths: Vec<PathBuf> = dirs.iter().map(|d| d.path().to_path_buf()).collect();
            let opts = walk_options(&root_paths);
            let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
            let view = Arc::new(IndexView::new(1, live.snapshot(), &root_paths));
            let app = tmp();
            let content = ContentService::new(test_options());
            content.configure(app.path());
            content.base_ready(&view);
            assert!(content.wait_idle(LONG));
            Fixture {
                dirs,
                _app: app,
                roots: root_paths,
                live,
                content,
                view,
            }
        }

        fn search(&self, text: &str, scope: Option<&Path>, search_content: bool) -> SearchOutcome {
            let request = SearchRequest {
                limit: 50,
                scope: scope.map(|p| {
                    SearchScope::parse(Some(&p.to_string_lossy()))
                        .unwrap()
                        .unwrap()
                }),
                search_content,
            };
            let no = AtomicBool::new(false);
            execute_search(
                Some(Arc::clone(&self.view)),
                &self.content,
                text,
                &request,
                &no,
            )
        }

        fn items(&self, text: &str, scope: Option<&Path>, search_content: bool) -> Vec<ResultItem> {
            match self.search(text, scope, search_content) {
                SearchOutcome::Hits { items, .. } => items,
                other => panic!("{text}: {other:?}"),
            }
        }

        /// Applies the paths to the live index and publishes a new view (generation 2+).
        fn apply(&mut self, paths: &[PathBuf]) {
            self.live.apply_paths(paths);
            let generation = self.view.generation() + 1;
            self.view = Arc::new(IndexView::new(
                generation,
                self.live.snapshot(),
                &self.roots,
            ));
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.content.shutdown();
        }
    }

    fn names(items: &[ResultItem]) -> Vec<&str> {
        let mut v: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        v.sort_unstable();
        v
    }

    fn js_slice(s: &str, (a, b): (usize, usize)) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        String::from_utf16(&units[a..b]).unwrap()
    }

    fn budget_files() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("تقرير 2024.docx", docx("ميزانية المشروع السنوية")),
            ("خطة.txt", "هذه ميزانية الفريق للربع الأول".into()),
            ("تقرير قديم.txt", "لا شيء مهم هنا".into()),
            ("notes.txt", "hello world".into()),
            ("ميزانية قديمة.txt", "ميزانية".into()),
            ("ميزانية.png", "not text".into()),
        ]
    }

    #[test]
    fn content_filter_finds_documents_by_their_text() {
        let f = Fixture::new(&[&budget_files()]);
        let items = f.items("content:ميزانية", None, false);
        assert_eq!(
            names(&items),
            vec!["تقرير 2024.docx", "خطة.txt", "ميزانية قديمة.txt"]
        );
        for item in &items {
            assert_eq!(item.matched, Matched::Content, "{item:?}");
            assert!(item.highlights.is_empty(), "no name part: {item:?}");
            let snippet = item.snippet.as_ref().expect("a snippet");
            assert!(!snippet.highlights.is_empty(), "{snippet:?}");
            assert_eq!(js_slice(&snippet.text, snippet.highlights[0]), "ميزانية");
            // The id resolves in the view the search used.
            let (generation, entry) = decode_id(item.id).unwrap();
            assert_eq!(generation, 1);
            assert_eq!(f.view.name(entry), item.name);
        }
        // The Arabic synonym works the same.
        assert_eq!(f.items("محتوى:ميزانية", None, false).len(), 3);
    }

    #[test]
    fn name_and_content_are_anded() {
        let f = Fixture::new(&[&budget_files()]);
        let items = f.items("تقرير content:ميزانية", None, false);
        assert_eq!(names(&items), vec!["تقرير 2024.docx"]);
        let item = &items[0];
        assert_eq!(item.matched, Matched::Both);
        assert_eq!(js_slice(&item.name, item.highlights[0]), "تقرير");
        assert!(item.snippet.is_some());
        // Filters restrict the content part too.
        assert_eq!(
            names(&f.items("content:ميزانية ext:txt", None, false)),
            vec!["خطة.txt", "ميزانية قديمة.txt"]
        );
        // Excluded content: name hits minus files containing the word.
        assert_eq!(
            names(&f.items("تقرير -content:ميزانية", None, false)),
            vec!["تقرير قديم.txt"]
        );
    }

    #[test]
    fn a_drive_scope_restricts_content_hits() {
        let a: Vec<(&str, Vec<u8>)> = vec![("budget-a.txt", "ميزانية".into())];
        let b: Vec<(&str, Vec<u8>)> = vec![("budget-b.txt", "ميزانية".into())];
        let f = Fixture::new(&[&a, &b]);
        assert_eq!(f.items("content:ميزانية", None, false).len(), 2);
        let scoped = f.items("content:ميزانية", Some(f.dirs[1].path()), false);
        assert_eq!(names(&scoped), vec!["budget-b.txt"]);
        let scoped = f.items("budget content:ميزانية", Some(f.dirs[0].path()), false);
        assert_eq!(names(&scoped), vec!["budget-a.txt"]);
        // A scope outside every root finds nothing.
        let elsewhere = tmp();
        assert!(
            f.items("content:ميزانية", Some(elsewhere.path()), false)
                .is_empty()
        );
    }

    #[test]
    fn search_content_unites_name_and_content_hits() {
        let f = Fixture::new(&[&budget_files()]);
        let items = f.items("ميزانية", None, true);
        let matched = |name: &str| {
            items
                .iter()
                .find(|i| i.name == name)
                .unwrap_or_else(|| panic!("{name} missing from {items:?}"))
                .matched
        };
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(matched("ميزانية قديمة.txt"), Matched::Both);
        assert_eq!(matched("ميزانية.png"), Matched::Name);
        assert_eq!(matched("خطة.txt"), Matched::Content);
        assert_eq!(matched("تقرير 2024.docx"), Matched::Content);
        // Found by both lists: first.
        assert_eq!(items[0].name, "ميزانية قديمة.txt");
        assert!(items[0].snippet.is_some());

        // Without `searchContent`: the name fast path, names only.
        let items = f.items("ميزانية", None, false);
        assert_eq!(names(&items), vec!["ميزانية قديمة.txt", "ميزانية.png"]);
        assert!(
            items
                .iter()
                .all(|i| i.matched == Matched::Name && i.snippet.is_none())
        );
    }

    #[test]
    fn hits_follow_the_live_name_index() {
        let mut f = Fixture::new(&[&budget_files()]);
        let dir = f.dirs[0].path().to_path_buf();
        // Deleted since it was indexed: the name index no longer has it, so it is dropped.
        let gone = dir.join("خطة.txt");
        fs::remove_file(&gone).unwrap();
        f.apply(std::slice::from_ref(&gone));
        let items = f.items("content:ميزانية", None, false);
        assert_eq!(names(&items), vec!["تقرير 2024.docx", "ميزانية قديمة.txt"]);
        // A new file (a delta entry of the live index), reported by the watcher.
        let added = dir.join("جديد.md");
        fs::write(&added, "ميزانية جديدة").unwrap();
        f.apply(std::slice::from_ref(&added));
        f.content
            .files_changed(&WatchBatch::Changed(vec![added.clone()]));
        assert!(f.content.wait_idle(LONG));
        let items = f.items("content:جديدة", None, false);
        assert_eq!(names(&items), vec!["جديد.md"]);
        let (generation, entry) = decode_id(items[0].id).unwrap();
        assert_eq!(generation, f.view.generation());
        assert_eq!(f.view.path(entry), added);
    }

    #[test]
    fn opens_boost_a_file_in_content_results() {
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("a.txt", "قرار اللجنة".into()),
            ("b.txt", "قرار اللجنة".into()),
            ("c.txt", "قرار اللجنة".into()),
        ];
        let f = Fixture::new(&[&files]);
        let before = f.items("content:قرار", None, false);
        assert_eq!(before.len(), 3);
        let last = before.last().unwrap();
        let (_, entry) = decode_id(last.id).unwrap();
        let path = f.view.path(entry);
        for _ in 0..3 {
            f.content.record_open(&path.to_string_lossy());
        }
        let after = f.items("content:قرار", None, false);
        assert_eq!(after[0].name, last.name, "{after:?}");
    }

    #[test]
    fn plan_errors_and_cancellation() {
        let content = ContentService::new(test_options());
        let no = AtomicBool::new(false);
        let request = SearchRequest {
            limit: 10,
            ..SearchRequest::default()
        };
        for (text, kind) in [
            ("تقرير | content:عقد", "ContentInOr"),
            ("!<content:عقد ext:pdf>", "NegatedContentGroup"),
            ("-content:عقد", "OnlyExcludedContent"),
            ("content:عقد szie:1", "UnknownFilter"),
            ("sem:عقد content:x", "Unsupported"),
        ] {
            match execute_search(None, &content, text, &request, &no) {
                SearchOutcome::Invalid(e) => assert_eq!(e.kind, kind, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
        // Valid but no index yet: empty.
        assert!(matches!(
            execute_search(None, &content, "content:عقد", &request, &no),
            SearchOutcome::Empty
        ));

        let f = Fixture::new(&[&budget_files()]);
        let yes = AtomicBool::new(true);
        assert!(matches!(
            execute_search(
                Some(Arc::clone(&f.view)),
                &f.content,
                "content:ميزانية",
                &request,
                &yes
            ),
            SearchOutcome::Cancelled
        ));
        // Truncation: more matches than the limit.
        let one = SearchRequest {
            limit: 1,
            ..SearchRequest::default()
        };
        match execute_search(
            Some(Arc::clone(&f.view)),
            &f.content,
            "content:ميزانية",
            &one,
            &no,
        ) {
            SearchOutcome::Hits {
                items, truncated, ..
            } => {
                assert_eq!(items.len(), 1);
                assert!(truncated);
            }
            other => panic!("{other:?}"),
        }
    }
}
