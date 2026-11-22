//! End-to-end tests: query text -> `athrix_query` parser -> compiled `Query` -> index search.

use std::sync::atomic::AtomicBool;

use athrix_fs_index::{EntryId, EntryMeta, FileIndex, IndexBuilder, Query, QueryError};
use athrix_query::FilterName;
use chrono::NaiveDate;

const OFFSET: i32 = 3 * 3600; // UTC+3

/// Unix seconds of a local (UTC+3) date and time.
fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|d| d.and_hms_opt(h, min, 0))
        .unwrap()
        .and_utc()
        .timestamp()
        - i64::from(OFFSET)
}

/// "Now" for every test: Saturday 2026-11-21 12:00 local time. ISO week: Mon 16 - Sun 22.
fn now() -> i64 {
    local(2026, 11, 21, 12, 0)
}

const MB: u64 = 1024 * 1024;

fn dir() -> EntryMeta {
    EntryMeta {
        is_dir: true,
        ..Default::default()
    }
}

fn file(size: u64, modified: Option<i64>) -> EntryMeta {
    EntryMeta {
        size,
        modified,
        ..Default::default()
    }
}

/// ```text
/// D:\Data
/// ├── Docs
/// │   ├── تقرير_2026.pdf     2 MB    today 10:00
/// │   ├── Report.PDF        500 B   yesterday
/// │   ├── notes.txt         0 B     last week (Sun 15 23:59)
/// │   └── مشروع
/// │       ├── صورة.JPG      3 MB    2026-03-15
/// │       └── clip.mp4      10 MB   2025-06-01
/// ├── Code
/// │   ├── main.rs           1 KB    this week (Mon 16 00:00)
/// │   ├── archive.tar.gz    1 MB+1  unknown time
/// │   └── Makefile          100 B   today 01:30 local (still yesterday in UTC)
/// └── My Documents
///     └── تقرير قديم.docx    5 KB    2024-12-31 23:00
/// ```
fn fixture() -> FileIndex {
    fixture_with(|meta| meta)
}

/// The same tree with every time moved from "modified" to "created" (and the modification
/// time unknown), so `dc:` must find exactly what `dm:` finds in [`fixture`].
fn created_fixture() -> FileIndex {
    fixture_with(|meta| EntryMeta {
        modified: None,
        created: meta.modified,
        ..meta
    })
}

fn fixture_with(map: impl Fn(EntryMeta) -> EntryMeta) -> FileIndex {
    let file = |size, modified| map(file(size, modified));
    let mut b = IndexBuilder::new();
    let root = b.push(None, "D:\\Data", dir());
    let docs = b.push(Some(root), "Docs", dir());
    b.push(
        Some(docs),
        "تقرير_2026.pdf",
        file(2 * MB, Some(local(2026, 11, 21, 10, 0))),
    );
    b.push(
        Some(docs),
        "Report.PDF",
        file(500, Some(local(2026, 11, 20, 9, 0))),
    );
    b.push(
        Some(docs),
        "notes.txt",
        file(0, Some(local(2026, 11, 15, 23, 59))),
    );
    let project = b.push(Some(docs), "مشروع", dir());
    b.push(
        Some(project),
        "صورة.JPG",
        file(3 * MB, Some(local(2026, 3, 15, 8, 0))),
    );
    b.push(
        Some(project),
        "clip.mp4",
        file(10 * MB, Some(local(2025, 6, 1, 8, 0))),
    );
    let code = b.push(Some(root), "Code", dir());
    b.push(
        Some(code),
        "main.rs",
        file(1024, Some(local(2026, 11, 16, 0, 0))),
    );
    b.push(Some(code), "archive.tar.gz", file(MB + 1, None));
    b.push(
        Some(code),
        "Makefile",
        file(100, Some(local(2026, 11, 21, 1, 30))),
    );
    let mydocs = b.push(Some(root), "My Documents", dir());
    b.push(
        Some(mydocs),
        "تقرير قديم.docx",
        file(5 * 1024, Some(local(2024, 12, 31, 23, 0))),
    );
    b.finish()
}

