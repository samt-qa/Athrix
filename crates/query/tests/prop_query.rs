//! Property tests of the query language: parser and planner robustness, error spans,
//! invariance under redundant brackets and whitespace, and Arabic aliases.
//!
//! Default: 256 cases per property. Raise with `PROPTEST_CASES=10000`.

use athrix_query::{Expr, ExprKind, PlanOptions, Query, SearchPlan, Span, parse, plan};
use proptest::prelude::*;

const OPTIONS: [PlanOptions; 2] = [
    PlanOptions {
        plain_words_search_content: false,
    },
    PlanOptions {
        plain_words_search_content: true,
    },
];

fn span_ok(s: Span, src: &str) -> bool {
    s.start <= s.end
        && s.end <= src.len()
        && src.is_char_boundary(s.start)
        && src.is_char_boundary(s.end)
}

fn spans_ok(e: &Expr, src: &str) -> bool {
    span_ok(e.span, src)
        && match &e.kind {
            ExprKind::Term(t) => !t.text.is_empty() || t.quoted,
            ExprKind::Filter(f) => {
                span_ok(f.name_span, src)
                    && span_ok(f.value_span, src)
                    && e.span.start <= f.name_span.start
                    && f.value_span.end <= e.span.end
            }
            ExprKind::Not(x) | ExprKind::Group(x) => {
                spans_ok(x, src) && e.span.start <= x.span.start && x.span.end <= e.span.end
            }
            ExprKind::And(v) | ExprKind::Or(v) => {
                v.len() >= 2
                    && v.iter().all(|x| {
                        spans_ok(x, src) && e.span.start <= x.span.start && x.span.end <= e.span.end
                    })
                    && v.windows(2).all(|w| w[0].span.end <= w[1].span.start)
            }
        }
}

/// Structure without spans or redundant groups: what a plan means.
fn shape(e: &Expr) -> String {
    let list = |op: &str, items: &[Expr]| {
        let inner: Vec<String> = items.iter().map(shape).collect();
        format!("({op} {})", inner.join(" "))
    };
    match &e.kind {
        ExprKind::Term(t) if t.quoted => format!("{:?}", t.text),
        ExprKind::Term(t) => t.text.clone(),
        ExprKind::Filter(f) => format!("{}:{:?}", f.name, f.value),
        ExprKind::Not(x) => format!("(not {})", shape(x)),
        ExprKind::And(v) => list("and", v),
        ExprKind::Or(v) => list("or", v),
        ExprKind::Group(x) => shape(x),
    }
}

fn q_shape(q: Option<&Query>) -> String {
    q.and_then(|q| q.root.as_ref())
        .map_or_else(|| "-".to_string(), shape)
}

/// A plan (or its error kind) without spans.
fn plan_shape(q: &str, opts: &PlanOptions) -> Option<String> {
    let ast = parse(q).ok()?;
    Some(match plan(&ast, opts) {
        Err(e) => format!("error {:?}", e.kind),
        Ok(SearchPlan::Empty) => "empty".into(),
        Ok(SearchPlan::Name(n)) => format!("name {}", q_shape(Some(&n))),
        Ok(SearchPlan::NameAndContent { name, content }) => {
            format!(
                "and {} / {}",
                q_shape(name.as_ref()),
                content.to_query_text()
            )
        }
        Ok(SearchPlan::NameOrContent {
            name,
            filters,
            content,
        }) => format!(
            "or {} / {} / {}",
            q_shape(Some(&name)),
            q_shape(filters.as_ref()),
            content.to_query_text()
        ),
    })
}

