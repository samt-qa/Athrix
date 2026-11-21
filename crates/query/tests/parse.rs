//! Parser behaviour tests: syntax, Arabic synonyms, errors, spans, robustness and speed.

use athrix_query::{
    CalendarDate, Comparison, DateSpec, Expr, ExprKind, FileKind, Filter, FilterName, FilterValue,
    NamedDate, ParseError, ParseErrorKind, Span, parse,
};
use proptest::prelude::*;

const MB: u64 = 1024 * 1024;

fn root(q: &str) -> Expr {
    parse(q)
        .unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
        .root
        .unwrap_or_else(|| panic!("`{q}` is empty"))
}

fn err(q: &str) -> ParseError {
    match parse(q) {
        Ok(ast) => panic!("`{q}` should fail, got {ast:?}"),
        Err(e) => e,
    }
}

/// Compact S-expression rendering for readable structure assertions.
fn sexp(e: &Expr) -> String {
    let list = |op: &str, items: &[Expr]| {
        let inner: Vec<String> = items.iter().map(sexp).collect();
        format!("({op} {})", inner.join(" "))
    };
    match &e.kind {
        ExprKind::Term(t) if t.quoted => format!("\"{}\"", t.text),
        ExprKind::Term(t) => t.text.clone(),
        ExprKind::Filter(f) => format!("{}:{:?}", f.name, f.value),
        ExprKind::Not(x) => format!("(not {})", sexp(x)),
        ExprKind::And(v) => list("and", v),
        ExprKind::Or(v) => list("or", v),
        ExprKind::Group(x) => format!("<{}>", sexp(x)),
    }
}

fn s(q: &str) -> String {
    sexp(&root(q))
}

fn filter(q: &str) -> Filter {
    match root(q).kind {
        ExprKind::Filter(f) => f,
        other => panic!("`{q}` is not a single filter: {other:?}"),
    }
}

fn value(q: &str) -> FilterValue {
    filter(q).value
}

// ---------------------------------------------------------------- boolean structure

#[test]
fn empty_query() {
    assert_eq!(parse("").unwrap().root, None);
    assert_eq!(parse("   \t ").unwrap().root, None);
}

#[test]
fn implicit_and_or_not_and_precedence() {
    assert_eq!(s("report"), "report");
    assert_eq!(s("a b c"), "(and a b c)");
    assert_eq!(s("a | b"), "(or a b)");
    assert_eq!(s("a b | c"), "(or (and a b) c)");
    assert_eq!(s("a | b c"), "(or a (and b c))");
    assert_eq!(s("!a b"), "(and (not a) b)");
    assert_eq!(s("-a | b"), "(or (not a) b)");
    assert_eq!(s("!!a"), "(not (not a))");
    assert_eq!(s("a|b|c"), "(or a b c)");
}

#[test]
fn grouping() {
    assert_eq!(s("<a | b> c"), "(and <(or a b)> c)");
    assert_eq!(s("!<a b>"), "(not <(and a b)>)");
    assert_eq!(s("< <a> >"), "<<a>>");
    assert_eq!(s("x<a|b>y"), "(and x <(or a b)> y)");
}

#[test]
fn quoted_phrases_and_words() {
    assert_eq!(s("\"annual report\" 2026"), "(and \"annual report\" 2026)");
    assert_eq!(s("Artist - Song"), "(and Artist - Song)");
    assert_eq!(s("a-b *.pdf ^IMG $"), "(and a-b *.pdf ^IMG $)");
    assert_eq!(s("تقرير -مسودة"), "(and تقرير (not مسودة))");
}

#[test]
fn colon_words_that_are_not_filters() {
    assert_eq!(s(r"C:\Users\me"), r"C:\Users\me");
    assert_eq!(s("10:30"), "10:30");
    assert_eq!(s("https://example.com"), "https://example.com");
}

// ---------------------------------------------------------------- filters

#[test]
fn ext_lists() {
    assert_eq!(
        value("ext:pdf;DOCX;.txt;*.md;"),
        FilterValue::Extensions(vec!["pdf".into(), "docx".into(), "txt".into(), "md".into()])
    );
    assert_eq!(
        value("امتداد:pdf؛docx"),
        FilterValue::Extensions(vec!["pdf".into(), "docx".into()])
    );
}

