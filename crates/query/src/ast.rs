//! The abstract syntax tree produced by [`crate::parse`].
//!
//! Every node carries a [`Span`] (byte offsets into the original query string) so the search
//! bar can color tokens and point at errors without re-tokenizing.

use std::fmt;
use std::ops::Range;

/// A half-open byte range `start..end` into the query string.
///
/// Both ends always fall on UTF-8 character boundaries of the parsed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    /// Byte offset of the first byte.
    pub start: usize,
    /// Byte offset one past the last byte.
    pub end: usize,
}

impl Span {
    /// Creates a span from `start` to `end`.
    pub const fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    /// Length in bytes.
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True when the span covers no bytes.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The text this span covers in `source`, or `""` if it does not fit `source`.
    pub fn slice<'a>(&self, source: &'a str) -> &'a str {
        source.get(self.start..self.end).unwrap_or("")
    }

    /// Smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }
}

impl From<Span> for Range<usize> {
    fn from(s: Span) -> Self {
        s.start..s.end
    }
}

/// A parsed query. `root` is `None` for an empty (or whitespace-only) query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    /// The top-level expression.
    pub root: Option<Expr>,
}

/// An expression node with its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What the node is.
    pub kind: ExprKind,
    /// The bytes it covers, including operators, quotes and brackets that belong to it.
    pub span: Span,
}

/// The kinds of expression.
///
/// Precedence, from tightest to loosest: `!`/`-` (NOT), then juxtaposition (AND), then `|` (OR).
/// `And` and `Or` are flattened n-ary nodes with at least two children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A plain word or a `"quoted phrase"` matched against names/paths.
    Term(Term),
    /// A `name:value` filter (or the `~` semantic prefix).
    Filter(Filter),
    /// `!x` or `-x`.
    Not(Box<Expr>),
    /// `a b c`: all must match.
    And(Vec<Expr>),
    /// `a | b | c`: any may match.
    Or(Vec<Expr>),
    /// `< x >`: kept as a node so the UI can color the brackets; evaluates like `x`.
    Group(Box<Expr>),
}

/// A search term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The text without surrounding quotes. Wildcards (`*`, `?`) and anchors (`^`, `$`) are
    /// kept verbatim; their meaning is decided by the name engine.
    pub text: String,
    /// True for `"quoted phrases"` (exact phrase, spaces included).
    pub quoted: bool,
}

/// A `name:value` filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Which filter, after resolving English/Arabic aliases.
    pub name: FilterName,
    /// Span of the name including the colon (`ext:`), or of `~` for the semantic prefix.
    pub name_span: Span,
    /// The parsed value.
    pub value: FilterValue,
    /// Span of the raw value (including quotes if quoted). Empty for valueless modifiers.
    pub value_span: Span,
}

/// All supported filters. Aliases (including Arabic ones) resolve to these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterName {
    /// `ext:` file extensions (list).
    Ext,
    /// `type:` file kinds (list).
    Type,
    /// `size:` size in bytes.
    Size,
    /// `dm:` date modified.
    DateModified,
    /// `dc:` date created.
    DateCreated,
    /// `da:` date accessed.
    DateAccessed,
    /// `path:` match against the full path.
    Path,
    /// `parent:` direct parent folder only.
    Parent,
    /// `infolder:` anywhere under a folder.
    InFolder,
    /// `content:` full-text search inside files.
    Content,
    /// `sem:` or `~`: semantic (meaning) search.
    Semantic,
    /// `regex:` regular expression on the name.
    Regex,
    /// `case:` case-sensitive match.
    Case,
    /// `ww:` whole-word match.
    WholeWord,
    /// `fuzzy:` fuzzy match (typos, acronyms, subsequences); valueless = switch for the whole
    /// query. Not to be confused with `~`, which is [`FilterName::Semantic`].
    Fuzzy,
    /// `dupe:` duplicates.
    Dupe,
    /// `similar:` similar files.
    Similar,
    /// `tag:` user tags (list).
    Tag,
}

