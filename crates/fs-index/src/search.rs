//! Compiled queries and the parallel scan over the index.
//!
//! The query text is parsed by [`athrix_query::parse`] (full syntax: `AND`/`|`/`!`/`< >`,
//! filters, Arabic synonyms) and then **compiled** once into a [`Query`]: every term is
//! normalized with [`athrix_text_ar::normalize_name_into`] (exactly like the index), wildcard
//! patterns are pre-split, extensions are lower-cased and dates are resolved against a clock.
//! [`FileIndex::search`] then evaluates the compiled tree for each entry.
//!
//! # Plain terms
//!
//! - `word` matches anywhere in the entry's **name** (substring).
//! - `^word` matches at the start of the name, `word$` at the end, `^word$` the whole name.
//! - A word containing `*` (any run of characters) or `?` (exactly one character) is a
//!   wildcard pattern matched against the **whole** name, Everything-style: `*.pdf`, `فاتورة_??`.
//! - A `"quoted phrase"` is a literal substring (spaces included; `*`, `?`, `^`, `$` are not
//!   special inside quotes).
//!
//! Both the query and the names go through the same Arabic/Latin normalization: case,
//! diacritics, tatweel and alef/yaa variants are ignored, Arabic-Indic digits equal ASCII digits,
//! and punctuation (`.`, `_`, `-`, ...) becomes a single space. Consequently `?` counts normalized
//! characters, and punctuation at the very edge of a name is dropped (`*.pdf` does not match a
//! file named just `.pdf`). A term that normalizes to nothing (a lone `^`) matches everything.
//!
//! The supported filters are listed in [`crate::exec`]'s documentation. Filters the name index
//! cannot answer are rejected at compile time with [`QueryError::Unsupported`] instead of
//! silently matching everything.
//!
//! Results come back in index (walk) order, except for queries with fuzzy terms (`fuzzy:`),
//! which come back best first (see [`Query::is_ranked`] and `fuzzy.rs`). General ranking of
//! plain results is a later task.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use athrix_core::{Error, Result};
use athrix_query::{FilterName, ParseError, Span};
use memchr::memmem;
use rayon::prelude::*;

use crate::exec::{self, Clock, Entry, Node, Scratch};
use crate::index::{EntryId, FileIndex};

/// Entries scanned per parallel task.
const CHUNK: usize = 16 * 1024;
/// How often (in entries) a task checks the cancellation flag.
const CANCEL_CHECK_EVERY: usize = 4 * 1024;

/// Why a query could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryError {
    /// The text is not a valid query (see [`athrix_query::ParseErrorKind`]).
    Parse(ParseError),
    /// The query is valid, but uses a filter (or a form of one) that the name index cannot
    /// answer yet, such as `content:` or `sem:`. The UI should show `message` at `span`.
    Unsupported {
        /// The filter.
        filter: FilterName,
        /// The bytes of the whole filter (`content:foo`) in the query string.
        span: Span,
        /// A friendly English explanation.
        message: String,
    },
}

impl QueryError {
    /// The offending bytes of the query string.
    pub fn span(&self) -> Span {
        match self {
            QueryError::Parse(e) => e.span,
            QueryError::Unsupported { span, .. } => *span,
        }
    }

    /// A friendly English explanation.
    pub fn message(&self) -> &str {
        match self {
            QueryError::Parse(e) => &e.message,
            QueryError::Unsupported { message, .. } => message,
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Parse(e) => e.fmt(f),
            QueryError::Unsupported { span, message, .. } => {
                write!(f, "unsupported query at position {}: {message}", span.start)
            }
        }
    }
}

impl std::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            QueryError::Parse(e) => Some(e),
            QueryError::Unsupported { .. } => None,
        }
    }
}

impl From<ParseError> for QueryError {
    fn from(e: ParseError) -> Self {
        QueryError::Parse(e)
    }
}

impl From<QueryError> for Error {
    fn from(e: QueryError) -> Self {
        match e {
            QueryError::Parse(p) => p.into(),
            QueryError::Unsupported { span, message, .. } => Error::InvalidQuery {
                position: span.start,
                message,
            },
        }
    }
}

/// A compiled query. Build it once per keystroke with [`Query::parse`] (or [`Query::compile`]
/// from an already parsed [`athrix_query::Query`]), then run it with [`FileIndex::search`].
#[derive(Debug, Clone)]
pub struct Query {
    plan: Plan,
    /// The plan has fuzzy terms: results are scored and sorted.
    ranked: bool,
}

