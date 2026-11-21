//! Name search over the normalized-name arena.
//!
//! Query syntax (a subset of `docs/01-RESEARCH-AND-FEATURES.md` §5.1):
//!
//! - Words separated by spaces are ANDed; each word must match the entry's **name**.
//!   (Matching words against the full path is planned and not done yet.)
//! - `word` matches anywhere in the name (substring).
//! - `^word` matches at the start of the name, `word$` at the end, `^word$` the whole name.
//! - A word containing `*` (any run of characters) or `?` (exactly one character) is a
//!   wildcard pattern matched against the **whole** name, Everything-style: `*.pdf`, `فاتورة_??`.
//!
//! Both the query and the names go through the same Arabic/Latin normalization
//! ([`athrix_text_ar::normalize_name_into`]): case, diacritics, tatweel and alef/yaa variants are
//! ignored, Arabic-Indic digits equal ASCII digits, and punctuation (`.`, `_`, `-`, ...) becomes a
//! single space. Consequently `?` counts normalized characters, and punctuation at the very edge
//! of a name is dropped (`*.pdf` does not match a file named just `.pdf`).
//!
//! Results come back in index (walk) order; ranking/sorting is a later task.

use std::sync::atomic::{AtomicBool, Ordering};

use athrix_core::{Error, Result};
use memchr::memmem;
use rayon::prelude::*;

use crate::index::{EntryId, FileIndex};

/// Entries scanned per parallel task.
const CHUNK: usize = 16 * 1024;
/// How often (in entries) a task checks the cancellation flag.
const CANCEL_CHECK_EVERY: usize = 4 * 1024;

/// A parsed, normalized name query. Parse once per keystroke with [`Query::parse`], then run it
/// with [`FileIndex::search`].
#[derive(Debug, Clone)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Debug, Clone)]
enum Term {
    Contains(Box<memmem::Finder<'static>>),
    Prefix(String),
    Suffix(String),
    Exact(String),
    Glob(Vec<Segment>),
}

/// A run of pieces between two `*`.
type Segment = Vec<Piece>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Lit(String),
    AnyChar,
}

impl Query {
    /// Parses `input` (see the module docs for the syntax). Words that normalize to nothing
    /// (for example a lone `-`) are dropped; an empty query matches every entry.
    pub fn parse(input: &str) -> Self {
        let mut buf = String::new();
        let terms = input
            .split_whitespace()
            .filter_map(|word| parse_term(word, &mut buf))
            .collect();
        Query { terms }
    }

    /// True if the query has no terms and therefore matches everything.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// True if `normalized_name` (a name already passed through
    /// [`athrix_text_ar::normalize_name_into`]) matches every term.
    pub fn matches(&self, normalized_name: &str) -> bool {
        self.terms.iter().all(|t| t.matches(normalized_name))
    }
}

fn parse_term(word: &str, buf: &mut String) -> Option<Term> {
    if word.contains(['*', '?']) {
        // Anchors are implicit for wildcard patterns; ignore explicit ones.
        let word = word.strip_prefix('^').unwrap_or(word);
        let word = word.strip_suffix('$').unwrap_or(word);
        return parse_glob(word, buf);
    }
    let (anchored_start, rest) = match word.strip_prefix('^') {
        Some(r) => (true, r),
        None => (false, word),
    };
    let (anchored_end, rest) = match rest.strip_suffix('$') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    athrix_text_ar::normalize_name_into(rest, buf);
    if buf.is_empty() {
        return None;
    }
    let s = buf.clone();
    Some(match (anchored_start, anchored_end) {
        (false, false) => Term::Contains(Box::new(memmem::Finder::new(s.as_bytes()).into_owned())),
        (true, false) => Term::Prefix(s),
        (false, true) => Term::Suffix(s),
        (true, true) => Term::Exact(s),
    })
}