fn compile(text: &str) -> Query {
    Query::parse_at(text, now(), OFFSET).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn find(ix: &FileIndex, text: &str) -> Vec<String> {
    let q = compile(text);
    let ids = ix.search(&q, usize::MAX, &AtomicBool::new(false)).unwrap();
    // The per-entry check agrees with the scan.
    let expected: Vec<EntryId> = (0..ix.len() as EntryId)
        .filter(|&id| ix.is_match(&q, id))
        .collect();
    assert_eq!(ids, expected, "{text}");
    ids.into_iter().map(|id| ix.name(id).to_owned()).collect()
}

fn unsupported(text: &str) -> (FilterName, usize, usize) {
    match Query::parse_at(text, now(), OFFSET) {
        Err(QueryError::Unsupported { filter, span, .. }) => (filter, span.start, span.end),
        other => panic!("{text}: expected Unsupported, got {other:?}"),
    }
}

#[test]
fn ext_filter_is_case_insensitive_and_skips_folders() {
    let ix = fixture();
    assert_eq!(find(&ix, "ext:pdf"), ["تقرير_2026.pdf", "Report.PDF"]);
    assert_eq!(find(&ix, "ext:PDF"), ["تقرير_2026.pdf", "Report.PDF"]);
    assert_eq!(find(&ix, "ext:.jpg;*.rs"), ["صورة.JPG", "main.rs"]);
    assert_eq!(find(&ix, "ext:gz"), ["archive.tar.gz"]);
    assert!(find(&ix, "ext:tar").is_empty()); // only the last extension counts
    assert!(find(&ix, "ext:data").is_empty());
}

#[test]
fn type_filter_maps_kinds_to_extensions() {
    let ix = fixture();
    assert_eq!(
        find(&ix, "type:doc"),
        [
            "تقرير_2026.pdf",
            "Report.PDF",
            "notes.txt",
            "تقرير قديم.docx"
        ]
    );
    assert_eq!(find(&ix, "type:image;video"), ["صورة.JPG", "clip.mp4"]);
    assert_eq!(find(&ix, "type:code"), ["main.rs"]);
    assert_eq!(find(&ix, "type:archive"), ["archive.tar.gz"]);
    assert_eq!(
        find(&ix, "type:folder"),
        ["D:\\Data", "Docs", "مشروع", "Code", "My Documents"]
    );
    assert_eq!(find(&ix, "نوع:صور"), ["صورة.JPG"]);
    assert!(find(&ix, "type:audio").is_empty());
}

#[test]
fn size_filter_compares_bytes_and_skips_folders() {
    let ix = fixture();
    assert_eq!(find(&ix, "size:>2mb"), ["صورة.JPG", "clip.mp4"]);
    assert_eq!(
        find(&ix, "size:>=2mb"),
        ["تقرير_2026.pdf", "صورة.JPG", "clip.mp4"]
    );
    assert_eq!(find(&ix, "size:1kb"), ["main.rs"]); // exact
    assert_eq!(find(&ix, "size:empty"), ["notes.txt"]); // folders (size 0) excluded
    assert_eq!(find(&ix, "size:<500"), ["notes.txt", "Makefile"]);
    assert_eq!(
        find(&ix, "size:<=500"),
        ["Report.PDF", "notes.txt", "Makefile"]
    );
    assert_eq!(
        find(&ix, "size:1mb..3mb"),
        ["تقرير_2026.pdf", "صورة.JPG", "archive.tar.gz"]
    );
    assert_eq!(
        find(&ix, "size:large"),
        ["تقرير_2026.pdf", "صورة.JPG", "clip.mp4", "archive.tar.gz"]
    );
    assert!(find(&ix, "size:<0").is_empty());
}

#[test]
fn date_filter_resolves_against_a_fixed_clock() {
    let ix = fixture();
    // Makefile is 01:30 local today, i.e. still "yesterday" in UTC: the offset matters.
    assert_eq!(find(&ix, "dm:today"), ["تقرير_2026.pdf", "Makefile"]);
    assert_eq!(find(&ix, "dm:yesterday"), ["Report.PDF"]);
    assert_eq!(
        find(&ix, "dm:thisweek"),
        ["تقرير_2026.pdf", "Report.PDF", "main.rs", "Makefile"]
    );
    assert_eq!(find(&ix, "dm:lastweek"), ["notes.txt"]);
    assert_eq!(
        find(&ix, "dm:thismonth"),
        [
            "تقرير_2026.pdf",
            "Report.PDF",
            "notes.txt",
            "main.rs",
            "Makefile"
        ]
    );
    assert!(find(&ix, "dm:lastmonth").is_empty());
    assert_eq!(find(&ix, "dm:lastyear"), ["clip.mp4"]);
    assert_eq!(find(&ix, "dm:2024"), ["تقرير قديم.docx"]);
    assert_eq!(find(&ix, "dm:2026-03"), ["صورة.JPG"]);
    assert_eq!(find(&ix, "dm:2026-03-15"), ["صورة.JPG"]);
    assert!(find(&ix, "dm:2026-03-14").is_empty());
    assert_eq!(find(&ix, "dm:<2026"), ["clip.mp4", "تقرير قديم.docx"]);
    assert_eq!(find(&ix, "dm:<=2025"), ["clip.mp4", "تقرير قديم.docx"]);
    assert_eq!(
        find(&ix, "dm:>2026-11-16"),
        ["تقرير_2026.pdf", "Report.PDF", "Makefile"]
    );
    assert_eq!(
        find(&ix, "dm:>=2026-11-16"),
        ["تقرير_2026.pdf", "Report.PDF", "main.rs", "Makefile"]
    );
    assert_eq!(find(&ix, "dm:2025-01..2026-03"), ["صورة.JPG", "clip.mp4"]);
    // Arabic named date; entries without a time (archive.tar.gz) never match.
    assert_eq!(find(&ix, "تاريخ:اليوم"), ["تقرير_2026.pdf", "Makefile"]);
    assert!(!find(&ix, "dm:>2000").contains(&"archive.tar.gz".to_string()));
}

#[test]
fn creation_date_filter_has_the_semantics_of_the_modification_date_filter() {
    let ix = fixture();
    let created = created_fixture();
    for date in [
        "today",
        "yesterday",
        "thisweek",
        "lastweek",
        "thismonth",
        "lastmonth",
        "thisyear",
        "lastyear",
        "2024",
        "2026-03",
        "2026-03-15",
        "2026-03-14",
        "<2026",
        "<=2025",
        ">2026-11-16",
        ">=2026-11-16",
        "2025-01..2026-03",
        ">2000",
    ] {
        let by_mtime = find(&ix, &format!("dm:{date}"));
        assert_eq!(find(&created, &format!("dc:{date}")), by_mtime, "dc:{date}");
        assert!(
            find(&ix, &format!("dc:{date}")).is_empty(),
            "no creation times"
        );
        assert!(
            find(&created, &format!("dm:{date}")).is_empty(),
            "no mtimes"
        );
    }
    // Combined with other filters and the Arabic synonym-free form.
    assert_eq!(find(&created, "dc:today ext:pdf"), ["تقرير_2026.pdf"]);
    assert_eq!(find(&created, "!dc:>2000"), find(&ix, "!dm:>2000"));
    assert_eq!(find(&created, "dc:2026 | dc:2024").len(), 7);
}

#[test]
fn week_starts_on_monday_and_month_rolls_over_the_year() {
    let ix = fixture();
    // On a Monday, "thisweek" starts that same day.
    let monday = local(2026, 11, 16, 9, 0);
    let q = Query::parse_at("dm:thisweek", monday, OFFSET).unwrap();
    let hits = ix.search(&q, usize::MAX, &AtomicBool::new(false)).unwrap();
    assert_eq!(hits.len(), 4);
    // In January, "lastmonth" is December of the previous year.
    let january = local(2025, 1, 10, 12, 0);
    let q = Query::parse_at("dm:lastmonth", january, OFFSET).unwrap();
    let names: Vec<_> = ix
        .search(&q, usize::MAX, &AtomicBool::new(false))
        .unwrap()
        .into_iter()
        .map(|id| ix.name(id))
        .collect();
    assert_eq!(names, ["تقرير قديم.docx"]);
}

#[test]
fn path_filters() {
    let ix = fixture();
    assert_eq!(
        find(&ix, "path:docs"),
        // "My Documents" normalizes to "my documents", which does not contain "docs".
        [
            "Docs",
            "تقرير_2026.pdf",
            "Report.PDF",
            "notes.txt",
            "مشروع",
            "صورة.JPG",
            "clip.mp4"
        ]
    );
    assert_eq!(find(&ix, "path:\"docs\\مشروع\" ext:mp4"), ["clip.mp4"]);
    assert_eq!(
        find(&ix, "path:^d:\\data\\code"),
        ["Code", "main.rs", "archive.tar.gz", "Makefile"]
    );
    assert_eq!(find(&ix, "path:*code\\main.rs"), ["main.rs"]);
    assert_eq!(find(&ix, "مسار:مشروع"), ["مشروع", "صورة.JPG", "clip.mp4"]);

    // parent: direct parent only, matched at word boundaries from the end of the path.
    assert_eq!(
        find(&ix, "parent:docs"),
        ["تقرير_2026.pdf", "Report.PDF", "notes.txt", "مشروع"]
    );
    assert_eq!(
        find(&ix, "parent:\"D:\\Data\\Docs\\مشروع\""),
        ["صورة.JPG", "clip.mp4"]
    );
    assert_eq!(find(&ix, "parent:documents"), ["تقرير قديم.docx"]);
    assert!(find(&ix, "parent:doc").is_empty()); // not a whole word
    assert_eq!(find(&ix, "parent:*data"), ["Docs", "Code", "My Documents"]);

    // infolder: anywhere below.
    assert_eq!(
        find(&ix, "infolder:docs"),
        [
            "تقرير_2026.pdf",
            "Report.PDF",
            "notes.txt",
            "مشروع",
            "صورة.JPG",
            "clip.mp4"
        ]
    );
    assert_eq!(find(&ix, "مجلد:مشروع"), ["صورة.JPG", "clip.mp4"]);
    assert_eq!(
        find(&ix, "infolder:\"D:\\Data\" type:folder"),
        ["Docs", "مشروع", "Code", "My Documents"]
    );
    assert_eq!(find(&ix, "infolder:data docs"), ["Docs"]); // `docs` is a separate name term
    assert_eq!(
        find(&ix, "infolder:d*code"),
        ["main.rs", "archive.tar.gz", "Makefile"]
    );
    assert!(find(&ix, "infolder:ata").is_empty());
}

#[test]
fn case_and_whole_word_modifiers() {
    let ix = fixture();
    assert_eq!(find(&ix, "case:Report"), ["Report.PDF"]);
    assert!(find(&ix, "case:report").is_empty());
    assert_eq!(find(&ix, "case:JPG"), ["صورة.JPG"]);
    assert_eq!(find(&ix, "ww:tar"), ["archive.tar.gz"]);
    assert!(find(&ix, "ww:arch").is_empty());
    assert_eq!(find(&ix, "ww:تقرير"), ["تقرير_2026.pdf", "تقرير قديم.docx"]);
    assert_eq!(unsupported("case: report"), (FilterName::Case, 0, 5));
    assert_eq!(unsupported("x ww:").0, FilterName::WholeWord);
}

#[test]
fn boolean_combinations() {
    let ix = fixture();
    assert_eq!(
        find(&ix, "ext:pdf | ext:rs"),
        ["تقرير_2026.pdf", "Report.PDF", "main.rs"]
    );
    assert_eq!(find(&ix, "ext:pdf !report"), ["تقرير_2026.pdf"]);
    assert_eq!(find(&ix, "ext:pdf -size:>1mb"), ["Report.PDF"]);
    assert_eq!(
        find(&ix, "<ext:pdf | type:image> size:>1mb"),
        ["تقرير_2026.pdf", "صورة.JPG"]
    );
    assert_eq!(
        find(&ix, "!type:folder !infolder:docs"),
        ["main.rs", "archive.tar.gz", "Makefile", "تقرير قديم.docx"]
    );
    // AND binds tighter than OR.
    assert_eq!(find(&ix, "report pdf | main"), ["Report.PDF", "main.rs"]);
    // Double negation, constants and empty groups of wildcards.
    assert_eq!(find(&ix, "!!ext:rs"), ["main.rs"]);
    assert!(find(&ix, "!*").is_empty());
    assert_eq!(find(&ix, "* | nothing").len(), ix.len());
    assert_eq!(find(&ix, "ext:rs | *").len(), ix.len());
}

#[test]
fn arabic_synonyms_end_to_end() {
    let ix = fixture();
    assert_eq!(find(&ix, "امتداد:pdf تقرير"), ["تقرير_2026.pdf"]);
    assert_eq!(
        find(&ix, "حجم:>1mb"),
        ["تقرير_2026.pdf", "صورة.JPG", "clip.mp4", "archive.tar.gz"]
    );
    assert_eq!(find(&ix, "حجم:>1ميجا امتداد:jpg"), ["صورة.JPG"]);
    assert_eq!(find(&ix, "تقرير !امتداد:pdf"), ["تقرير قديم.docx"]);
    assert_eq!(find(&ix, "إمتداد:docx"), ["تقرير قديم.docx"]);
}

#[test]
fn unsupported_filters_are_typed_errors() {
    for (text, filter) in [
        ("content:invoice", FilterName::Content),
        ("sem:rental contract", FilterName::Semantic),
        ("~rental contract", FilterName::Semantic),
        ("regex:^a.*b$", FilterName::Regex),
        ("!fuzzy: report", FilterName::Fuzzy),
        ("dupe:", FilterName::Dupe),
        ("similar:a.jpg", FilterName::Similar),
        ("tag:work", FilterName::Tag),
        ("da:2026", FilterName::DateAccessed),
        ("محتوى:فاتورة", FilterName::Content),
    ] {
        assert_eq!(unsupported(text).0, filter, "{text}");
    }
    // Even under OR/NOT, and with a span pointing at the filter.
    let (filter, start, end) = unsupported("report | !content:x");
    assert_eq!((filter, start, end), (FilterName::Content, 10, 19));
    // Parse errors are passed through.
    assert!(matches!(
        Query::parse("szie:1"),
        Err(QueryError::Parse(e)) if e.suggestion.as_deref() == Some("size")
    ));
}

#[test]
fn system_clock_parse_works() {
    let ix = fixture();
    let q = Query::parse("dm:today | ext:rs").unwrap();
    let hits = ix.search(&q, 10, &AtomicBool::new(false)).unwrap();
    assert!(hits.iter().any(|&id| ix.name(id) == "main.rs"));
}

/// A flat index of files under one root, for the fuzzy tests.
fn flat(names: &[&str]) -> FileIndex {
    let mut b = IndexBuilder::new();
    let root = b.push(None, "/r", dir());
    for n in names {
        b.push(Some(root), n, file(1, None));
    }
    b.finish()
}

fn ranked(ix: &FileIndex, text: &str, limit: usize) -> Vec<String> {
    let q = compile(text);
    assert!(q.is_ranked(), "{text}");
    ix.search(&q, limit, &AtomicBool::new(false))
        .unwrap()
        .into_iter()
        .map(|id| ix.name(id).to_owned())
        .collect()
}

#[test]
fn fuzzy_terms_are_ranked_best_first() {
    let ix = flat(&[
        "rxpxrxt.txt",
        "Monthly Report Summary.pdf",
        "my report.pdf",
        "report.pdf",
        "unrelated.doc",
        "reprot draft.docx",
        "report 2026.pdf",
    ]);
    assert_eq!(
        ranked(&ix, "fuzzy:report", usize::MAX),
        [
            "report.pdf",
            "report 2026.pdf",
            "my report.pdf",
            "Monthly Report Summary.pdf",
            "reprot draft.docx",
        ]
    );
    // Limit keeps the best ones.
    assert_eq!(
        ranked(&ix, "fuzzy:report", 2),
        ["report.pdf", "report 2026.pdf"]
    );
    assert_eq!(
        ranked(&ix, "fuzzy:rprt", usize::MAX),
        [
            "report.pdf",
            "my report.pdf",
            "report 2026.pdf",
            "reprot draft.docx",
            "Monthly Report Summary.pdf",
            "rxpxrxt.txt",
        ]
    );
    assert_eq!(
        ranked(&ix, "fuzzy:mrs", usize::MAX),
        ["Monthly Report Summary.pdf"]
    );
    // Combined with filters: plain filters do not change the order.
    assert_eq!(
        ranked(&ix, "fuzzy:rprt ext:pdf !my", usize::MAX),
        [
            "report.pdf",
            "report 2026.pdf",
            "Monthly Report Summary.pdf"
        ]
    );
    // Unranked queries keep index order.
    assert!(!compile("report").is_ranked());
    assert!(!compile("!fuzzy:report").is_ranked());
}

#[test]
fn fuzzy_switch_makes_plain_words_fuzzy() {
    let ix = flat(&[
        "تقرير المبيعات السنوي.xlsx",
        "تقرير_2026.pdf",
        "MyTravelReport.docx",
        "notes.txt",
    ]);
    assert_eq!(
        ranked(&ix, "fuzzy: تمس", 10),
        ["تقرير المبيعات السنوي.xlsx"]
    );
    assert_eq!(
        ranked(&ix, "تمس fuzzy:", 10),
        ["تقرير المبيعات السنوي.xlsx"]
    );
    assert_eq!(ranked(&ix, "fuzzy: mtr", 10), ["MyTravelReport.docx"]);
    // Two fuzzy words are ANDed.
    assert_eq!(ranked(&ix, "fuzzy: تقرر 2026", 10), ["تقرير_2026.pdf"]);
    // Quoted phrases and anchored words stay exact.
    let quoted = compile("fuzzy: \"mtr\" ^my");
    assert!(!quoted.is_ranked());
    assert!(
        ix.search(&quoted, 10, &AtomicBool::new(false))
            .unwrap()
            .is_empty()
    );
    // `fuzzy:` alone matches everything.
    assert_eq!(
        ix.search(&compile("fuzzy:"), 10, &AtomicBool::new(false))
            .unwrap()
            .len(),
        ix.len()
    );
}

#[test]
fn fuzzy_scores_and_highlights() {
    let ix = flat(&["report.pdf", "Monthly Report Summary.pdf", "notes.txt"]);
    let q = compile("fuzzy:mrs | fuzzy:report");
    let score = |name: &str| {
        let id = (0..ix.len() as EntryId)
            .find(|&i| ix.name(i) == name)
            .unwrap();
        ix.score(&q, id)
    };
    assert!(score("report.pdf") > score("Monthly Report Summary.pdf"));
    assert_eq!(score("notes.txt"), None);
    let lit = |name: &str| -> Vec<String> {
        q.fuzzy_highlights(name)
            .into_iter()
            .map(|r| name[r].to_owned())
            .collect()
    };
    // Both terms match: the acronym initials and the substring, merged.
    assert_eq!(lit("Monthly Report Summary.pdf"), ["M", "Report", "S"]);
    assert_eq!(lit("report.pdf"), ["report"]);
    assert!(lit("notes.txt").is_empty());
    assert!(compile("report").fuzzy_highlights("report.pdf").is_empty());
    // An unranked query scores every match 0.
    let plain = compile("report");
    assert_eq!(ix.score(&plain, 1), Some(0));
}

/// Name search folds ة = ه and the hamza seats ؤ = و, ئ = ي (decision 7), in plain words and
/// wildcards, either way round.
#[test]
fn taa_marbuta_and_hamza_seats_match_either_way() {
    let mut b = IndexBuilder::new();
    let root = b.push(None, r"D:\Data", dir());
    for name in [
        "فاتورة الكهرباء.pdf",
        "فاتوره قديمة.pdf",
        "تقرير المسؤول.docx",
        "مسوول المبيعات.xlsx",
        "مسئولية.txt",
        "ماء.txt",
    ] {
        b.push(Some(root), name, file(1, None));
    }
    let ix = b.finish();
    let both = ["فاتورة الكهرباء.pdf", "فاتوره قديمة.pdf"];
    assert_eq!(find(&ix, "فاتوره"), both);
    assert_eq!(find(&ix, "فاتورة"), both);
    assert_eq!(find(&ix, "فاتور*"), both);
    let both = ["تقرير المسؤول.docx", "مسوول المبيعات.xlsx"];
    assert_eq!(find(&ix, "مسوول"), both);
    assert_eq!(find(&ix, "مسؤول"), both);
    assert_eq!(find(&ix, "مسيولية"), ["مسئولية.txt"]);
    // The standalone hamza is not deleted: «ما» is not «ماء».
    assert_eq!(find(&ix, "ماء"), ["ماء.txt"]);
}
