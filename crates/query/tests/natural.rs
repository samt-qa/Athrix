//! Natural-language queries → filters (`athrix_query::interpret`): a table of Arabic and
//! English cases, cases that must be left alone, and properties (the output always parses
//! and plans, interpreting it again gives nothing, arbitrary input never panics).
//!
//! "Today" is fixed at Sunday 2026-11-22.

use athrix_query::{
    CalendarDate, InterpretOptions, PartRole, PlanOptions, interpret, interpret_with, parse, plan,
};
use proptest::prelude::*;

const TODAY: CalendarDate = CalendarDate {
    year: 2026,
    month: Some(11),
    day: Some(22),
};

fn opts() -> InterpretOptions {
    InterpretOptions::with_today(TODAY).with_date_kinds()
}

fn run(text: &str) -> Option<String> {
    interpret_with(text, &opts()).map(|i| i.query)
}

const CASES: &[(&str, &str)] = &[
    // --- the four examples of the task ---
    (
        "ملفات PDF من الأسبوع الماضي أكبر من 5 ميجا",
        "ext:pdf dm:lastweek size:>5mb",
    ),
    (
        "excel files from yesterday bigger than 2mb",
        "ext:xlsx;xls dm:yesterday size:>2mb",
    ),
    ("صور رحلة إسطنبول ٢٠٢٥", "type:image رحلة إسطنبول dm:2025"),
    (
        "مستندات تحتوي على عقد الإيجار",
        "type:doc content:\"عقد الإيجار\"",
    ),
    // --- Arabic kinds and named dates ---
    ("ملفات وورد من هذا الشهر", "ext:docx;doc dm:thismonth"),
    ("عروض تقديمية من السنة الماضية", "ext:pptx;ppt dm:lastyear"),
    ("فيديوهات أكبر من 1 جيجا", "type:video size:>1gb"),
    ("صوتيات من أمس", "type:audio dm:yesterday"),
    ("كود من اليوم", "type:code dm:today"),
    ("صور من هذا الأسبوع", "type:image dm:thisweek"),
    ("ملفات مضغوطة من الأسبوع الماضي", "type:archive dm:lastweek"),
    ("ملفات معدلة اليوم", "dm:today"),
    ("صور وفيديوهات من أمس", "type:image;video dm:yesterday"),
    ("ملفات PDF أو وورد", "ext:pdf;docx;doc"),
    ("ملفات اكسل من الشهر الماضي", "ext:xlsx;xls dm:lastmonth"),
    ("اغاني اكبر من ٥ ميجا", "type:audio size:>5mb"),
    ("ملفات بوربوينت من العام الماضي", "ext:pptx;ppt dm:lastyear"),
    ("مستندات PDF من البارحة", "ext:pdf dm:yesterday"),
    // --- Arabic years and months ---
    ("ملفات PDF في 2024", "ext:pdf dm:2024"),
    ("ملفات من سنة 2023", "dm:2023"),
    ("مستندات منذ 2024", "type:doc dm:>=2024"),
    ("ملفات قبل 2020", "dm:<2020"),
    ("صور من مارس 2025", "type:image dm:2025-03"),
    ("صور في عام ٢٠٢٢", "type:image dm:2022"),
    ("ملفات بعد 2021", "dm:>2021"),
    ("صور في أغسطس", "type:image dm:2026-08"),
    // --- Arabic relative dates (today = 2026-11-22) ---
    ("ملفات قبل 3 أيام", "dm:2026-11-19"),
    ("صور منذ شهرين", "type:image dm:>=2026-09-23"),
    ("مستندات آخر 7 أيام", "type:doc dm:>=2026-11-16"),
    ("ملفات أقدم من سنة", "dm:<2025-11-22"),
    ("ملفات قبل أسبوعين", "dm:2026-11-08..2026-11-14"),
    ("ملفات من شهر", "dm:2026-10"),
    ("صور خلال آخر ثلاثة أيام", "type:image dm:>=2026-11-20"),
    ("ملفات قبل يومين", "dm:2026-11-20"),
    // --- Arabic sizes ---
    (
        "ابحث عن ملفات إكسل أصغر من ٢٠٠ ك",
        "ext:xlsx;xls size:<200kb",
    ),
    ("ملفات أكبر من ١٫٥ جيجا", "size:>1.5gb"),
    ("ملفات بين 1 و 5 ميجا", "size:1mb..5mb"),
    ("ملفات بين 100 و500 ك", "size:100kb..500kb"),
    (
        "ملفات بحجم أكبر من 10 ميجا من الشهر الماضي",
        "size:>10mb dm:lastmonth",
    ),
    ("ملفات PDF أكبر من خمسة ميجا", "ext:pdf size:>5mb"),
    ("فيديو أكثر من 700م", "type:video size:>700mb"),
    ("صور لا تزيد عن 2 ميجابايت", "type:image size:<=2mb"),
    // --- Arabic content and places ---
    ("أريد ملفات PDF عن الضرائب", "ext:pdf content:الضرائب"),
    ("فيها كلمة ميزانية", "content:ميزانية"),
    ("ملفات نصية تحتوي على فاتورة", "ext:txt content:فاتورة"),
    (
        "مستندات تحتوي على عقد الإيجار من الشهر الماضي",
        "type:doc content:\"عقد الإيجار\" dm:lastmonth",
    ),
    ("مجلدات في مجلد المشاريع", "type:folder infolder:المشاريع"),
    ("ملفات على القرص D", "infolder:D:"),
    ("تقارير PDF في مجلد العمل", "تقارير ext:pdf infolder:العمل"),
    (
        "صور في مجلد \"رحلة الصيف\" من 2024",
        "type:image infolder:\"رحلة الصيف\" dm:2024",
    ),
    // --- English ---
    ("pdf files from last week", "ext:pdf dm:lastweek"),
    ("show me photos from yesterday", "type:image dm:yesterday"),
    (
        "word documents modified this month",
        "ext:docx;doc dm:thismonth",
    ),
    ("videos larger than 1gb", "type:video size:>1gb"),
    ("music under 10 mb", "type:audio size:<10mb"),
    ("spreadsheets from 2024", "ext:xlsx;xls dm:2024"),
    (
        "presentations created last year",
        "ext:pptx;ppt dc:lastyear",
    ),
    (
        "documents containing quarterly budget",
        "type:doc content:\"quarterly budget\"",
    ),
    (
        "files that mention invoice from last month",
        "content:invoice dm:lastmonth",
    ),
    (
        "images in folder Downloads",
        "type:image infolder:Downloads",
    ),
    ("code in the projects folder", "type:code infolder:projects"),
    ("files on drive D bigger than 4 gb", "infolder:D: size:>4gb"),
    ("pdf files 3 days ago", "ext:pdf dm:2026-11-19"),
    ("photos from the last 7 days", "type:image dm:>=2026-11-16"),
    ("files older than 30 days", "dm:<2026-10-23"),
    ("excel files at least 1.5 mb", "ext:xlsx;xls size:>=1.5mb"),
    ("files between 10mb and 1gb", "size:10mb..1gb"),
    ("folders from today", "type:folder dm:today"),
    ("report pdf from last week", "report ext:pdf dm:lastweek"),
    ("mp3 files from this year", "ext:mp3 dm:thisyear"),
    ("find invoices since 2023", "invoices dm:>=2023"),
    ("pictures of istanbul 2025", "type:image istanbul dm:2025"),
    ("files in march 2025", "dm:2025-03"),
    ("pdf documents about taxes", "ext:pdf content:taxes"),
    ("show me excel files on D:", "ext:xlsx;xls infolder:D:"),
    ("videos past 2 weeks", "type:video dm:>=2026-11-09"),
    ("zip files over 500 mb", "type:archive size:>500mb"),
    ("files modified in the last 2 months", "dm:>=2026-09-23"),
    (
        "songs from a week ago",
        "type:audio dm:2026-11-15..2026-11-21",
    ),
    (
        "photos and videos from yesterday",
        "type:image;video dm:yesterday",
    ),
    (
        "pdf or images from today",
        "<ext:pdf | type:image> dm:today",
    ),
    (
        "documents with the text \"net profit\"",
        "type:doc content:\"net profit\"",
    ),
    // --- creation and access dates ---
    ("الملفات التي أُنشئت الأسبوع الماضي", "dc:lastweek"),
    ("ملفات انشئت في 2024", "dc:2024"),
    ("صور أنشأتها الشهر الماضي", "type:image dc:lastmonth"),
    ("مستندات PDF المنشأة منذ 2023", "ext:pdf dc:>=2023"),
    ("ملفات تم إنشاؤها قبل 3 أيام", "dc:2026-11-19"),
    ("ملفات فُتحت أمس", "da:yesterday"),
    ("ملفات وورد فتحتها هذا الأسبوع", "ext:docx;doc da:thisweek"),
    ("صور استُخدمت في آخر 7 أيام", "type:image da:>=2026-11-16"),
    ("ملفات عدلتها اليوم", "dm:today"),
    ("files created in 2024", "dc:2024"),
    ("pdf files i created last month", "ext:pdf dc:lastmonth"),
    ("photos created on 2025-06-01", "type:image dc:2025-06-01"),
    ("files opened yesterday", "da:yesterday"),
    (
        "documents accessed in the last 7 days",
        "type:doc da:>=2026-11-16",
    ),
    (
        "spreadsheets last opened before 2023",
        "ext:xlsx;xls da:<2023",
    ),
    ("files used 3 days ago", "da:2026-11-19"),
    // the same cases as the mock interpreter's self-checks (apps/desktop)
    ("ملفات أُنشئت أمس", "dc:yesterday"),
    ("صور أنشأتها من الشهر الماضي", "type:image dc:lastmonth"),
    ("documents accessed from last week", "type:doc da:lastweek"),
    ("large videos", "size:>100mb type:video"),
    ("files last modified this week", "dm:thisweek"),
    // --- size words without a number ---
    ("ملفات كبيرة", "size:>100mb"),
    ("الملفات الضخمة", "size:>1gb"),
    (
        "فيديوهات كبيرة من السنة الماضية",
        "type:video size:>100mb dm:lastyear",
    ),
    ("ملفات حجمها كبير", "size:>100mb"),
    ("ملفات كبيرة جدا", "size:>1gb"),
    ("صور صغيرة", "type:image size:<100kb"),
    ("ملفات صغيرة جدا", "size:tiny"),
    ("ملفات فارغة", "size:empty"),
    ("مجلدات فارغة", "type:folder size:empty"),
    ("large files", "size:>100mb"),
    ("big pdf files", "size:>100mb ext:pdf"),
    ("huge videos", "size:>1gb type:video"),
    ("very large files from last year", "size:>1gb dm:lastyear"),
    ("small images", "size:<100kb type:image"),
    ("tiny files", "size:tiny"),
    ("empty folders", "size:empty type:folder"),
    // --- quoted phrases and negated words are kept as they are ---
    (
        "\"last week\" notes pdf files",
        "\"last week\" notes ext:pdf",
    ),
    ("-draft pdf files from today", "-draft ext:pdf dm:today"),
];

