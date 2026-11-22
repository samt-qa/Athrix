//! `dupe:` / `مكرر:` searches: files with at least one byte-identical copy in the index.
//!
//! The `dupe:` switch is taken out of the query ([`split_dupe`]); the rest runs on the name
//! index like any search (scope included) and gives the candidates. Folders and empty files
//! are dropped, then files whose size no other candidate shares; the remaining ones go to
//! [`athrix_dedupe::find_duplicates`] (partial then full BLAKE3 hashes, in parallel, within a
//! time/byte budget, cancelled by a newer search). Hashes are cached in
//! `<app_data>/hashes/dupes.sqlite3`, keyed by path, size and modification time, so the next
//! `dupe:` search (or "continue the scan" after an incomplete one) reads only new files.
//!
//! With `content:` (`dupe: content:عقد ext:pdf`) the candidates are the files the content
//! search finds instead ([`dupe_search_with`]), at most [`CONTENT_CANDIDATES`] of them:
//! byte-identical files have the same text, so a copy of a match matches too (when the content
//! index holds it). Rows keep the `matched` part and snippet the content search gave them.
//!
//! Results are the members of each group, adjacent, biggest wasted space first; each row
//! carries `dupeGroup` / `dupeCount` and the response a [`DupeSummary`].
//!
//! After a search the cache is pruned in the background (rows of deleted or resized files,
//! [`HashCache::prune`]), at most once every [`PRUNE_EVERY`] and within
//! [`athrix_dedupe::PruneLimits`]' default bounds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use athrix_dedupe::{Candidate, HashCache, Options, PruneLimits};
use athrix_fs_index::EntryId;
use athrix_query::{
    Expr, ExprKind, FilterName, FilterValue, PlanOptions, Query as AstQuery, SearchPlan,
};
use tauri::{AppHandle, Manager};

use crate::content::ContentService;
use crate::indexing::{GENERATION_MASK, IndexView};
use crate::model::{DupeSummary, QueryError, ResultItem};
use crate::query::{CompiledQuery, compile_ast, map_plan_error, utf16_offset};
use crate::results::{SearchFailure, SearchScope, build_item, decode_id, search_ids};
use crate::roots::path_key;

/// Folder under the app data dir holding the hash cache.
pub const CACHE_DIR: &str = "hashes";

/// The hash cache file inside [`CACHE_DIR`].
pub const CACHE_FILE: &str = "dupes.sqlite3";

/// Time budget of one `dupe:` search; when it runs out the response is `incomplete`.
pub const TIME_BUDGET: Duration = Duration::from_secs(6);

/// Byte budget of one `dupe:` search (cache hits are free).
pub const BYTE_BUDGET: u64 = 4 * 1024 * 1024 * 1024;

/// Most content matches a `dupe: content:` search looks for duplicates among.
pub const CONTENT_CANDIDATES: usize = 10_000;

/// Least time between two background prunes of the hash cache.
pub const PRUNE_EVERY: Duration = Duration::from_secs(10 * 60);

/// Where the cache lives (set at start-up) and the cache once opened.
static CACHE_PATH: OnceLock<PathBuf> = OnceLock::new();
static CACHE: Mutex<Option<Arc<HashCache>>> = Mutex::new(None);

/// Remembers `<app_data>/hashes/dupes.sqlite3` as the cache file. It is opened on the first
/// `dupe:` search, never at start-up. Without an app data folder hashes are not cached.
pub fn configure(app: &AppHandle) {
    match app.path().app_data_dir() {
        Ok(dir) => {
            let _ = CACHE_PATH.set(dir.join(CACHE_DIR).join(CACHE_FILE));
        }
        Err(e) => tracing::warn!(error = %e, "no app data folder; duplicate hashes not cached"),
    }
}

/// The hash cache, opened on first use (`None` if unconfigured or it cannot be opened).
fn cache() -> Option<Arc<HashCache>> {
    let path = CACHE_PATH.get()?;
    let mut slot = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        match HashCache::open(path) {
            Ok(c) => *slot = Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "cannot open the duplicate hash cache");
                return None;
            }
        }
    }
    slot.clone()
}

/// When the last background prune started.
static LAST_PRUNE: Mutex<Option<Instant>> = Mutex::new(None);

