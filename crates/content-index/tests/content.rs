//! End-to-end tests of the content index: Arabic and English documents.

use athrix_content_index::{ContentIndex, Error, FileKey, FileMeta, IndexOptions, RebuildReason};
use tempfile::TempDir;

const META: FileMeta = FileMeta {
    size: 100,
    mtime: 1,
};

/// Builds an index in a fresh temp dir with the given (path, text) documents, committed.
fn index_with(docs: &[(&str, &str)]) -> (TempDir, ContentIndex) {
    let dir = TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    for (path, text) in docs {
        idx.add_or_update(path, META, text).unwrap();
    }
    idx.commit().unwrap();
    (dir, idx)
}

fn paths(idx: &ContentIndex, q: &str) -> Vec<String> {
    let mut p: Vec<_> = idx
        .search(q, 50)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect();
    p.sort();
    p
}

const LIBRARIES: &str = "نَشَرَتِ الْمَكْتَبَاتُ الْعَامَّةُ تَقَارِيرَهَا السَّنَوِيَّةَ عَنْ عَدَدِ الْمُعَلِّمِينَ.";
const CONTRACT: &str = "هذا عقد الإيجار بين المؤجر والمستأجر لمدة سنة واحدة في مدينة الرياض.";
const HOSPITAL: &str = "زار رئيس المؤتمر مستشفى المدينة، وقدّم الهيئة تقريرًا عن الأطباء.";
const ENGLISH: &str = "The quarterly Report lists revenue and the annual reports of 2026.";

fn corpus() -> (TempDir, ContentIndex) {
    index_with(&[
        ("C:/docs/libraries.txt", LIBRARIES),
        ("C:/docs/contract.docx", CONTRACT),
        ("C:/docs/hospital.pdf", HOSPITAL),
        ("C:/docs/english.md", ENGLISH),
    ])
}

#[test]
fn diacritized_text_is_found_by_a_plain_query() {
    let (_d, idx) = corpus();
    assert_eq!(paths(&idx, "المكتبات العامة"), ["C:/docs/libraries.txt"]);
    // and a diacritized query finds plain text
    assert_eq!(paths(&idx, "عَقْدُ الْإِيجَارِ"), ["C:/docs/contract.docx"]);
}

#[test]
fn hamza_taa_marbuta_and_alef_maqsura_variants_match() {
    let (_d, idx) = corpus();
    assert_eq!(paths(&idx, "موتمر"), ["C:/docs/hospital.pdf"]); // مؤتمر
    assert_eq!(paths(&idx, "رييس"), ["C:/docs/hospital.pdf"]); // رئيس
    assert_eq!(paths(&idx, "مستشفي"), ["C:/docs/hospital.pdf"]); // مستشفى
    assert_eq!(paths(&idx, "الهيئه"), ["C:/docs/hospital.pdf"]); // الهيئة
    assert_eq!(paths(&idx, "الايجار"), ["C:/docs/contract.docx"]); // الإيجار
    assert_eq!(paths(&idx, "المستاجر"), ["C:/docs/contract.docx"]); // المستأجر
}

#[test]
fn stemming_recall() {
    let (_d, idx) = corpus();
    // Sound plurals and the article: مكتبة ⇠ المكتبات, معلم ⇠ المعلمين.
    assert_eq!(paths(&idx, "مكتبة"), ["C:/docs/libraries.txt"]);
    assert_eq!(paths(&idx, "معلم"), ["C:/docs/libraries.txt"]);
    // تفاعيل broken plural + pronoun: تقرير ⇠ تقاريرها, and the singular in the other doc.
    assert_eq!(
        paths(&idx, "تقرير"),
        ["C:/docs/hospital.pdf", "C:/docs/libraries.txt"]
    );
    assert_eq!(
        paths(&idx, "التقارير"),
        ["C:/docs/hospital.pdf", "C:/docs/libraries.txt"]
    );
    // Conjunction: «والمستأجر» is found by «مستأجر».
    assert_eq!(paths(&idx, "مستأجر"), ["C:/docs/contract.docx"]);
}

