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
//! Results are the members of each group, adjacent, biggest wasted space first; each row
//! carries `dupeGroup` / `dupeCount` and the response a [`DupeSummary`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use athrix_dedupe::{Candidate, HashCache, Options};
use athrix_fs_index::EntryId;
use athrix_query::{Expr, ExprKind, FilterName, FilterValue, PlanOptions, Query as AstQuery};
use tauri::{AppHandle, Manager};

use crate::indexing::IndexView;
use crate::model::{DupeSummary, QueryError, ResultItem};
use crate::query::{compile_ast, utf16_offset};
use crate::results::{SearchFailure, SearchScope, build_item, search_ids};
use crate::roots::path_key;

/// Folder under the app data dir holding the hash cache.
pub const CACHE_DIR: &str = "hashes";

/// The hash cache file inside [`CACHE_DIR`].
pub const CACHE_FILE: &str = "dupes.sqlite3";

/// Time budget of one `dupe:` search; when it runs out the response is `incomplete`.
pub const TIME_BUDGET: Duration = Duration::from_secs(6);

/// Byte budget of one `dupe:` search (cache hits are free).
pub const BYTE_BUDGET: u64 = 4 * 1024 * 1024 * 1024;

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

/// Runs a `dupe:` search: `rest` (the query without `dupe:`, see [`split_dupe`]) picks the
/// candidates under `scope`; returns the duplicate groups among them, at most `limit` rows.
pub fn dupe_search(
    view: &IndexView,
    text: &str,
    rest: &AstQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<DupeHits, DupeFailure> {
    // Content filters need the content index, which has no notion of duplicates yet.
    if let Ok(plan) = athrix_query::plan(rest, &PlanOptions::default()) {
        if plan.uses_content() {
            return Err(DupeFailure::Invalid(QueryError {
                message: "`dupe:` cannot be combined with `content:` yet".to_owned(),
                kind: "DupeWithContent".to_owned(),
                start: 0,
                end: text.encode_utf16().count(),
                suggestion: None,
            }));
        }
    }
    let query = compile_ast(text, rest).map_err(DupeFailure::Invalid)?;
    let all = view.id_bound().saturating_add(1);
    let ids = search_ids(view, &query.query, all, scope, cancel).map_err(DupeFailure::Search)?;
    let (ids, candidates) = candidates(view, ids, cancel).map_err(DupeFailure::Search)?;
    let cache = cache();
    let options = Options {
        max_bytes: BYTE_BUDGET,
        max_time: TIME_BUDGET,
        threads: 0,
    };
    let report = athrix_dedupe::find_duplicates(&candidates, cache.as_deref(), &options, cancel)
        .map_err(|_| DupeFailure::Search(SearchFailure::Cancelled))?;
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
            let mut item = build_item(view, generation, ids[m], &query);
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
    fn content_with_dupe_is_rejected() {
        let (_dir, view) = fixture();
        let text = "dupe: content:x";
        let ast = athrix_query::parse(text).unwrap();
        let rest = split_dupe(text, &ast).unwrap().unwrap();
        match dupe_search(&view, text, &rest, 10, None, &AtomicBool::new(false)) {
            Err(DupeFailure::Invalid(e)) => assert_eq!(e.kind, "DupeWithContent"),
            other => panic!("{other:?}"),
        }
    }
}
