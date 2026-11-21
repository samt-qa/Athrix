//! Running a compiled query against the index and turning hits into [`ResultItem`]s.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use athrix_fs_index::{EntryId, FileIndex, IndexSnapshot, Query};

use crate::indexing::{GENERATION_MASK, IndexView};
use crate::model::{Matched, ResultItem};
use crate::query::CompiledQuery;
use crate::roots::{key_has_prefix, normalize_root, path_key};

/// Largest valid result id: generation and entry bits all set, below 2^53.
pub const MAX_RESULT_ID: u64 = ((GENERATION_MASK as u64) << 32) | u32::MAX as u64;

/// The id the UI gets for entry `entry` of the index of generation `generation`:
/// `generation << 32 | entry`. With generations of at most 21 bits it stays below 2^53, so a
/// JavaScript number holds it exactly.
pub fn encode_id(generation: u32, entry: EntryId) -> u64 {
    (u64::from(generation & GENERATION_MASK) << 32) | u64::from(entry)
}

/// Splits a result id into `(generation, entry)`; `None` if it cannot come from
/// [`encode_id`].
pub fn decode_id(id: u64) -> Option<(u32, EntryId)> {
    if id > MAX_RESULT_ID {
        return None;
    }
    let generation = u32::try_from(id >> 32).ok()?;
    let entry = u32::try_from(id & u64::from(u32::MAX)).ok()?;
    Some((generation, entry))
}

/// Why a search produced no result list.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchFailure {
    /// A newer request set the cancel flag.
    Cancelled,
    /// The index reported an unexpected error (English message).
    Failed(String),
}

/// Hits of one search.
#[derive(Debug)]
pub struct SearchHits {
    /// At most `limit` items.
    pub items: Vec<ResultItem>,
    /// The index had more than `limit` matches.
    pub truncated: bool,
}

/// Where to search: a folder (usually a drive root such as `D:\`); only entries under it are
/// returned. Comparison is case-insensitive on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope {
    path: PathBuf,
    key: String,
}

impl SearchScope {
    /// Parses the `scope` argument of `search`: `None`, `""` or blanks mean everywhere; `d:`,
    /// `D:/` and `D:\` all mean drive D; any other absolute path is a folder. A relative path is
    /// an error (English message).
    pub fn parse(scope: Option<&str>) -> Result<Option<SearchScope>, String> {
        let Some(raw) = scope.filter(|s| !s.trim().is_empty()) else {
            return Ok(None);
        };
        let path = normalize_root(raw).map_err(|e| format!("invalid search scope: {e}"))?;
        let key = path_key(&path);
        Ok(Some(SearchScope { path, key }))
    }

    /// The scope folder.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How much of the root with key `root_key` lies in this scope.
    fn coverage(&self, root_key: &str) -> Coverage {
        if key_has_prefix(root_key, &self.key) {
            Coverage::Whole
        } else if key_has_prefix(&self.key, root_key) {
            Coverage::Part
        } else {
            Coverage::None
        }
    }

    /// True if `path` (an entry's full path) is in the scope.
    pub fn contains(&self, path: &Path) -> bool {
        key_has_prefix(&path_key(path), &self.key)
    }
}

/// How much of one walked root a scope covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// The root is the scope or inside it: all its entries are in.
    Whole,
    /// The scope is a folder inside the root: check each matching entry's path.
    Part,
    /// Unrelated: skip the root.
    None,
}

/// Ids scanned per task by the scoped scan.
const SCOPED_CHUNK: usize = 32 * 1024;

/// Runs `query` against `view` and builds up to `limit` items, only from `scope` if one is
/// given. Asks for `limit + 1` ids to learn whether the list is truncated. Returns
/// [`SearchFailure::Cancelled`] if `cancel` is set before the items are built. The item ids
/// carry the view's generation and resolve only with that view.
pub fn run_search(
    view: &IndexView,
    query: &CompiledQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<SearchHits, SearchFailure> {
    let want = limit.saturating_add(1);
    let mut ids = search_ids(view, &query.query, want, scope, cancel)?;
    let truncated = ids.len() > limit;
    ids.truncate(limit);
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        items.push(build_item(view, view.generation(), id, query));
    }
    Ok(SearchHits { items, truncated })
}