/// Prunes `cache` on a background thread, unless a prune ran less than [`PRUNE_EVERY`] ago.
fn prune_later(cache: Arc<HashCache>) {
    {
        let mut last = LAST_PRUNE.lock().unwrap_or_else(PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < PRUNE_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    let spawned = std::thread::Builder::new()
        .name("dupe-cache-prune".to_owned())
        .spawn(move || match cache.prune(&PruneLimits::default()) {
            Ok(r) => tracing::debug!(
                checked = r.checked,
                removed = r.removed,
                wrapped = r.wrapped,
                vacuumed = r.vacuumed,
                "duplicate hash cache pruned"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot prune the duplicate hash cache"),
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the duplicate hash cache prune");
    }
}

/// Results of a `dupe:` search.
#[derive(Debug)]
pub struct DupeHits {
    /// Group members, at most `limit`.
    pub items: Vec<ResultItem>,
    /// More members exist than `limit`.
    pub truncated: bool,
    /// Totals for the status line.
    pub summary: DupeSummary,
}

/// Why a `dupe:` search gave no hits.
#[derive(Debug)]
pub enum DupeFailure {
    /// The rest of the query is invalid or unsupported.
    Invalid(QueryError),
    /// Cancelled or failed like a plain search.
    Search(SearchFailure),
}

/// If `ast` uses `dupe:`, the query without it; `Ok(None)` if it does not. `dupe:` must be a
/// top-level switch (alone or ANDed with the rest, not under `|`, `!`/`-` or `< >`) without a
/// value; anything else is an error on the filter, of kind `DupeNotTopLevel` or
/// `DupeTakesNoValue`.
pub fn split_dupe(text: &str, ast: &AstQuery) -> Result<Option<AstQuery>, QueryError> {
    let Some(root) = &ast.root else {
        return Ok(None);
    };
    let top: Vec<&Expr> = match &root.kind {
        ExprKind::And(xs) => xs.iter().collect(),
        _ => vec![root],
    };
    let (switches, rest): (Vec<&Expr>, Vec<&Expr>) = top.into_iter().partition(|e| is_dupe(e));
    if let Some(bad) = rest.iter().find_map(|e| nested_dupe(e)) {
        return Err(dupe_error(
            text,
            bad,
            "DupeNotTopLevel",
            "`dupe:` applies to the whole query: use it outside `|`, `!` and `< >`",
        ));
    }
    if switches.is_empty() {
        return Ok(None);
    }
    if let Some(valued) = switches
        .iter()
        .find(|e| matches!(&e.kind, ExprKind::Filter(f) if f.value != FilterValue::None))
    {
        return Err(dupe_error(
            text,
            valued,
            "DupeTakesNoValue",
            "`dupe:` takes no value; write it alone, for example `dupe: ext:jpg`",
        ));
    }
    let root = match rest.len() {
        0 => None,
        1 => Some(rest[0].clone()),
        _ => Some(Expr {
            span: root.span,
            kind: ExprKind::And(rest.into_iter().cloned().collect()),
        }),
    };
    Ok(Some(AstQuery { root }))
}

fn is_dupe(e: &Expr) -> bool {
    matches!(&e.kind, ExprKind::Filter(f) if f.name == FilterName::Dupe)
}

/// A `dupe:` filter somewhere inside `e`.
fn nested_dupe(e: &Expr) -> Option<&Expr> {
    match &e.kind {
        ExprKind::Filter(f) if f.name == FilterName::Dupe => Some(e),
        ExprKind::Term(_) | ExprKind::Filter(_) => None,
        ExprKind::Not(x) | ExprKind::Group(x) => nested_dupe(x),
        ExprKind::And(xs) | ExprKind::Or(xs) => xs.iter().find_map(nested_dupe),
    }
}

fn dupe_error(text: &str, e: &Expr, kind: &str, message: &str) -> QueryError {
    QueryError {
        message: message.to_owned(),
        kind: kind.to_owned(),
        start: utf16_offset(text, e.span.start),
        end: utf16_offset(text, e.span.end),
        suggestion: None,
    }
}

/// Runs a `dupe:` search on names only: [`dupe_search_with`] without a content service, so
/// `content:` in `rest` is an error of kind `DupeWithContent`.
pub fn dupe_search(
    view: &IndexView,
    text: &str,
    rest: &AstQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<DupeHits, DupeFailure> {
    dupe_search_with(view, None, text, rest, limit, scope, cancel)
}

/// Runs a `dupe:` search: `rest` (the query without `dupe:`, see [`split_dupe`]) picks the
/// candidates under `scope`, through `content` when it uses `content:`; returns the duplicate
/// groups among them, at most `limit` rows.
pub fn dupe_search_with(
    view: &IndexView,
    content: Option<&ContentService>,
    text: &str,
    rest: &AstQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<DupeHits, DupeFailure> {
    let plan = athrix_query::plan(rest, &PlanOptions::default())
        .map_err(|e| DupeFailure::Invalid(map_plan_error(text, &e)))?;
    let (ids, mut rows) = if plan.uses_content() {
        let Some(content) = content else {
            return Err(DupeFailure::Invalid(QueryError {
                message: "`dupe:` with `content:` needs the content index".to_owned(),
                kind: "DupeWithContent".to_owned(),
                start: 0,
                end: text.encode_utf16().count(),
                suggestion: None,
            }));
        };
        let (ids, rows) = content_candidates(view, content, text, &plan, scope, cancel)?;
        (ids, Rows::Built(rows))
    } else {
        let query = compile_ast(text, rest).map_err(DupeFailure::Invalid)?;
        let all = view.id_bound().saturating_add(1);
        let ids =
            search_ids(view, &query.query, all, scope, cancel).map_err(DupeFailure::Search)?;
        (ids, Rows::Query(query))
    };
    let (ids, candidates) = candidates(view, ids, cancel).map_err(DupeFailure::Search)?;
    let cache = cache();
    let options = Options {
        max_bytes: BYTE_BUDGET,
        max_time: TIME_BUDGET,
        threads: 0,
    };
    let report = athrix_dedupe::find_duplicates(&candidates, cache.as_deref(), &options, cancel)
        .map_err(|_| DupeFailure::Search(SearchFailure::Cancelled))?;
    if let Some(cache) = cache {
        prune_later(cache);
    }
    let mut items = Vec::new();
    let mut truncated = false;
    let generation = view.generation();
    'groups: for (n, group) in report.groups.iter().enumerate() {
        let count = u32::try_from(group.members.len()).unwrap_or(u32::MAX);
        let group_id = u32::try_from(n + 1).unwrap_or(u32::MAX);
        for &m in &group.members {
            if items.len() >= limit {
                truncated = true;
                break 'groups;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(DupeFailure::Search(SearchFailure::Cancelled));
            }
            let id = ids[m];
            let mut item = match &mut rows {
                Rows::Query(query) => build_item(view, generation, id, query),
                Rows::Built(built) => match built.remove(&id) {
                    Some(item) => item,
                    None => continue,
                },
            };
            item.dupe_group = Some(group_id);
            item.dupe_count = Some(count);
            items.push(item);
        }
    }
    let summary = DupeSummary {
        groups: report.groups.len(),
        files: report.groups.iter().map(|g| g.members.len()).sum(),
        wasted_bytes: report.wasted(),
        incomplete: report.incomplete,
        skipped: report.skipped,
    };
    Ok(DupeHits {
        items,
        truncated,
        summary,
    })
}

/// How result rows are made: from the name query, or already built by the content search.
enum Rows {
    Query(CompiledQuery),
    Built(HashMap<EntryId, ResultItem>),
}

/// The entries the content search finds for `plan` (at most [`CONTENT_CANDIDATES`]), and
/// their rows (with `matched` and, for the first ones, a snippet).
fn content_candidates(
    view: &IndexView,
    content: &ContentService,
    text: &str,
    plan: &SearchPlan,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<(Vec<EntryId>, HashMap<EntryId, ResultItem>), DupeFailure> {
    let name = match plan {
        SearchPlan::NameAndContent { name, .. } => name.as_ref(),
        SearchPlan::NameOrContent { name, .. } => Some(name),
        SearchPlan::Name(_) | SearchPlan::Empty => None,
    };
    let name_query = name
        .map(|q| compile_ast(text, q))
        .transpose()
        .map_err(DupeFailure::Invalid)?;
    let hits = content
        .search(
            view,
            plan,
            name_query.as_ref(),
            CONTENT_CANDIDATES,
            scope,
            cancel,
        )
        .map_err(DupeFailure::Search)?;
    if hits.truncated {
        tracing::debug!(
            limit = CONTENT_CANDIDATES,
            "dupe: with content: looks at the first content matches only"
        );
    }
    let generation = view.generation() & GENERATION_MASK;
    let mut ids = Vec::with_capacity(hits.items.len());
    let mut rows = HashMap::with_capacity(hits.items.len());
    for item in hits.items {
        if let Some((g, id)) = decode_id(item.id) {
            if g == generation && rows.insert(id, item).is_none() {
                ids.push(id);
            }
        }
    }
    Ok((ids, rows))
}

/// The files among `ids` that may have a copy: not folders, not empty, and of a size shared
/// with another of them. Paths are only built for those.
fn candidates(
    view: &IndexView,
    ids: Vec<EntryId>,
    cancel: &AtomicBool,
) -> Result<(Vec<EntryId>, Vec<Candidate>), SearchFailure> {
    let sized: Vec<(EntryId, u64)> = ids
        .into_iter()
        .filter_map(|id| {
            let meta = view.meta(id);
            (!meta.is_dir && meta.size > 0).then_some((id, meta.size))
        })
        .collect();
    let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    for &(_, size) in &sized {
        *counts.entry(size).or_default() += 1;
    }
    let mut out_ids = Vec::new();
    let mut out = Vec::new();
    for (n, (id, size)) in sized.into_iter().enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        if counts.get(&size).copied().unwrap_or(0) < 2 {
            continue;
        }
        let path = view.path(id);
        out.push(Candidate {
            key: path_key(&path),
            path,
            size,
        });
        out_ids.push(id);
    }
    Ok((out_ids, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::walk_options;
    use athrix_fs_index::{LiveIndex, walk};
    use std::fs;

    fn split(text: &str) -> Result<Option<String>, String> {
        let ast = athrix_query::parse(text).unwrap();
        split_dupe(text, &ast)
            .map(|q| q.map(|q| format!("{:?}", q.root.map(|r| r.span))))
            .map_err(|e| e.kind)
    }

    #[test]
    fn dupe_is_split_off_the_top_level_and_rejected_elsewhere() {
        assert_eq!(split("ext:jpg").unwrap(), None);
        assert_eq!(split("dupe:").unwrap(), Some("None".to_owned()));
        assert_eq!(split("مكرر:").unwrap(), Some("None".to_owned()));
        let rest = split_dupe(
            "dupe: ext:jpg",
            &athrix_query::parse("dupe: ext:jpg").unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(rest.root.unwrap().kind, ExprKind::Filter(f) if f.name == FilterName::Ext)
        );
        let rest = split_dupe("a dupe: b", &athrix_query::parse("a dupe: b").unwrap())
            .unwrap()
            .unwrap();
        assert!(matches!(rest.root.unwrap().kind, ExprKind::And(xs) if xs.len() == 2));
        assert_eq!(split("a | dupe:").unwrap_err(), "DupeNotTopLevel");
        assert_eq!(split("-dupe: a").unwrap_err(), "DupeNotTopLevel");
        assert_eq!(split("<dupe: a>").unwrap_err(), "DupeNotTopLevel");
        assert_eq!(split("dupe:x a").unwrap_err(), "DupeTakesNoValue");
    }

    /// A walked index of a temp folder with some duplicate files.
    fn fixture() -> (tempfile::TempDir, IndexView) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("photos/old")).unwrap();
        fs::write(root.join("photos/a.jpg"), b"same picture").unwrap();
        fs::write(root.join("photos/old/a copy.jpg"), b"same picture").unwrap();
        fs::write(root.join("b.jpg"), b"same picture").unwrap();
        fs::write(root.join("other.jpg"), b"diff picture").unwrap(); // same size
        fs::write(root.join("big1.bin"), vec![7u8; 5000]).unwrap();
        fs::write(root.join("big2.bin"), vec![7u8; 5000]).unwrap();
        fs::write(root.join("empty1.txt"), b"").unwrap();
        fs::write(root.join("empty2.txt"), b"").unwrap();
        let roots = vec![root.to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let view = IndexView::new(1, live.snapshot(), &roots);
        (dir, view)
    }

    fn run(view: &IndexView, text: &str, scope: Option<&SearchScope>, limit: usize) -> DupeHits {
        let ast = athrix_query::parse(text).unwrap();
        let rest = split_dupe(text, &ast).unwrap().unwrap();
        match dupe_search(view, text, &rest, limit, scope, &AtomicBool::new(false)) {
            Ok(h) => h,
            Err(e) => panic!("{text}: {e:?}"),
        }
    }

    fn names(h: &DupeHits) -> Vec<(String, u32, u32)> {
        h.items
            .iter()
            .map(|i| (i.name.clone(), i.dupe_group.unwrap(), i.dupe_count.unwrap()))
            .collect()
    }

    #[test]
    fn finds_groups_biggest_waste_first_with_group_ids() {
        let (_dir, view) = fixture();
        let h = run(&view, "dupe:", None, 100);
        let got = names(&h);
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(&got[0].0[..3], "big");
        assert_eq!((got[0].1, got[0].2), (1, 2));
        assert_eq!((got[1].1, got[1].2), (1, 2));
        let jpgs: Vec<&str> = got[2..].iter().map(|g| g.0.as_str()).collect();
        assert!(jpgs.contains(&"a.jpg") && jpgs.contains(&"a copy.jpg") && jpgs.contains(&"b.jpg"));
        assert!(got[2..].iter().all(|g| g.1 == 2 && g.2 == 3));
        assert!(!jpgs.contains(&"other.jpg"));
        assert_eq!(h.summary.groups, 2);
        assert_eq!(h.summary.files, 5);
        assert_eq!(h.summary.wasted_bytes, 5000 + 2 * 12);
        assert!(!h.summary.incomplete);
        assert!(!h.truncated);
    }

    #[test]
    fn other_filters_and_the_scope_narrow_the_candidates() {
        let (dir, view) = fixture();
        let h = run(&view, "dupe: ext:jpg", None, 100);
        assert_eq!(h.items.len(), 3);
        // Within photos only two copies remain.
        let scope = SearchScope::parse(Some(&dir.path().join("photos").to_string_lossy()))
            .unwrap()
            .unwrap();
        let h = run(&view, "مكرر: ext:jpg", Some(&scope), 100);
        assert_eq!(h.items.len(), 2);
        assert!(h.items.iter().all(|i| i.dupe_count == Some(2)));
        let h = run(&view, "dupe: size:>1kb", None, 100);
        assert_eq!(h.items.len(), 2);
        // The limit cuts the list and says so.
        let h = run(&view, "dupe:", None, 3);
        assert_eq!(h.items.len(), 3);
        assert!(h.truncated);
    }

    #[test]
    fn content_without_a_content_service_is_an_error() {
        let (_dir, view) = fixture();
        let text = "dupe: content:x";
        let ast = athrix_query::parse(text).unwrap();
        let rest = split_dupe(text, &ast).unwrap().unwrap();
        match dupe_search(&view, text, &rest, 10, None, &AtomicBool::new(false)) {
            Err(DupeFailure::Invalid(e)) => assert_eq!(e.kind, "DupeWithContent"),
            other => panic!("{other:?}"),
        }
        // A content filter the planner refuses is reported as such.
        let text = "dupe: <a | content:x>";
        let ast = athrix_query::parse(text).unwrap();
        let rest = split_dupe(text, &ast).unwrap().unwrap();
        match dupe_search(&view, text, &rest, 10, None, &AtomicBool::new(false)) {
            Err(DupeFailure::Invalid(e)) => assert_eq!(e.kind, "ContentInOr"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn content_picks_the_candidates() {
        use crate::content::{ContentService, test_options};
        use crate::indexing::IndexObserver;

        // Not under a dot folder (`.tmp…`): the content indexer skips hidden folders.
        let dir = tempfile::Builder::new()
            .prefix("athrix-dupe-")
            .tempdir()
            .unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("copies")).unwrap();
        let contract = "عقد الإيجار السنوي للشقة";
        fs::write(root.join("contract.txt"), contract).unwrap();
        fs::write(root.join("copies/contract (1).txt"), contract).unwrap();
        fs::write(root.join("copies/contract (2).txt"), contract).unwrap();
        // Same size and words, other bytes: not a copy.
        fs::write(root.join("other.txt"), "عقد الإيجار السنوي للشقه").unwrap();
        // Copies of a text without the words: not candidates.
        fs::write(root.join("notes1.txt"), "shopping list").unwrap();
        fs::write(root.join("notes2.txt"), "shopping list").unwrap();
        let roots = vec![root.to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let view = Arc::new(IndexView::new(1, live.snapshot(), &roots));
        let app = tempfile::tempdir().unwrap();
        let content = ContentService::new(test_options());
        content.configure(app.path());
        content.base_ready(&view);
        assert!(content.wait_idle(Duration::from_secs(60)));

        let run = |text: &str| {
            let ast = athrix_query::parse(text).unwrap();
            let rest = split_dupe(text, &ast).unwrap().unwrap();
            let no = AtomicBool::new(false);
            dupe_search_with(&view, Some(&content), text, &rest, 50, None, &no)
                .unwrap_or_else(|e| panic!("{text}: {e:?}"))
        };
        let h = run("dupe: content:الإيجار");
        let mut got: Vec<String> = h.items.iter().map(|i| i.name.clone()).collect();
        got.sort();
        assert_eq!(
            got,
            ["contract (1).txt", "contract (2).txt", "contract.txt"]
        );
        assert!(
            h.items
                .iter()
                .all(|i| i.dupe_group == Some(1) && i.dupe_count == Some(3))
        );
        assert_eq!(h.summary.groups, 1);
        // Names narrow the content matches further.
        let h = run("dupe: content:الإيجار path:copies");
        assert_eq!(h.items.len(), 2, "{:?}", h.items);
        // No content match: no groups, although other copies exist.
        assert!(run("dupe: content:غيرموجود").items.is_empty());
        content.shutdown();
    }
}