#[test]
fn broken_plurals_outside_tafaeel_are_not_conflated() {
    // Documented limit: only تفاعيل → تفعيل is folded. مكاتب (plural of مكتب) is not مكتب.
    let (_d, idx) = index_with(&[("a", "المكاتب الجديدة"), ("b", "مكتب واحد")]);
    assert_eq!(paths(&idx, "مكتب"), ["b"]);
    assert_eq!(paths(&idx, "مكاتب"), ["a"]);
}

#[test]
fn english_is_lowercased_not_stemmed() {
    let (_d, idx) = corpus();
    assert_eq!(paths(&idx, "report"), ["C:/docs/english.md"]);
    assert_eq!(paths(&idx, "REVENUE 2026"), ["C:/docs/english.md"]);
    let reports = idx.search("report", 10).unwrap();
    let s = &reports[0].snippet;
    let words: Vec<_> = s
        .highlights
        .iter()
        .map(|r| &s.fragment[r.clone()])
        .collect();
    assert_eq!(words, ["Report"], "«reports» is a different term");
}

#[test]
fn words_are_anded() {
    let (_d, idx) = corpus();
    assert_eq!(paths(&idx, "عقد الرياض"), ["C:/docs/contract.docx"]);
    assert!(paths(&idx, "عقد مستشفى").is_empty());
}