/// Up to `limit` matching live ids of `view`, only under `scope` if given, in the order of
/// [`run_search`] (best first for a ranked query).
pub fn search_ids(
    view: &IndexView,
    query: &Query,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    match scope {
        None => full_search(view, query, limit, cancel),
        Some(scope) => scoped_search(view, query, limit, scope, cancel),
    }
}

/// How many entries (at most `limit`) match `query` in `scope`, without building items: the
/// cheap probe behind the "did you mean" suggestion.
pub fn count_matches(
    view: &IndexView,
    query: &Query,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<usize, SearchFailure> {
    let ids = search_ids(view, query, limit, scope, cancel)?;
    Ok(ids.len().min(limit))
}

fn full_search(
    index: &IndexSnapshot,
    query: &Query,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    match index.search(query, limit, cancel) {
        Ok(ids) => Ok(ids),
        Err(athrix_core::Error::Cancelled) => Err(SearchFailure::Cancelled),
        Err(e) => Err(SearchFailure::Failed(e.to_string())),
    }
}

/// Up to `limit` matching live ids under `scope`: entries added or updated since the base
/// first (as in [`IndexSnapshot::search`]), then base entries in index order. A ranked query
/// ([`Query::is_ranked`]) returns the best `limit` by score instead, ties in id order, like
/// [`IndexSnapshot::search`] (see [`ranked_scoped_search`]).
///
/// A scope covering every indexed root is a plain search. Otherwise:
/// - the delta (small: compaction bounds it) is scanned entry by entry and filtered by path;
/// - in the base, only the runs of the roots the scope touches are scanned, in parallel
///   chunks, skipping removed entries (tombstones), stopping once `limit` hits are found.
fn scoped_search(
    view: &IndexView,
    query: &Query,
    limit: usize,
    scope: &SearchScope,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    if !view.root_keys().is_empty()
        && view
            .root_keys()
            .iter()
            .all(|k| key_has_prefix(k, &scope.key))
    {
        return full_search(view, query, limit, cancel);
    }
    if query.is_ranked() {
        return ranked_scoped_search(view, query, limit, scope, cancel);
    }
    let mut out = scan_delta(view, query, limit, scope, cancel)?;
    if out.len() >= limit {
        return Ok(out);
    }
    let chunks = scope_chunks(view, scope);
    let threads = scan_threads();
    let want = limit - out.len();
    let snapshot: &IndexSnapshot = view;
    let mut base_hits: Vec<EntryId> = Vec::new();
    for wave in chunks.chunks(threads) {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        let parts: Vec<Option<Vec<EntryId>>> = std::thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|&(start, end, coverage)| {
                    s.spawn(move || {
                        scan_chunk(snapshot, query, start, end, coverage, scope, want, cancel)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(None))
                .collect()
        });
        for part in parts {
            base_hits.extend(part.ok_or(SearchFailure::Cancelled)?);
            if base_hits.len() >= want {
                base_hits.truncate(want);
                out.extend(base_hits);
                return Ok(out);
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(SearchFailure::Cancelled);
    }
    out.extend(base_hits);
    Ok(out)
}

/// The base id ranges `start..end` (at most [`SCOPED_CHUNK`] long) of the roots `scope`
/// touches, with how much of their root the scope covers.
fn scope_chunks(view: &IndexView, scope: &SearchScope) -> Vec<(usize, usize, Coverage)> {
    let mut chunks = Vec::new();
    let base_len = view.base().len();
    for run in view.roots() {
        let coverage = scope.coverage(&run.key);
        if coverage == Coverage::None {
            continue;
        }
        let (start, end) = (run.start as usize, (run.end as usize).min(base_len));
        let mut at = start;
        while at < end {
            let next = (at + SCOPED_CHUNK).min(end);
            chunks.push((at, next, coverage));
            at = next;
        }
    }
    chunks
}

/// Threads per wave of the scoped scan.
fn scan_threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get().min(16))
}

/// A scored hit: `(score, id)`.
type Scored = (u32, EntryId);

/// Keeps the best `limit` of `hits` (higher score first, ties by id), sorted that way.
fn keep_best(hits: &mut Vec<Scored>, limit: usize) {
    let key = |&(score, id): &Scored| (std::cmp::Reverse(score), id);
    if limit > 0 && hits.len() > limit {
        hits.select_nth_unstable_by_key(limit - 1, key);
    }
    hits.truncate(limit);
    hits.sort_unstable_by_key(key);
}

/// [`scoped_search`] for a ranked query: every matching live entry under `scope` (the delta
/// and the base runs the scope touches) is scored with [`IndexSnapshot::score`]; the best
/// `limit` are returned, best first, ties by id (the order [`IndexSnapshot::search`] uses).
/// Each chunk keeps only its best `limit`, so memory stays bounded; there is no early stop.
fn ranked_scoped_search(
    view: &IndexView,
    query: &Query,
    limit: usize,
    scope: &SearchScope,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let snapshot: &IndexSnapshot = view;
    let mut best: Vec<Scored> = Vec::new();
    if snapshot.delta_entries() > 0 {
        for (n, id) in (snapshot.base().len()..snapshot.id_bound()).enumerate() {
            if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                return Err(SearchFailure::Cancelled);
            }
            let Ok(id) = EntryId::try_from(id) else {
                break;
            };
            // `contains` masks the hidden anchor copies of base folders.
            if !snapshot.contains(id) {
                continue;
            }
            let Some(score) = snapshot.score(query, id) else {
                continue;
            };
            if scope.contains(&snapshot.path(id)) {
                best.push((score, id));
                if best.len() >= limit.saturating_mul(2) {
                    keep_best(&mut best, limit);
                }
            }
        }
    }
    let chunks = scope_chunks(view, scope);
    for wave in chunks.chunks(scan_threads()) {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        let parts: Vec<Option<Vec<Scored>>> = std::thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|&(start, end, coverage)| {
                    s.spawn(move || {
                        let chunk = Chunk {
                            start,
                            end,
                            coverage,
                        };
                        scan_chunk_ranked(snapshot, query, chunk, scope, limit, cancel)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(None))
                .collect()
        });
        for part in parts {
            best.extend(part.ok_or(SearchFailure::Cancelled)?);
        }
        keep_best(&mut best, limit);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(SearchFailure::Cancelled);
    }
    keep_best(&mut best, limit);
    Ok(best.into_iter().map(|(_, id)| id).collect())
}