fn parse_glob(word: &str, buf: &mut String) -> Option<Term> {
    let mut segments: Vec<Segment> = Vec::new();
    for raw_segment in word.split('*') {
        let mut seg = Segment::new();
        for (i, lit) in raw_segment.split('?').enumerate() {
            if i > 0 {
                seg.push(Piece::AnyChar);
            }
            if !lit.is_empty() {
                let norm = normalize_inner(lit, buf);
                if !norm.is_empty() {
                    seg.push(Piece::Lit(norm));
                }
            }
        }
        segments.push(seg);
    }
    // Names are trimmed by normalization, so trim the pattern's outer edges the same way.
    if let Some(Piece::Lit(s)) = segments.first_mut().and_then(|s| s.first_mut()) {
        *s = s.trim_start().to_owned();
    }
    if let Some(Piece::Lit(s)) = segments.last_mut().and_then(|s| s.last_mut()) {
        *s = s.trim_end().to_owned();
    }
    for seg in &mut segments {
        seg.retain(|p| !matches!(p, Piece::Lit(s) if s.is_empty()));
    }
    // A pattern that is only stars matches everything: drop it like an empty word.
    if segments.iter().all(Vec::is_empty) {
        return None;
    }
    Some(Term::Glob(segments))
}

/// Normalizes a literal that sits between wildcards, keeping a leading/trailing separator as a
/// space (plain normalization would trim it, so `*.pdf` would also match `xpdf`).
fn normalize_inner(lit: &str, buf: &mut String) -> String {
    const SENTINEL: char = 'q';
    let wrapped = format!("{SENTINEL}{lit}{SENTINEL}");
    athrix_text_ar::normalize_name_into(&wrapped, buf);
    match buf
        .strip_prefix(SENTINEL)
        .and_then(|s| s.strip_suffix(SENTINEL))
    {
        Some(inner) => inner.to_owned(),
        None => {
            athrix_text_ar::normalize_name_into(lit, buf);
            buf.clone()
        }
    }
}

impl Term {
    #[inline]
    fn matches(&self, name: &str) -> bool {
        match self {
            Term::Contains(f) => f.find(name.as_bytes()).is_some(),
            Term::Prefix(p) => name.starts_with(p.as_str()),
            Term::Suffix(s) => name.ends_with(s.as_str()),
            Term::Exact(s) => name == s,
            Term::Glob(segs) => glob_match(name, segs),
        }
    }
}

/// Matches `seg` at byte `pos` of `hay`; returns the end position.
fn match_segment_at(hay: &str, mut pos: usize, seg: &[Piece]) -> Option<usize> {
    for piece in seg {
        match piece {
            Piece::Lit(s) => {
                if !hay[pos..].starts_with(s.as_str()) {
                    return None;
                }
                pos += s.len();
            }
            Piece::AnyChar => pos += hay[pos..].chars().next()?.len_utf8(),
        }
    }
    Some(pos)
}

/// Char-boundary positions from `from` to `hay.len()` inclusive.
fn boundaries(hay: &str, from: usize) -> impl Iterator<Item = usize> + '_ {
    hay[from..]
        .char_indices()
        .map(move |(i, _)| from + i)
        .chain(std::iter::once(hay.len()))
}

/// Whole-string glob match. `segs` are the parts between `*`s, so there is an implicit `*`
/// between consecutive segments. Leftmost matching of middle segments is optimal because each
/// segment has a fixed length in characters.
fn glob_match(hay: &str, segs: &[Segment]) -> bool {
    let (first, rest) = match segs.split_first() {
        Some(x) => x,
        None => return hay.is_empty(),
    };
    let Some(mut pos) = match_segment_at(hay, 0, first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return pos == hay.len();
    };
    for seg in middle {
        match boundaries(hay, pos).find_map(|p| match_segment_at(hay, p, seg)) {
            Some(end) => pos = end,
            None => return false,
        }
    }
    // Fast path: an all-literal last segment (`*.pdf`) is a suffix check.
    if last.iter().all(|p| matches!(p, Piece::Lit(_))) {
        let len: usize = last
            .iter()
            .map(|p| match p {
                Piece::Lit(s) => s.len(),
                Piece::AnyChar => 0,
            })
            .sum();
        return hay.len() >= pos + len
            && hay.is_char_boundary(hay.len() - len)
            && match_segment_at(hay, hay.len() - len, last) == Some(hay.len());
    }
    boundaries(hay, pos).any(|p| match_segment_at(hay, p, last) == Some(hay.len()))
}

