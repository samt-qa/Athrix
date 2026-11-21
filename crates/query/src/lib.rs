//! Athrix query language: parser and AST.
//!
//! [`parse`] turns what the user types in the search bar into a [`Query`] tree. The syntax is
//! Everything-compatible where possible (see `docs/01-RESEARCH-AND-FEATURES.md` §7) with
//! Arabic synonyms.
//!
//! # Syntax
//!
//! | syntax | meaning |
//! |---|---|
//! | `a b` | AND (implicit, by whitespace) |
//! | `a \| b` | OR |
//! | `!a`, `-a` | NOT (a lone `!`/`-` followed by a space is a literal term) |
//! | `"a b"` | exact phrase (no escapes) |
//! | `< a \| b > c` | grouping |
//! | `ext:pdf;docx` | extensions (`;` or Arabic `؛` separated; leading `.`/`*.` dropped) |
//! | `type:image;video` | kinds: doc, image, video, audio, code, archive, folder |
//! | `size:>100mb`, `size:10mb..50mb`, `size:1mb-2mb`, `size:large` | size (1024-based units) |
//! | `dm:` `dc:` `da:` + `today`, `2026`, `2026-03`, `>=2026-03-15`, `2026-01..2026-03` | dates |
//! | `path:` `parent:` `infolder:` | path filters |
//! | `content:` | full-text |
//! | `sem:words…`, `~words…` | semantic; takes free text up to the next filter, `\|`, bracket or `!`/`-` word |
//! | `regex:` `case:` `ww:` `fuzzy:` | match modes (`case:` etc. may stand alone as a switch) |
//! | `dupe:` `similar:` `tag:` | duplicates, similar files, tags |
//!
//! Precedence: NOT binds tightest, then AND, then OR, so `a b | c` is `(a AND b) OR c`.
//! (Note: Everything itself binds OR tighter than AND; Athrix follows the conventional order.)
//!
//! Filter values end at whitespace, `|`, `<`, `>` or `"`; quote a value to include those
//! (`regex:"a|b"`, `path:"My Documents"`). For `size:` and `dm:`/`dc:`/`da:` a leading
//! comparison operator (`>`, `>=`, `<`, `<=`, `=`) is part of the value.
//!
//! Words of the form `x:` where `x` is a single character (drive letters like `C:\`), contains a
//! digit (`10:30`), or is followed by `//` (URLs) are plain terms. Any other unknown `name:` is
//! an error that suggests the closest known filter.
//!
//! ## Arabic
//!
//! Filter names: `امتداد:` (ext), `نوع:` (type), `حجم:` (size), `تاريخ:` (dm), `تاريخ_الإنشاء:` (dc),
//! `تاريخ_الوصول:` (da), `مسار:` (path), `مجلد:` (infolder), `محتوى:` (content), `دلالي:` (sem),
//! `مكرر:` (dupe), `مشابه:` (similar), `وسم:` (tag). Matching ignores hamza variants
//! (أ/إ/آ → ا), ى/ي, tatweel and harakat. Kind values (`صور`, `مستندات`, `فيديو`, `صوت`,
//! `كود`, `أرشيف`, `مجلدات`…), named dates (`اليوم`, `أمس`, `هذا_الأسبوع`, `الشهر_الماضي`…),
//! size units (`ميجا`, `جيجا`…) and Arabic-Indic digits (`٠`-`٩`, `۰`-`۹`, `٫`) are accepted.
//!
//! # Why a hand-written parser
//!
//! `winnow` and `chumsky` were considered (the research doc names both). A hand-written
//! recursive-descent parser was chosen because:
//!
//! - The lexing is context-sensitive: `>` closes a group but is also a comparison inside
//!   `size:>1mb`; `-` is NOT at the start of a word, part of a word inside it, and a range
//!   separator in `size:1mb-2mb`; `~` swallows free text. Expressing this with combinators
//!   is possible but no shorter, and harder to audit.
//! - Exact control over byte spans and friendly, typed errors (with suggestions) for the search
//!   bar, which matter more here than generic "expected one of …" messages.
//! - It must parse on every keystroke and never panic on arbitrary input: a small parser with
//!   bounded recursion ([`MAX_DEPTH`]) and only char-boundary slicing is easy to reason about
//!   and is fuzzed with `proptest`. A typical query parses in a few microseconds.
//! - Zero runtime dependencies and fast compile times.
//!
//! # Purity
//!
//! The parser does not read the clock or the file system. Relative dates stay as
//! [`NamedDate`] values; the executor resolves them.

pub mod ast;
pub mod error;
mod lexicon;
mod parser;
mod values;

pub use ast::{
    CalendarDate, Comparison, DateSpec, Expr, ExprKind, FileKind, Filter, FilterName, FilterValue,
    NamedDate, Query, Span, Term,
};
pub use error::{ParseError, ParseErrorKind};
pub use parser::{MAX_DEPTH, parse};

impl std::str::FromStr for Query {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s)
    }
}