/// Base ids `start..end` of one root run, and how much of the root the scope covers.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    start: usize,
    end: usize,
    coverage: Coverage,
}

/// The best `limit` scored, matching live base ids of `chunk` under `scope`; `None` if
/// cancelled (or on a panic).
fn scan_chunk_ranked(
    index: &IndexSnapshot,
    query: &Query,
    chunk: Chunk,
    scope: &SearchScope,
    limit: usize,
    cancel: &AtomicBool,
) -> Option<Vec<Scored>> {
    let base: &FileIndex = index.base();
    let mut out: Vec<Scored> = Vec::new();
    for (n, id) in (chunk.start..chunk.end).enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let id = EntryId::try_from(id).ok()?;
        let Some(score) = base.score(query, id) else {
            continue;
        };
        if index.contains(id)
            && (chunk.coverage == Coverage::Whole || scope.contains(&base.path(id)))
        {
            out.push((score, id));
            if out.len() >= limit.saturating_mul(2) {
                keep_best(&mut out, limit);
            }
        }
    }
    keep_best(&mut out, limit);
    Some(out)
}

/// Matching live delta entries (ids `base.len()..id_bound()`) under `scope`, at most `limit`.
fn scan_delta(
    index: &IndexSnapshot,
    query: &Query,
    limit: usize,
    scope: &SearchScope,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    let mut out = Vec::new();
    if index.delta_entries() == 0 {
        return Ok(out);
    }
    for (n, id) in (index.base().len()..index.id_bound()).enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        let Ok(id) = EntryId::try_from(id) else {
            break;
        };
        // `contains` masks the hidden anchor copies of base folders.
        if index.contains(id) && index.is_match(query, id) && scope.contains(&index.path(id)) {
            out.push(id);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Matching live base ids among `start..end` (at most `limit`); `None` if cancelled (or on
/// a panic).
#[allow(clippy::too_many_arguments)]
fn scan_chunk(
    index: &IndexSnapshot,
    query: &Query,
    start: usize,
    end: usize,
    coverage: Coverage,
    scope: &SearchScope,
    limit: usize,
    cancel: &AtomicBool,
) -> Option<Vec<EntryId>> {
    let base: &FileIndex = index.base();
    let mut out = Vec::new();
    for (n, id) in (start..end).enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let id = EntryId::try_from(id).ok()?;
        if base.is_match(query, id)
            && index.contains(id)
            && (coverage == Coverage::Whole || scope.contains(&base.path(id)))
        {
            out.push(id);
            if out.len() >= limit {
                break;
            }
        }
    }
    Some(out)
}

/// Builds the UI row for the live entry `id` of `index`, a snapshot of generation
/// `generation`.
pub fn build_item(
    index: &IndexSnapshot,
    generation: u32,
    id: EntryId,
    query: &CompiledQuery,
) -> ResultItem {
    build_row(index, generation, id, |name| query.highlights(name))
}

/// Builds the UI row for the live entry `id` of `index` (generation `generation`), with the
/// name highlights `highlights` computes from the original name. `matched` is
/// [`Matched::Name`] and `snippet` is `None`; content search sets them afterwards.
pub fn build_row(
    index: &IndexSnapshot,
    generation: u32,
    id: EntryId,
    highlights: impl FnOnce(&str) -> Vec<(usize, usize)>,
) -> ResultItem {
    let name = index.name(id);
    let meta = index.meta(id);
    let parent = index
        .parent(id)
        .map(|p| index.path(p).to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = if meta.is_dir {
        String::new()
    } else {
        Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    ResultItem {
        id: encode_id(generation, id),
        name: name.to_owned(),
        parent,
        is_dir: meta.is_dir,
        size: (!meta.is_dir).then_some(meta.size),
        modified: meta.modified,
        ext,
        highlights: highlights(name),
        matched: Matched::Name,
        snippet: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::walk_options;
    use crate::query::compile_query;
    use athrix_fs_index::{EntryMeta, IndexBuilder, LiveIndex, walk};
    use std::fs;
    use std::path::PathBuf;

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    fn file(size: u64) -> EntryMeta {
        EntryMeta {
            size,
            modified: Some(1_700_000_000),
            ..Default::default()
        }
    }

    fn sample() -> IndexView {
        IndexView::from_index(7, sample_index())
    }

    fn sample_index() -> FileIndex {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/data", dir());
        let docs = b.push(Some(root), "تقارير", dir());
        b.push(Some(docs), "تَقْرِيرُ_Report.PDF", file(42));
        b.push(Some(docs), "تقرير قديم", file(7));
        b.push(Some(docs), ".gitignore", file(1));
        b.finish()
    }

    #[test]
    fn builds_items_with_parent_ext_size_and_highlights() {
        let ix = sample();
        let q = compile_query("تقرير").unwrap();
        let item = build_item(&ix, ix.generation(), 2, &q);
        let parent: PathBuf = ["/data", "تقارير"].iter().collect();
        assert_eq!(item.id, (7 << 32) | 2);
        assert_eq!(decode_id(item.id), Some((7, 2)));
        assert_eq!(item.name, "تَقْرِيرُ_Report.PDF");
        assert_eq!(item.parent, parent.to_string_lossy());
        assert!(!item.is_dir);
        assert_eq!(item.size, Some(42));
        assert_eq!(item.modified, Some(1_700_000_000));
        assert_eq!(item.ext, "pdf");
        assert_eq!(item.highlights, vec![(0, "تَقْرِيرُ".encode_utf16().count())]);

        let none = compile_query("").unwrap();
        let folder = build_item(&ix, 0, 1, &none);
        assert!(folder.is_dir);
        assert_eq!(folder.size, None);
        assert_eq!(folder.ext, "");
        assert_eq!(folder.parent, "/data");

        assert_eq!(build_item(&ix, 0, 4, &none).ext, "");
        assert_eq!(build_item(&ix, 0, 0, &none).parent, "");
    }

    #[test]
    fn limit_plus_one_sets_truncated() {
        let ix = sample();
        let q = compile_query("تقرير").unwrap();
        let cancel = AtomicBool::new(false);
        let all = run_search(&ix, &q, 10, None, &cancel).unwrap();
        assert_eq!(all.items.len(), 2);
        assert!(!all.truncated);
        let one = run_search(&ix, &q, 1, None, &cancel).unwrap();
        assert_eq!(one.items.len(), 1);
        assert!(one.truncated);
        let exact = run_search(&ix, &q, 2, None, &cancel).unwrap();
        assert!(!exact.truncated);
    }

    #[test]
    fn a_set_cancel_flag_cancels() {
        let ix = sample();
        let q = compile_query("تقرير").unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            run_search(&ix, &q, 10, None, &cancel).unwrap_err(),
            SearchFailure::Cancelled
        );
    }

    /// Two walked roots like two drives: `C:\` (or `/c`) and `D:\` (or `/d`), each with a
    /// folder and two "report" files.
    fn drives() -> (IndexView, [&'static str; 2]) {
        let (c, d) = if cfg!(windows) {
            ("C:\\", "D:\\")
        } else {
            ("/c", "/d")
        };
        let mut b = IndexBuilder::new();
        for drive in [c, d] {
            let root = b.push(None, drive, dir());
            let work = b.push(Some(root), "Work", dir());
            b.push(Some(work), "report.pdf", file(1));
            b.push(Some(root), "report.txt", file(2));
        }
        (IndexView::from_index(1, b.finish()), [c, d])
    }

    fn scoped(ix: &IndexView, text: &str, scope: &str, limit: usize) -> Vec<String> {
        let q = compile_query(text).unwrap();
        let scope = SearchScope::parse(Some(scope)).unwrap();
        let cancel = AtomicBool::new(false);
        run_search(ix, &q, limit, scope.as_ref(), &cancel)
            .unwrap()
            .items
            .iter()
            .map(|i| {
                let parent = PathBuf::from(&i.parent);
                parent.join(&i.name).to_string_lossy().into_owned()
            })
            .collect()
    }

    #[test]
    fn scope_returns_only_entries_under_the_chosen_root() {
        let (ix, [c, d]) = drives();
        assert_eq!(ix.roots().len(), 2);
        assert_eq!((ix.roots()[0].start, ix.roots()[0].end), (0, 4));
        assert_eq!((ix.roots()[1].start, ix.roots()[1].end), (4, 8));
        let under = |root: &str, hits: &[String]| {
            hits.iter()
                .all(|h| path_key(Path::new(h)).starts_with(&path_key(Path::new(root))))
        };

        let everywhere = scoped(&ix, "report", "", 10);
        assert_eq!(everywhere.len(), 4);
        let only_d = scoped(&ix, "report", d, 10);
        assert_eq!(only_d.len(), 2, "{only_d:?}");
        assert!(under(d, &only_d), "{only_d:?}");
        let only_c = scoped(&ix, "report", c, 10);
        assert_eq!(only_c.len(), 2);
        assert!(under(c, &only_c), "{only_c:?}");

        // A folder inside a root: per-entry path check.
        let work: PathBuf = [d, "Work"].iter().collect();
        let in_work = scoped(&ix, "report", &work.to_string_lossy(), 10);
        assert_eq!(in_work.len(), 1, "{in_work:?}");
        assert!(in_work[0].ends_with("report.pdf"));

        // A scope outside every root: nothing, not an error.
        let elsewhere = if cfg!(windows) { "Q:\\" } else { "/q" };
        assert!(scoped(&ix, "report", elsewhere, 10).is_empty());

        // Truncation still works within a scope.
        let q = compile_query("report").unwrap();
        let scope = SearchScope::parse(Some(d)).unwrap();
        let hits = run_search(&ix, &q, 1, scope.as_ref(), &AtomicBool::new(false)).unwrap();
        assert_eq!(hits.items.len(), 1);
        assert!(hits.truncated);
        // And so does cancellation.
        assert_eq!(
            run_search(&ix, &q, 10, scope.as_ref(), &AtomicBool::new(true)).unwrap_err(),
            SearchFailure::Cancelled
        );
    }

    #[cfg(windows)]
    #[test]
    fn drive_scopes_ignore_letter_case_and_separators() {
        let (ix, _) = drives();
        for scope in ["d:", "D:", "d:\\", "D:/", r" d:\ "] {
            let hits = scoped(&ix, "report", scope, 10);
            assert_eq!(hits.len(), 2, "{scope}: {hits:?}");
            assert!(
                hits.iter().all(|h| h.starts_with("D:\\")),
                "{scope}: {hits:?}"
            );
        }
        assert_eq!(scoped(&ix, "report", r"d:\WORK", 10).len(), 1);
    }

    #[test]
    fn invalid_scopes_are_rejected_and_blank_means_everywhere() {
        assert_eq!(SearchScope::parse(None).unwrap(), None);
        assert_eq!(SearchScope::parse(Some("  ")).unwrap(), None);
        for bad in ["relative/folder", "D", "..", "C:relative"] {
            let err = SearchScope::parse(Some(bad)).unwrap_err();
            assert!(err.starts_with("invalid search scope"), "{bad}: {err}");
        }
        let ok = if cfg!(windows) { "e:" } else { "/media/usb" };
        let scope = SearchScope::parse(Some(ok)).unwrap().unwrap();
        assert!(scope.path().is_absolute());
    }

    #[test]
    fn scoped_scan_spans_several_chunks_in_order() {
        // More entries than one chunk so the parallel waves and the early stop are exercised.
        let mut b = IndexBuilder::new();
        let (c, d) = if cfg!(windows) {
            ("C:\\", "D:\\")
        } else {
            ("/c", "/d")
        };
        for drive in [c, d] {
            let root = b.push(None, drive, dir());
            for i in 0..(SCOPED_CHUNK * 2 + 10) {
                b.push(Some(root), &format!("f{i}.txt"), file(1));
            }
        }
        let ix = IndexView::from_index(1, b.finish());
        let q = compile_query("ext:txt").unwrap();
        let scope = SearchScope::parse(Some(d)).unwrap();
        let no = AtomicBool::new(false);
        let all = run_search(&ix, &q, MAX, scope.as_ref(), &no).unwrap();
        assert_eq!(all.items.len(), SCOPED_CHUNK * 2 + 10);
        assert!(!all.truncated);
        let ids: Vec<u32> = all
            .items
            .iter()
            .map(|i| decode_id(i.id).unwrap().1)
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "index order");
        assert_eq!(ids[0] as usize, SCOPED_CHUNK * 2 + 12, "first file of D");
        let some = run_search(&ix, &q, 5, scope.as_ref(), &no).unwrap();
        assert_eq!(some.items.len(), 5);
        assert!(some.truncated);
    }

    const MAX: usize = 1_000_000;

    /// The highlighted text of a row, as JavaScript would slice it.
    fn highlighted(item: &ResultItem) -> String {
        let units: Vec<u16> = item.name.encode_utf16().collect();
        item.highlights
            .iter()
            .map(|&(s, e)| String::from_utf16(&units[s..e]).unwrap())
            .collect()
    }

    #[test]
    fn fuzzy_hits_are_highlighted() {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/data", dir());
        b.push(Some(root), "report.pdf", file(1));
        b.push(Some(root), "Monthly Report Summary.xlsx", file(1));
        b.push(Some(root), "تقرير 😀 report.txt", file(1));
        let ix = IndexView::from_index(1, b.finish());
        let row = |text: &str, id: EntryId| build_item(&ix, 1, id, &compile_query(text).unwrap());

        let item = row("fuzzy:rprt", 1);
        assert_eq!(highlighted(&item), "rprt", "{:?}", item.highlights);
        assert_eq!(item.highlights, vec![(0, 1), (2, 3), (4, 6)]);
        // The switch makes plain words fuzzy too.
        assert_eq!(highlighted(&row("fuzzy: rprt", 1)), "rprt");
        // An acronym highlights the initials.
        assert_eq!(highlighted(&row("fuzzy:mrs", 2)), "MRS");
        // Mixed with a plain term (and after a surrogate pair): merged, sorted, no overlap.
        let item = row("تقرير fuzzy:rprt", 3);
        let h = &item.highlights;
        assert!(h.windows(2).all(|w| w[0].1 < w[1].0), "{h:?}");
        assert_eq!(highlighted(&item), "تقريرrprt");
    }

    /// Two drives with names that match `fuzzy:rprt` more or less well, in an order where the
    /// best one is not first.
    fn ranked_drives() -> (IndexView, [&'static str; 2]) {
        let (c, d) = if cfg!(windows) {
            ("C:\\", "D:\\")
        } else {
            ("/c", "/d")
        };
        let mut b = IndexBuilder::new();
        for drive in [c, d] {
            let root = b.push(None, drive, dir());
            let work = b.push(Some(root), "Work", dir());
            for name in [
                "r_x_p_x_r_x_t_long_name.txt",
                "my report.txt",
                "unrelated.txt",
                "rprt.txt",
                "Report.doc",
            ] {
                b.push(Some(work), name, file(1));
            }
        }
        (IndexView::from_index(1, b.finish()), [c, d])
    }

    /// What `run_search` returns: `(entry id, score)` pairs.
    fn ranked(ix: &IndexView, text: &str, scope: Option<&str>, limit: usize) -> Vec<(u32, u32)> {
        let q = compile_query(text).unwrap();
        let scope = scope.map(|s| SearchScope::parse(Some(s)).unwrap().unwrap());
        run_search(ix, &q, limit, scope.as_ref(), &AtomicBool::new(false))
            .unwrap()
            .items
            .iter()
            .map(|i| {
                let id = decode_id(i.id).unwrap().1;
                (id, ix.score(&q.query, id).unwrap())
            })
            .collect()
    }

    fn best_first(hits: &[(u32, u32)]) -> bool {
        hits.windows(2)
            .all(|w| w[0].1 > w[1].1 || (w[0].1 == w[1].1 && w[0].0 < w[1].0))
    }

    #[test]
    fn scoped_ranked_search_returns_the_best_by_score() {
        let (ix, [_, d]) = ranked_drives();
        let q = compile_query("fuzzy:rprt").unwrap();
        assert!(q.query.is_ranked());
        let everywhere = ranked(&ix, "fuzzy:rprt", None, MAX);
        assert!(best_first(&everywhere), "{everywhere:?}");
        let on_d: Vec<(u32, u32)> = everywhere
            .iter()
            .copied()
            .filter(|&(id, _)| path_key(&ix.path(id)).starts_with(&path_key(Path::new(d))))
            .collect();
        assert!(on_d.len() >= 3, "{on_d:?}");

        // Whole root and folder inside it (per-entry path check): same order as unscoped.
        let scoped_d = ranked(&ix, "fuzzy:rprt", Some(d), MAX);
        assert_eq!(scoped_d, on_d);
        let work: PathBuf = [d, "Work"].iter().collect();
        assert_eq!(
            ranked(&ix, "fuzzy:rprt", Some(&work.to_string_lossy()), MAX),
            on_d
        );
        // Not index order: the best match is not the first matching entry.
        let first_in_index = on_d.iter().map(|&(id, _)| id).min().unwrap();
        assert_ne!(
            on_d[0].0, first_in_index,
            "the test data must exercise ranking"
        );

        // The top `limit`, not the first `limit` found.
        for limit in [1, 2] {
            let q = compile_query("fuzzy:rprt").unwrap();
            let scope = SearchScope::parse(Some(d)).unwrap();
            let hits = run_search(&ix, &q, limit, scope.as_ref(), &AtomicBool::new(false)).unwrap();
            let ids: Vec<u32> = hits
                .items
                .iter()
                .map(|i| decode_id(i.id).unwrap().1)
                .collect();
            let expected: Vec<u32> = on_d.iter().take(limit).map(|&(id, _)| id).collect();
            assert_eq!(ids, expected);
            assert!(hits.truncated);
        }
        assert_eq!(
            run_search(
                &ix,
                &q,
                10,
                SearchScope::parse(Some(d)).unwrap().as_ref(),
                &AtomicBool::new(true)
            )
            .unwrap_err(),
            SearchFailure::Cancelled
        );
    }

    #[test]
    fn keep_best_orders_by_score_then_id() {
        let mut hits = vec![(1, 5), (9, 7), (3, 2), (9, 1), (1, 0), (5, 3)];
        keep_best(&mut hits, 3);
        assert_eq!(hits, vec![(9, 1), (9, 7), (5, 3)]);
        let mut few = vec![(1, 2), (2, 1)];
        keep_best(&mut few, 10);
        assert_eq!(few, vec![(2, 1), (1, 2)]);
        keep_best(&mut few, 0);
        assert!(few.is_empty());
    }

    /// A ranked scoped search mixes delta and base hits by score.
    #[test]
    fn scoped_ranked_search_merges_live_changes_by_score() {
        let c = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        for dir in [&c, &d] {
            fs::write(dir.path().join("r_x_p_x_r_x_t_long.txt"), b"x").unwrap();
            fs::write(dir.path().join("my report.txt"), b"x").unwrap();
        }
        let roots = vec![c.path().to_path_buf(), d.path().to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let created = d.path().join("rprt.txt");
        fs::write(&created, b"x").unwrap();
        live.apply_paths(&[&created]);
        let view = IndexView::new(2, live.snapshot(), &roots);
        assert!(view.has_changes());

        let s = d.path().to_string_lossy().into_owned();
        let hits = ranked(&view, "fuzzy:rprt", Some(&s), MAX);
        assert!(best_first(&hits), "{hits:?}");
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(
            hits.iter()
                .all(|&(id, _)| view.path(id).starts_with(d.path()))
        );
        let everywhere = ranked(&view, "fuzzy:rprt", None, MAX);
        let on_d: Vec<(u32, u32)> = everywhere
            .into_iter()
            .filter(|&(id, _)| view.path(id).starts_with(d.path()))
            .collect();
        assert_eq!(hits, on_d);
        assert!(hits.iter().any(|&(id, _)| view.path(id) == created));
    }

    #[test]
    fn ids_round_trip_and_stay_javascript_safe() {
        const JS_MAX_SAFE: u64 = (1 << 53) - 1;
        const { assert!(MAX_RESULT_ID <= JS_MAX_SAFE) };
        for (generation, entry) in [
            (0, 0),
            (1, 0),
            (0, 1),
            (5, 123_456),
            (GENERATION_MASK, u32::MAX),
        ] {
            let id = encode_id(generation, entry);
            assert!(id <= JS_MAX_SAFE, "{id}");
            assert_eq!(decode_id(id), Some((generation, entry)));
        }
        assert_eq!(encode_id(GENERATION_MASK, u32::MAX), MAX_RESULT_ID);
        assert_eq!(decode_id(MAX_RESULT_ID + 1), None);
        assert_eq!(decode_id(u64::MAX), None);
        // The generation wraps within its width instead of leaking into unsafe bits.
        assert_eq!(encode_id(GENERATION_MASK + 1, 3), encode_id(0, 3));
    }

    /// Two walked folders standing in for two drives, then files created and deleted after
    /// the walk (as the watcher would report them).
    #[test]
    fn scoped_search_sees_live_changes_on_the_right_root_only() {
        let c = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        for dir in [&c, &d] {
            fs::create_dir_all(dir.path().join("Work")).unwrap();
            fs::write(dir.path().join("Work").join("report-old.txt"), b"x").unwrap();
        }
        // D first, so that after compaction its new entry is not adjacent to its run.
        let roots = vec![d.path().to_path_buf(), c.path().to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let created = d.path().join("Work").join("report-fresh.txt");
        fs::write(&created, b"x").unwrap();
        let deleted = c.path().join("Work").join("report-old.txt");
        fs::remove_file(&deleted).unwrap();
        let stats = live.apply_paths(&[&created, &deleted]);
        assert_eq!((stats.upserted, stats.removed), (1, 1), "{stats:?}");

        let check = |view: &IndexView| {
            let s = |p: &Path| p.to_string_lossy().into_owned();
            let mut on_d = scoped(view, "report", &s(d.path()), 10);
            on_d.sort();
            let expected_d = {
                let mut v = vec![
                    s(&created),
                    s(&d.path().join("Work").join("report-old.txt")),
                ];
                v.sort();
                v
            };
            assert_eq!(
                on_d, expected_d,
                "created file found with its drive's scope"
            );
            assert_eq!(
                scoped(view, "report", &s(&d.path().join("Work")), 10).len(),
                2,
                "and with a folder scope inside it"
            );
            assert!(
                scoped(view, "fresh", &s(c.path()), 10).is_empty(),
                "not with another drive's scope"
            );
            assert!(
                scoped(view, "report", &s(c.path()), 10).is_empty(),
                "the deleted file is gone"
            );
            assert!(scoped(view, "report", &s(&c.path().join("Work")), 10).is_empty());
            assert_eq!(scoped(view, "report", "", 10).len(), 2);
        };

        let view = IndexView::new(2, live.snapshot(), &roots);
        assert!(view.has_changes());
        check(&view);
        // The new file is a delta entry, listed first, and its id resolves in this view.
        let q = compile_query("report").unwrap();
        let scope = SearchScope::parse(Some(&d.path().to_string_lossy())).unwrap();
        let hits = run_search(&view, &q, 10, scope.as_ref(), &AtomicBool::new(false)).unwrap();
        let (generation, entry) = decode_id(hits.items[0].id).unwrap();
        assert_eq!(generation, 2);
        assert!(entry as usize >= view.base().len());
        assert!(view.has_entry(entry));
        assert_eq!(view.path(entry), created);
        // Truncation counts delta and base hits together.
        let one = run_search(&view, &q, 1, scope.as_ref(), &AtomicBool::new(false)).unwrap();
        assert_eq!(one.items.len(), 1);
        assert!(one.truncated);

        // After compaction the new entry sits after both roots' runs: still scoped right.
        live.compact();
        let view = IndexView::new(3, live.snapshot(), &roots);
        assert!(!view.has_changes());
        assert_eq!(view.roots().len(), 3, "{:?}", view.roots());
        check(&view);
    }
}