#[test]
fn types_in_english_and_arabic() {
    assert_eq!(
        value("type:image;video"),
        FilterValue::Kinds(vec![FileKind::Image, FileKind::Video])
    );
    for (q, k) in [
        ("نوع:صور", FileKind::Image),
        ("نوع:مستندات", FileKind::Document),
        ("نوع:فيديو", FileKind::Video),
        ("نوع:صوت", FileKind::Audio),
        ("نوع:كود", FileKind::Code),
        ("نوع:أرشيف", FileKind::Archive),
        ("نوع:ارشيف", FileKind::Archive),
        ("نوع:مجلدات", FileKind::Folder),
        ("type:folder", FileKind::Folder),
        ("type:doc", FileKind::Document),
    ] {
        assert_eq!(value(q), FilterValue::Kinds(vec![k]), "{q}");
    }
}

#[test]
fn sizes() {
    assert_eq!(
        value("size:>100mb"),
        FilterValue::Size(Comparison::Gt(100 * MB))
    );
    assert_eq!(
        value("size:10mb..50mb"),
        FilterValue::Size(Comparison::Range {
            min: 10 * MB,
            max: 50 * MB
        })
    );
    assert_eq!(
        value("حجم:>١٠٠mb"),
        FilterValue::Size(Comparison::Gt(100 * MB))
    );
    assert_eq!(value("size:<=1kb"), FilterValue::Size(Comparison::Le(1024)));
    assert_eq!(
        value("size:\"> 2 gb\""),
        FilterValue::Size(Comparison::Gt(2048 * MB))
    );
}

#[test]
fn dates_stay_unresolved() {
    let cal = |year, month, day| DateSpec::Calendar(CalendarDate { year, month, day });
    assert_eq!(
        value("dm:today"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::Today)))
    );
    assert_eq!(
        value("dc:yesterday"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::Yesterday)))
    );
    assert_eq!(
        value("da:thisweek"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::ThisWeek)))
    );
    assert_eq!(
        value("dm:lastmonth"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::LastMonth)))
    );
    assert_eq!(
        value("dm:2026"),
        FilterValue::Date(Comparison::Eq(cal(2026, None, None)))
    );
    assert_eq!(
        value("dm:2026-01..2026-03"),
        FilterValue::Date(Comparison::Range {
            min: cal(2026, Some(1), None),
            max: cal(2026, Some(3), None)
        })
    );
    assert_eq!(
        value("dm:>=2026-03-15"),
        FilterValue::Date(Comparison::Ge(cal(2026, Some(3), Some(15))))
    );
    assert_eq!(
        value("تاريخ:اليوم"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::Today)))
    );
    assert_eq!(
        value("تاريخ:أمس"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::Yesterday)))
    );
    assert_eq!(
        value("تاريخ:امس"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::Yesterday)))
    );
    assert_eq!(
        value("تاريخ:هذا_الأسبوع"),
        FilterValue::Date(Comparison::Eq(DateSpec::Named(NamedDate::ThisWeek)))
    );
    assert_eq!(
        value("تاريخ:٢٠٢٦"),
        FilterValue::Date(Comparison::Eq(cal(2026, None, None)))
    );
    assert_eq!(filter("تاريخ:اليوم").name, FilterName::DateModified);
}

#[test]
fn text_filters() {
    for (q, name, text) in [
        ("path:projects", FilterName::Path, "projects"),
        ("parent:Downloads", FilterName::Parent, "Downloads"),
        ("infolder:\"My Docs\"", FilterName::InFolder, "My Docs"),
        ("content:\"عقد الإيجار\"", FilterName::Content, "عقد الإيجار"),
        ("محتوى:فاتورة", FilterName::Content, "فاتورة"),
        ("مسار:مشاريع", FilterName::Path, "مشاريع"),
        ("regex:^IMG_\\d{4}", FilterName::Regex, "^IMG_\\d{4}"),
        ("regex:\"a|b\"", FilterName::Regex, "a|b"),
        ("case:Report", FilterName::Case, "Report"),
        ("ww:report", FilterName::WholeWord, "report"),
        ("fuzzy:rprt", FilterName::Fuzzy, "rprt"),
        ("similar:photo.jpg", FilterName::Similar, "photo.jpg"),
    ] {
        let f = filter(q);
        assert_eq!(
            (f.name, f.value),
            (name, FilterValue::Text(text.into())),
            "{q}"
        );
    }
    assert_eq!(
        value("tag:work;مهم"),
        FilterValue::Tags(vec!["work".into(), "مهم".into()])
    );
    assert_eq!(filter("وسم:عمل").name, FilterName::Tag);
    assert_eq!(value("dupe:"), FilterValue::None);
    assert_eq!(value("case:"), FilterValue::None);
}

