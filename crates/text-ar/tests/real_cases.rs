//! Real-world cases: Arabic file names and queries as people type them, with the spelling
//! variants that must (or must not) match. Table-driven so a new report becomes one line.

use athrix_text_ar::{find_in_name, normalize_name, swap_layout, tokenize};

/// `(file name, query, highlighted original text or None when it must not match)`.
///
/// Name search uses the nass `Search` level: harakat, tatweel and invisible marks are ignored,
/// أ/إ/آ/ٱ = ا, ى = ي, Persian ک/ی = ك/ي, Arabic-Indic and Persian digits = ASCII digits,
/// punctuation = space, Latin is case-insensitive. On top of that, names fold ة = ه, ؤ = و and
/// ئ = ي like content search's `Loose` level (decision 7); the standalone hamza ء is kept.
const NAME_CASES: &[(&str, &str, Option<&str>)] = &[
    // Hamza forms of alef.
    ("أحمد - السيرة الذاتية.pdf", "احمد", Some("أحمد")),
    ("احمد.docx", "أحمد", Some("احمد")),
    ("إقرار ضريبي 2025.pdf", "اقرار", Some("إقرار")),
    ("آخر تحديث.txt", "اخر", Some("آخر")),
    ("ٱلقرآن.pdf", "القران", Some("ٱلقرآن")),
    // Alef maqsura and yaa.
    ("مستشفى الملك فهد.pdf", "مستشفي", Some("مستشفى")),
    ("على الهامش.docx", "علي", Some("على")),
    // Persian/Urdu letter forms.
    ("کتاب فارسی.pdf", "كتاب", Some("کتاب")),
    ("کتاب فارسی.pdf", "فارسي", Some("فارسی")),
    // Harakat, shadda and tatweel in the name, or in the query.
    ("مُحَاضَرَةُ الفِقْهِ.pdf", "الفقه", Some("الفِقْهِ")),
    ("مُحَاضَرَةُ الفِقْهِ.pdf", "محاضرة", Some("مُحَاضَرَةُ")),
    ("محاضرة الفقه.pdf", "مُحَاضَرَة", Some("محاضرة")),
    (
        "الـــتقـريـر_الســنوي.xlsx",
        "التقرير السنوي",
        Some("الـــتقـريـر_الســنوي"),
    ),
    ("مُدَرِّس.txt", "مدرس", Some("مُدَرِّس")),
    // Digits: Arabic-Indic, Persian and ASCII are the same.
    ("فاتورة ١٢٣.pdf", "123", Some("١٢٣")),
    ("فاتورة 123.pdf", "١٢٣", Some("123")),
    ("گزارش ۱۴۰۲.pdf", "1402", Some("۱۴۰۲")),
    ("تقرير ٢٠٢٦-٠٣.docx", "2026 03", Some("٢٠٢٦-٠٣")),
    // Mixed Arabic and English, case-insensitive Latin, separators.
    ("تقرير_Report_2026.pdf", "report", Some("Report")),
    ("تقرير_Report_2026.pdf", "تقرير", Some("تقرير")),
    ("Invoice-فاتورة.PDF", "pdf", Some("PDF")),
    (
        "عقد الإيجار - Lease Agreement.docx",
        "lease agreement",
        Some("Lease Agreement"),
    ),
    // The article and common words.
    ("عقد الإيجار.docx", "الايجار", Some("الإيجار")),
    ("عقد الإيجار.docx", "إيجار", Some("إيجار")),
    ("عقد الإيجار.docx", "عقد الايجار", Some("عقد الإيجار")),
    (
        "محضر اجتماع مجلس الإدارة.docx",
        "محضر اجتماع",
        Some("محضر اجتماع"),
    ),
    ("محضر_اجتماع_2026.docx", "محضر اجتماع", Some("محضر_اجتماع")),
    ("فاتورة الكهرباء.pdf", "فاتورة", Some("فاتورة")),
    ("التقرير الشهري.pdf", "تقرير", Some("تقرير")),
    // ZWNJ / RLM inside a name (copied from a Persian keyboard or an RTL editor).
    ("می\u{200C}خواهم.txt", "میخواهم", Some("می\u{200C}خواهم")),
    ("\u{200F}تقرير\u{200F}.pdf", "تقرير", Some("تقرير\u{200F}")),
    // Taa marbuta and hamza seats, either way round.
    ("فاتورة.pdf", "فاتوره", Some("فاتورة")),
    ("فاتوره.pdf", "فاتورة", Some("فاتوره")),
    ("مؤتمر.pdf", "موتمر", Some("مؤتمر")),
    ("تقرير المسؤول.docx", "مسوول", Some("مسؤول")),
    ("مسوول المبيعات.xlsx", "مسؤول", Some("مسوول")),
    ("مسئولية.pdf", "مسيولية", Some("مسئولية")),
    ("مُحَاضَرَةُ الفِقْهِ.pdf", "محاضره", Some("مُحَاضَرَةُ")),
    // Must NOT match: different letters.
    ("ماء.pdf", "ما ء", None),
    ("تقرير.pdf", "تقارير", None),
    ("عقد.pdf", "عقود", None),
];

