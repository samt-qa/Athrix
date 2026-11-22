//! Natural-language queries → filters (task 6.8).
//!
//! [`interpret`] reads what a user types in plain Arabic or English — «ملفات PDF من الأسبوع
//! الماضي أكبر من 5 ميجا», "excel files from yesterday bigger than 2mb" — and proposes the
//! equivalent query in the filter syntax (`ext:pdf dm:lastweek size:>5mb`). It is **purely
//! lexical**: phrase tables and a few number/date rules, no model, no network, no clock (the
//! caller passes today's date for relative phrases such as «قبل 3 أيام»).
//!
//! # When it answers
//!
//! It returns `None` (nothing to suggest) unless all of these hold:
//!
//! - the text parses and contains **no filter syntax** and no `|` / `< >` grouping: it is a
//!   plain list of words, `"quoted phrases"` and `-negated` words. Interpreting a query that
//!   already uses filters (including this module's own output) therefore returns `None`;
//! - at least one **strong** cue (a date phrase with a preposition or of several words, a size,
//!   a content cue, a folder/drive, a kind next to «ملفات»/"files"), or two **weak** cues (a
//!   bare kind word such as «صور», a bare year such as `2025`, a lone «اليوم»/"today"), or one
//!   weak cue next to a filler word («ابحث عن», "show me", «من»…). So «تقرير المبيعات», "report
//!   final" or "report 2025" are left alone.
//!
//! Quoted phrases and negated words are never interpreted: `"last week" notes` keeps the
//! phrase as a name term. Words that are not cues stay as name terms, in order. Filler words
//! («ملفات», «أريد», "find", "the"…) are dropped. The result always parses and plans with
//! [`crate::parse`] / [`crate::plan()`] (checked before returning).
//!
//! # What it recognises
//!
//! | cue | examples | output |
//! |---|---|---|
//! | kinds | «ملفات PDF», «وورد», «إكسل», «عروض», «صور», «فيديو», «صوتيات», «كود», «مجلدات», "pdf", "word documents", "spreadsheets", "photos", "music", "folders" | `ext:pdf`, `ext:docx;doc`, `type:image`… (several kinds are merged; `ext:` and `type:` together become `<ext:… \| type:…>`) |
//! | named dates | «اليوم»، «أمس»، «هذا الأسبوع»، «الأسبوع الماضي»، «الشهر الماضي»، «السنة الماضية», "today", "last week", "this year" | `dm:today`, `dm:lastweek`… |
//! | years, months | «في 2024»، «سنة ٢٠٢٣»، «مارس 2025», "in 2024", "since 2023", "before 2020", "march 2025" | `dm:2024`, `dm:>=2023`, `dm:<2020`, `dm:2025-03` |
//! | relative (needs today) | «قبل 3 أيام»، «منذ شهرين»، «آخر 7 أيام»، «أقدم من سنة», "3 days ago", "last 7 days", "past 2 weeks", "older than 30 days" | `dm:2026-11-19`, `dm:>=2026-09-22`, `dm:<2025-11-22`… |
//! | sizes | «أكبر من 5 ميجا»، «أقل من ٢٠٠ ك»، «بين 1 و 5 جيجا», "bigger than 2mb", "under 500 kb", "at least 1gb" | `size:>5mb`, `size:<200kb`, `size:1gb..5gb`, `size:>=1gb` |
//! | content | «تحتوي على عقد الإيجار»، «فيها كلمة ميزانية»، «عن الضرائب» (after a kind), "containing budget", "that mention invoice", "with the text …" | `content:"عقد الإيجار"`, `content:budget` |
//! | location | «في مجلد التنزيلات»، «على القرص D», "in folder Downloads", "in the Projects folder", "on drive D", "on D:" | `infolder:التنزيلات`, `infolder:D:` |
//!
//! Arabic matching ignores hamza forms, ى/ي, ة/ه, tatweel and harakat, and accepts
//! Arabic-Indic digits. Dates always use `dm:` (modified): "created" is read as a filler word,
//! because the name index does not support `dc:` yet.

use crate::ast::{CalendarDate, Expr, ExprKind, Span};
use crate::lexicon::{ascii_digits, normalize_word};
use crate::plan::{PlanOptions, plan};
use crate::values::{parse_date, parse_size};

/// Inputs longer than this (in bytes) are not interpreted.
const MAX_INPUT_BYTES: usize = 1024;
/// Inputs with more words than this are not interpreted.
const MAX_TOKENS: usize = 64;

/// Options for [`interpret_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterpretOptions {
    /// Today's local date (year, month **and** day). Needed for relative phrases («قبل 3 أيام»,
    /// "last 7 days", "older than a year", a month without a year): without it those phrases
    /// are not recognised and stay as words. Named periods («الأسبوع الماضي» → `dm:lastweek`)
    /// do not need it: the executor resolves them.
    pub today: Option<CalendarDate>,
}

impl InterpretOptions {
    /// Options with today's date set.
    pub const fn with_today(today: CalendarDate) -> Self {
        InterpretOptions { today: Some(today) }
    }
}

/// What a piece of the input was understood as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartRole {
    /// A file kind (`ext:` / `type:`).
    Kind,
    /// A date (`dm:`).
    Date,
    /// A size (`size:`).
    Size,
    /// Words to find inside files (`content:`).
    Content,
    /// A folder or drive (`infolder:`).
    Location,
    /// Kept as a name term (also quoted phrases and negated words).
    Term,
    /// A filler word that was dropped («ابحث عن», "files", "the"…).
    Ignored,
}

impl PartRole {
    /// A stable lower-case name (`kind`, `date`, `size`, `content`, `location`, `term`,
    /// `ignored`), for serialization.
    pub const fn as_str(self) -> &'static str {
        match self {
            PartRole::Kind => "kind",
            PartRole::Date => "date",
            PartRole::Size => "size",
            PartRole::Content => "content",
            PartRole::Location => "location",
            PartRole::Term => "term",
            PartRole::Ignored => "ignored",
        }
    }
}

/// One piece of the input and what it became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Byte range of the piece in the input text.
    pub span: Span,
    /// What it was understood as.
    pub role: PartRole,
    /// What it contributes to the query (`ext:pdf`, `dm:lastweek`, a kept word), or `""` for
    /// [`PartRole::Ignored`]. Kinds are listed one by one here and merged in
    /// [`Interpretation::query`].
    pub output: String,
}

/// The result of [`interpret`]: a suggested query and how it was derived.
#[derive(Debug, Clone, PartialEq)]
pub struct Interpretation {
    /// The suggested query, in the filter syntax. Always parses and plans.
    pub query: String,
    /// Every word of the input, grouped into pieces, in input order.
    pub parts: Vec<Part>,
    /// A rough 0.0-1.0 score: higher with more (and stronger) cues, lower with more words
    /// left as name terms. Only a hint for the UI; it is not a probability.
    pub confidence: f32,
}

/// Interprets `text` without knowing today's date (relative phrases such as «قبل 3 أيام» are
/// not recognised). See the [module docs](self) and [`interpret_with`].
pub fn interpret(text: &str) -> Option<Interpretation> {
    interpret_with(text, &InterpretOptions::default())
}