#[derive(Debug, Clone)]
enum Plan {
    /// Matches every entry.
    All,
    /// Matches no entry.
    Nothing,
    /// Only name terms, all ANDed: the hot path, evaluated without the tree machinery.
    Names(Vec<NameMatcher>),
    /// Anything else.
    Tree(Node),
}

impl Query {
    /// Parses and compiles `text`, resolving relative dates (`dm:today`) against the system
    /// clock and local time zone.
    pub fn parse(text: &str) -> std::result::Result<Self, QueryError> {
        Self::compile(&athrix_query::parse(text)?)
    }

    /// Like [`Query::parse`], with an explicit clock (see [`Query::compile_at`]).
    pub fn parse_at(
        text: &str,
        now_unix_secs: i64,
        utc_offset_secs: i32,
    ) -> std::result::Result<Self, QueryError> {
        Self::compile_at(&athrix_query::parse(text)?, now_unix_secs, utc_offset_secs)
    }

    /// Compiles a parsed query against the system clock and local time zone.
    pub fn compile(ast: &athrix_query::Query) -> std::result::Result<Self, QueryError> {
        let (now, offset) = exec::system_clock();
        Self::compile_at(ast, now, offset)
    }

    /// Compiles a parsed query. Relative and calendar dates are resolved as local dates for a
    /// clock reading `now_unix_secs` (seconds since the Unix epoch) in a zone `utc_offset_secs`
    /// east of UTC (for example `10800` for UTC+3). The offset is applied to all dates, so a
    /// period on the other side of a daylight-saving change may be off by that hour.
    pub fn compile_at(
        ast: &athrix_query::Query,
        now_unix_secs: i64,
        utc_offset_secs: i32,
    ) -> std::result::Result<Self, QueryError> {
        let clock = Clock {
            now: now_unix_secs,
            utc_offset: i64::from(utc_offset_secs),
        };
        let node = match &ast.root {
            None => Node::Const(true),
            Some(e) => exec::compile(e, &clock)?,
        };
        let plan = match node {
            Node::Const(true) => Plan::All,
            Node::Const(false) => Plan::Nothing,
            Node::Name(m) => Plan::Names(vec![m]),
            Node::And(children) if children.iter().all(|c| matches!(c, Node::Name(_))) => {
                Plan::Names(
                    children
                        .into_iter()
                        .filter_map(|c| match c {
                            Node::Name(m) => Some(m),
                            _ => None,
                        })
                        .collect(),
                )
            }
            other => Plan::Tree(other),
        };
        let ranked = matches!(&plan, Plan::Tree(n) if n.is_ranked());
        Ok(Query { plan, ranked })
    }

    /// True if the query matches every entry (empty query, or only terms such as `*`).
    pub fn is_empty(&self) -> bool {
        matches!(self.plan, Plan::All)
    }

    /// True if the query has fuzzy terms (`fuzzy:word` or the `fuzzy:` switch) outside a NOT.
    /// Its results are then ordered best first by [`FileIndex::score`] (ties in index order)
    /// instead of index order, and a search scans the whole index before returning.
    pub fn is_ranked(&self) -> bool {
        self.ranked
    }

