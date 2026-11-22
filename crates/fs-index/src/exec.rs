//! Query execution: compiling an [`athrix_query`] AST into a [`Node`] tree and evaluating it
//! per entry.
//!
//! # Supported filters
//!
//! | filter | meaning on the name index |
//! |---|---|
//! | `ext:pdf;docx` (`امتداد:`) | The original name's extension (text after the last `.`), case-insensitive. Folders never match. |
//! | `type:image;folder` (`نوع:`) | `folder` = is a directory; other kinds = a file whose extension is in the kind's list (see [`kind_extensions`]). |
//! | `size:>1mb` (`حجم:`) | Size in bytes (1024-based units, from the parser). `size:1mb` is exact. Folders never match. |
//! | `dm:today`, `dm:2026-03`, `dm:>=2026-01-15` (`تاريخ:`) | Modification time, in local dates. A date as `=` means "within that day/month/year/period". Entries without a known time never match. |
//! | `path:x` (`مسار:`) | `x` with plain-term rules (substring, `^`/`$`, wildcards) against the normalized **full path** (including the name). |
//! | `parent:x` | The normalized path of the direct parent folder ends with `x` at a word boundary (`parent:docs`, `parent:"D:\Data\Docs"`). With wildcards: the whole parent path matches the pattern. |
//! | `infolder:x` (`مجلد:`) | Like `parent:`, for any ancestor folder (the entry is somewhere under it). |
//! | `case:Word` | Case-sensitive substring of the **original** name (no normalization). |
//! | `ww:word` | `word` (normalized) appears in the name as whole words, bounded by the name's start/end or a separator. |
//! | `fuzzy:word` | Fuzzy match of `word` on the normalized name: substring, acronym (`mrs` → `Monthly Report Summary`), typos (`reprot` → `report`) or subsequence (`rprt` → `report`); see `fuzzy.rs`. Results are ranked best first. |
//! | `fuzzy:` alone | A switch: every plain word of the query (not quoted, no wildcards or `^`/`$` anchors, not a filter value) is matched like `fuzzy:word`, so `fuzzy: rprt 2026` = `fuzzy:rprt fuzzy:2026`. Cannot be negated. |
//!
//! Rejected with [`QueryError::Unsupported`]: `content:`, `sem:`/`~` (`~` is the semantic
//! prefix, not fuzzy), `regex:`, `dupe:`, `similar:`, `tag:`, `dc:`, `da:` (the index stores
//! only the modification time), `case:`/`ww:` written without a value (as global switches),
//! and a negated `fuzzy:` switch.
//!
//! # Ranking
//!
//! A query with a fuzzy term outside a NOT is *ranked*: every match gets a score (an AND adds
//! its children's scores, an OR takes its best matching child, other nodes score 0) and results
//! come back best first, ties in index order. A ranked search scans the whole index (no early
//! stop at `limit`). Queries without fuzzy terms keep index order and the early stop.
//!
//! # Paths
//!
//! The normalized path of an entry is the normalized names of its ancestors and itself joined
//! with single spaces (normalization turns `\` and `/` into spaces anyway, so a path typed in
//! the query normalizes the same way). Paths are not stored: they are rebuilt from parent links
//! only when a path filter is actually evaluated, which costs O(depth) per entry. The parent
//! path is cached per scan task and reused while consecutive entries share a parent (the
//! common case in walk order). AND children are ordered cheapest first (name terms, then
//! metadata, then path filters), so `report path:2026` builds paths only for names that
//! already contain `report`.
//!
//! # Dates
//!
//! Dates are resolved at compile time against a clock (`now` + a fixed UTC offset). Weeks
//! start on **Monday** (ISO 8601): `thisweek` is Monday 00:00 to the next Monday 00:00 local
//! time, `lastweek` the seven days before. `thismonth`/`thisyear` cover the whole calendar
//! period (future days included); `lastmonth`/`lastyear` the previous one.

use athrix_query::{
    CalendarDate, Comparison, DateSpec, Expr, ExprKind, FileKind, Filter, FilterName, FilterValue,
    NamedDate,
};
use std::collections::HashMap;

use memchr::memmem;

use crate::fuzzy::{FuzzyBuf, FuzzyMatcher};
use crate::index::{EntryId, FileIndex};
use crate::search::{
    NameMatcher, QueryError, Segment, compile_glob, compile_phrase, compile_term, glob_match,
    is_wildcard,
};

const DAY: i64 = 86_400;

