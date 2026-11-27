//! End-to-end tests of the content index: Arabic and English documents.

use athrix_content_index::{
    ContentIndex, Error, FileKey, FileMeta, IndexOptions, RebuildReason, Snippet,
};
use std::fmt::Write as _;

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

/// The snippet of the document at `path` for `q`.
fn snippet(idx: &ContentIndex, q: &str, path: &str) -> Snippet {
    let mut s = idx.snippets(q, &[path]).unwrap();
    assert_eq!(s.len(), 1);
    s.pop().unwrap().expect("the path is indexed")
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
    let s = snippet(&idx, "report", &reports[0].path);
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
    let s = snippet(&idx, "مكتبة تقرير", &hits[0].path);
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
    // Store more than the default 64 KiB so the scan goes past its first window.
    let dir = TempDir::new().unwrap();
    let opts = IndexOptions {
        stored_text_limit: 512 << 10,
        ..IndexOptions::default()
    };
    let idx = ContentIndex::open_or_create_with(dir.path(), opts).unwrap();
    idx.add_or_update("long", META, &text).unwrap();
    idx.commit().unwrap();
    let hit = &idx.search("المطلوبة", 1).unwrap()[0];
    let s = snippet(&idx, "المطلوبة", &hit.path);
    assert!(s.fragment.len() <= IndexOptions::default().snippet_bytes);
    let r = s.original_ranges().next().unwrap();
    assert_eq!(&text[r], "الْمَطْلُوبَةُ");
}

#[test]
fn snippets_follow_the_order_of_the_paths_and_skip_unknown_ones() {
    let (_d, idx) = corpus();
    let got = idx
        .snippets(
            "تقرير",
            &[
                "C:/docs/hospital.pdf",
                "C:/nope.txt",
                "C:/docs/libraries.txt",
            ],
        )
        .unwrap();
    assert_eq!(got.len(), 3);
    assert!(got[1].is_none());
    let word = |s: &Option<Snippet>, text: &str| {
        let s = s.as_ref().unwrap();
        let r = s.original_ranges().next().unwrap();
        text[r].to_owned()
    };
    assert_eq!(word(&got[0], HOSPITAL), "تقريرًا");
    assert_eq!(word(&got[2], LIBRARIES), "تَقَارِيرَهَا");
    // Paths compare by path key: another case / separator is the same document on Windows.
    if cfg!(windows) {
        let s = idx.snippets("عقد", &["c:\\DOCS\\contract.docx"]).unwrap();
        assert!(s[0].as_ref().is_some_and(|s| !s.highlights.is_empty()));
    }
    assert!(idx.snippets("عقد", &[] as &[&str]).unwrap().is_empty());
}

