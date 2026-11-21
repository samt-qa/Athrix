//! Path keys, prefix matching and search scopes ([`PathFilter`]).
//!
//! Every document has a `pkey` field next to its `path` (the path in the form used for
//! identity and prefix matching, produced by [`path_key`]) and a `phash` fast column (its
//! hash, for path sets). The case / separator / prefix rules are in the crate docs.

use std::borrow::Cow;
use std::ops::Bound;
use std::sync::Arc;
use tantivy::query::{
    BooleanQuery, ConstScorer, EnableScoring, Explanation, Query, RangeQuery, Scorer, TermQuery,
    Weight,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED, Term};

/// Name of the fast column holding [`path_hash`] of each document's key.
pub(crate) const PATH_HASH_FIELD: &str = "phash";

/// The identity / prefix-matching form of `path` on this platform: on Windows `\` → `/` and
/// lowercase; elsewhere unchanged. See the [crate docs](crate#paths-case-and-separators).
pub fn path_key(path: &str) -> Cow<'_, str> {
    #[cfg(windows)]
    {
        let changes = |c: char| {
            if c == '\\' {
                return true;
            }
            let mut lower = c.to_lowercase();
            lower.next() != Some(c) || lower.next().is_some()
        };
        if path.chars().any(changes) {
            Cow::Owned(path.replace('\\', "/").to_lowercase())
        } else {
            Cow::Borrowed(path)
        }
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

/// 64-bit hash of a path key (FNV-1a, then a splitmix64 finalizer so every bit is mixed).
pub(crate) fn path_hash(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// The query matching every document whose key is `prefix` (already a key) or under it.
/// `None` when the prefix is empty.
pub(crate) fn prefix_query(field: Field, prefix: &str) -> Option<Box<dyn Query>> {
    if prefix.is_empty() {
        return None;
    }
    // Keys under the folder are the range [folder + "/", folder + "0"): '0' follows '/'.
    let (exact, folder) = match prefix.strip_suffix('/') {
        Some(folder) => (None, folder),
        None => (Some(prefix), prefix),
    };
    let range = RangeQuery::new(
        Bound::Included(Term::from_field_text(field, &format!("{folder}/"))),
        Bound::Excluded(Term::from_field_text(field, &format!("{folder}0"))),
    );
    let Some(exact) = exact else {
        return Some(Box::new(range));
    };
    let exact = TermQuery::new(
        Term::from_field_text(field, exact),
        IndexRecordOption::Basic,
    );
    Some(Box::new(BooleanQuery::union(vec![
        Box::new(range),
        Box::new(exact),
    ])))
}

/// Restricts a search to a set of files or to folders (drive or folder scope).
///
/// The filter is applied inside tantivy as a required clause with score 0, so a scoped query
/// returns exactly the top hits *within* the scope (BM25 scores unchanged), with no "fetch
/// more and filter" rounds.
///
/// - A **set** is a hash table of 64-bit hashes of the path keys (16 bytes per path; 100k
///   paths take about 2 MB). Per segment, the `phash` fast column is scanned once and probed,
///   giving the doc set: O(documents), independent of the set size. Two different paths with
///   the same 64-bit hash would let one through by mistake; with 100k paths against 1M
///   documents the chance is about 10⁻⁸.
/// - A **prefix** is a range over the sorted `pkey` term dictionary: O(documents under it).
///
/// Build it once and reuse it for every page of the same scope.
///
/// Paths and prefixes are compared through [`path_key`]: case-insensitive with either
/// separator on Windows, byte for byte elsewhere.
#[derive(Clone, Debug)]
pub struct PathFilter {
    /// `None`: matches nothing (empty set, empty prefix).
    query: Option<SharedQuery>,
}

impl PathFilter {
    /// Only these files (for example the name side of an AND query). Duplicates are fine; an
    /// empty set matches nothing.
    pub fn paths<I, S>(paths: I) -> PathFilter
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let hashes: Vec<u64> = paths
            .into_iter()
            .map(|p| path_hash(&path_key(p.as_ref())))
            .collect();
        if hashes.is_empty() {
            return PathFilter { query: None };
        }
        let set = PathHashSet::new(&hashes);
        PathFilter {
            query: Some(SharedQuery(Arc::new(PathSetQuery(Arc::new(set))))),
        }
    }

    /// Only files in the folder `prefix` or below it. `C:/` scopes to a drive. See the
    /// [crate docs](crate#paths-case-and-separators) for the folder-boundary rule.
    pub fn prefix(prefix: &str) -> PathFilter {
        PathFilter::prefixes([prefix])
    }

    /// Files in any of these folders (or below). Empty prefixes are ignored.
    pub fn prefixes<I, S>(prefixes: I) -> PathFilter
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let field = crate::Fields::schema().1.pkey;
        let mut parts: Vec<Box<dyn Query>> = prefixes
            .into_iter()
            .filter_map(|p| prefix_query(field, &path_key(p.as_ref())))
            .collect();
        let query: Option<Arc<dyn Query>> = match parts.len() {
            0 => None,
            1 => Some(Arc::from(parts.pop().expect("one part"))),
            _ => Some(Arc::new(BooleanQuery::union(parts))),
        };
        PathFilter {
            query: query.map(SharedQuery),
        }
    }

    /// True when the filter can match nothing (empty set or only empty prefixes).
    pub fn is_empty(&self) -> bool {
        self.query.is_none()
    }

    pub(crate) fn query(&self) -> Option<Box<dyn Query>> {
        self.query
            .as_ref()
            .map(|q| Box::new(q.clone()) as Box<dyn Query>)
    }
}