#[test]
fn filter_names_are_case_and_hamza_insensitive() {
    assert_eq!(filter("EXT:pdf").name, FilterName::Ext);
    assert_eq!(filter("إمتداد:pdf").name, FilterName::Ext);
    assert_eq!(filter("أمتداد:pdf").name, FilterName::Ext);
    assert_eq!(filter("محتوي:x").name, FilterName::Content);
}

#[test]
fn semantic() {
    let f = filter("~عقد الإيجار");
    assert_eq!(f.name, FilterName::Semantic);
    assert_eq!(f.value, FilterValue::Text("عقد الإيجار".into()));
    assert_eq!(
        s("~rental contract ext:pdf"),
        "(and sem:Text(\"rental contract\") ext:Extensions([\"pdf\"]))"
    );
    assert_eq!(
        s("sem:old invoices | x"),
        "(or sem:Text(\"old invoices\") x)"
    );
    assert_eq!(
        s("<~beach photos> -draft"),
        "(and <sem:Text(\"beach photos\")> (not draft))"
    );
}

#[test]
fn combined_query() {
    assert_eq!(
        s("تقرير ext:pdf;docx size:>1mb dm:thisweek | <type:image !tag:old>"),
        "(or (and تقرير ext:Extensions([\"pdf\", \"docx\"]) size:Size(Gt(1048576)) \
         dm:Date(Eq(Named(ThisWeek)))) <(and type:Kinds([Image]) (not tag:Tags([\"old\"])))>)"
    );
}

// ---------------------------------------------------------------- spans

#[test]
fn spans_point_at_source() {
    let q = "تقرير ext:pdf <a | \"b c\">";
    let e = root(q);
    assert_eq!(e.span, Span::new(0, q.len()));
    let ExprKind::And(items) = &e.kind else {
        panic!()
    };
    assert_eq!(items[0].span.slice(q), "تقرير");
    assert_eq!(items[1].span.slice(q), "ext:pdf");
    let ExprKind::Filter(f) = &items[1].kind else {
        panic!()
    };
    assert_eq!(f.name_span.slice(q), "ext:");
    assert_eq!(f.value_span.slice(q), "pdf");
    assert_eq!(items[2].span.slice(q), "<a | \"b c\">");
    let ExprKind::Group(inner) = &items[2].kind else {
        panic!()
    };
    let ExprKind::Or(alts) = &inner.kind else {
        panic!()
    };
    assert_eq!(alts[1].span.slice(q), "\"b c\"");

    let q = "!~meeting notes";
    let e = root(q);
    assert_eq!(e.span.slice(q), q);
    let ExprKind::Not(inner) = &e.kind else {
        panic!()
    };
    let ExprKind::Filter(f) = &inner.kind else {
        panic!()
    };
    assert_eq!(f.name_span.slice(q), "~");
    assert_eq!(f.value_span.slice(q), "meeting notes");
}

// ---------------------------------------------------------------- errors

#[test]
fn unknown_filter_suggests() {
    let e = err("report szie:>1mb");
    assert_eq!(e.kind, ParseErrorKind::UnknownFilter);
    assert_eq!(e.position(), 7);
    assert_eq!(e.suggestion.as_deref(), Some("size"));
    assert!(e.message.contains("did you mean `size:`"), "{}", e.message);

    let e = err("حجن:>1mb");
    assert_eq!(e.suggestion.as_deref(), Some("حجم"));

    let e = err("qwertyuiop:x");
    assert_eq!(e.suggestion, None);
    assert!(e.message.contains("known filters"));
}