impl FilterName {
    /// The canonical English spelling, without the colon.
    pub const fn canonical(self) -> &'static str {
        match self {
            FilterName::Ext => "ext",
            FilterName::Type => "type",
            FilterName::Size => "size",
            FilterName::DateModified => "dm",
            FilterName::DateCreated => "dc",
            FilterName::DateAccessed => "da",
            FilterName::Path => "path",
            FilterName::Parent => "parent",
            FilterName::InFolder => "infolder",
            FilterName::Content => "content",
            FilterName::Semantic => "sem",
            FilterName::Regex => "regex",
            FilterName::Case => "case",
            FilterName::WholeWord => "ww",
            FilterName::Fuzzy => "fuzzy",
            FilterName::Dupe => "dupe",
            FilterName::Similar => "similar",
            FilterName::Tag => "tag",
        }
    }
}

impl fmt::Display for FilterName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical())
    }
}

/// A parsed filter value. Which variant appears depends on the [`FilterName`]:
///
/// | filter | value |
/// |---|---|
/// | `ext` | `Extensions` |
/// | `type` | `Kinds` |
/// | `tag` | `Tags` |
/// | `size` | `Size` |
/// | `dm` `dc` `da` | `Date` |
/// | `case` `ww` `fuzzy` `dupe` | `Text`, or `None` when written without a value (a global switch) |
/// | everything else | `Text` |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterValue {
    /// No value (`case:` on its own).
    None,
    /// Free text (quotes removed).
    Text(String),
    /// Lower-cased extensions without the leading dot.
    Extensions(Vec<String>),
    /// File kinds.
    Kinds(Vec<FileKind>),
    /// Tag names.
    Tags(Vec<String>),
    /// A size comparison in bytes.
    Size(Comparison<u64>),
    /// A date comparison.
    Date(Comparison<DateSpec>),
}

/// A comparison against a value. Ranges are inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison<T> {
    /// `v` (for dates: within that day/month/year/period).
    Eq(T),
    /// `<v`
    Lt(T),
    /// `<=v` (also `..v`)
    Le(T),
    /// `>v`
    Gt(T),
    /// `>=v` (also `v..`)
    Ge(T),
    /// `a..b`
    Range {
        /// Lower bound, inclusive.
        min: T,
        /// Upper bound, inclusive.
        max: T,
    },
}

/// A file kind for `type:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// Documents (pdf, docx, txt...).
    Document,
    /// Images.
    Image,
    /// Video.
    Video,
    /// Audio.
    Audio,
    /// Source code.
    Code,
    /// Archives (zip, 7z, rar...).
    Archive,
    /// Folders.
    Folder,
}

/// A date as written in the query. Relative dates are **not** resolved here: the parser is
/// pure and clock-free; the executor resolves `Today` etc. against the current local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSpec {
    /// A relative period such as `today` or `lastmonth`.
    Named(NamedDate),
    /// A calendar year, month or day such as `2026`, `2026-03` or `2026-03-15`.
    Calendar(CalendarDate),
}

/// Relative periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedDate {
    /// `today` / `اليوم`
    Today,
    /// `yesterday` / `أمس`
    Yesterday,
    /// `thisweek` / `هذا_الأسبوع`
    ThisWeek,
    /// `lastweek` / `الأسبوع_الماضي`
    LastWeek,
    /// `thismonth` / `هذا_الشهر`
    ThisMonth,
    /// `lastmonth` / `الشهر_الماضي`
    LastMonth,
    /// `thisyear` / `هذا_العام`
    ThisYear,
    /// `lastyear` / `العام_الماضي`
    LastYear,
}

/// A calendar date with year, month or day precision. Validated: month is 1-12 and day exists
/// in that month (leap years included).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalendarDate {
    /// Year, 1-9999.
    pub year: u16,
    /// Month 1-12, if given.
    pub month: Option<u8>,
    /// Day of month, if given (only when `month` is given).
    pub day: Option<u8>,
}
