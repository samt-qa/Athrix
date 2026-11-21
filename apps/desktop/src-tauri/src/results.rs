//! Running a compiled query against the index and turning hits into [`ResultItem`]s.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use athrix_fs_index::{EntryId, FileIndex, Query};

use crate::indexing::{GENERATION_MASK, IndexRoot, LiveIndex};
use crate::model::ResultItem;
use crate::query::{CompiledQuery, highlights};
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

    /// How much of the walked root `root` lies in this scope.
    fn coverage(&self, root: &IndexRoot) -> Coverage {
        if key_has_prefix(&root.key, &self.key) {
            Coverage::Whole
        } else if key_has_prefix(&self.key, &root.key) {
            Coverage::Part
        } else {
            Coverage::None
        }
    }

    /// True if `path` (an entry's full path) is in the scope.
    fn contains(&self, path: &Path) -> bool {
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

/// Runs `query` and builds up to `limit` items, only from `scope` if one is given. Asks for
/// `limit + 1` ids to learn whether the list is truncated. Returns [`SearchFailure::Cancelled`]
/// if `cancel` is set before the items are built.
pub fn run_search(
    index: &LiveIndex,
    query: &CompiledQuery,
    limit: usize,
    scope: Option<&SearchScope>,
    cancel: &AtomicBool,
) -> Result<SearchHits, SearchFailure> {
    let want = limit.saturating_add(1);
    let mut ids = match scope {
        None => full_search(index, &query.query, want, cancel)?,
        Some(scope) => scoped_search(index, &query.query, want, scope, cancel)?,
    };
    let truncated = ids.len() > limit;
    ids.truncate(limit);
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        items.push(build_item(
            index,
            index.generation(),
            id,
            &query.highlight_terms,
        ));
    }
    Ok(SearchHits { items, truncated })
}

fn full_search(
    index: &FileIndex,
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

/// Up to `limit` matching ids under `scope`, in index order. A scope covering every root is a
/// plain search; otherwise only the id blocks of the roots the scope touches are scanned, in
/// parallel chunks, stopping once `limit` hits are found.
fn scoped_search(
    index: &LiveIndex,
    query: &Query,
    limit: usize,
    scope: &SearchScope,
    cancel: &AtomicBool,
) -> Result<Vec<EntryId>, SearchFailure> {
    let covered: Vec<(&IndexRoot, Coverage)> = index
        .roots()
        .iter()
        .map(|r| (r, scope.coverage(r)))
        .filter(|(_, c)| *c != Coverage::None)
        .collect();
    if covered.len() == index.roots().len() && covered.iter().all(|(_, c)| *c == Coverage::Whole) {
        return full_search(index, query, limit, cancel);
    }
    let mut chunks: Vec<(usize, usize, Coverage)> = Vec::new();
    for (root, coverage) in covered {
        let (start, end) = (root.start as usize, (root.end as usize).min(index.len()));
        let mut at = start;
        while at < end {
            let next = (at + SCOPED_CHUNK).min(end);
            chunks.push((at, next, coverage));
            at = next;
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    let mut out: Vec<EntryId> = Vec::new();
    for wave in chunks.chunks(threads) {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        let parts: Vec<Option<Vec<EntryId>>> = std::thread::scope(|s| {
            let handles: Vec<_> = wave
                .iter()
                .map(|&(start, end, coverage)| {
                    s.spawn(move || {
                        scan_chunk(index, query, start, end, coverage, scope, limit, cancel)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(None))
                .collect()
        });
        for part in parts {
            out.extend(part.ok_or(SearchFailure::Cancelled)?);
            if out.len() >= limit {
                out.truncate(limit);
                return Ok(out);
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(SearchFailure::Cancelled);
    }
    Ok(out)
}

/// Matching ids among `start..end` (at most `limit`); `None` if cancelled (or on a panic).
#[allow(clippy::too_many_arguments)]
fn scan_chunk(
    index: &FileIndex,
    query: &Query,
    start: usize,
    end: usize,
    coverage: Coverage,
    scope: &SearchScope,
    limit: usize,
    cancel: &AtomicBool,
) -> Option<Vec<EntryId>> {
    let mut out = Vec::new();
    for (n, id) in (start..end).enumerate() {
        if n % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let id = EntryId::try_from(id).ok()?;
        if index.is_match(query, id)
            && (coverage == Coverage::Whole || scope.contains(&index.path(id)))
        {
            out.push(id);
            if out.len() >= limit {
                break;
            }
        }
    }
    Some(out)
}

/// Builds the UI row for `id` (which must be `< index.len()`) of an index of generation
/// `generation`.
pub fn build_item(
    index: &FileIndex,
    generation: u32,
    id: EntryId,
    highlight_terms: &[String],
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
        highlights: highlights(name, highlight_terms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::compile_query;
    use athrix_fs_index::{EntryMeta, IndexBuilder};
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

    fn sample() -> LiveIndex {
        LiveIndex::new(7, sample_index())
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
        let item = build_item(&ix, ix.generation(), 2, &q.highlight_terms);
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

        let folder = build_item(&ix, 0, 1, &[]);
        assert!(folder.is_dir);
        assert_eq!(folder.size, None);
        assert_eq!(folder.ext, "");
        assert_eq!(folder.parent, "/data");

        assert_eq!(build_item(&ix, 0, 4, &[]).ext, "");
        assert_eq!(build_item(&ix, 0, 0, &[]).parent, "");
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
    fn drives() -> (LiveIndex, [&'static str; 2]) {
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
        (LiveIndex::new(1, b.finish()), [c, d])
    }

    fn scoped(ix: &LiveIndex, text: &str, scope: &str, limit: usize) -> Vec<String> {
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
        let work: PathBuf = [d, "work"].iter().collect();
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
        let ix = LiveIndex::new(1, b.finish());
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
}