/// Interprets `text` as a natural-language query. Returns `None` when there is nothing useful
/// to suggest (see the [module docs](self)). Never panics.
pub fn interpret_with(text: &str, options: &InterpretOptions) -> Option<Interpretation> {
    if text.len() > MAX_INPUT_BYTES {
        return None;
    }
    let ast = crate::parse(text).ok()?;
    let toks = tokens(text, ast.root.as_ref()?)?;
    if toks.len() > MAX_TOKENS {
        return None;
    }
    let today = options.today.and_then(Day::from_calendar);
    let cx = Cx { toks: &toks, today };

    let mut hits: Vec<(usize, Hit)> = Vec::new();
    let mut seen_kind_or_files = false;
    let mut files_end: Option<usize> = None;
    let mut i = 0;
    while i < toks.len() {
        if !toks[i].plain {
            hits.push((i, Hit::term(&toks[i])));
            i += 1;
            continue;
        }
        let after_files = files_end == Some(i);
        let hit = cx
            .size_at(i)
            .or_else(|| cx.date_at(i))
            .or_else(|| cx.location_at(i))
            .or_else(|| cx.content_at(i, seen_kind_or_files))
            .or_else(|| cx.kind_at(i, after_files))
            .or_else(|| cx.filler_at(i))
            .unwrap_or_else(|| Hit::term(&toks[i]));
        let len = hit.len.max(1);
        if hit.files || hit.role == PartRole::Kind {
            seen_kind_or_files = true;
        }
        if hit.files {
            files_end = Some(i + len);
        }
        hits.push((i, hit));
        i += len;
    }

    let count = |f: &dyn Fn(&Hit) -> bool| hits.iter().filter(|(_, h)| f(h)).count();
    let strong = count(&|h| h.strength == Strength::Strong);
    let weak = count(&|h| h.strength == Strength::Weak);
    let filler = count(&|h| h.role == PartRole::Ignored);
    let terms = count(&|h| h.role == PartRole::Term);
    if !(strong >= 1 || weak >= 2 || (weak >= 1 && filler >= 1)) {
        return None;
    }

    let query = build_query(&hits);
    // The contract: the suggestion always parses, carries filters and plans.
    let parsed = crate::parse(&query).ok()?;
    if !has_filter(parsed.root.as_ref()?) {
        return None;
    }
    plan(&parsed, &PlanOptions::default()).ok()?;

    let parts = hits
        .iter()
        .map(|(start, h)| {
            let first = toks[*start].span;
            let last = toks[(*start + h.len.max(1) - 1).min(toks.len() - 1)].span;
            Part {
                span: first.to(last),
                role: h.role,
                output: h.output.clone(),
            }
        })
        .collect();
    let score =
        0.45 * strong as f32 + 0.25 * weak as f32 + 0.05 * filler as f32 - 0.08 * terms as f32;
    Some(Interpretation {
        query,
        parts,
        confidence: score.clamp(0.1, 1.0),
    })
}

fn has_filter(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Filter(_) => true,
        ExprKind::Term(_) => false,
        ExprKind::Not(x) | ExprKind::Group(x) => has_filter(x),
        ExprKind::And(v) | ExprKind::Or(v) => v.iter().any(has_filter),
    }
}

// ---------------------------------------------------------------------------------------------
// Tokens

/// A top-level word of the input.
struct Tok<'a> {
    /// The text as typed (with quotes / `-` for non-plain tokens).
    raw: &'a str,
    span: Span,
    /// Normalized for matching (see [`norm`]). Empty for non-plain tokens.
    norm: String,
    /// An unquoted, non-negated word: the only kind of token cues are read from.
    plain: bool,
    /// The inside of a quoted phrase.
    quoted: Option<&'a str>,
}

/// The input's top-level words, or `None` if it uses anything but plain words, quoted phrases
/// and negated words (filters, OR, groups).
fn tokens<'a>(text: &'a str, root: &'a Expr) -> Option<Vec<Tok<'a>>> {
    let items: &[Expr] = match &root.kind {
        ExprKind::And(v) => v,
        _ => std::slice::from_ref(root),
    };
    let mut out = Vec::with_capacity(items.len());
    for e in items {
        let raw = text.get(e.span.start..e.span.end)?;
        let tok = match &e.kind {
            ExprKind::Term(t) if t.quoted => Tok {
                raw,
                span: e.span,
                norm: String::new(),
                plain: false,
                quoted: Some(t.text.as_str()).filter(|s| !s.contains('"')),
            },
            ExprKind::Term(_) => Tok {
                raw,
                span: e.span,
                norm: norm(raw),
                plain: true,
                quoted: None,
            },
            ExprKind::Not(x) if matches!(x.kind, ExprKind::Term(_)) => Tok {
                raw,
                span: e.span,
                norm: String::new(),
                plain: false,
                quoted: None,
            },
            _ => return None,
        };
        out.push(tok);
    }
    Some(out)
}

/// Punctuation that may stick to a word in a sentence.
const PUNCT: &[char] = &[
    ',', '.', '،', '؛', ';', '?', '؟', '!', ':', '(', ')', '«', '»', '\'', '’',
];

/// Matching form: ASCII digits, lower case, Arabic spelling variants folded (see
/// [`normalize_word`]) plus ة → ه, surrounding punctuation removed.
fn norm(s: &str) -> String {
    let s = normalize_word(&ascii_digits(s.trim_matches(PUNCT)));
    s.replace('ة', "ه")
}

/// `raw` without surrounding sentence punctuation (for words copied into the output).
fn clean(raw: &str) -> &str {
    let t = raw.trim_matches(PUNCT);
    if t.is_empty() { raw } else { t }
}

// ---------------------------------------------------------------------------------------------
// Hits

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    Strong,
    Weak,
    Zero,
}

#[derive(Debug, Clone)]
struct Hit {
    /// Tokens covered.
    len: usize,
    role: PartRole,
    output: String,
    strength: Strength,
    /// A kind: its extensions or `type:` value, for merging.
    kind: Option<KindVal>,
    /// A «ملفات» / "files" word (makes a neighbouring kind strong).
    files: bool,
}

impl Hit {
    fn term(t: &Tok<'_>) -> Hit {
        let output = if t.plain { clean(t.raw) } else { t.raw };
        Hit {
            len: 1,
            role: PartRole::Term,
            output: output.to_owned(),
            strength: Strength::Zero,
            kind: None,
            files: false,
        }
    }