#[test]
fn phrase_queries() {
    let (_d, idx) = index_with(&[
        ("a", "وقّع الطرفان عقدَ الإيجارِ أمس"),
        ("b", "عقد العمل لا يشمل الإيجار"),
    ]);
    assert_eq!(paths(&idx, "عقد الإيجار"), ["a", "b"]);
    assert_eq!(paths(&idx, r#""عقد الإيجار""#), ["a"]);
    assert_eq!(paths(&idx, r#""الإيجار عقد""#), Vec::<String>::new());
    // A word the analyzer splits is a phrase too.
    let (_d2, idx2) = index_with(&[("x", "file report_2026.pdf"), ("y", "2026 report")]);
    assert_eq!(paths(&idx2, "report_2026"), ["x"]);
}

#[test]
fn negation() {
    let (_d, idx) = corpus();
    assert_eq!(paths(&idx, "تقرير -مستشفى"), ["C:/docs/libraries.txt"]);
    assert!(
        paths(&idx, "-تقرير").is_empty(),
        "negation alone matches nothing"
    );
}

#[test]
fn delete_and_update() {
    let (_d, idx) = corpus();
    idx.delete("C:/docs/contract.docx");
    assert_eq!(paths(&idx, "عقد").len(), 1, "invisible before commit");
    idx.commit().unwrap();
    assert!(paths(&idx, "عقد").is_empty());

    let new_meta = FileMeta { size: 7, mtime: 2 };
    assert!(idx.is_current("C:/docs/hospital.pdf", META).unwrap());
    let key = idx
        .add_or_update("C:/docs/hospital.pdf", new_meta, "نص جديد تماما")
        .unwrap();
    idx.commit().unwrap();
    assert_eq!(key, FileKey::of("C:/docs/hospital.pdf", new_meta));
    assert!(paths(&idx, "مستشفى").is_empty(), "old text is gone");
    let hits = idx.search("جديد", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!((hits[0].key, hits[0].meta), (key, new_meta));
    assert!(idx.is_current("C:/docs/hospital.pdf", new_meta).unwrap());
    assert!(!idx.is_current("C:/docs/hospital.pdf", META).unwrap());
    assert!(!idx.is_current("C:/nope", META).unwrap());
    assert_eq!(idx.num_docs(), 3);
}

#[test]
fn snippet_highlights_point_at_the_diacritized_original() {
    let (_d, idx) = corpus();
    let hits = idx.search("مكتبة تقرير", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let s = &hits[0].snippet;
    let words: Vec<_> = s.original_ranges().map(|r| &LIBRARIES[r]).collect();
    assert_eq!(words, ["الْمَكْتَبَاتُ", "تَقَارِيرَهَا"]);
    assert_eq!(
        &LIBRARIES[s.offset..s.offset + s.fragment.len()],
        s.fragment
    );
}

#[test]
fn snippet_of_a_long_document_is_short_and_centered() {
    let filler = "هذه جملة عادية لا تحتوي على الكلمة. ".repeat(200);
    let text = format!("{filler}وهنا الْكَلِمَةُ الْمَطْلُوبَةُ أخيرا. {filler}");
    let (_d, idx) = index_with(&[("long", &text)]);
    let hit = &idx.search("المطلوبة", 1).unwrap()[0];
    assert!(hit.snippet.fragment.len() <= IndexOptions::default().snippet_bytes);
    let r = hit.snippet.original_ranges().next().unwrap();
    assert_eq!(&text[r], "الْمَطْلُوبَةُ");
}

#[test]
fn match_past_the_stored_prefix_still_matches() {
    let dir = TempDir::new().unwrap();
    let opts = IndexOptions {
        stored_text_limit: 64,
        ..IndexOptions::default()
    };
    let idx = ContentIndex::open_or_create_with(dir.path(), opts).unwrap();
    let text = format!("{} نهاية_المستند", "بداية ".repeat(100));
    idx.add_or_update("p", META, &text).unwrap();
    idx.commit().unwrap();
    let hit = &idx.search("نهاية", 1).unwrap()[0];
    assert!(hit.snippet.highlights.is_empty());
    assert_eq!(hit.snippet.offset, 0);
}

#[test]
fn reopen_keeps_documents() {
    let dir = TempDir::new().unwrap();
    {
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();
        idx.add_or_update("a", META, "المكتبات").unwrap();
        idx.commit().unwrap();
    }
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    assert!(idx.rebuilt().is_none());
    assert_eq!(paths(&idx, "مكتبة"), ["a"]);
}

#[test]
fn second_writer_is_locked() {
    let (dir, _idx) = corpus();
    let err = ContentIndex::open_or_create(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Locked(_)), "{err:?}");
}

#[test]
fn corrupt_index_is_rebuilt() {
    let dir = TempDir::new().unwrap();
    {
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();
        idx.add_or_update("a", META, "نص").unwrap();
        idx.commit().unwrap();
    }
    std::fs::write(dir.path().join("meta.json"), b"{ not json").unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    assert!(
        matches!(idx.rebuilt(), Some(RebuildReason::Corrupt(_))),
        "{:?}",
        idx.rebuilt()
    );
    assert_eq!(idx.num_docs(), 0);
    idx.add_or_update("b", META, "نص").unwrap();
    idx.commit().unwrap();
    assert_eq!(paths(&idx, "نص"), ["b"]);
}

#[test]
fn outdated_analyzer_version_is_rebuilt() {
    let dir = TempDir::new().unwrap();
    drop(ContentIndex::open_or_create(dir.path()).unwrap());
    std::fs::write(
        dir.path().join("athrix-content-index.version"),
        "schema=1 analyzer=0\n",
    )
    .unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    assert!(matches!(
        idx.rebuilt(),
        Some(RebuildReason::Outdated { .. })
    ));
}

#[test]
fn foreign_directory_is_never_wiped() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("precious.txt"), "keep me").unwrap();
    let err = ContentIndex::open_or_create(dir.path()).unwrap_err();
    assert!(matches!(err, Error::NotAnIndex(_)));
    assert!(dir.path().join("precious.txt").exists());
}

#[test]
fn empty_and_unsearchable_queries() {
    let (_d, idx) = corpus();
    assert!(idx.search("", 10).unwrap().is_empty());
    assert!(idx.search("  ، . ", 10).unwrap().is_empty());
    assert!(idx.search("مكتبة", 0).unwrap().is_empty());
}

#[test]
fn index_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ContentIndex>();
}