/// Plain names and anything that already uses the query syntax: left alone.
const LEFT_ALONE: &[&str] = &[
    "",
    "   ",
    "تقرير المبيعات",
    "report final",
    "report 2025",
    "صور",
    "photos",
    "pdf",
    "music",
    "\"last week\"",
    "\"last week\" report",
    "صور \"الأسبوع الماضي\"",
    "تقرير اليوم",
    "the report",
    "ابحث عن تقرير",
    "march madness",
    "may report",
    "2024-03 notes",
    "budget.xlsx",
    "ext:pdf from last week",
    "type:image صور من أمس",
    "invoice | receipt from today",
    "<a b> pdf files from today",
    "~cats pdf files from today",
    "content:x from last week",
    "C:\\Users",
    "قبل",
    "أكبر من",
    "bigger than 5",
    "big report",
    "small business plan",
    "كبيرة",
    "large",
    "created",
    "files opened",
    "ملفات أنشئت",
];

#[test]
fn table_of_cases() {
    assert!(CASES.len() >= 60, "{} cases", CASES.len());
    let mut failures = Vec::new();
    for (input, expected) in CASES {
        let got = run(input);
        if got.as_deref() != Some(*expected) {
            failures.push(format!(
                "{input:?}\n   expected {expected:?}\n   got      {got:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn left_alone() {
    let mut failures = Vec::new();
    for input in LEFT_ALONE {
        if let Some(i) = interpret_with(input, &opts()) {
            failures.push(format!("{input:?} -> {:?}", i.query));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn relative_dates_need_today() {
    assert_eq!(interpret("ملفات قبل 3 أيام"), None);
    assert_eq!(interpret("files from the last 7 days"), None);
    // Named periods do not need it.
    assert_eq!(
        interpret("ملفات PDF من الأسبوع الماضي").map(|i| i.query),
        Some("ext:pdf dm:lastweek".to_owned())
    );
}

#[test]
fn parts_cover_the_input_in_order() {
    let text = "ابحث عن ملفات PDF من الأسبوع الماضي أكبر من 5 ميجا تقرير";
    let i = interpret_with(text, &opts()).expect("interpreted");
    let roles: Vec<PartRole> = i.parts.iter().map(|p| p.role).collect();
    assert_eq!(
        roles,
        [
            PartRole::Ignored,
            PartRole::Ignored,
            PartRole::Kind,
            PartRole::Date,
            PartRole::Size,
            PartRole::Term
        ]
    );
    let texts: Vec<&str> = i.parts.iter().map(|p| p.span.slice(text)).collect();
    assert_eq!(
        texts,
        [
            "ابحث عن",
            "ملفات",
            "PDF",
            "من الأسبوع الماضي",
            "أكبر من 5 ميجا",
            "تقرير"
        ]
    );
    assert_eq!(i.parts[3].output, "dm:lastweek");
    assert!(i.parts.windows(2).all(|w| w[0].span.end <= w[1].span.start));
    assert!(i.confidence > 0.5 && i.confidence <= 1.0);
}

#[test]
fn confidence_grows_with_cues() {
    let weak = interpret_with("صور 2025", &opts()).expect("two weak cues");
    let strong = interpret_with("ملفات PDF من الأسبوع الماضي أكبر من 5 ميجا", &opts())
        .expect("three strong cues");
    assert!(weak.confidence < strong.confidence);
}

/// Checks the contract for one input.
fn check(text: &str) -> Result<(), TestCaseError> {
    for options in [InterpretOptions::default(), opts()] {
        if let Some(i) = interpret_with(text, &options) {
            let q = parse(&i.query);
            prop_assert!(q.is_ok(), "{text:?} -> {:?} does not parse", i.query);
            let q = q.unwrap();
            for p in [
                PlanOptions::default(),
                PlanOptions {
                    plain_words_search_content: true,
                },
            ] {
                prop_assert!(plan(&q, &p).is_ok(), "{text:?} -> {:?}", i.query);
            }
            prop_assert_eq!(interpret_with(&i.query, &options), None);
            for part in &i.parts {
                prop_assert!(part.span.end <= text.len());
                prop_assert!(text.is_char_boundary(part.span.start));
                prop_assert!(text.is_char_boundary(part.span.end));
            }
        }
    }
    Ok(())
}

#[test]
fn every_case_satisfies_the_contract() {
    for (input, _) in CASES {
        check(input).unwrap();
    }
}

const VOCAB: &[&str] = &[
    "ملفات",
    "PDF",
    "pdf",
    "من",
    "الأسبوع",
    "الماضي",
    "أكبر",
    "أصغر",
    "5",
    "٥",
    "ميجا",
    "mb",
    "1.5",
    "gb",
    "صور",
    "وفيديو",
    "تحتوي",
    "على",
    "عن",
    "في",
    "مجلد",
    "القرص",
    "D",
    "D:",
    "قبل",
    "منذ",
    "آخر",
    "أيام",
    "شهرين",
    "2024",
    "٢٠٢٥",
    "مارس",
    "اليوم",
    "أمس",
    "files",
    "from",
    "last",
    "week",
    "7",
    "days",
    "ago",
    "bigger",
    "than",
    "between",
    "and",
    "containing",
    "about",
    "in",
    "folder",
    "the",
    "on",
    "drive",
    "older",
    "since",
    "before",
    "may",
    "excel",
    "word",
    "documents",
    "\"last week\"",
    "\"a|b\"",
    "-draft",
    "!x",
    "-",
    "!",
    "|",
    "<",
    ">",
    "ext:pdf",
    "size:>1mb",
    "~",
    "\"",
    ":",
    "x:",
    "تقرير",
    "report",
    "بين",
    "و",
    "1",
    "3",
    "ك",
    "م",
    "،",
    "؟",
    ".",
    "kb",
    "a",
    "older than",
    "at least",
    "with the text",
    "infolder",
    "أنشئت",
    "فتحتها",
    "created",
    "opened",
    "last opened",
    "كبيرة",
    "الضخمة",
    "جدا",
    "large",
    "very",
    "empty",
    "small",
];

proptest! {
    #[test]
    fn vocabulary_soup_keeps_the_contract(words in prop::collection::vec(prop::sample::select(VOCAB), 0..14)) {
        check(&words.join(" "))?;
    }

    #[test]
    fn arbitrary_text_never_panics(text in "\\PC{0,80}") {
        check(&text)?;
    }

    #[test]
    fn arbitrary_today_never_panics(
        y in 1u16..=9999,
        m in 1u8..=12,
        d in 1u8..=31,
        words in prop::collection::vec(prop::sample::select(VOCAB), 0..10),
    ) {
        let options = InterpretOptions::with_today(CalendarDate { year: y, month: Some(m), day: Some(d) });
        if let Some(i) = interpret_with(&words.join(" "), &options) {
            prop_assert!(parse(&i.query).is_ok(), "{:?}", i.query);
        }
    }
}