#[test]
fn friendly_errors() {
    let cases = [
        ("\"open", ParseErrorKind::UnclosedQuote, 0),
        ("\"\"", ParseErrorKind::EmptyPhrase, 0),
        ("a <b", ParseErrorKind::UnclosedGroup, 2),
        ("a <b c", ParseErrorKind::UnclosedGroup, 2),
        ("a >", ParseErrorKind::UnmatchedClose, 2),
        ("<>", ParseErrorKind::EmptyGroup, 0),
        ("a |", ParseErrorKind::DanglingOperator, 2),
        ("| a", ParseErrorKind::DanglingOperator, 0),
        ("a | | b", ParseErrorKind::DanglingOperator, 2),
        ("ext:", ParseErrorKind::MissingValue, 0),
        ("ext:;", ParseErrorKind::MissingValue, 0),
        ("~", ParseErrorKind::MissingValue, 0),
        ("x size:>1zb", ParseErrorKind::InvalidSize, 7),
        ("size:50mb..10mb", ParseErrorKind::InvertedRange, 5),
        ("dm:2026-13", ParseErrorKind::InvalidDate, 3),
        ("dm:tomorow", ParseErrorKind::InvalidDate, 3),
        ("type:pdf", ParseErrorKind::InvalidKind, 5),
        ("type:image;imgae", ParseErrorKind::InvalidKind, 11),
    ];
    for (q, kind, pos) in cases {
        let e = err(q);
        assert_eq!((e.kind, e.position()), (kind, pos), "{q}: {e}");
        assert!(!e.message.is_empty());
    }
    assert_eq!(err("type:imgae").suggestion.as_deref(), Some("image"));
}

#[test]
fn converts_into_core_error() {
    let e: athrix_core::Error = err("szie:1").into();
    match e {
        athrix_core::Error::InvalidQuery { position, message } => {
            assert_eq!(position, 0);
            assert!(message.contains("size"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let e = err(&"<".repeat(100_000));
    assert_eq!(e.kind, ParseErrorKind::TooDeep);
    let e = err(&format!("{}a", "!".repeat(100_000)));
    assert_eq!(e.kind, ParseErrorKind::TooDeep);
    let ok = format!("{}a{}", "<".repeat(32), ">".repeat(32));
    assert!(parse(&ok).is_ok());
}

// ---------------------------------------------------------------- robustness & speed

fn check_span(s: Span, src: &str) {
    assert!(s.start <= s.end && s.end <= src.len());
    assert!(src.is_char_boundary(s.start) && src.is_char_boundary(s.end));
}

fn check_spans(e: &Expr, src: &str) {
    check_span(e.span, src);
    match &e.kind {
        ExprKind::Term(_) => {}
        ExprKind::Filter(f) => {
            check_span(f.name_span, src);
            check_span(f.value_span, src);
        }
        ExprKind::Not(x) | ExprKind::Group(x) => check_spans(x, src),
        ExprKind::And(v) | ExprKind::Or(v) => v.iter().for_each(|x| check_spans(x, src)),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn never_panics_on_arbitrary_text(input in "\\PC{0,120}") {
        if let Ok(q) = parse(&input) { if let Some(r) = q.root { check_spans(&r, &input); } }
    }

    #[test]
    fn never_panics_on_syntax_soup(
        input in "[a-z0-9:;.<>|!\\-~\" =*\\\\/_\u{0660}-\u{0669}أإاىمتدنوعحجصرسـ؛٫]{0,80}"
    ) {
        match parse(&input) {
            Ok(q) => { if let Some(r) = q.root { check_spans(&r, &input); } }
            Err(e) => {
                prop_assert!(e.span.start <= e.span.end && e.span.end <= input.len());
                prop_assert!(input.is_char_boundary(e.span.start));
                prop_assert!(input.is_char_boundary(e.span.end));
            }
        }
    }
}

#[test]
fn typical_query_parses_well_under_a_millisecond() {
    let q = "تقرير \"annual report\" ext:pdf;docx size:10mb..50mb dm:2026-01..2026-03 \
             | <type:image !tag:old> ~عقد الإيجار";
    assert!(parse(q).is_ok());
    let runs = 1000;
    let start = std::time::Instant::now();
    for _ in 0..runs {
        std::hint::black_box(parse(std::hint::black_box(q)).ok());
    }
    let per_parse = start.elapsed() / runs;
    // Generous bound so it holds in unoptimized debug builds on slow CI machines.
    assert!(per_parse.as_micros() < 1000, "parse took {per_parse:?}");
    eprintln!("typical query: {per_parse:?} per parse");
}