/// Well-formed query pieces: words, phrases and filters in English and Arabic.
const ATOMS: &[&str] = &[
    "تقرير",
    "فاتورة",
    "report",
    "2026",
    "عقد",
    "*.pdf",
    "a-b",
    "IMG_2041",
    "\"عقد الإيجار\"",
    "\"annual report\"",
    "ext:pdf;docx",
    "امتداد:pdf؛xlsx",
    "type:image",
    "نوع:صور",
    "size:>1mb",
    "حجم:10mb..50mb",
    "size:١٠ميجا",
    "dm:2026-03",
    "dm:today",
    "تاريخ:أمس",
    "path:\"My Docs\"",
    "مسار:المستندات",
    "parent:docs",
    "infolder:العمل",
    "content:عقد",
    "content:\"annual report\"",
    "محتوى:ايجار",
    "fuzzy:rprt",
    "fuzzy:",
    "case:",
    "tag:old;new",
    "regex:\"a|b\"",
    "dupe:",
];

#[derive(Debug, Clone)]
struct Piece {
    atom: &'static str,
    prefix: &'static str,
    /// Joined to the previous piece with OR instead of AND.
    or: bool,
}

fn pieces() -> impl Strategy<Value = Vec<Piece>> {
    prop::collection::vec(
        (
            prop::sample::select(ATOMS),
            prop::sample::select(&["", "", "", "-", "!"][..]),
            prop::bool::weighted(0.25),
        )
            .prop_map(|(atom, prefix, or)| Piece { atom, prefix, or }),
        1..7,
    )
}

/// Whitespace runs, including multi-byte spaces.
fn ws() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(&[" ", " ", "\t", "\n", "\u{A0}", "\u{3000}", "\u{2003}"][..]),
        1..4,
    )
    .prop_map(|v| v.concat())
}

/// Renders pieces with single spaces, or with the given whitespace runs and bracketed atoms.
fn render(pieces: &[Piece], seps: Option<&[String]>, bracket_atoms: &[bool]) -> String {
    let mut out = String::new();
    for (i, p) in pieces.iter().enumerate() {
        let sep = seps.map_or(" ", |s| s[i % s.len()].as_str());
        if i > 0 {
            out.push_str(sep);
            if p.or {
                out.push('|');
                out.push_str(sep);
            }
        }
        out.push_str(p.prefix);
        if bracket_atoms.get(i).copied().unwrap_or(false) {
            out.push('<');
            out.push_str(p.atom);
            out.push('>');
        } else {
            out.push_str(p.atom);
        }
    }
    out
}

/// Characters of query syntax plus Arabic letters, marks, digits and odd whitespace.
fn soup() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => prop::sample::select(&[
                'a', 'z', 'e', 'x', 't', ':', ';', '؛', '.', '<', '>', '|', '!', '-', '~', '"', ' ',
                '=', '*', '\\', '/', '_', '0', '9', '٠', '٩', '۵', '٫', 'ا', 'أ', 'إ', 'ى', 'م', 'ت',
                'د', 'ن', 'و', 'ع', 'ح', 'ج', 'ـ', '\u{064E}', '\u{0651}', '\u{200F}', '\u{A0}',
                '\u{3000}', '\u{FEFF}', 'İ', 'ß',
            ][..]),
            1 => any::<char>(),
        ],
        0..80,
    )
    .prop_map(|v| v.into_iter().collect())
}

/// Arabic filter names, their English equivalent, and values to try with both.
type Alias = (&'static str, &'static str, &'static [&'static str]);
const ALIASES: &[Alias] = &[
    ("امتداد", "ext", &["pdf", "pdf؛docx", ".txt;*.md"]),
    (
        "نوع",
        "type",
        &["image", "صور", "مستندات;video", "أرشيف", "zzz"],
    ),
    (
        "حجم",
        "size",
        &[">1mb", "10mb..50mb", "١٠ميجا", "<=٢جيجا", "large", "x"],
    ),
    (
        "تاريخ",
        "dm",
        &[
            "2026",
            "٢٠٢٦-٠٣",
            "today",
            "أمس",
            ">=2026-03-15",
            "هذا_الأسبوع",
        ],
    ),
    ("تاريخ_التعديل", "dm", &["2026-01..2026-03"]),
    ("تاريخ_الإنشاء", "dc", &["2025", "الشهر_الماضي"]),
    ("تاريخ_الوصول", "da", &["today"]),
    ("مسار", "path", &["C:\\work", "\"My Docs\""]),
    ("مجلد", "infolder", &["العمل"]),
    ("محتوى", "content", &["عقد", "\"عقد الإيجار\""]),
    ("دلالي", "sem", &["عقد ايجار"]),
    ("مكرر", "dupe", &["", "name"]),
    ("مشابه", "similar", &["report.pdf"]),
    ("وسم", "tag", &["مهم;work"]),
];