/// Extension lists for `type:` kinds (lower case, without the dot). A file matches a kind if
/// its extension is in the list; `type:folder` matches directories instead.
pub fn kind_extensions(kind: FileKind) -> &'static [&'static str] {
    match kind {
        FileKind::Document => &[
            "csv", "doc", "docm", "docx", "dot", "dotx", "epub", "key", "md", "markdown",
            "numbers", "odp", "ods", "odt", "pages", "pdf", "pps", "ppsx", "ppt", "pptx", "rtf",
            "tex", "txt", "xls", "xlsb", "xlsm", "xlsx", "xps",
        ],
        FileKind::Image => &[
            "arw", "avif", "bmp", "cr2", "cr3", "dng", "gif", "heic", "heif", "ico", "jfif",
            "jpeg", "jpg", "nef", "orf", "png", "psd", "raw", "svg", "tif", "tiff", "webp",
        ],
        FileKind::Video => &[
            "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "ogv",
            "vob", "webm", "wmv",
        ],
        FileKind::Audio => &[
            "aac", "aif", "aiff", "alac", "amr", "flac", "m4a", "mid", "midi", "mp3", "oga", "ogg",
            "opus", "wav", "wma",
        ],
        FileKind::Code => &[
            "bat", "c", "cc", "cmd", "cpp", "cs", "css", "cxx", "dart", "go", "h", "hpp", "htm",
            "html", "java", "js", "json", "jsx", "kt", "kts", "lua", "mjs", "php", "pl", "ps1",
            "py", "r", "rb", "rs", "scala", "scss", "sh", "sql", "svelte", "swift", "toml", "ts",
            "tsx", "vue", "xml", "yaml", "yml",
        ],
        FileKind::Archive => &[
            "7z", "bz2", "cab", "gz", "iso", "lz", "lzma", "rar", "tar", "tbz2", "tgz", "txz",
            "xz", "zip", "zst",
        ],
        FileKind::Folder => &[],
    }
}

/// A clock for resolving dates: Unix seconds and the local offset east of UTC in seconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    pub(crate) now: i64,
    pub(crate) utc_offset: i64,
}

/// The system clock and the local zone's current UTC offset.
pub(crate) fn system_clock() -> (i64, i32) {
    let now = chrono::Local::now();
    (now.timestamp(), now.offset().local_minus_utc())
}