/// A query behind an `Arc`, so a large filter is not deep-copied for each search.
#[derive(Clone, Debug)]
struct SharedQuery(Arc<dyn Query>);

impl Query for SharedQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        self.0.weight(enable_scoring)
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.0.query_terms(visitor);
    }
}

/// Open-addressing set of u64 (already well-mixed hashes), linear probing, load <= 0.5.
struct PathHashSet {
    slots: Vec<u64>,
    mask: usize,
    /// 0 marks an empty slot, so the value 0 is kept aside.
    has_zero: bool,
    len: usize,
}

impl PathHashSet {
    fn new(values: &[u64]) -> PathHashSet {
        let cap = (values.len() * 2).next_power_of_two().max(16);
        let mut set = PathHashSet {
            slots: vec![0; cap],
            mask: cap - 1,
            has_zero: false,
            len: 0,
        };
        for &v in values {
            set.insert(v);
        }
        set
    }

    fn insert(&mut self, v: u64) {
        if v == 0 {
            self.len += usize::from(!self.has_zero);
            self.has_zero = true;
            return;
        }
        let mut i = v as usize & self.mask;
        loop {
            match self.slots[i] {
                0 => {
                    self.slots[i] = v;
                    self.len += 1;
                    return;
                }
                s if s == v => return,
                _ => i = (i + 1) & self.mask,
            }
        }
    }

    fn contains(&self, v: u64) -> bool {
        if v == 0 {
            return self.has_zero;
        }
        let mut i = v as usize & self.mask;
        loop {
            match self.slots[i] {
                0 => return false,
                s if s == v => return true,
                _ => i = (i + 1) & self.mask,
            }
        }
    }
}

/// Matches documents whose `phash` is in the set.
#[derive(Clone)]
struct PathSetQuery(Arc<PathHashSet>);

impl std::fmt::Debug for PathSetQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PathSetQuery({} paths)", self.0.len)
    }
}

impl Query for PathSetQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(PathSetWeight(Arc::clone(&self.0))))
    }
}

struct PathSetWeight(Arc<PathHashSet>);

impl Weight for PathSetWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        const CHUNK: u32 = 1024;
        let column = reader.fast_fields().u64(PATH_HASH_FIELD)?;
        let max_doc = reader.max_doc();
        let mut matching: Vec<DocId> = Vec::new();
        let mut ids: Vec<DocId> = Vec::with_capacity(CHUNK as usize);
        let mut vals: Vec<Option<u64>> = vec![None; CHUNK as usize];
        let mut start = 0;
        while start < max_doc {
            let end = max_doc.min(start + CHUNK);
            ids.clear();
            ids.extend(start..end);
            let vals = &mut vals[..ids.len()];
            column.first_vals(&ids, vals);
            matching.extend(
                ids.iter()
                    .zip(vals.iter())
                    .filter(|(_, v)| v.is_some_and(|h| self.0.contains(h)))
                    .map(|(&doc, _)| doc),
            );
            start = end;
        }
        Ok(Box::new(ConstScorer::new(
            SortedDocs {
                docs: matching,
                cursor: 0,
            },
            boost,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("PathFilter", scorer.score()))
        } else {
            Err(tantivy::TantivyError::InvalidArgument(
                "document is not in the path set".to_owned(),
            ))
        }
    }
}

/// A sorted list of doc ids as a [`DocSet`].
struct SortedDocs {
    docs: Vec<DocId>,
    cursor: usize,
}

impl DocSet for SortedDocs {
    fn advance(&mut self) -> DocId {
        self.cursor = (self.cursor + 1).min(self.docs.len());
        self.doc()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let rest = &self.docs[self.cursor..];
        self.cursor += rest.partition_point(|&d| d < target);
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        u32::try_from(self.docs.len() - self.cursor).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn windows_keys_fold_case_and_separators() {
        assert_eq!(path_key(r"C:\Docs\Report.PDF"), "c:/docs/report.pdf");
        assert_eq!(path_key("C:/DOCS\\تقرير.txt"), "c:/docs/تقرير.txt");
        assert!(matches!(path_key("c:/docs/تقرير.txt"), Cow::Borrowed(_)));
    }

    #[test]
    #[cfg(not(windows))]
    fn portable_keys_are_unchanged() {
        assert_eq!(path_key(r"/Docs/A\B.txt"), r"/Docs/A\B.txt");
        assert!(matches!(path_key("/Docs/x"), Cow::Borrowed(_)));
    }

    #[test]
    fn hash_set_membership() {
        let values: Vec<u64> = (0..1000u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect();
        let set = PathHashSet::new(&values);
        assert_eq!(set.len, 1000, "0 is a member too");
        assert!(values.iter().all(|&v| set.contains(v)));
        assert!(!set.contains(1));
        assert_ne!(path_hash("C:/a"), path_hash("C:/b"));
    }

    #[test]
    fn empty_filters_match_nothing() {
        assert!(PathFilter::paths(Vec::<String>::new()).is_empty());
        assert!(PathFilter::prefix("").is_empty());
        assert!(!PathFilter::prefix("/").is_empty());
        assert!(!PathFilter::paths(["a"]).is_empty());
    }
}