    fn new(len: usize, role: PartRole, output: String, strength: Strength) -> Hit {
        Hit {
            len,
            role,
            output,
            strength,
            kind: None,
            files: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindVal {
    /// `;`-separated extensions.
    Ext(&'static str),
    /// A `type:` value.
    Type(&'static str),
}

impl KindVal {
    fn output(self) -> String {
        match self {
            KindVal::Ext(e) => format!("ext:{e}"),
            KindVal::Type(t) => format!("type:{t}"),
        }
    }
}

/// Joins the hits into the query: kinds merged at the position of the first kind, other
/// outputs in input order, fillers dropped.
fn build_query(hits: &[(usize, Hit)]) -> String {
    let mut exts: Vec<&str> = Vec::new();
    let mut types: Vec<&str> = Vec::new();
    for (_, h) in hits {
        match h.kind {
            Some(KindVal::Ext(e)) => {
                for x in e.split(';') {
                    if !exts.contains(&x) {
                        exts.push(x);
                    }
                }
            }
            Some(KindVal::Type(t)) if !types.contains(&t) => types.push(t),
            _ => {}
        }
    }
    // «مستندات PDF» / "pdf documents": the extension is the more precise of the two.
    if !exts.is_empty() && types.iter().all(|t| *t == "doc") {
        types.clear();
    }
    let kinds = match (exts.is_empty(), types.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("ext:{}", exts.join(";")),
        (true, false) => format!("type:{}", types.join(";")),
        (false, false) => format!("<ext:{} | type:{}>", exts.join(";"), types.join(";")),
    };
    let mut out: Vec<String> = Vec::new();
    let mut kinds_done = false;
    for (_, h) in hits {
        if h.kind.is_some() {
            if !kinds_done {
                out.push(kinds.clone());
                kinds_done = true;
            }
        } else if !h.output.is_empty() && h.role != PartRole::Ignored {
            out.push(h.output.clone());
        }
    }
    out.join(" ")
}

// ---------------------------------------------------------------------------------------------
// Phrase tables. Entries are written naturally; matching normalizes both sides.

/// Words that stand for "files" and make a neighbouring kind strong.
const FILES_WORDS: &[&str] = &[
    "ملفات",
    "ملف",
    "الملفات",
    "الملف",
    "files",
    "file",
    "documents",
    "document",
    "docs",
];

/// Dropped filler words and phrases.
const FILLERS: &[&str] = &[
    "ابحث عن",
    "أبحث عن",
    "ابحث",
    "أبحث",
    "بحث عن",
    "دور على",
    "دورلي على",
    "دور لي على",
    "أريد",
    "اريد",
    "أبي",
    "أبغى",
    "عايز",
    "عاوز",
    "بدي",
    "اعرض لي",
    "اعرض",
    "أظهر لي",
    "أظهر",
    "هات لي",
    "هات",
    "جيب لي",
    "جيب",
    "أعطني",
    "لي",
    "كل",
    "جميع",
    "التي",
    "اللي",
    "الذي",
    "تم",
    "المعدلة",
    "معدلة",
    "عدلت",
    "عدلتها",
    "تعديلها",
    "تعديله",
    "المحفوظة",
    "حفظت",
    "أنشئت",
    "المنشأة",
    "إنشاؤها",
    "حجمها",
    "حجمه",
    "بحجم",
    "حجم",
    "ذات",
    "من فضلك",
    "لو سمحت",
    "رجاء",
    "على الجهاز",
    "في الجهاز",
    "و",
    "أو",
    "من",
    "في",
    "على",
    "عن",
    "إلى",
    "الى",
    "show me",
    "show",
    "find me",
    "find",
    "search for",
    "search",
    "look for",
    "get me",
    "get",
    "give me",
    "list",
    "i want",
    "i need",
    "all",
    "my",
    "the",
    "a",
    "an",
    "some",
    "any",
    "that",
    "which",
    "are",
    "were",
    "was",
    "is",
    "modified",
    "edited",
    "changed",
    "updated",
    "saved",
    "created",
    "sized",
    "size",
    "with size",
    "please",
    "on my computer",
    "on my pc",
    "from",
    "in",
    "of",
    "with",
    "and",
    "or",
    "on",
    "for",
];

/// File kinds. Single-word entries may also carry an attached «و» ("and").
const KINDS: &[(&str, KindVal)] = &[
    ("pdf", KindVal::Ext("pdf")),
    ("pdfs", KindVal::Ext("pdf")),
    ("بي دي اف", KindVal::Ext("pdf")),
    ("بي دي إف", KindVal::Ext("pdf")),
    ("وورد", KindVal::Ext("docx;doc")),
    ("ورد", KindVal::Ext("docx;doc")),
    ("word files", KindVal::Ext("docx;doc")),
    ("word file", KindVal::Ext("docx;doc")),
    ("word documents", KindVal::Ext("docx;doc")),
    ("word document", KindVal::Ext("docx;doc")),
    ("word docs", KindVal::Ext("docx;doc")),
    ("word doc", KindVal::Ext("docx;doc")),
    ("ms word", KindVal::Ext("docx;doc")),
    ("microsoft word", KindVal::Ext("docx;doc")),
    ("إكسل", KindVal::Ext("xlsx;xls")),
    ("إكسيل", KindVal::Ext("xlsx;xls")),
    ("excel", KindVal::Ext("xlsx;xls")),
    ("spreadsheets", KindVal::Ext("xlsx;xls")),
    ("spreadsheet", KindVal::Ext("xlsx;xls")),
    ("جداول بيانات", KindVal::Ext("xlsx;xls")),
    ("جدول بيانات", KindVal::Ext("xlsx;xls")),
    ("بوربوينت", KindVal::Ext("pptx;ppt")),
    ("باوربوينت", KindVal::Ext("pptx;ppt")),
    ("باور بوينت", KindVal::Ext("pptx;ppt")),
    ("عروض تقديمية", KindVal::Ext("pptx;ppt")),
    ("عرض تقديمي", KindVal::Ext("pptx;ppt")),
    ("عروض", KindVal::Ext("pptx;ppt")),
    ("شرائح", KindVal::Ext("pptx;ppt")),
    ("powerpoint", KindVal::Ext("pptx;ppt")),
    ("power point", KindVal::Ext("pptx;ppt")),
    ("presentations", KindVal::Ext("pptx;ppt")),
    ("presentation", KindVal::Ext("pptx;ppt")),
    ("slides", KindVal::Ext("pptx;ppt")),
    ("نصية", KindVal::Ext("txt")),
    ("text files", KindVal::Ext("txt")),
    ("text file", KindVal::Ext("txt")),
    ("صور", KindVal::Type("image")),
    ("صورة", KindVal::Type("image")),
    ("الصور", KindVal::Type("image")),
    ("صوري", KindVal::Type("image")),
    ("لقطات شاشة", KindVal::Type("image")),
    ("images", KindVal::Type("image")),
    ("image", KindVal::Type("image")),
    ("photos", KindVal::Type("image")),
    ("photo", KindVal::Type("image")),
    ("pictures", KindVal::Type("image")),
    ("picture", KindVal::Type("image")),
    ("pics", KindVal::Type("image")),
    ("screenshots", KindVal::Type("image")),
    ("مقاطع فيديو", KindVal::Type("video")),
    ("مقطع فيديو", KindVal::Type("video")),
    ("فيديو", KindVal::Type("video")),
    ("فيديوهات", KindVal::Type("video")),
    ("الفيديو", KindVal::Type("video")),
    ("الفيديوهات", KindVal::Type("video")),
    ("مقاطع", KindVal::Type("video")),
    ("أفلام", KindVal::Type("video")),
    ("videos", KindVal::Type("video")),
    ("video", KindVal::Type("video")),
    ("movies", KindVal::Type("video")),
    ("clips", KindVal::Type("video")),
    ("تسجيلات صوتية", KindVal::Type("audio")),
    ("صوت", KindVal::Type("audio")),
    ("صوتيات", KindVal::Type("audio")),
    ("الصوتيات", KindVal::Type("audio")),
    ("صوتية", KindVal::Type("audio")),
    ("أغاني", KindVal::Type("audio")),
    ("الأغاني", KindVal::Type("audio")),
    ("موسيقى", KindVal::Type("audio")),
    ("الموسيقى", KindVal::Type("audio")),
    ("تسجيلات", KindVal::Type("audio")),
    ("music", KindVal::Type("audio")),
    ("songs", KindVal::Type("audio")),
    ("audio", KindVal::Type("audio")),
    ("recordings", KindVal::Type("audio")),
    ("podcasts", KindVal::Type("audio")),
    ("كود", KindVal::Type("code")),
    ("الكود", KindVal::Type("code")),
    ("أكواد", KindVal::Type("code")),
    ("برمجة", KindVal::Type("code")),
    ("برمجية", KindVal::Type("code")),
    ("شيفرة", KindVal::Type("code")),
    ("source code", KindVal::Type("code")),
    ("source files", KindVal::Type("code")),
    ("code", KindVal::Type("code")),
    ("scripts", KindVal::Type("code")),
    ("مجلدات", KindVal::Type("folder")),
    ("المجلدات", KindVal::Type("folder")),
    ("فولدرات", KindVal::Type("folder")),
    ("folders", KindVal::Type("folder")),
    ("directories", KindVal::Type("folder")),
    ("مستندات", KindVal::Type("doc")),
    ("المستندات", KindVal::Type("doc")),
    ("مستند", KindVal::Type("doc")),
    ("وثائق", KindVal::Type("doc")),
    ("الوثائق", KindVal::Type("doc")),
    ("documents", KindVal::Type("doc")),
    ("docs", KindVal::Type("doc")),
    ("ملفات مضغوطة", KindVal::Type("archive")),
    ("مضغوطة", KindVal::Type("archive")),
    ("أرشيفات", KindVal::Type("archive")),
    ("archives", KindVal::Type("archive")),
    ("zips", KindVal::Type("archive")),
    ("zip files", KindVal::Type("archive")),
    ("compressed", KindVal::Type("archive")),
];

/// Extensions accepted on their own (`mp3 files`, «ملفات csv»).
const KNOWN_EXTS: &[&str] = &[
    "docx", "doc", "xlsx", "xls", "pptx", "ppt", "txt", "csv", "rtf", "odt", "md", "json", "xml",
    "html", "jpg", "jpeg", "png", "gif", "webp", "heic", "svg", "bmp", "tiff", "mp3", "wav",
    "flac", "m4a", "ogg", "mp4", "mkv", "avi", "mov", "webm", "zip", "rar", "7z", "iso", "exe",
    "msi", "apk", "py", "rs", "js", "ts", "cs", "java", "cpp", "epub", "psd", "dwg",
];

/// Named periods (output value, multi-word phrases are strong on their own).
const NAMED_DATES: &[(&str, &str)] = &[
    ("اليوم", "today"),
    ("هذا اليوم", "today"),
    ("النهارده", "today"),
    ("أمس", "yesterday"),
    ("بالأمس", "yesterday"),
    ("البارحة", "yesterday"),
    ("امبارح", "yesterday"),
    ("مبارح", "yesterday"),
    ("هذا الأسبوع", "thisweek"),
    ("الأسبوع الحالي", "thisweek"),
    ("الأسبوع هذا", "thisweek"),
    ("هالأسبوع", "thisweek"),
    ("الأسبوع الماضي", "lastweek"),
    ("الأسبوع السابق", "lastweek"),
    ("الأسبوع الفائت", "lastweek"),
    ("الأسبوع اللي فات", "lastweek"),
    ("هذا الشهر", "thismonth"),
    ("الشهر الحالي", "thismonth"),
    ("الشهر هذا", "thismonth"),
    ("هالشهر", "thismonth"),
    ("الشهر الماضي", "lastmonth"),
    ("الشهر السابق", "lastmonth"),
    ("الشهر الفائت", "lastmonth"),
    ("الشهر اللي فات", "lastmonth"),
    ("هذه السنة", "thisyear"),
    ("هذا العام", "thisyear"),
    ("السنة الحالية", "thisyear"),
    ("العام الحالي", "thisyear"),
    ("السنة هذه", "thisyear"),
    ("هالسنة", "thisyear"),
    ("السنة الماضية", "lastyear"),
    ("العام الماضي", "lastyear"),
    ("السنة السابقة", "lastyear"),
    ("العام السابق", "lastyear"),
    ("السنة الفائتة", "lastyear"),
    ("العام الفائت", "lastyear"),
    ("السنة اللي فاتت", "lastyear"),
    ("today", "today"),
    ("yesterday", "yesterday"),
    ("this week", "thisweek"),
    ("last week", "lastweek"),
    ("previous week", "lastweek"),
    ("this month", "thismonth"),
    ("last month", "lastmonth"),
    ("previous month", "lastmonth"),
    ("this year", "thisyear"),
    ("last year", "lastyear"),
    ("previous year", "lastyear"),
];

/// Words that may introduce a date ("from last week", «من الشهر الماضي»).
const DATE_PREPS: &[&str] = &[
    "من",
    "في",
    "خلال",
    "بتاريخ",
    "عدلت",
    "المعدلة",
    "معدلة",
    "تم تعديلها",
    "from",
    "in",
    "during",
    "on",
    "of",
    "dated",
    "modified",
    "edited",
    "changed",
    "updated",
    "created",
    "saved",
    "modified in",
    "modified on",
    "edited in",
    "created in",
    "created on",
];

/// "Since" / "before" / "after" a year.
const YEAR_OPS: &[(&str, &str)] = &[
    ("منذ", ">="),
    ("منذ عام", ">="),
    ("منذ سنة", ">="),
    ("من بعد", ">"),
    ("بعد", ">"),
    ("بعد عام", ">"),
    ("قبل", "<"),
    ("قبل عام", "<"),
    ("قبل سنة", "<"),
    ("since", ">="),
    ("after", ">"),
    ("before", "<"),
];

/// Words written between a preposition and a year: «في عام 2024», "in year 2024".
const YEAR_WORDS: &[&str] = &["عام", "سنة", "العام", "year", "the year"];

const MONTHS: &[(&str, u8)] = &[
    ("يناير", 1),
    ("كانون الثاني", 1),
    ("فبراير", 2),
    ("شباط", 2),
    ("مارس", 3),
    ("آذار", 3),
    ("أبريل", 4),
    ("ابريل", 4),
    ("نيسان", 4),
    ("مايو", 5),
    ("أيار", 5),
    ("يونيو", 6),
    ("يونيه", 6),
    ("حزيران", 6),
    ("يوليو", 7),
    ("يوليه", 7),
    ("تموز", 7),
    ("أغسطس", 8),
    ("آب", 8),
    ("سبتمبر", 9),
    ("أيلول", 9),
    ("أكتوبر", 10),
    ("تشرين الأول", 10),
    ("نوفمبر", 11),
    ("تشرين الثاني", 11),
    ("ديسمبر", 12),
    ("كانون الأول", 12),
    ("january", 1),
    ("jan", 1),
    ("february", 2),
    ("feb", 2),
    ("march", 3),
    ("mar", 3),
    ("april", 4),
    ("apr", 4),
    ("may", 5),
    ("june", 6),
    ("jun", 6),
    ("july", 7),
    ("jul", 7),
    ("august", 8),
    ("aug", 8),
    ("september", 9),
    ("sep", 9),
    ("sept", 9),
    ("october", 10),
    ("oct", 10),
    ("november", 11),
    ("nov", 11),
    ("december", 12),
    ("dec", 12),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
}

/// Time units: (word, unit, implied count) — Arabic duals imply 2, other words 1 (a number in
/// front overrides it).
const UNITS: &[(&str, Unit, u32)] = &[
    ("يوم", Unit::Day, 1),
    ("أيام", Unit::Day, 1),
    ("يوما", Unit::Day, 1),
    ("يومين", Unit::Day, 2),
    ("يومان", Unit::Day, 2),
    ("أسبوع", Unit::Week, 1),
    ("أسابيع", Unit::Week, 1),
    ("أسبوعين", Unit::Week, 2),
    ("أسبوعان", Unit::Week, 2),
    ("شهر", Unit::Month, 1),
    ("أشهر", Unit::Month, 1),
    ("شهور", Unit::Month, 1),
    ("شهرين", Unit::Month, 2),
    ("شهران", Unit::Month, 2),
    ("سنة", Unit::Year, 1),
    ("سنوات", Unit::Year, 1),
    ("سنين", Unit::Year, 1),
    ("عام", Unit::Year, 1),
    ("أعوام", Unit::Year, 1),
    ("سنتين", Unit::Year, 2),
    ("سنتان", Unit::Year, 2),
    ("عامين", Unit::Year, 2),
    ("عامان", Unit::Year, 2),
    ("day", Unit::Day, 1),
    ("days", Unit::Day, 1),
    ("week", Unit::Week, 1),
    ("weeks", Unit::Week, 1),
    ("month", Unit::Month, 1),
    ("months", Unit::Month, 1),
    ("year", Unit::Year, 1),
    ("years", Unit::Year, 1),
];

/// Relative-date openers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rel {
    /// «قبل 3 أيام»: that day / week / month / year.
    Ago,
    /// «آخر 7 أيام», «منذ شهرين», "last 7 days": from then until now.
    Within,
    /// «أقدم من سنة», "older than 30 days": before then.
    Older,
    /// «أحدث من أسبوع», "newer than 2 weeks": from then on.
    Newer,
}

const REL_OPENERS: &[(&str, Rel)] = &[
    ("قبل", Rel::Ago),
    ("من", Rel::Ago),
    ("آخر", Rel::Within),
    ("في آخر", Rel::Within),
    ("خلال آخر", Rel::Within),
    ("خلال", Rel::Within),
    ("منذ", Rel::Within),
    ("أقدم من", Rel::Older),
    ("اقدم من", Rel::Older),
    ("أحدث من", Rel::Newer),
    ("last", Rel::Within),
    ("past", Rel::Within),
    ("the last", Rel::Within),
    ("the past", Rel::Within),
    ("in the last", Rel::Within),
    ("in the past", Rel::Within),
    ("within the last", Rel::Within),
    ("within the past", Rel::Within),
    ("within", Rel::Within),
    ("during the last", Rel::Within),
    ("over the last", Rel::Within),
    ("older than", Rel::Older),
    ("newer than", Rel::Newer),
];

/// Size comparisons: phrase → operator. A following «من»/«عن»/"than" is optional.
const SIZE_OPS: &[(&str, &str)] = &[
    ("أكبر", ">"),
    ("أكثر", ">"),
    ("أضخم", ">"),
    ("يزيد", ">"),
    ("تزيد", ">"),
    ("يتجاوز", ">"),
    ("تتجاوز", ">"),
    ("يفوق", ">"),
    ("فوق", ">"),
    ("لا يقل", ">="),
    ("لا تقل", ">="),
    ("على الأقل", ">="),
    ("أصغر", "<"),
    ("أقل", "<"),
    ("تحت", "<"),
    ("دون", "<"),
    ("لا يزيد", "<="),
    ("لا تزيد", "<="),
    ("لا يتجاوز", "<="),
    ("لا تتجاوز", "<="),
    ("bigger", ">"),
    ("larger", ">"),
    ("greater", ">"),
    ("more", ">"),
    ("heavier", ">"),
    ("over", ">"),
    ("above", ">"),
    ("exceeding", ">"),
    ("at least", ">="),
    ("no less", ">="),
    ("smaller", "<"),
    ("less", "<"),
    ("lighter", "<"),
    ("under", "<"),
    ("below", "<"),
    ("at most", "<="),
    ("up to", "<="),
    ("no more", "<="),
    ("no bigger", "<="),
    ("no larger", "<="),
];

const THAN: &[&str] = &["من", "عن", "than"];

const BETWEEN: &[&str] = &["بين", "ما بين", "between"];
const AND: &[&str] = &["و", "and", "to", "إلى", "الى"];

/// Content cues: phrase → needs a kind or «ملفات»/"files" before it (for common words).
const CONTENT_CUES: &[(&str, bool)] = &[
    ("يحتوي على", false),
    ("تحتوي على", false),
    ("يحتوي", false),
    ("تحتوي", false),
    ("تحتوى على", false),
    ("يتضمن", false),
    ("تتضمن", false),
    ("فيها كلمة", false),
    ("فيه كلمة", false),
    ("فيها عبارة", false),
    ("فيه عبارة", false),
    ("فيها نص", false),
    ("مكتوب فيها", false),
    ("مكتوب فيه", false),
    ("بداخله", false),
    ("بداخلها", false),
    ("داخله", false),
    ("داخلها", false),
    ("تذكر", true),
    ("يذكر", true),
    ("تتحدث عن", false),
    ("يتحدث عن", false),
    ("تتكلم عن", false),
    ("فيه", true),
    ("فيها", true),
    ("بها", true),
    ("عن", true),
    ("حول", true),
    ("بخصوص", true),
    ("containing", false),
    ("contains", false),
    ("contain", false),
    ("that contain", false),
    ("that contains", false),
    ("which contain", false),
    ("mentioning", false),
    ("that mention", false),
    ("that mentions", false),
    ("which mention", false),
    ("with the text", false),
    ("with the word", false),
    ("with the words", false),
    ("with the phrase", false),
    ("with text", false),
    ("including the text", false),
    ("with content", false),
    ("about", true),
    ("regarding", true),
];

/// Words that may follow a content cue and are not part of the text («كلمة», "the word").
const CONTENT_QUALIFIERS: &[&str] = &[
    "على",
    "كلمة",
    "الكلمة",
    "عبارة",
    "العبارة",
    "نص",
    "النص",
    "the word",
    "the words",
    "the text",
    "the phrase",
    "word",
    "words",
    "text",
    "phrase",
];

const FOLDER_CUES: &[&str] = &[
    "في مجلد",
    "في المجلد",
    "داخل مجلد",
    "داخل المجلد",
    "ضمن مجلد",
    "تحت مجلد",
    "بمجلد",
    "في فولدر",
    "in folder",
    "in the folder",
    "inside folder",
    "inside the folder",
    "under folder",
    "under the folder",
    "in directory",
    "in the directory",
];

const FOLDER_WORDS: &[&str] = &["folder", "directory", "dir"];
const FOLDER_PREPS: &[&str] = &["in the", "in", "inside the", "inside", "under the", "under"];

const DRIVE_CUES: &[&str] = &[
    "على القرص",
    "في القرص",
    "على قرص",
    "في قرص",
    "بالقرص",
    "على الدرايف",
    "في الدرايف",
    "على درايف",
    "على البارتشن",
    "القرص",
    "قرص",
    "درايف",
    "on drive",
    "in drive",
    "on the drive",
    "drive",
];
const DRIVE_WORDS: &[&str] = &["drive", "disk", "partition"];
const DRIVE_PREPS: &[&str] = &["on the", "on", "in", "from", "في", "على", "من"];

const NUMBER_WORDS: &[(&str, u32)] = &[
    ("واحد", 1),
    ("واحدة", 1),
    ("اثنين", 2),
    ("اثنان", 2),
    ("ثلاث", 3),
    ("ثلاثة", 3),
    ("أربع", 4),
    ("أربعة", 4),
    ("خمس", 5),
    ("خمسة", 5),
    ("ست", 6),
    ("ستة", 6),
    ("سبع", 7),
    ("سبعة", 7),
    ("ثمان", 8),
    ("ثماني", 8),
    ("ثمانية", 8),
    ("تسع", 9),
    ("تسعة", 9),
    ("عشر", 10),
    ("عشرة", 10),
    ("عشرين", 20),
    ("ثلاثين", 30),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("twenty", 20),
    ("thirty", 30),
];

// ---------------------------------------------------------------------------------------------
// Matching

struct Cx<'t, 'a> {
    toks: &'t [Tok<'a>],
    today: Option<Day>,
}

impl Cx<'_, '_> {
    /// The normalized word at `i`, if it is a plain word.
    fn word(&self, i: usize) -> Option<&str> {
        self.toks
            .get(i)
            .filter(|t| t.plain)
            .map(|t| t.norm.as_str())
    }

    /// Number of tokens `phrase` covers at `i`, if it matches there.
    fn phrase_at(&self, i: usize, phrase: &str) -> Option<usize> {
        let mut n = 0;
        for w in phrase.split(' ') {
            if self.word(i + n)? != norm(w) {
                return None;
            }
            n += 1;
        }
        Some(n)
    }

    /// The longest entry of `table` matching at `i`: (tokens covered, value).
    fn longest<T: Copy>(&self, i: usize, table: &[(&str, T)]) -> Option<(usize, T)> {
        table
            .iter()
            .filter_map(|(p, v)| self.phrase_at(i, p).map(|n| (n, *v)))
            .max_by_key(|(n, _)| *n)
    }

    fn longest_word(&self, i: usize, table: &[&str]) -> Option<usize> {
        table.iter().filter_map(|p| self.phrase_at(i, p)).max()
    }

    // -- sizes ------------------------------------------------------------------------------

    fn size_at(&self, i: usize) -> Option<Hit> {
        if let Some(n) = self.longest_word(i, BETWEEN) {
            let (a_num, a_unit, a_len) = self.number_unit(i + n)?;
            let mut j = i + n + a_len;
            // «و5» (attached «و») or «و 5» / "and 5"
            let attached = self.word(j).and_then(|w| w.strip_prefix('و'));
            let (b_num, b_unit, b_len) = match attached.and_then(split_number) {
                Some((num, unit)) if !num.is_empty() => {
                    let (u, extra) = if unit.is_empty() {
                        self.unit_at(j + 1)
                    } else {
                        (Some(unit_of(unit)?), 0)
                    };
                    (num.to_owned(), u, 1 + extra)
                }
                _ => {
                    j += self.longest_word(j, AND)?;
                    self.number_unit(j)?
                }
            };
            let b_unit = b_unit.or(a_unit)?;
            let a_unit = a_unit.unwrap_or(b_unit);
            let value = format!("{a_num}{a_unit}..{b_num}{b_unit}");
            parse_size(&value).ok()?;
            let len = j + b_len - i;
            return Some(Hit::new(
                len,
                PartRole::Size,
                format!("size:{value}"),
                Strength::Strong,
            ));
        }
        let (n, op) = self.longest(i, SIZE_OPS)?;
        let mut j = i + n;
        if let Some(m) = self.longest_word(j, THAN) {
            j += m;
        }
        let (num, unit, len) = self.number_unit(j)?;
        let unit = unit?;
        let value = format!("{op}{num}{unit}");
        parse_size(&value).ok()?;
        Some(Hit::new(
            j + len - i,
            PartRole::Size,
            format!("size:{value}"),
            Strength::Strong,
        ))
    }

    /// A number at `i` with an optional unit (attached or in the next word): (digits, unit,
    /// tokens).
    fn number_unit(&self, i: usize) -> Option<(String, Option<&'static str>, usize)> {
        let w = self.word(i)?;
        if let Some((_, n)) = NUMBER_WORDS.iter().find(|(p, _)| norm(p) == w) {
            let (u, extra) = self.unit_at(i + 1);
            return Some((n.to_string(), u, 1 + extra));
        }
        let (num, unit) = split_number(w)?;
        if num.is_empty() {
            return None;
        }
        if unit.is_empty() {
            let (u, extra) = self.unit_at(i + 1);
            Some((num.to_owned(), u, 1 + extra))
        } else {
            Some((num.to_owned(), Some(unit_of(unit)?), 1))
        }
    }

    fn unit_at(&self, i: usize) -> (Option<&'static str>, usize) {
        match self.word(i).and_then(unit_of) {
            Some(u) => (Some(u), 1),
            None => (None, 0),
        }
    }

    // -- dates ------------------------------------------------------------------------------

    fn date_at(&self, i: usize) -> Option<Hit> {
        // Without a preposition first, then after the longest preposition that leads to a date.
        if let Some(h) = self.date_body(i, false) {
            return Some(h);
        }
        let n = self.longest_word(i, DATE_PREPS)?;
        let mut h = self.date_body(i + n, true)?;
        h.len += n;
        Some(h)
    }

    fn date_body(&self, i: usize, after_prep: bool) -> Option<Hit> {
        let strong_if = |s: bool| {
            if s { Strength::Strong } else { Strength::Weak }
        };
        let date = |len, value: String, strength| {
            parse_date(&value).ok()?;
            Some(Hit::new(
                len,
                PartRole::Date,
                format!("dm:{value}"),
                strength,
            ))
        };
        // «منذ 2024», "before 2020", "since march 2024".
        if let Some((n, op)) = self.longest(i, YEAR_OPS) {
            if let Some((value, len, _)) = self.calendar_at(i + n, true) {
                return date(n + len, format!("{op}{value}"), Strength::Strong);
            }
        }
        if let Some((n, value)) = self.longest(i, NAMED_DATES) {
            return date(n, value.to_owned(), strong_if(after_prep || n > 1));
        }
        // Relative: «قبل 3 أيام», «آخر أسبوع», "3 days ago", "older than a year".
        if let Some(h) = self.relative_at(i) {
            return Some(h);
        }
        // «في عام 2024», "in year 2024"
        if let Some(n) = self.longest_word(i, YEAR_WORDS) {
            if let Some((value, len, _)) = self.calendar_at(i + n, false) {
                return date(n + len, value, Strength::Strong);
            }
        }
        let (value, len, month_name) = self.calendar_at(i, after_prep)?;
        date(len, value, strong_if(after_prep || month_name))
    }

    /// A year, a month name with or without a year, or an ISO-style date at `i`: (value,
    /// tokens, whether a month name was used). A month without a year needs today's date and
    /// `with_context`.
    fn calendar_at(&self, i: usize, with_context: bool) -> Option<(String, usize, bool)> {
        if let Some((n, month)) = self.longest(i, MONTHS) {
            if let Some(y) = self.word(i + n).and_then(year_of) {
                return Some((format!("{y}-{month:02}"), n + 1, true));
            }
            // "may" alone is too common an English word.
            if with_context && self.word(i) != Some("may") {
                let today = self.today?;
                let (ty, tm, _) = today.civil();
                let y = if u32::from(month) <= tm { ty } else { ty - 1 };
                if y < 1 {
                    return None;
                }
                return Some((format!("{y:04}-{month:02}"), n, true));
            }
            return None;
        }
        let w = self.word(i)?;
        if let Some(y) = year_of(w) {
            return Some((y.to_string(), 1, false));
        }
        // 2024-03, 2024-03-15, 2024/3
        let ok = w.len() <= 10
            && w.bytes()
                .all(|b| b.is_ascii_digit() || b == b'-' || b == b'/')
            && w.split(['-', '/']).next().and_then(year_of).is_some();
        if ok {
            let value = w.replace('/', "-");
            if parse_date(&value).is_ok() {
                return Some((value, 1, false));
            }
        }
        None
    }

    fn relative_at(&self, i: usize) -> Option<Hit> {
        let today = self.today?;
        let make = |len: usize, rel: Rel, count: u32, unit: Unit| -> Option<Hit> {
            let value = relative_value(today, rel, count, unit)?;
            parse_date(&value).ok()?;
            Some(Hit::new(
                len,
                PartRole::Date,
                format!("dm:{value}"),
                Strength::Strong,
            ))
        };
        // Opener first: «قبل 3 أيام», «آخر أسبوع», "last 7 days", "older than a year".
        if let Some((n, rel)) = self.longest(i, REL_OPENERS) {
            if let Some((count, unit, len)) = self.count_unit(i + n) {
                // «من سنة 2023» is "from the year 2023", not "a year ago".
                let year_follows = self.word(i + n + len).and_then(year_of).is_some();
                if !(len == 1 && year_follows) {
                    return make(n + len, rel, count, unit);
                }
            }
        }
        // "3 days ago", «3 أيام مضت»
        let (count, unit, len) = self.count_unit(i)?;
        let ago = self.longest_word(i + len, &["ago", "مضت", "مضى", "فاتت", "back"])?;
        make(len + ago, Rel::Ago, count, unit)
    }

    /// `[N] unit` at `i`: (count, unit, tokens). "a"/"an" and a missing number count as one
    /// (Arabic duals as two).
    fn count_unit(&self, i: usize) -> Option<(u32, Unit, usize)> {
        let unit_word = |j: usize| -> Option<(Unit, u32)> {
            let w = self.word(j)?;
            UNITS
                .iter()
                .find(|(p, _, _)| norm(p) == w)
                .map(|(_, u, c)| (*u, *c))
        };
        if let Some((unit, implied)) = unit_word(i) {
            return Some((implied, unit, 1));
        }
        let w = self.word(i)?;
        let count = if w == "a" || w == "an" {
            Some(1)
        } else if let Some((_, n)) = NUMBER_WORDS.iter().find(|(p, _)| norm(p) == w) {
            Some(*n)
        } else if !w.is_empty() && w.len() <= 5 && w.bytes().all(|b| b.is_ascii_digit()) {
            w.parse::<u32>().ok()
        } else {
            None
        }?;
        if count == 0 {
            return None;
        }
        let (unit, _) = unit_word(i + 1)?;
        Some((count, unit, 2))
    }

    // -- locations --------------------------------------------------------------------------

    fn location_at(&self, i: usize) -> Option<Hit> {
        // «في مجلد X», "in folder X"
        if let Some(n) = self.longest_word(i, FOLDER_CUES) {
            let name = self.folder_name(i + n)?;
            return Some(Hit::new(
                n + 1,
                PartRole::Location,
                format!("infolder:{name}"),
                Strength::Strong,
            ));
        }
        // "in the X folder", "in X folder"
        if let Some(n) = self.longest_word(i, FOLDER_PREPS) {
            if let Some(name) = self.folder_name(i + n) {
                if let Some(m) = self.longest_word(i + n + 1, FOLDER_WORDS) {
                    return Some(Hit::new(
                        n + 1 + m,
                        PartRole::Location,
                        format!("infolder:{name}"),
                        Strength::Strong,
                    ));
                }
            }
        }
        // «على القرص D», "on drive D"
        if let Some(n) = self.longest_word(i, DRIVE_CUES) {
            if let Some(letter) = self.toks.get(i + n).and_then(|t| drive_letter(t.raw)) {
                return Some(drive_hit(n + 1, letter));
            }
        }
        // "on D drive", "on the D drive", "on D:", «على D:»
        let n = self.longest_word(i, DRIVE_PREPS)?;
        let t = self.toks.get(i + n)?;
        let letter = drive_letter(t.raw)?;
        if let Some(m) = self.longest_word(i + n + 1, DRIVE_WORDS) {
            return Some(drive_hit(n + 1 + m, letter));
        }
        t.raw.contains(':').then(|| drive_hit(n + 1, letter))
    }

    /// A folder name at `i`: a plain word or a quoted phrase, ready to follow `infolder:`.
    fn folder_name(&self, i: usize) -> Option<String> {
        let t = self.toks.get(i)?;
        if let Some(q) = t.quoted {
            return (!q.trim().is_empty()).then(|| format!("\"{q}\""));
        }
        if !t.plain {
            return None;
        }
        let name = clean(t.raw);
        if name.contains(':') && drive_letter(name).is_none() {
            return None;
        }
        Some(name.to_owned())
    }

    // -- content ----------------------------------------------------------------------------

    fn content_at(&self, i: usize, has_context: bool) -> Option<Hit> {
        let (n, needs_context) = self.longest(i, CONTENT_CUES)?;
        if needs_context && !has_context {
            return None;
        }
        let mut j = i + n;
        if let Some(m) = self.longest_word(j, CONTENT_QUALIFIERS) {
            j += m;
        }
        let start = j;
        let mut words: Vec<&str> = Vec::new();
        while let Some(t) = self.toks.get(j) {
            if let Some(q) = t.quoted {
                // A quoted phrase is taken whole, and only as the first thing.
                if words.is_empty() && !q.trim().is_empty() {
                    words.push(q);
                    j += 1;
                }
                break;
            }
            if !t.plain || self.stops_content(j) {
                break;
            }
            words.push(clean(t.raw));
            j += 1;
        }
        if words.is_empty() {
            return None;
        }
        let text = words.join(" ");
        let simple = words.len() == 1
            && self.toks[start].plain
            && !text.contains(['|', '<', '>', '"', ':'])
            && !text.starts_with(['-', '!', '~']);
        let value = if simple { text } else { format!("\"{text}\"") };
        Some(Hit::new(
            j - i,
            PartRole::Content,
            format!("content:{value}"),
            Strength::Strong,
        ))
    }

    /// Whether the content text ends before `j`: a strong date, a size or a place starts there,
    /// or a conjunction followed by one.
    fn stops_content(&self, j: usize) -> bool {
        let cue = |k: usize| {
            self.size_at(k).is_some()
                || self.location_at(k).is_some()
                || self
                    .date_at(k)
                    .is_some_and(|h| h.strength == Strength::Strong)
        };
        if cue(j) {
            return true;
        }
        let conj = self.longest_word(j, &["و", "and", "أو", "or"]);
        conj.is_some_and(|n| cue(j + n) || self.kind_at(j + n, false).is_some())
    }

    // -- kinds and fillers ------------------------------------------------------------------

    fn kind_at(&self, i: usize, after_files: bool) -> Option<Hit> {
        let (n, kind, text) = self.kind_word(i)?;
        let mut len = n;
        // "word files", «ملفات مضغوطة»: the phrase itself says "files".
        let mut strong = after_files
            || (n > 1 && (i..i + n).any(|k| self.longest_word(k, FILES_WORDS).is_some()));
        if let Some(m) = self.longest_word(i + n, FILES_WORDS) {
            len += m;
            strong = true;
        }
        Some(Hit {
            len,
            role: PartRole::Kind,
            output: text,
            strength: if strong {
                Strength::Strong
            } else {
                Strength::Weak
            },
            kind: Some(kind),
            files: false,
        })
    }

    fn kind_word(&self, i: usize) -> Option<(usize, KindVal, String)> {
        if let Some((n, k)) = self.longest(i, KINDS) {
            return Some((n, k, k.output()));
        }
        let w = self.word(i)?;
        // «وفيديو»: attached «و» ("and").
        if let Some(rest) = w.strip_prefix('و') {
            if let Some((_, k)) = KINDS
                .iter()
                .find(|(p, _)| !p.contains(' ') && norm(p) == rest)
            {
                return Some((1, *k, k.output()));
            }
        }
        let ext = w.strip_prefix('.').unwrap_or(w);
        let known = KNOWN_EXTS.iter().find(|e| **e == ext)?;
        Some((1, KindVal::Ext(known), format!("ext:{known}")))
    }

    fn filler_at(&self, i: usize) -> Option<Hit> {
        if let Some(n) = self.longest_word(i, FILES_WORDS) {
            let mut h = Hit::new(n, PartRole::Ignored, String::new(), Strength::Zero);
            h.files = true;
            return Some(h);
        }
        let n = self.longest_word(i, FILLERS)?;
        Some(Hit::new(
            n,
            PartRole::Ignored,
            String::new(),
            Strength::Zero,
        ))
    }
}

fn drive_hit(len: usize, letter: char) -> Hit {
    Hit::new(
        len,
        PartRole::Location,
        format!("infolder:{letter}:"),
        Strength::Strong,
    )
}

/// `D`, `d`, `D:`, `D:\`, `D:/` → `D`.
fn drive_letter(raw: &str) -> Option<char> {
    let raw = raw.trim_end_matches([',', '.', '،', '?', '؟']);
    let mut chars = raw.chars();
    let c = chars.next().filter(char::is_ascii_alphabetic)?;
    match chars.as_str() {
        "" | ":" | ":\\" | ":/" => Some(c.to_ascii_uppercase()),
        _ => None,
    }
}

/// A plausible year: four ASCII digits between 1900 and 2100.
fn year_of(w: &str) -> Option<u16> {
    if w.len() != 4 || !w.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    w.parse().ok().filter(|y| (1900..=2100).contains(y))
}

/// Splits `5mb` / `1.5` / `٥م` (already ASCII digits) into the number and the rest.
fn split_number(w: &str) -> Option<(&str, &str)> {
    let end = w
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map_or(w.len(), |(i, _)| i);
    let (num, rest) = w.split_at(end);
    if num.is_empty() {
        return Some(("", rest));
    }
    let valid = num.len() <= 12
        && !num.starts_with('.')
        && !num.ends_with('.')
        && num.matches('.').count() <= 1;
    valid.then_some((num, rest))
}

/// Size unit words → the unit written in the query (`""` = bytes).
fn unit_of(w: &str) -> Option<&'static str> {
    const UNITS: &[(&str, &str)] = &[
        ("b", ""),
        ("byte", ""),
        ("bytes", ""),
        ("بايت", ""),
        ("k", "kb"),
        ("kb", "kb"),
        ("kib", "kb"),
        ("kilobyte", "kb"),
        ("kilobytes", "kb"),
        ("ك", "kb"),
        ("كيلو", "kb"),
        ("كيلوبايت", "kb"),
        ("ك.ب", "kb"),
        ("m", "mb"),
        ("mb", "mb"),
        ("mib", "mb"),
        ("meg", "mb"),
        ("megs", "mb"),
        ("megabyte", "mb"),
        ("megabytes", "mb"),
        ("م", "mb"),
        ("ميجا", "mb"),
        ("ميغا", "mb"),
        ("ميقا", "mb"),
        ("ميجابايت", "mb"),
        ("ميغابايت", "mb"),
        ("م.ب", "mb"),
        ("g", "gb"),
        ("gb", "gb"),
        ("gib", "gb"),
        ("gig", "gb"),
        ("gigs", "gb"),
        ("gigabyte", "gb"),
        ("gigabytes", "gb"),
        ("ج", "gb"),
        ("جيجا", "gb"),
        ("غيغا", "gb"),
        ("قيقا", "gb"),
        ("جيجابايت", "gb"),
        ("غيغابايت", "gb"),
        ("ج.ب", "gb"),
        ("t", "tb"),
        ("tb", "tb"),
        ("terabyte", "tb"),
        ("terabytes", "tb"),
        ("تيرا", "tb"),
        ("تيرابايت", "tb"),
    ];
    UNITS.iter().find(|(p, _)| norm(p) == w).map(|(_, u)| *u)
}

// ---------------------------------------------------------------------------------------------
// Calendar arithmetic (proleptic Gregorian, day numbers since 1970-01-01).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Day(i64);

impl Day {
    fn from_calendar(d: CalendarDate) -> Option<Day> {
        let (m, day) = (d.month?, d.day?);
        if d.year == 0
            || !(1..=12).contains(&m)
            || day == 0
            || u32::from(day) > days_in_month(i64::from(d.year), u32::from(m))
        {
            return None;
        }
        Some(Day(days_from_civil(
            i64::from(d.year),
            u32::from(m),
            u32::from(day),
        )))
    }

    fn civil(self) -> (i64, u32, u32) {
        civil_from_days(self.0)
    }

    fn iso(self) -> Option<String> {
        let (y, m, d) = self.civil();
        (1..=9999)
            .contains(&y)
            .then(|| format!("{y:04}-{m:02}-{d:02}"))
    }

    /// The same day `months` months earlier (clamped to the month's length).
    fn minus_months(self, months: u32) -> Option<Day> {
        let (y, m, d) = self.civil();
        let total = y * 12 + i64::from(m) - 1 - i64::from(months);
        let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
        if ny < 1 {
            return None;
        }
        let nd = d.min(days_in_month(ny, nm));
        Some(Day(days_from_civil(ny, nm, nd)))
    }
}

/// The `dm:` value for a relative phrase, or `None` if it falls outside years 1-9999.
fn relative_value(today: Day, rel: Rel, count: u32, unit: Unit) -> Option<String> {
    let count = count.min(100_000);
    let back = |n: u32| -> Option<Day> {
        match unit {
            Unit::Day => Some(Day(today.0 - i64::from(n))),
            Unit::Week => Some(Day(today.0 - 7 * i64::from(n))),
            Unit::Month => today.minus_months(n),
            Unit::Year => today.minus_months(n.checked_mul(12)?),
        }
    };
    let then = back(count)?;
    Some(match rel {
        Rel::Ago => match unit {
            Unit::Day => then.iso()?,
            Unit::Week => format!("{}..{}", then.iso()?, Day(then.0 + 6).iso()?),
            Unit::Month => {
                let (y, m, _) = then.civil();
                format!("{y:04}-{m:02}")
            }
            Unit::Year => format!("{:04}", then.civil().0),
        },
        // "last 7 days" = today and the 6 days before it.
        Rel::Within | Rel::Newer => format!(">={}", Day(then.0 + 1).iso()?),
        Rel::Older => format!("<{}", then.iso()?),
    })
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (H. Hinnant's algorithm).
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
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trip() {
        for z in [-1000, 0, 1, 20_000, 20_779, 2_932_896] {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
        let today = Day(days_from_civil(2026, 3, 31));
        assert_eq!(
            today.minus_months(1).and_then(Day::iso).as_deref(),
            Some("2026-02-28")
        );
        assert_eq!(
            today.minus_months(15).and_then(Day::iso).as_deref(),
            Some("2024-12-31")
        );
    }

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter("d"), Some('D'));
        assert_eq!(drive_letter("D:\\"), Some('D'));
        assert_eq!(drive_letter("DD"), None);
        assert_eq!(drive_letter("١"), None);
    }

    #[test]
    fn numbers() {
        assert_eq!(split_number("5mb"), Some(("5", "mb")));
        assert_eq!(split_number("1.5"), Some(("1.5", "")));
        assert_eq!(split_number("1..5"), None);
        assert_eq!(split_number("mb"), Some(("", "mb")));
    }
}