/// A compiled query node.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Const(bool),
    /// A plain term on the normalized name.
    Name(NameMatcher),
    /// `case:` substring of the original name.
    CaseSensitive(Box<memmem::Finder<'static>>),
    /// `ww:` whole-word occurrence in the normalized name.
    WholeWord(Box<memmem::Finder<'static>>),
    /// `fuzzy:` (or a word under the `fuzzy:` switch) on the normalized name.
    Fuzzy(Box<FuzzyMatcher>),
    /// `ext:` lower-cased extensions.
    Ext(Box<[Box<str>]>),
    /// `type:` sorted union of the kinds' extensions, plus whether folders match.
    Kind {
        exts: Box<[&'static str]>,
        folders: bool,
    },
    /// `size:` inclusive byte range.
    Size {
        lo: u64,
        hi: u64,
    },
    /// `dm:` inclusive range of Unix seconds.
    Modified {
        lo: i64,
        hi: i64,
    },
    /// `path:` term on the normalized full path.
    Path(NameMatcher),
    /// `parent:` (with its memo slot in [`Scratch`])
    Parent(FolderMatcher, usize),
    /// `infolder:` (with its memo slot in [`Scratch`])
    InFolder(FolderMatcher, usize),
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

#[derive(Debug, Clone)]
pub(crate) enum FolderMatcher {
    /// Normalized folder path suffix, matched at word boundaries.
    Words(Box<memmem::Finder<'static>>),
    /// Whole folder path pattern.
    Glob(Vec<Segment>),
}

/// Compiles an expression.
pub(crate) fn compile(e: &Expr, clock: &Clock) -> Result<Node, QueryError> {
    let mut cx = Compiler {
        clock: *clock,
        buf: String::new(),
        slots: 0,
        fuzzy_all: fuzzy_switch(e, false)?,
    };
    compile_expr(e, &mut cx)
}

struct Compiler {
    clock: Clock,
    buf: String,
    /// Memo slots handed out to `parent:`/`infolder:` nodes.
    slots: usize,
    /// A `fuzzy:` switch is present: plain words become fuzzy terms.
    fuzzy_all: bool,
}

/// True if `e` contains a valueless `fuzzy:` switch; an error if one is negated.
fn fuzzy_switch(e: &Expr, negated: bool) -> Result<bool, QueryError> {
    Ok(match &e.kind {
        ExprKind::Filter(f) if f.name == FilterName::Fuzzy && f.value == FilterValue::None => {
            if negated {
                return Err(unsupported(
                    f,
                    e,
                    "`fuzzy:` as a switch cannot be negated; remove the `!`/`-`",
                ));
            }
            true
        }
        ExprKind::Term(_) | ExprKind::Filter(_) => false,
        ExprKind::Not(x) => fuzzy_switch(x, true)?,
        ExprKind::Group(x) => fuzzy_switch(x, negated)?,
        ExprKind::And(xs) | ExprKind::Or(xs) => {
            let mut any = false;
            for x in xs {
                any |= fuzzy_switch(x, negated)?;
            }
            any
        }
    })
}

/// A fuzzy node for `text`, or `Const(true)` if it normalizes to nothing.
fn fuzzy_node(text: &str, buf: &mut String) -> Node {
    athrix_text_ar::normalize_name_into(text, buf);
    if buf.is_empty() {
        Node::Const(true)
    } else {
        Node::Fuzzy(Box::new(FuzzyMatcher::new(buf)))
    }
}

fn compile_expr(e: &Expr, cx: &mut Compiler) -> Result<Node, QueryError> {
    Ok(match &e.kind {
        ExprKind::Term(t)
            if cx.fuzzy_all
                && !t.quoted
                && !is_wildcard(&t.text)
                && !t.text.starts_with('^')
                && !t.text.ends_with('$') =>
        {
            fuzzy_node(&t.text, &mut cx.buf)
        }
        ExprKind::Term(t) => {
            let m = if t.quoted {
                compile_phrase(&t.text, &mut cx.buf)
            } else {
                compile_term(&t.text, &mut cx.buf)
            };
            m.map_or(Node::Const(true), Node::Name)
        }
        ExprKind::Filter(f) => compile_filter(f, e, cx)?,
        ExprKind::Group(x) => compile_expr(x, cx)?,
        ExprKind::Not(x) => match compile_expr(x, cx)? {
            Node::Const(b) => Node::Const(!b),
            Node::Not(inner) => *inner,
            n => Node::Not(Box::new(n)),
        },
        ExprKind::And(xs) => {
            let mut children = Vec::with_capacity(xs.len());
            let mut constant_false = false;
            for x in xs {
                match compile_expr(x, cx)? {
                    Node::Const(true) => {}
                    Node::Const(false) => constant_false = true,
                    Node::And(inner) => children.extend(inner),
                    n => children.push(n),
                }
            }
            if constant_false {
                Node::Const(false)
            } else {
                // Cheap checks first; stable, so equal-cost children keep query order.
                children.sort_by_key(Node::cost);
                collapse(children, Node::And, true)
            }
        }
        ExprKind::Or(xs) => {
            let mut children = Vec::with_capacity(xs.len());
            let mut constant_true = false;
            for x in xs {
                match compile_expr(x, cx)? {
                    Node::Const(false) => {}
                    Node::Const(true) => constant_true = true,
                    Node::Or(inner) => children.extend(inner),
                    n => children.push(n),
                }
            }
            if constant_true {
                Node::Const(true)
            } else {
                children.sort_by_key(Node::cost);
                collapse(children, Node::Or, false)
            }
        }
    })
}

fn collapse(mut children: Vec<Node>, wrap: fn(Vec<Node>) -> Node, empty: bool) -> Node {
    match children.len() {
        0 => Node::Const(empty),
        1 => children.pop().unwrap_or(Node::Const(empty)),
        _ => wrap(children),
    }
}

fn unsupported(f: &Filter, e: &Expr, message: impl Into<String>) -> QueryError {
    QueryError::Unsupported {
        filter: f.name,
        span: e.span,
        message: message.into(),
    }
}

fn compile_filter(f: &Filter, e: &Expr, cx: &mut Compiler) -> Result<Node, QueryError> {
    let buf = &mut cx.buf;
    let not_yet = |what: &str| {
        unsupported(
            f,
            e,
            format!(
                "`{}:` ({what}) is not supported by the name index yet",
                f.name
            ),
        )
    };
    let bad_value = || unsupported(f, e, format!("unexpected value for `{}:`", f.name));
    Ok(match (f.name, &f.value) {
        (FilterName::Ext, FilterValue::Extensions(exts)) => {
            Node::Ext(exts.iter().map(|x| x.to_lowercase().into()).collect())
        }
        (FilterName::Type, FilterValue::Kinds(kinds)) => {
            let mut exts: Vec<&'static str> = kinds
                .iter()
                .flat_map(|k| kind_extensions(*k).iter().copied())
                .collect();
            exts.sort_unstable();
            exts.dedup();
            Node::Kind {
                exts: exts.into(),
                folders: kinds.contains(&FileKind::Folder),
            }
        }
        (FilterName::Size, FilterValue::Size(cmp)) => match size_range(*cmp) {
            Some((lo, hi)) => Node::Size { lo, hi },
            None => Node::Const(false),
        },
        (FilterName::DateModified, FilterValue::Date(cmp)) => {
            let (lo, hi) = date_range(*cmp, &cx.clock);
            if lo > hi {
                Node::Const(false)
            } else {
                Node::Modified { lo, hi }
            }
        }
        (FilterName::Path, FilterValue::Text(t)) => {
            compile_term(t, buf).map_or(Node::Const(true), Node::Path)
        }
        (FilterName::Parent | FilterName::InFolder, FilterValue::Text(t)) => {
            let m = if is_wildcard(t) {
                compile_glob(t, buf).map(FolderMatcher::Glob)
            } else {
                athrix_text_ar::normalize_name_into(t, buf);
                (!buf.is_empty()).then(|| FolderMatcher::Words(finder(buf)))
            };
            let slot = cx.slots;
            cx.slots += 1;
            match (m, f.name) {
                (None, _) => Node::Const(true),
                (Some(m), FilterName::Parent) => Node::Parent(m, slot),
                (Some(m), _) => Node::InFolder(m, slot),
            }
        }
        (FilterName::Case, FilterValue::Text(t)) => Node::CaseSensitive(finder(t)),
        (FilterName::WholeWord, FilterValue::Text(t)) => {
            athrix_text_ar::normalize_name_into(t, buf);
            if buf.is_empty() {
                Node::Const(true)
            } else {
                Node::WholeWord(finder(buf))
            }
        }
        (FilterName::Fuzzy, FilterValue::Text(t)) => fuzzy_node(t, buf),
        // The switch itself (see `fuzzy_switch`) matches everything.
        (FilterName::Fuzzy, FilterValue::None) => Node::Const(true),
        (FilterName::Case | FilterName::WholeWord, FilterValue::None) => {
            return Err(unsupported(
                f,
                e,
                format!(
                    "`{0}:` as a switch for the whole query is not supported yet; \
                     give it a word, for example `{0}:report`",
                    f.name
                ),
            ));
        }
        (FilterName::Ext | FilterName::Type | FilterName::Size, _)
        | (FilterName::DateModified | FilterName::Path, _)
        | (FilterName::Parent | FilterName::InFolder, _)
        | (FilterName::Case | FilterName::WholeWord | FilterName::Fuzzy, _) => {
            return Err(bad_value());
        }
        (FilterName::DateCreated, _) => return Err(not_yet("creation date")),
        (FilterName::DateAccessed, _) => return Err(not_yet("access date")),
        (FilterName::Content, _) => return Err(not_yet("file contents")),
        (FilterName::Semantic, _) => return Err(not_yet("semantic search")),
        (FilterName::Regex, _) => return Err(not_yet("regular expressions")),
        (FilterName::Dupe, _) => return Err(not_yet("duplicates")),
        (FilterName::Similar, _) => return Err(not_yet("similar files")),
        (FilterName::Tag, _) => return Err(not_yet("tags")),
    })
}

fn finder(s: &str) -> Box<memmem::Finder<'static>> {
    Box::new(memmem::Finder::new(s.as_bytes()).into_owned())
}

/// An inclusive byte range, or `None` if nothing can match.
fn size_range(cmp: Comparison<u64>) -> Option<(u64, u64)> {
    let r = match cmp {
        Comparison::Eq(v) => (v, v),
        Comparison::Lt(v) => (0, v.checked_sub(1)?),
        Comparison::Le(v) => (0, v),
        Comparison::Gt(v) => (v.checked_add(1)?, u64::MAX),
        Comparison::Ge(v) => (v, u64::MAX),
        Comparison::Range { min, max } => (min, max),
    };
    (r.0 <= r.1).then_some(r)
}

/// An inclusive range of Unix seconds (empty when `lo > hi`).
fn date_range(cmp: Comparison<DateSpec>, clock: &Clock) -> (i64, i64) {
    let p = |d| period(d, clock);
    let (start, end) = match cmp {
        Comparison::Eq(d) => p(d),
        Comparison::Lt(d) => (i64::MIN, p(d).0),
        Comparison::Le(d) => (i64::MIN, p(d).1),
        Comparison::Gt(d) => (p(d).1, i64::MAX),
        Comparison::Ge(d) => (p(d).0, i64::MAX),
        Comparison::Range { min, max } => (p(min).0, p(max).1),
    };
    // `end` is exclusive; i64::MAX stands for "no upper bound".
    let hi = if end == i64::MAX { end } else { end - 1 };
    (start, hi)
}

/// The half-open interval of Unix seconds covered by `d` in local time.
fn period(d: DateSpec, clock: &Clock) -> (i64, i64) {
    let (first_day, end_day) = match d {
        DateSpec::Named(n) => named_days(n, clock),
        DateSpec::Calendar(c) => calendar_days(c),
    };
    let at = |day: i64| day * DAY - clock.utc_offset;
    (at(first_day), at(end_day))
}

/// Half-open range of local day numbers (days since 1970-01-01) for a named period.
fn named_days(n: NamedDate, clock: &Clock) -> (i64, i64) {
    let today = (clock.now + clock.utc_offset).div_euclid(DAY);
    let (y, m, _) = civil_from_days(today);
    // ISO weekday with Monday = 0; 1970-01-01 was a Thursday.
    let monday = today - (today + 3).rem_euclid(7);
    let month = |y: i64, m: u32| {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        (days_from_civil(y, m, 1), days_from_civil(ny, nm, 1))
    };
    match n {
        NamedDate::Today => (today, today + 1),
        NamedDate::Yesterday => (today - 1, today),
        NamedDate::ThisWeek => (monday, monday + 7),
        NamedDate::LastWeek => (monday - 7, monday),
        NamedDate::ThisMonth => month(y, m),
        NamedDate::LastMonth => {
            if m == 1 {
                month(y - 1, 12)
            } else {
                month(y, m - 1)
            }
        }
        NamedDate::ThisYear => (days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)),
        NamedDate::LastYear => (days_from_civil(y - 1, 1, 1), days_from_civil(y, 1, 1)),
    }
}

/// Half-open range of day numbers for a calendar year, month or day.
fn calendar_days(c: CalendarDate) -> (i64, i64) {
    let y = i64::from(c.year);
    match (c.month, c.day) {
        (Some(m), Some(d)) => {
            let day = days_from_civil(y, u32::from(m), u32::from(d));
            (day, day + 1)
        }
        (Some(m), None) => {
            let m = u32::from(m);
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (days_from_civil(y, m, 1), days_from_civil(ny, nm, 1))
        }
        _ => (days_from_civil(y, 1, 1), days_from_civil(y + 1, 1, 1)),
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The entry being evaluated.
pub(crate) struct Entry<'a> {
    pub(crate) ix: &'a FileIndex,
    pub(crate) id: EntryId,
    /// Its normalized name.
    pub(crate) name: &'a str,
}

/// Per-task reusable buffers for lazily built paths.
///
/// `path` holds the normalized path of the cached parent (`..parent_len()`), optionally followed
/// by `' ' + name` of the entry in `cached_full`. Moving to another parent keeps the longest
/// shared ancestor prefix and appends only the missing components, so walking siblings and
/// cousins in index order costs O(new components) instead of O(depth) per entry.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    /// The parent whose path is cached (`Some(None)` = a root's empty parent path).
    cached_parent: Option<Option<EntryId>>,
    /// Ancestor ids of the cached parent, root first (the parent last).
    chain: Vec<EntryId>,
    /// `bounds[i]`: end offset in `path` of `chain[i]`'s path (component boundaries).
    bounds: Vec<usize>,
    path: String,
    pending: Vec<EntryId>,
    /// The entry whose full path currently follows the parent path in `path`.
    cached_full: Option<EntryId>,
    /// Results of wildcard `parent:`/`infolder:` nodes for the cached parent, by node slot.
    memo: Vec<Option<bool>>,
    /// Per `infolder:` slot: directory -> "this directory or an ancestor matches".
    dir_memo: Vec<HashMap<EntryId, bool>>,
    /// Buffers of the fuzzy matcher.
    fuzzy: FuzzyBuf,
}

impl Scratch {
    fn parent_len(&self) -> usize {
        self.bounds.last().copied().unwrap_or(0)
    }

    fn ensure_parent(&mut self, ix: &FileIndex, id: EntryId) {
        let parent = ix.parent(id);
        if self.cached_parent == Some(parent) {
            return;
        }
        self.cached_full = None;
        self.memo.iter_mut().for_each(|m| *m = None);
        // Walk up from the new parent until an ancestor already in the cached chain. Ids grow
        // from root to leaf, so the chain is sorted; a miss only costs a longer rebuild.
        self.pending.clear();
        let mut keep = 0;
        let mut cur = parent;
        while let Some(c) = cur {
            if let Ok(i) = self.chain.binary_search(&c) {
                keep = i + 1;
                break;
            }
            self.pending.push(c);
            cur = ix.parent(c);
        }
        self.chain.truncate(keep);
        self.bounds.truncate(keep);
        self.path.truncate(self.parent_len());
        for &c in self.pending.iter().rev() {
            let n = ix.normalized_name(c);
            if !n.is_empty() {
                if !self.path.is_empty() {
                    self.path.push(' ');
                }
                self.path.push_str(n);
            }
            self.chain.push(c);
            self.bounds.push(self.path.len());
        }
        self.cached_parent = Some(parent);
    }

    fn full_path(&mut self, e: &Entry<'_>) -> &str {
        self.ensure_parent(e.ix, e.id);
        if self.cached_full != Some(e.id) {
            self.path.truncate(self.parent_len());
            if !self.path.is_empty() && !e.name.is_empty() {
                self.path.push(' ');
            }
            self.path.push_str(e.name);
            self.cached_full = Some(e.id);
        }
        &self.path
    }

    /// `infolder:` with plain words: true if the path of any ancestor of `id` ends with
    /// `needle` at a word boundary. Memoized per directory, so each directory's own check
    /// (which costs about `needle.len()`, not its depth) runs once per scan task.
    fn in_folder_words(&mut self, ix: &FileIndex, id: EntryId, slot: usize, needle: &[u8]) -> bool {
        if self.dir_memo.len() <= slot {
            self.dir_memo.resize_with(slot + 1, HashMap::new);
        }
        let memo = &mut self.dir_memo[slot];
        self.pending.clear();
        let mut result = false;
        let mut cur = ix.parent(id);
        while let Some(d) = cur {
            if let Some(&r) = memo.get(&d) {
                result = r;
                break;
            }
            self.pending.push(d);
            cur = ix.parent(d);
        }
        for &d in self.pending.iter().rev() {
            result = result || path_ends_with_words(ix, d, needle);
            memo.insert(d, result);
        }
        result
    }

    /// Evaluates a folder filter once per parent.
    fn memoized(
        &mut self,
        e: &Entry<'_>,
        slot: usize,
        f: impl FnOnce(&str, &[usize]) -> bool,
    ) -> bool {
        self.ensure_parent(e.ix, e.id);
        if self.memo.len() <= slot {
            self.memo.resize(slot + 1, None);
        }
        if let Some(r) = self.memo[slot] {
            return r;
        }
        let n = self.parent_len();
        let r = f(&self.path[..n], &self.bounds);
        self.memo[slot] = Some(r);
        r
    }
}

/// True if the normalized path of `dir` ends with `needle` at a word boundary. Compares
/// backwards one ancestor name at a time, so it never builds the path and stops after about
/// `needle.len()` bytes.
fn path_ends_with_words(ix: &FileIndex, dir: EntryId, needle: &[u8]) -> bool {
    let mut rest = needle;
    let mut cur = Some(dir);
    while let Some(c) = cur {
        let n = ix.normalized_name(c).as_bytes();
        cur = ix.parent(c);
        if n.is_empty() {
            continue;
        }
        if rest.len() <= n.len() {
            // The component boundary before `n` is a space (or the path start) anyway.
            return n.ends_with(rest)
                && (rest.len() == n.len() || n[n.len() - rest.len() - 1] == b' ');
        }
        if !rest.ends_with(n) || rest[rest.len() - n.len() - 1] != b' ' {
            return false;
        }
        rest = &rest[..rest.len() - n.len() - 1];
    }
    false
}

/// True if `f` occurs in `hay` bounded by the start/end of `hay` or a space on both sides, and
/// (when `ends` is given) the occurrence ends at one of those offsets.
fn find_words(hay: &str, f: &memmem::Finder<'_>, ends: Option<&[usize]>) -> bool {
    let bytes = hay.as_bytes();
    let len = f.needle().len();
    memmem::find_iter(bytes, f.needle()).any(|i| {
        let end = i + len;
        (i == 0 || bytes[i - 1] == b' ')
            && (end == bytes.len() || bytes[end] == b' ')
            && ends.is_none_or(|ends| ends.binary_search(&end).is_ok())
    })
}

/// The extension of an original name (after the last `.`), or `""`.
fn extension(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// Calls `f` with the lower-cased `ext`, avoiding allocation for short ASCII extensions.
fn with_lowercase<R>(ext: &str, f: impl FnOnce(&str) -> R) -> R {
    let mut stack = [0u8; 16];
    if ext.is_ascii() && ext.len() <= stack.len() {
        let out = &mut stack[..ext.len()];
        out.copy_from_slice(ext.as_bytes());
        out.make_ascii_lowercase();
        // ASCII stays valid UTF-8.
        f(std::str::from_utf8(out).unwrap_or_default())
    } else {
        f(&ext.to_lowercase())
    }
}

impl Node {
    /// Rough evaluation cost, for ordering AND/OR children.
    fn cost(&self) -> u8 {
        match self {
            Node::Const(_) => 0,
            Node::Name(_) | Node::CaseSensitive(_) | Node::WholeWord(_) => 1,
            Node::Ext(_) | Node::Kind { .. } | Node::Size { .. } | Node::Modified { .. } => 2,
            Node::Fuzzy(_) => 3,
            Node::Path(_) | Node::Parent(..) | Node::InFolder(..) => 5,
            Node::Not(x) => x.cost(),
            Node::And(xs) | Node::Or(xs) => xs.iter().map(Node::cost).max().unwrap_or(0),
        }
    }

    pub(crate) fn eval(&self, e: &Entry<'_>, s: &mut Scratch) -> bool {
        match self {
            Node::Const(b) => *b,
            Node::Name(m) => m.matches(e.name),
            Node::CaseSensitive(f) => f.find(e.ix.name(e.id).as_bytes()).is_some(),
            Node::WholeWord(f) => find_words(e.name, f, None),
            Node::Fuzzy(m) => m
                .score_at_least(e.name, e.ix.name(e.id), &mut s.fuzzy, 0)
                .is_some(),
            Node::Ext(exts) => {
                if e.ix.meta(e.id).is_dir {
                    return false;
                }
                let ext = extension(e.ix.name(e.id));
                exts.iter().any(|x| {
                    if x.is_ascii() {
                        x.eq_ignore_ascii_case(ext)
                    } else {
                        with_lowercase(ext, |l| l == &**x)
                    }
                })
            }
            Node::Kind { exts, folders } => {
                if e.ix.meta(e.id).is_dir {
                    return *folders;
                }
                let ext = extension(e.ix.name(e.id));
                !ext.is_empty() && with_lowercase(ext, |l| exts.binary_search(&l).is_ok())
            }
            Node::Size { lo, hi } => {
                let m = e.ix.meta(e.id);
                !m.is_dir && (*lo..=*hi).contains(&m.size)
            }
            Node::Modified { lo, hi } => {
                e.ix.meta(e.id)
                    .modified
                    .is_some_and(|t| (*lo..=*hi).contains(&t))
            }
            Node::Path(m) => m.matches(s.full_path(e)),
            Node::Parent(FolderMatcher::Words(f), _) => {
                e.ix.parent(e.id)
                    .is_some_and(|p| path_ends_with_words(e.ix, p, f.needle()))
            }
            Node::Parent(FolderMatcher::Glob(segs), slot) => {
                s.memoized(e, *slot, |p, _| !p.is_empty() && glob_match(p, segs))
            }
            Node::InFolder(FolderMatcher::Words(f), slot) => {
                s.in_folder_words(e.ix, e.id, *slot, f.needle())
            }
            Node::InFolder(FolderMatcher::Glob(segs), slot) => s.memoized(e, *slot, |p, bounds| {
                bounds.iter().any(|&b| glob_match(&p[..b], segs))
            }),
            Node::Not(x) => !x.eval(e, s),
            Node::And(xs) => xs.iter().all(|x| x.eval(e, s)),
            Node::Or(xs) => xs.iter().any(|x| x.eval(e, s)),
        }
    }

    /// True if the node has a fuzzy term outside a NOT (its results are ranked).
    pub(crate) fn is_ranked(&self) -> bool {
        match self {
            Node::Fuzzy(_) => true,
            Node::And(xs) | Node::Or(xs) => xs.iter().any(Node::is_ranked),
            _ => false,
        }
    }

    /// [`Node::eval`] and the rank score in one pass: `Some(score)` if the entry matches with
    /// `score >= floor` (see the module docs, "Ranking"), else `None`. A ranked scan passes the
    /// score it already has enough results at or above, so fuzzy terms skip the tiers that
    /// cannot reach it; `floor = 0` is exactly "matches, and its score".
    pub(crate) fn eval_score(&self, e: &Entry<'_>, s: &mut Scratch, floor: u32) -> Option<u32> {
        match self {
            Node::Fuzzy(m) => m.score_at_least(e.name, e.ix.name(e.id), &mut s.fuzzy, floor),
            Node::And(xs) => {
                // The score is the sum of the ranked children's scores; with one ranked child
                // it is that child's score, so the floor applies to it directly.
                let ranked = xs.iter().filter(|x| x.is_ranked()).count();
                let child_floor = if ranked == 1 { floor } else { 0 };
                let mut total = 0u32;
                for x in xs {
                    if x.is_ranked() {
                        total = total.saturating_add(x.eval_score(e, s, child_floor)?);
                    } else if !x.eval(e, s) {
                        return None;
                    }
                }
                (total >= floor).then_some(total)
            }
            Node::Or(xs) => {
                // The score is the best ranked child that matches (0 if only unranked ones
                // do). A child scoring below the floor cannot lift the result above it.
                let mut best: Option<u32> = None;
                let mut matched = false;
                for x in xs {
                    if x.is_ranked() {
                        if let Some(v) = x.eval_score(e, s, floor) {
                            best = Some(best.map_or(v, |b| b.max(v)));
                        }
                    } else if floor == 0 && !matched && best.is_none() {
                        matched = x.eval(e, s);
                    }
                }
                best.or_else(|| matched.then_some(0))
            }
            _ => (floor == 0 && self.eval(e, s)).then_some(0),
        }
    }

    /// The score of an entry that matches, the two-pass way (reference for `eval_score`).
    #[cfg(test)]
    pub(crate) fn score(&self, e: &Entry<'_>, s: &mut Scratch) -> u32 {
        match self {
            Node::Fuzzy(m) => m.score(e.name, e.ix.name(e.id), &mut s.fuzzy).unwrap_or(0),
            Node::And(xs) => xs
                .iter()
                .filter(|x| x.is_ranked())
                .fold(0u32, |acc, x| acc.saturating_add(x.score(e, s))),
            Node::Or(xs) => {
                let mut best = 0;
                for x in xs.iter().filter(|x| x.is_ranked()) {
                    if x.eval(e, s) {
                        best = best.max(x.score(e, s));
                    }
                }
                best
            }
            _ => 0,
        }
    }

    /// Calls `f` with every fuzzy matcher outside a NOT.
    pub(crate) fn for_each_fuzzy(&self, f: &mut impl FnMut(&FuzzyMatcher)) {
        match self {
            Node::Fuzzy(m) => f(m),
            Node::And(xs) | Node::Or(xs) => xs.iter().for_each(|x| x.for_each_fuzzy(f)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for z in [-800_000, -1, 0, 59, 11_016, 20_000, 2_000_000] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn incremental_path_cache_matches_a_fresh_rebuild() {
        use crate::index::{EntryMeta, IndexBuilder};
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let mut b = IndexBuilder::new();
        let root = b.push(None, r"D:\Root", dir);
        let mut dirs = vec![root];
        for i in 0..400u32 {
            // A deterministic mix of deep chains, siblings and jumps back up.
            let parent = dirs[(i as usize * 7919) % dirs.len()];
            let name = match i % 5 {
                0 => format!("مجلد_{i}"),
                1 => "...".to_string(), // normalizes to nothing
                _ => format!("Dir {i}"),
            };
            let id = b.push(
                Some(parent),
                &name,
                if i % 3 == 0 {
                    EntryMeta::default()
                } else {
                    dir
                },
            );
            if i % 3 != 0 {
                dirs.push(id);
            }
        }
        let ix = b.finish();
        let fresh = |id: EntryId| {
            let mut parts = Vec::new();
            let mut cur = Some(id);
            while let Some(c) = cur {
                parts.push(ix.normalized_name(c));
                cur = ix.parent(c);
            }
            parts.reverse();
            parts.retain(|p| !p.is_empty());
            parts.join(" ")
        };
        let mut s = Scratch::default();
        let ids: Vec<EntryId> = (0..ix.len() as EntryId)
            .chain((0..ix.len() as EntryId).rev())
            .chain((0..ix.len() as EntryId).map(|i| (i * 37) % ix.len() as EntryId))
            .collect();
        for id in ids {
            let e = Entry {
                ix: &ix,
                id,
                name: ix.normalized_name(id),
            };
            assert_eq!(s.full_path(&e), fresh(id), "id {id}");
            let parent = ix.parent(id).map(fresh).unwrap_or_default();
            assert_eq!(&s.path[..s.parent_len()], parent, "parent of {id}");
            // The backward word-suffix check agrees with the full path.
            let path = fresh(id);
            for needle in [
                "dir 5",
                "5",
                "مجلد",
                "مجلد 0",
                "d root",
                "ir 5",
                "root dir 2",
                "x",
            ] {
                let expected = path == needle || path.ends_with(&format!(" {needle}"));
                assert_eq!(
                    path_ends_with_words(&ix, id, needle.as_bytes()),
                    expected,
                    "{path:?} / {needle:?}"
                );
            }
        }
    }

    #[test]
    fn size_ranges() {
        assert_eq!(size_range(Comparison::Lt(0)), None);
        assert_eq!(size_range(Comparison::Gt(u64::MAX)), None);
        assert_eq!(size_range(Comparison::Lt(10)), Some((0, 9)));
        assert_eq!(size_range(Comparison::Gt(10)), Some((11, u64::MAX)));
        assert_eq!(size_range(Comparison::Eq(5)), Some((5, 5)));
    }

    #[test]
    fn kind_lists_are_sorted_lowercase_and_disjoint() {
        let kinds = [
            FileKind::Document,
            FileKind::Image,
            FileKind::Video,
            FileKind::Audio,
            FileKind::Code,
            FileKind::Archive,
        ];
        let mut all: Vec<&str> = Vec::new();
        for k in kinds {
            let l = kind_extensions(k);
            assert!(
                l.iter()
                    .all(|x| *x == x.to_lowercase() && !x.starts_with('.'))
            );
            all.extend(l);
        }
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "an extension belongs to two kinds");
    }
}