proptest! {
    #[test]
    fn parse_and_plan_never_panic_and_spans_are_valid(input in soup()) {
        match parse(&input) {
            Ok(q) => {
                if let Some(r) = &q.root {
                    prop_assert!(spans_ok(r, &input), "{:?}: {:?}", input, r);
                }
                for opts in &OPTIONS {
                    match plan(&q, opts) {
                        Ok(p) => { let _ = p.name_words(); let _ = p.uses_content(); }
                        Err(e) => {
                            prop_assert!(span_ok(e.span, &input) && span_ok(e.group_span, &input),
                                "{:?}: {:?}", input, e);
                            prop_assert!(e.group_span.start <= e.span.start && e.span.end <= e.group_span.end,
                                "{:?}: span outside its group {:?}", input, e);
                            prop_assert!(!e.message.is_empty());
                        }
                    }
                }
            }
            Err(e) => {
                prop_assert!(span_ok(e.span, &input), "{:?}: {:?}", input, e);
                prop_assert!(!e.message.is_empty());
                prop_assert!(e.position() <= input.len());
            }
        }
    }

    #[test]
    fn arbitrary_unicode_never_panics(input in "\\PC{0,100}") {
        if let Ok(q) = parse(&input) {
            if let Some(r) = &q.root {
                prop_assert!(spans_ok(r, &input));
            }
            for opts in &OPTIONS { let _ = plan(&q, opts); }
        }
    }

    /// Structured queries always parse, and their plan does not depend on how much (or which)
    /// whitespace separates the pieces.
    #[test]
    fn whitespace_does_not_change_the_plan(p in pieces(), seps in prop::collection::vec(ws(), 1..4), lead in ws(), trail in ws()) {
        let plain = render(&p, None, &[]);
        prop_assert!(parse(&plain).is_ok(), "{:?}: {:?}", plain, parse(&plain));
        let spaced = format!("{lead}{}{trail}", render(&p, Some(&seps), &[]));
        for opts in &OPTIONS {
            prop_assert_eq!(plan_shape(&plain, opts), plan_shape(&spaced, opts), "{:?} vs {:?}", plain, spaced);
        }
    }

    /// Brackets around the whole query or around single pieces change nothing.
    #[test]
    fn redundant_brackets_do_not_change_the_plan(p in pieces(), bracket in prop::collection::vec(any::<bool>(), 7)) {
        let plain = render(&p, None, &[]);
        let wrapped = format!("<{plain}>");
        let atoms = render(&p, None, &bracket);
        for opts in &OPTIONS {
            let want = plan_shape(&plain, opts);
            prop_assert!(want.is_some());
            prop_assert_eq!(&want, &plan_shape(&wrapped, opts), "{:?}", wrapped);
            prop_assert_eq!(&want, &plan_shape(&atoms, opts), "{:?}", atoms);
        }
    }

    /// Arabic filter names (with hamza variants, tatweel and harakat) mean the same as the
    /// English ones, for any value.
    #[test]
    fn arabic_filter_names_equal_english(
        (ar, en, value) in prop::sample::select(ALIASES)
            .prop_flat_map(|(ar, en, values)| (Just(ar), Just(en), prop::sample::select(values))),
        decorate in prop::collection::vec(prop::sample::select(&["", "", "", "ـ", "\u{064E}", "\u{0651}", "\u{0652}"][..]), 16),
        hamza in prop::sample::select(&["", "أ", "إ", "آ"][..]),
    ) {
        // Decorate the Arabic name: a mark or tatweel after some letters, and a hamza form for
        // a leading alef.
        let mut name = String::new();
        for (i, c) in ar.chars().enumerate() {
            if i == 0 && c == 'ا' && !hamza.is_empty() {
                name.push_str(hamza);
            } else {
                name.push(c);
            }
            if c != '_' {
                name.push_str(decorate[i % decorate.len()]);
            }
        }
        let a = parse(&format!("{name}:{value}"));
        let e = parse(&format!("{en}:{value}"));
        match (&a, &e) {
            (Ok(a), Ok(e)) => {
                let (a, e) = (a.root.as_ref().unwrap(), e.root.as_ref().unwrap());
                prop_assert_eq!(shape(a), shape(e), "{:?}", name);
            }
            (Err(a), Err(e)) => prop_assert_eq!(a.kind, e.kind),
            _ => prop_assert!(false, "{:?}:{:?} gives {:?}, {}:… gives {:?}", name, value, a, en, e),
        }
    }

    /// Arabic-Indic and Persian digits mean the same as ASCII digits in sizes and dates.
    #[test]
    fn eastern_digits_equal_ascii(
        n in 1u32..5000,
        y in 1990u32..2100, m in 1u32..=12, d in 1u32..=28,
        op in prop::sample::select(&["", ">", ">=", "<", "<=", "="][..]),
        unit in prop::sample::select(&["", "kb", "mb", "gb", "ميجا", "كيلو", "جيجا"][..]),
        persian: bool,
    ) {
        let east = |s: &str| -> String {
            let base = if persian { 0x06F0 } else { 0x0660 };
            s.chars().map(|c| match c.to_digit(10) {
                Some(v) => char::from_u32(base + v).unwrap(),
                None => c,
            }).collect()
        };
        for q in [
            format!("size:{op}{n}{unit}"),
            format!("dm:{op}{y}-{m:02}-{d:02}"),
            format!("dm:{y}-{m:02}..{y}-{m:02}"),
            format!("size:{n}.5{unit}"),
        ] {
            let ascii = parse(&q).map(|q| q.root.map(|r| shape(&r))).map_err(|e| e.kind);
            let eastern = parse(&east(&q).replace('.', if q.contains("..") { "." } else { "٫" }))
                .map(|q| q.root.map(|r| shape(&r))).map_err(|e| e.kind);
            prop_assert_eq!(ascii, eastern, "{}", q);
        }
    }
}

