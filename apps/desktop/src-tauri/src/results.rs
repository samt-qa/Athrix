//! Running a compiled query against the index and turning hits into [`ResultItem`]s.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use athrix_fs_index::{EntryId, FileIndex};

use crate::model::ResultItem;
use crate::query::{CompiledQuery, highlights};

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

/// Runs `query` and builds up to `limit` items. Asks the index for `limit + 1` ids to learn
/// whether the list is truncated. Returns [`SearchFailure::Cancelled`] if `cancel` is set
/// before the items are built.
pub fn run_search(
    index: &FileIndex,
    query: &CompiledQuery,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<SearchHits, SearchFailure> {
    let mut ids = match index.search(&query.query, limit.saturating_add(1), cancel) {
        Ok(ids) => ids,
        Err(athrix_core::Error::Cancelled) => return Err(SearchFailure::Cancelled),
        Err(e) => return Err(SearchFailure::Failed(e.to_string())),
    };
    let truncated = ids.len() > limit;
    ids.truncate(limit);
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if cancel.load(Ordering::Relaxed) {
            return Err(SearchFailure::Cancelled);
        }
        items.push(build_item(index, id, &query.highlight_terms));
    }
    Ok(SearchHits { items, truncated })
}

/// Builds the UI row for `id` (which must be `< index.len()`).
pub fn build_item(index: &FileIndex, id: EntryId, highlight_terms: &[String]) -> ResultItem {
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
        id,
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

    fn sample() -> FileIndex {
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
        let item = build_item(&ix, 2, &q.highlight_terms);
        let parent: PathBuf = ["/data", "تقارير"].iter().collect();
        assert_eq!(item.name, "تَقْرِيرُ_Report.PDF");
        assert_eq!(item.parent, parent.to_string_lossy());
        assert!(!item.is_dir);
        assert_eq!(item.size, Some(42));
        assert_eq!(item.modified, Some(1_700_000_000));
        assert_eq!(item.ext, "pdf");
        assert_eq!(item.highlights, vec![(0, "تَقْرِيرُ".encode_utf16().count())]);

        let folder = build_item(&ix, 1, &[]);
        assert!(folder.is_dir);
        assert_eq!(folder.size, None);
        assert_eq!(folder.ext, "");
        assert_eq!(folder.parent, "/data");

        assert_eq!(build_item(&ix, 4, &[]).ext, "");
        assert_eq!(build_item(&ix, 0, &[]).parent, "");
    }

    #[test]
    fn limit_plus_one_sets_truncated() {
        let ix = sample();
        let q = compile_query("تقرير").unwrap();
        let cancel = AtomicBool::new(false);
        let all = run_search(&ix, &q, 10, &cancel).unwrap();
        assert_eq!(all.items.len(), 2);
        assert!(!all.truncated);
        let one = run_search(&ix, &q, 1, &cancel).unwrap();
        assert_eq!(one.items.len(), 1);
        assert!(one.truncated);
        let exact = run_search(&ix, &q, 2, &cancel).unwrap();
        assert!(!exact.truncated);
    }

    #[test]
    fn a_set_cancel_flag_cancels() {
        let ix = sample();
        let q = compile_query("تقرير").unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            run_search(&ix, &q, 10, &cancel).unwrap_err(),
            SearchFailure::Cancelled
        );
    }
}