#[test]
fn snippets_of_many_hits_match_one_at_a_time() {
    // Enough paths to use several threads; each result must be the snippet of its own path.
    let docs: Vec<(String, String)> = (0..40)
        .map(|i| {
            (
                format!("C:/many/{i:02}.txt"),
                format!("رقم {i} ثم تقرير رقم {i}"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = docs.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let (_d, idx) = index_with(&refs);
    let paths: Vec<&str> = docs.iter().map(|(p, _)| p.as_str()).collect();
    let all = idx.snippets("تقرير", &paths).unwrap();
    for ((path, text), s) in docs.iter().zip(&all) {
        let s = s.as_ref().unwrap();
        assert_eq!(*s, snippet(&idx, "تقرير", path));
        assert_eq!(s.fragment, *text);
        assert_eq!(
            s.original_ranges().map(|r| &text[r]).collect::<Vec<_>>(),
            ["تقرير"]
        );
    }
}

#[test]
fn a_long_document_gets_the_same_snippet_as_a_full_scan() {
    // The match sits far past the first scan window (64 KiB) of a long stored text.
    let filler = "هذه جملة عادية لا تحتوي على الكلمة. ".repeat(5000);
    assert!(filler.len() > 200 << 10);
    let text = format!("{filler}وهنا الْكَلِمَةُ الْمَطْلُوبَةُ أخيرا. {filler}");
    // Store more than the default 64 KiB so the scan goes past its first window.
    let dir = TempDir::new().unwrap();
    let opts = IndexOptions {
        stored_text_limit: 512 << 10,
        ..IndexOptions::default()
    };
    let idx = ContentIndex::open_or_create_with(dir.path(), opts).unwrap();
    idx.add_or_update("long", META, &text).unwrap();
    idx.commit().unwrap();
    let hits = idx.search("المطلوبة", 5).unwrap();
    assert_eq!(hits.len(), 1);
    let s = snippet(&idx, "المطلوبة", &hits[0].path);
    let words: Vec<_> = s.original_ranges().map(|r| &text[r]).collect();
    assert_eq!(words, ["الْمَطْلُوبَةُ"]);
    assert_eq!(&text[s.offset..s.offset + s.fragment.len()], s.fragment);
}

#[test]
fn hits_carry_identity_from_the_fast_columns() {
    let (_d, idx) = corpus();
    let meta = FileMeta {
        size: 1234,
        mtime: -5,
    };
    let key = idx
        .add_or_update("C:/docs/Extra Report.md", meta, "report extra")
        .unwrap();
    idx.commit().unwrap();
    let hits = idx.search("extra", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "C:/docs/Extra Report.md");
    assert_eq!((hits[0].key, hits[0].meta), (key, meta));
    assert!(hits[0].score > 0.0);
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
    let s = snippet(&idx, "نهاية", &hit.path);
    assert!(s.highlights.is_empty());
    assert_eq!(s.offset, 0);
}

#[test]
fn text_waiting_for_the_indexing_thread_is_bounded() {
    const LIMIT: usize = 128 << 10;
    let dir = TempDir::new().unwrap();
    let opts = IndexOptions {
        pending_text_limit: LIMIT,
        ..IndexOptions::default()
    };
    let idx = ContentIndex::open_or_create_with(dir.path(), opts).unwrap();
    // 120 documents of about 40 KB, handed over far faster than one thread indexes them.
    let mut most = 0;
    let mut longest = 0;
    for i in 0..120 {
        let mut text = String::new();
        for w in 0..4_000 {
            write!(text, "w{i}x{w} ").unwrap();
        }
        if i == 119 {
            text.push_str("zyzzyva");
        }
        longest = longest.max(text.len() as u64);
        idx.add_or_update(&format!("doc{i}"), META, &text).unwrap();
        most = most.max(idx.pending_text_bytes());
    }
    // Before each add at most LIMIT waits, so after it at most LIMIT and that document.
    assert!(most <= LIMIT as u64 + longest, "{most} bytes waited");
    idx.commit().unwrap();
    assert_eq!(idx.pending_text_bytes(), 0);
    assert_eq!(idx.num_docs(), 120);
    assert_eq!(paths(&idx, "zyzzyva"), ["doc119"]);
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

#[test]
fn a_read_only_open_searches_beside_the_writer_and_refuses_writes() {
    let (dir, writer) = index_with(&[("C:/a/عقد.txt", "عقد الإيجار")]);
    // The writer still holds its lock: a read-only open works anyway.
    let ro = ContentIndex::open_read_only(dir.path()).unwrap();
    assert!(ro.is_read_only() && !writer.is_read_only());
    assert_eq!(ro.search("عقد", 10).unwrap().len(), 1);
    assert!(matches!(
        ro.add_or_update("C:/a/b.txt", META, "x"),
        Err(Error::ReadOnly(_))
    ));
    assert!(matches!(ro.commit(), Err(Error::ReadOnly(_))));
    assert!(matches!(ro.delete_prefix("C:/a"), Err(Error::ReadOnly(_))));
    assert!(matches!(ro.retain(|_, _| false), Err(Error::ReadOnly(_))));
    ro.delete("C:/a/عقد.txt");
    // Later commits of the writer show after opening again.
    writer
        .add_or_update("C:/a/b.txt", META, "عقد جديد")
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(ro.search("عقد", 10).unwrap().len(), 1);
    let again = ContentIndex::open_read_only(dir.path()).unwrap();
    assert_eq!(again.search("عقد", 10).unwrap().len(), 2);
}

#[test]
fn a_read_only_open_never_creates_or_rebuilds() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("nothing");
    assert!(matches!(
        ContentIndex::open_read_only(&missing),
        Err(Error::Io { .. })
    ));
    assert!(!missing.exists(), "nothing created");
    std::fs::write(
        dir.path().join("athrix-content-index.version"),
        "schema=0\n",
    )
    .unwrap();
    assert!(
        ContentIndex::open_read_only(dir.path())
            .unwrap_err()
            .is_corrupt()
    );
    assert!(dir.path().join("athrix-content-index.version").exists());
}