proptest! {
    /// Every `content:` value reaches the content query once, quoted, with its negation.
    #[test]
    fn content_values_reach_the_content_query(
        words in prop::collection::vec(("[^\"\\s<>|:]{1,8}", any::<bool>(), any::<bool>()), 1..5),
    ) {
        let q: Vec<String> = words.iter().map(|(w, neg, arabic)| {
            let name = if *arabic { "محتوى" } else { "content" };
            format!("{}{name}:{w}", if *neg { "-" } else { "" })
        }).collect();
        let q = format!("ext:pdf {}", q.join(" "));
        let ast = parse(&q).map_err(|e| TestCaseError::fail(format!("{q:?}: {e}")))?;
        let p = plan(&ast, &OPTIONS[0]).map_err(|e| TestCaseError::fail(format!("{q:?}: {e}")))?;
        let SearchPlan::NameAndContent { content, name } = p else {
            return Err(TestCaseError::fail(format!("{q:?}: not NameAndContent")));
        };
        prop_assert!(name.is_some());
        prop_assert_eq!(content.clauses.len(), words.len());
        for (c, (w, neg, _)) in content.clauses.iter().zip(&words) {
            prop_assert_eq!(&c.text, w);
            prop_assert_eq!(c.negated, *neg);
            prop_assert_eq!(c.span.slice(&q).contains(w.as_str()), true);
        }
        let text = content.to_query_text();
        prop_assert_eq!(text.matches('"').count(), 2 * words.len(), "{}", text);
    }
}