impl FileIndex {
    /// Runs `query` and returns up to `limit` matching ids in index order.
    ///
    /// The scan runs in parallel (rayon) in waves of chunks and stops early once `limit`
    /// results are found. `cancel` is polled regularly; when it becomes `true` the search
    /// returns [`Error::Cancelled`], so the UI can drop a query superseded by a newer keystroke.
    pub fn search(&self, query: &Query, limit: usize, cancel: &AtomicBool) -> Result<Vec<EntryId>> {
        let n = self.len();
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        let chunks = n.div_ceil(CHUNK);
        let wave = rayon::current_num_threads().max(1) * 4;
        let mut first = 0;
        while first < chunks {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let last = (first + wave).min(chunks);
            let parts: Vec<Option<Vec<EntryId>>> = (first..last)
                .into_par_iter()
                .map(|c| self.scan(c * CHUNK, ((c + 1) * CHUNK).min(n), query, limit, cancel))
                .collect();
            for part in parts {
                out.extend(part.ok_or(Error::Cancelled)?);
                if out.len() >= limit {
                    out.truncate(limit);
                    return Ok(out);
                }
            }
            first = last;
        }
        Ok(out)
    }

    /// Scans ids `start..end`; `None` if cancelled.
    fn scan(
        &self,
        start: usize,
        end: usize,
        query: &Query,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Option<Vec<EntryId>> {
        let arena = &self.normalized;
        let text = arena.text.as_str();
        let mut hits = Vec::new();
        let mut from = if start == 0 {
            0
        } else {
            arena.ends[start - 1] as usize
        };
        for (i, &to) in arena.ends[start..end].iter().enumerate() {
            if i % CANCEL_CHECK_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let to = to as usize;
            if query.matches(&text[from..to]) {
                hits.push((start + i) as EntryId);
                if hits.len() >= limit {
                    break;
                }
            }
            from = to;
        }
        Some(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{EntryMeta, IndexBuilder};

    fn index(names: &[&str]) -> FileIndex {
        let mut b = IndexBuilder::new();
        let root = b.push(
            None,
            "/root",
            EntryMeta {
                is_dir: true,
                ..Default::default()
            },
        );
        for n in names {
            b.push(Some(root), n, EntryMeta::default());
        }
        b.finish()
    }

    fn find(ix: &FileIndex, q: &str) -> Vec<String> {
        ix.search(&Query::parse(q), usize::MAX, &AtomicBool::new(false))
            .unwrap()
            .into_iter()
            .map(|id| ix.name(id).to_owned())
            .collect()
    }

    #[test]
    fn substring_ignores_diacritics_alef_variants_case_and_digits() {
        let ix = index(&[
            "مُحَاضَرَةُ الفِقْهِ.docx",
            "أحمد_CV.pdf",
            "إحصاء ٢٠٢٦.xlsx",
            "Report.PDF",
            "notes.txt",
        ]);
        assert_eq!(find(&ix, "محاضرة"), ["مُحَاضَرَةُ الفِقْهِ.docx"]);
        assert_eq!(find(&ix, "مُحاضَرة"), ["مُحَاضَرَةُ الفِقْهِ.docx"]);
        assert_eq!(find(&ix, "احمد"), ["أحمد_CV.pdf"]);
        assert_eq!(find(&ix, "احصاء 2026"), ["إحصاء ٢٠٢٦.xlsx"]);
        assert_eq!(find(&ix, "report"), ["Report.PDF"]);
        assert_eq!(find(&ix, "REPORT"), ["Report.PDF"]);
    }

    #[test]
    fn multi_word_and() {
        let ix = index(&["تقرير Report 2026.pdf", "تقرير 2025.pdf", "Report 2026.txt"]);
        assert_eq!(find(&ix, "تقرير 2026"), ["تقرير Report 2026.pdf"]);
        assert_eq!(
            find(&ix, "report 2026"),
            ["تقرير Report 2026.pdf", "Report 2026.txt"]
        );
        assert!(find(&ix, "تقرير missing").is_empty());
    }

    #[test]
    fn prefix_suffix_and_exact() {
        let ix = index(&["report.pdf", "my report.pdf", "report.docx", "IMG_2026.jpg"]);
        assert_eq!(find(&ix, "^report"), ["report.pdf", "report.docx"]);
        assert_eq!(find(&ix, "pdf$"), ["report.pdf", "my report.pdf"]);
        assert_eq!(find(&ix, "^report pdf$"), ["report.pdf"]);
        assert_eq!(find(&ix, "^img_2026.jpg$"), ["IMG_2026.jpg"]);
        assert_eq!(find(&ix, "^img_2026$"), Vec::<String>::new());
    }

    #[test]
    fn wildcards_match_the_whole_name() {
        let ix = index(&[
            "report.pdf",
            "mypdf",
            "فاتورة_12",
            "فاتورة_123",
            "فَاتُورَة_٤٥",
            "a.b.c",
        ]);
        assert_eq!(find(&ix, "*.pdf"), ["report.pdf"]);
        assert_eq!(find(&ix, "*pdf"), ["report.pdf", "mypdf"]);
        assert_eq!(find(&ix, "فاتورة_??"), ["فاتورة_12", "فَاتُورَة_٤٥"]);
        assert_eq!(
            find(&ix, "فاتورة_*"),
            ["فاتورة_12", "فاتورة_123", "فَاتُورَة_٤٥"]
        );
        assert_eq!(find(&ix, "a*c"), ["a.b.c"]);
        assert_eq!(find(&ix, "a*b*c"), ["a.b.c"]);
        assert_eq!(find(&ix, "?.b.?"), ["a.b.c"]);
        assert!(find(&ix, "a*x").is_empty());
        assert!(find(&ix, "report").len() == 1); // substring, not wildcard
    }

    #[test]
    fn glob_matcher_edge_cases() {
        let q = |p: &str, s: &str| Query::parse(p).matches(s);
        assert!(q("*a", "aaa"));
        assert!(q("a*a", "aa"));
        assert!(!q("a*a", "a"));
        assert!(q("*?*", "x"));
        assert!(!q("?", ""));
        assert!(q("?", "ح"));
        assert!(q("ab*ab*ab", "abxabyab"));
    }

    #[test]
    fn empty_query_matches_everything_and_limit_caps() {
        let ix = index(&["a", "b", "c"]);
        assert_eq!(find(&ix, "   ").len(), 4);
        assert_eq!(find(&ix, "*").len(), 4);
        let ids = ix
            .search(&Query::parse(""), 2, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(ids, [0, 1]);
        assert!(
            ix.search(&Query::parse(""), 0, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn results_keep_index_order_and_limit_across_chunks() {
        let names: Vec<String> = (0..100_000).map(|i| format!("file_{i}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ix = index(&refs);
        let all = ix
            .search(&Query::parse("file"), usize::MAX, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(all.len(), 100_000);
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        let some = ix
            .search(&Query::parse("file"), 50_000, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(some, all[..50_000]);
        let nines = ix
            .search(&Query::parse("99$"), usize::MAX, &AtomicBool::new(false))
            .unwrap();
        assert!(nines.is_empty()); // names end in "txt"
    }

    #[test]
    fn cancellation_returns_cancelled() {
        let ix = index(&["a", "b"]);
        let err = ix
            .search(&Query::parse("a"), 10, &AtomicBool::new(true))
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