#[test]
fn name_matching_and_highlights() {
    let mut failures = Vec::new();
    for &(name, query, want) in NAME_CASES {
        let got: Vec<&str> = find_in_name(name, query)
            .iter()
            .map(|m| &name[m.start..m.end])
            .collect();
        let ok = match want {
            Some(w) => got.contains(&w),
            None => got.is_empty(),
        };
        if !ok {
            failures.push(format!("{name:?} / {query:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// `(text, query)` pairs the content analyzer must match: every query term occurs among the
/// text's terms. The content index uses the `Loose` level plus light stemming, so it is more
/// tolerant than name search: ة = ه, ؤ = و, ئ = ي, plurals and the article fold.
const CONTENT_MATCHES: &[(&str, &str)] = &[
    ("فاتورة الكهرباء لشهر مارس", "فاتوره"),
    // Not «الفواتير»: broken plurals (فواعيل) are not folded by the light stemmer, by design.
    ("فواتير الكهرباء", "الفواتير"),
    ("انعقد المؤتمر في الرياض", "موتمر"),
    ("قدّم رئيس الهيئة تقريرًا", "رييس تقرير"),
    ("وقّع الطرفان عقدَ الإيجارِ", "عقد الايجار"),
    ("نشرت المكتبات تقاريرها السنوية", "تقرير مكتبة"),
    ("محضر اجتماع مجلس الإدارة رقم ٤", "محضر الاجتماع 4"),
    ("المعلمون والمعلمات", "معلم"),
    ("وبالتقارير المرفقة", "التقارير"),
    ("الـــعربية لغة جميلة", "العربيه"),
    ("مستشفى الملك فهد", "مستشفي"),
    ("Annual REPORT 2026", "annual report"),
    ("صلى الله عليه وسلم ﷺ", "الله"),
];

#[test]
fn content_terms_match() {
    let mut failures = Vec::new();
    for &(text, query) in CONTENT_MATCHES {
        let doc: Vec<String> = tokenize(text).map(|t| t.text).collect();
        for t in tokenize(query).filter(|t| !t.is_variant) {
            if !doc.contains(&t.text) {
                failures.push(format!(
                    "{query:?} term {:?} not in {text:?}: {doc:?}",
                    t.text
                ));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// `(typed with the wrong layout active, meant)`.
const LAYOUT_CASES: &[(&str, &str)] = &[
    ("hgjrvdv", "التقرير"),
    ("thj,vm", "فاتورة"),
    ("ur] hgYd[hv", "عقد الإيجار"),
    ("lpqv h[jlhu", "محضر اجتماع"),
    ("lgt", "ملف"),
    ("شىىعشم قثحخقف", "annual report"),
    ("هىرخهؤث", "invoice"),
    ("ؤخىفقشؤف", "contract"),
];

#[test]
fn keyboard_layout_cases() {
    for &(typed, meant) in LAYOUT_CASES {
        assert_eq!(swap_layout(typed).as_deref(), Some(meant), "{typed}");
    }
}

/// Normalization equivalences people rely on: each group normalizes to one name key.
const SAME_KEY: &[&[&str]] = &[
    &["أحمد", "احمد", "إحمد", "آحمد", "اَحْمَد", "احـمد"],
    &["مستشفى", "مستشفي", "مُسْتَشْفَى"],
    &["٢٠٢٦", "۲۰۲۶", "2026"],
    &["کتاب", "كتاب", "كِتَاب"],
    &["Report", "REPORT", "report"],
    &["عقد_الإيجار", "عقد الايجار", "عقد-الإيجار"],
    // Taa marbuta and hamza seats fold like content search (decision 7, 2026-11-22).
    &["فاتورة", "فاتوره", "فَاتُورَة", "فاتـورة"],
    &["مسؤول", "مسوول", "مَسْؤُول"],
    &["مسئول", "مسيول"],
    &["مكتبة الجامعة", "مكتبه الجامعه"],
];

/// Names that must stay apart: the standalone hamza is not deleted in names.
#[test]
fn standalone_hamza_is_kept_in_names() {
    assert_ne!(normalize_name("ماء"), normalize_name("ما"));
    assert_ne!(normalize_name("جزء"), normalize_name("جز"));
}

#[test]
fn normalization_groups() {
    for group in SAME_KEY {
        let key = normalize_name(group[0]);
        for v in &group[1..] {
            assert_eq!(normalize_name(v), key, "{v:?} vs {:?}", group[0]);
        }
    }
}