    /// Highlight ranges for the fuzzy terms of the query in an ORIGINAL (displayed) name:
    /// byte ranges of `name`, sorted and merged, covering the characters each fuzzy term
    /// matched (the substring, the acronym's initials, the subsequence's letters or the
    /// typo-matched part). Plain terms are not included (highlight those with
    /// [`athrix_text_ar::find_in_name`]). Meant for the rows on screen; empty if the query has
    /// no fuzzy terms or none matches `name`.
    pub fn fuzzy_highlights(&self, name: &str) -> Vec<std::ops::Range<usize>> {
        let Plan::Tree(node) = &self.plan else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        node.for_each_fuzzy(&mut |m| ranges.extend(m.highlights(name)));
        ranges.sort_unstable_by_key(|r| (r.start, r.end));
        let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
        for r in ranges {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    /// Evaluates the query against a name alone. `normalized_name` must already be passed
    /// through [`athrix_text_ar::normalize_name_into`]. Returns `None` when the query also needs
    /// other entry data (extension, size, dates, path...): use [`FileIndex::is_match`] then.
    pub fn matches_name(&self, normalized_name: &str) -> Option<bool> {
        match &self.plan {
            Plan::All => Some(true),
            Plan::Nothing => Some(false),
            Plan::Names(terms) => Some(terms.iter().all(|t| t.matches(normalized_name))),
            Plan::Tree(_) => None,
        }
    }
}

/// A compiled plain term, matched against a normalized name or path.
#[derive(Debug, Clone)]
pub(crate) enum NameMatcher {
    Contains(Box<memmem::Finder<'static>>),
    Prefix(Box<str>),
    Suffix(Box<str>),
    Exact(Box<str>),
    Glob(Vec<Segment>),
}

/// A run of pieces between two `*`.
pub(crate) type Segment = Vec<Piece>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Lit(String),
    AnyChar,
}

/// True if `word` is a wildcard pattern.
pub(crate) fn is_wildcard(word: &str) -> bool {
    word.contains(['*', '?'])
}

/// Compiles an unquoted term. `None` if it normalizes to nothing (matches everything).
pub(crate) fn compile_term(word: &str, buf: &mut String) -> Option<NameMatcher> {
    if is_wildcard(word) {
        // Anchors are implicit for wildcard patterns; ignore explicit ones.
        let word = word.strip_prefix('^').unwrap_or(word);
        let word = word.strip_suffix('$').unwrap_or(word);
        return compile_glob(word, buf).map(NameMatcher::Glob);
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
    let s: Box<str> = buf.as_str().into();
    Some(match (anchored_start, anchored_end) {
        (false, false) => NameMatcher::contains(&s),
        (true, false) => NameMatcher::Prefix(s),
        (false, true) => NameMatcher::Suffix(s),
        (true, true) => NameMatcher::Exact(s),
    })
}

/// Compiles a quoted phrase: a literal substring. `None` if it normalizes to nothing.
pub(crate) fn compile_phrase(text: &str, buf: &mut String) -> Option<NameMatcher> {
    athrix_text_ar::normalize_name_into(text, buf);
    (!buf.is_empty()).then(|| NameMatcher::contains(buf))
}

/// Splits a wildcard pattern into normalized segments. `None` if it is only stars.
pub(crate) fn compile_glob(word: &str, buf: &mut String) -> Option<Vec<Segment>> {
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
    Some(segments)
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

impl NameMatcher {
    fn contains(s: &str) -> Self {
        NameMatcher::Contains(Box::new(memmem::Finder::new(s.as_bytes()).into_owned()))
    }

    #[inline]
    pub(crate) fn matches(&self, name: &str) -> bool {
        match self {
            NameMatcher::Contains(f) => f.find(name.as_bytes()).is_some(),
            NameMatcher::Prefix(p) => name.starts_with(&**p),
            NameMatcher::Suffix(s) => name.ends_with(&**s),
            NameMatcher::Exact(s) => name == &**s,
            NameMatcher::Glob(segs) => glob_match(name, segs),
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
pub(crate) fn glob_match(hay: &str, segs: &[Segment]) -> bool {
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
    /// Runs `query` and returns up to `limit` matching ids in index order, or best first for a
    /// ranked query ([`Query::is_ranked`]).
    ///
    /// The scan runs in parallel (rayon) in waves of chunks and stops early once `limit`
    /// results are found (a ranked query scans everything and keeps the best `limit`).
    /// `cancel` is polled regularly; when it becomes `true` the search returns
    /// [`Error::Cancelled`], so the UI can drop a query superseded by a newer keystroke.
    pub fn search(&self, query: &Query, limit: usize, cancel: &AtomicBool) -> Result<Vec<EntryId>> {
        self.search_masked(query, limit, cancel, None)
    }

    /// The rank score of entry `id` for `query`: `None` if it does not match, `Some(0)` for
    /// any match of an unranked query; for a ranked query higher is better (the order
    /// [`FileIndex::search`] returns). Lets callers that filter ids themselves (scoped
    /// searches) sort the same way.
    ///
    /// # Panics
    /// If `id` is out of range.
    pub fn score(&self, query: &Query, id: EntryId) -> Option<u32> {
        let name = self.normalized_name(id);
        match &query.plan {
            Plan::Tree(node) if query.ranked => {
                let e = Entry { ix: self, id, name };
                let mut s = Scratch::default();
                node.eval(&e, &mut s).then(|| node.score(&e, &mut s))
            }
            _ => self.is_match(query, id).then_some(0),
        }
    }

    /// Ranked search: up to `limit` `(score, id)` pairs, best first, ties by id.
    pub(crate) fn search_scored_masked(
        &self,
        query: &Query,
        limit: usize,
        cancel: &AtomicBool,
        mask: Option<&[u64]>,
    ) -> Result<Vec<(u32, EntryId)>> {
        let n = self.len();
        let mut out: Vec<(u32, EntryId)> = Vec::new();
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let Plan::Tree(node) = &query.plan else {
            // Not ranked: every hit scores 0, in index order.
            return Ok(self
                .search_masked(query, limit, cancel, mask)?
                .into_iter()
                .map(|id| (0, id))
                .collect());
        };
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
            let parts: Vec<Option<Vec<(u32, EntryId)>>> = (first..last)
                .into_par_iter()
                .map(|c| {
                    let end = ((c + 1) * CHUNK).min(n);
                    self.scan_scored(c * CHUNK, end, node, limit, cancel, mask)
                })
                .collect();
            for part in parts {
                out.extend(part.ok_or(Error::Cancelled)?);
            }
            keep_best(&mut out, limit);
            first = last;
        }
        out.sort_unstable_by_key(|&(s, id)| (std::cmp::Reverse(s), id));
        Ok(out)
    }

    /// Scans ids `start..end` for a ranked query, keeping the best `limit`; `None` if
    /// cancelled.
    fn scan_scored(
        &self,
        start: usize,
        end: usize,
        node: &Node,
        limit: usize,
        cancel: &AtomicBool,
        mask: Option<&[u64]>,
    ) -> Option<Vec<(u32, EntryId)>> {
        let arena = &self.normalized;
        let text = arena.text.as_str();
        let mut hits = Vec::new();
        let mut from = if start == 0 {
            0
        } else {
            arena.ends[start - 1] as usize
        };
        let mut scratch = Scratch::default();
        for (i, &to) in arena.ends[start..end].iter().enumerate() {
            if i % CANCEL_CHECK_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let to = to as usize;
            let name = &text[from..to];
            from = to;
            let id = (start + i) as EntryId;
            let e = Entry { ix: self, id, name };
            if node.eval(&e, &mut scratch) && !mask.is_some_and(|m| bit_is_set(m, id)) {
                hits.push((node.score(&e, &mut scratch), id));
                if hits.len() >= limit.saturating_mul(2).max(1024) {
                    keep_best(&mut hits, limit);
                }
            }
        }
        keep_best(&mut hits, limit);
        Some(hits)
    }

    /// [`FileIndex::search`] that skips every id whose bit is set in `mask` (bit `id % 64` of
    /// word `id / 64`; missing words count as clear). Used by the live layer for removed base
    /// entries and for the delta's ancestor copies.
    pub(crate) fn search_masked(
        &self,
        query: &Query,
        limit: usize,
        cancel: &AtomicBool,
        mask: Option<&[u64]>,
    ) -> Result<Vec<EntryId>> {
        if query.ranked {
            return Ok(self
                .search_scored_masked(query, limit, cancel, mask)?
                .into_iter()
                .map(|(_, id)| id)
                .collect());
        }
        let n = self.len();
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        if matches!(query.plan, Plan::Nothing) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
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
                .map(|c| {
                    let end = ((c + 1) * CHUNK).min(n);
                    self.scan(c * CHUNK, end, query, limit, cancel, mask)
                })
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

    /// True if entry `id` matches `query` (the per-entry check [`FileIndex::search`] runs).
    ///
    /// # Panics
    /// If `id` is out of range.
    pub fn is_match(&self, query: &Query, id: EntryId) -> bool {
        let name = self.normalized_name(id);
        match &query.plan {
            Plan::All => true,
            Plan::Nothing => false,
            Plan::Names(terms) => terms.iter().all(|t| t.matches(name)),
            Plan::Tree(node) => node.eval(&Entry { ix: self, id, name }, &mut Scratch::default()),
        }
    }

    /// Scans ids `start..end`; `None` if cancelled.
    fn scan(
        &self,
        start: usize,
        end: usize,
        query: &Query,
        limit: usize,
        cancel: &AtomicBool,
        mask: Option<&[u64]>,
    ) -> Option<Vec<EntryId>> {
        let arena = &self.normalized;
        let text = arena.text.as_str();
        let mut hits = Vec::new();
        let mut from = if start == 0 {
            0
        } else {
            arena.ends[start - 1] as usize
        };
        let mut scratch = Scratch::default();
        for (i, &to) in arena.ends[start..end].iter().enumerate() {
            if i % CANCEL_CHECK_EVERY == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let to = to as usize;
            let name = &text[from..to];
            let id = (start + i) as EntryId;
            let hit = match &query.plan {
                Plan::All => true,
                Plan::Nothing => false,
                Plan::Names(terms) => terms.iter().all(|t| t.matches(name)),
                Plan::Tree(node) => node.eval(&Entry { ix: self, id, name }, &mut scratch),
            };
            if hit && !mask.is_some_and(|m| bit_is_set(m, id)) {
                hits.push(id);
                if hits.len() >= limit {
                    break;
                }
            }
            from = to;
        }
        Some(hits)
    }
}

/// Keeps the `limit` best `(score, id)` pairs (highest score, then lowest id), unordered.
pub(crate) fn keep_best(hits: &mut Vec<(u32, EntryId)>, limit: usize) {
    if hits.len() > limit {
        let key = |&(s, id): &(u32, EntryId)| (std::cmp::Reverse(s), id);
        hits.select_nth_unstable_by_key(limit, key);
        hits.truncate(limit);
    }
}

/// Bit `id` of a bitset stored as `u64` words; ids beyond the words are clear.
#[inline]
pub(crate) fn bit_is_set(words: &[u64], id: EntryId) -> bool {
    words
        .get(id as usize / 64)
        .is_some_and(|w| w & (1 << (id % 64)) != 0)
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

    fn q(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    fn find(ix: &FileIndex, text: &str) -> Vec<String> {
        ix.search(&q(text), usize::MAX, &AtomicBool::new(false))
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
        let m = |p: &str, s: &str| q(p).matches_name(s).unwrap();
        assert!(m("*a", "aaa"));
        assert!(m("a*a", "aa"));
        assert!(!m("a*a", "a"));
        assert!(m("*?*", "x"));
        assert!(!m("?", ""));
        assert!(m("?", "ح"));
        assert!(m("ab*ab*ab", "abxabyab"));
    }

    #[test]
    fn quoted_phrase_is_a_literal_substring() {
        let ix = index(&["annual report 2026.pdf", "report annual.pdf", "a*b.txt"]);
        assert_eq!(find(&ix, "\"annual report\""), ["annual report 2026.pdf"]);
        // `*` inside quotes is not a wildcard (it normalizes like punctuation).
        assert_eq!(find(&ix, "\"a*b\""), ["a*b.txt"]);
    }

    #[test]
    fn empty_query_matches_everything_and_limit_caps() {
        let ix = index(&["a", "b", "c"]);
        assert_eq!(find(&ix, "   ").len(), 4);
        assert_eq!(find(&ix, "*").len(), 4);
        assert!(q("").is_empty());
        let ids = ix.search(&q(""), 2, &AtomicBool::new(false)).unwrap();
        assert_eq!(ids, [0, 1]);
        assert!(
            ix.search(&q(""), 0, &AtomicBool::new(false))
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
            .search(&q("file"), usize::MAX, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(all.len(), 100_000);
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        let some = ix
            .search(&q("file"), 50_000, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(some, all[..50_000]);
        let nines = ix
            .search(&q("99$"), usize::MAX, &AtomicBool::new(false))
            .unwrap();
        assert!(nines.is_empty()); // names end in "txt"
        // A tree query also keeps order and limit across chunks.
        let tree = ix
            .search(&q("file | nothing"), 70_000, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(tree, all[..70_000]);
    }

    #[test]
    fn cancellation_returns_cancelled() {
        let ix = index(&["a", "b"]);
        for text in ["a", "a | b", "ext:pdf", "!a !*"] {
            let err = ix.search(&q(text), 10, &AtomicBool::new(true)).unwrap_err();
            assert!(matches!(err, Error::Cancelled), "{text}");
        }
    }

    #[test]
    fn errors_convert_into_core_errors() {
        let e: Error = Query::parse("szie:1").unwrap_err().into();
        assert!(matches!(e, Error::InvalidQuery { position: 0, .. }));
        let e: Error = Query::parse("a content:x").unwrap_err().into();
        assert!(matches!(e, Error::InvalidQuery { position: 2, .. }));
    }
}
