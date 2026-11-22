//! Property tests of the content index through its public API: arbitrary query text never
//! fails a search, snippets of any query are valid, and a diacritized word is found and
//! highlighted by its bare spelling.
//!
//! Each case that builds an index costs a temp dir and a commit, so those properties run 24
//! cases by default; `PROPTEST_CASES` overrides every count here.

use athrix_content_index::{ContentIndex, FileMeta, Snippet};
use proptest::prelude::*;
use std::sync::OnceLock;
use tempfile::TempDir;

const META: FileMeta = FileMeta { size: 1, mtime: 1 };

const DOCS: &[(&str, &str)] = &[
    (
        "C:/docs/عقد.docx",
        "هذا عَقْدُ الإِيجَارِ بين المؤجر والمستأجر\u{A0}لمدة سنة في مدينة الرياض.",
    ),
    (
        "C:/docs/report.txt",
        "The quarterly Report lists revenue\u{3000}and the annual reports of 2026.",
    ),
    (
        "C:/docs/فاتورة.pdf",
        "فاتورة رقم ١٢٣٤ بتاريخ ٢٠٢٦/٠٣/١٥ — الإجمالي: ٥٠٠ ريال\u{2003}شاملة الضريبة.",
    ),
    (
        "C:/docs/mixed.md",
        "مُحَاضَرَةُ الفِقْهِ: Lecture notes ﷺ «الْمَكْتَبَاتُ» تق\u{AD}رير‌ات",
    ),
    ("C:/docs/empty.txt", ""),
];

fn cases(default: u32) -> u32 {
    if std::env::var_os("PROPTEST_CASES").is_some() {
        ProptestConfig::default().cases
    } else {
        default
    }
}

/// One shared index over [`DOCS`] for the read-only properties.
fn shared() -> &'static ContentIndex {
    static INDEX: OnceLock<(TempDir, ContentIndex)> = OnceLock::new();
    &INDEX
        .get_or_init(|| {
            let dir = TempDir::new().unwrap();
            let idx = ContentIndex::open_or_create(dir.path()).unwrap();
            for (path, text) in DOCS {
                idx.add_or_update(path, META, text).unwrap();
            }
            idx.commit().unwrap();
            (dir, idx)
        })
        .1
}

fn check_snippet(text: &str, s: &Snippet) -> Result<(), TestCaseError> {
    prop_assert!(s.offset + s.fragment.len() <= text.len());
    prop_assert_eq!(
        &text[s.offset..s.offset + s.fragment.len()],
        s.fragment.as_str()
    );
    let mut last = 0;
    for r in &s.highlights {
        prop_assert!(last <= r.start && r.start < r.end && r.end <= s.fragment.len());
        prop_assert!(s.fragment.is_char_boundary(r.start) && s.fragment.is_char_boundary(r.end));
        last = r.end;
    }
    Ok(())
}

/// Query text: content-query syntax (quotes, `-`) mixed with words from the documents.
fn query() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            4 => prop::sample::select(&[
                "عقد", "الايجار", "إيجار", "report", "REPORTS", "2026", "٢٠٢٦", "1234", "فاتوره",
                "المكتبة", "تقرير", "الفقه", "\"", "-", " ", "\u{A0}", "\u{3000}", "ﷺ", "*", "?",
                "\\", ":", "(", ")", "+", "~", "^", "\u{200C}",
            ][..]).prop_map(str::to_owned),
            1 => "\\PC{1,4}",
        ],
        0..8,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(256), ..ProptestConfig::default() })]

    #[test]
    fn search_count_and_snippets_never_fail(q in query()) {
        let idx = shared();
        let hits = idx.search(&q, 10).map_err(|e| TestCaseError::fail(format!("{q:?}: {e}")))?;
        let n = idx.count(&q).map_err(|e| TestCaseError::fail(format!("{q:?}: {e}")))?;
        prop_assert_eq!(n as usize, hits.len(), "{:?}", q);
        let paths: Vec<&str> = DOCS.iter().map(|(p, _)| *p).collect();
        let snippets = idx.snippets(&q, &paths).map_err(|e| TestCaseError::fail(format!("{q:?}: {e}")))?;
        for ((_, text), s) in DOCS.iter().zip(&snippets) {
            check_snippet(text, s.as_ref().expect("indexed"))?;
        }
        // Every hit has a snippet; a hit's snippet highlights something unless the query
        // only matched through stemming variants of excluded words (never: excluded words
        // are not highlighted, but a hit needs a positive clause).
        for h in &hits {
            let i = DOCS.iter().position(|(p, _)| *p == h.path).expect("known path");
            let s = snippets[i].as_ref().expect("indexed");
            prop_assert!(!s.highlights.is_empty(), "{:?} hit {:?} without highlights", q, h.path);
        }
    }

    #[test]
    fn arbitrary_unicode_queries_never_fail(q in "\\PC{0,40}") {
        let idx = shared();
        prop_assert!(idx.search(&q, 5).is_ok());
        prop_assert!(idx.snippets(&q, &["C:/docs/mixed.md", "C:/nope"]).is_ok());
    }
}

/// Arabic letters (without tatweel) and harakat for generated words.
fn arabic_word() -> impl Strategy<Value = (String, String)> {
    prop::collection::vec(
        (
            prop_oneof![(0x0621u32..=0x063A), (0x0641u32..=0x064A)]
                .prop_map(|u| char::from_u32(u).unwrap()),
            prop::option::of(prop::sample::select(
                &[
                    '\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}', '\u{0652}', '\u{0640}',
                ][..],
            )),
        ),
        2..9,
    )
    .prop_map(|v| {
        let bare: String = v.iter().map(|(c, _)| *c).collect();
        let vocalized: String = v
            .iter()
            .flat_map(|(c, m)| std::iter::once(*c).chain(*m))
            .collect();
        (bare, vocalized)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(24), ..ProptestConfig::default() })]

    /// A diacritized word in a document is found by its bare spelling, and the snippet
    /// highlights exactly the diacritized original.
    #[test]
    fn bare_query_finds_and_highlights_the_vocalized_word(
        (bare, vocalized) in arabic_word(),
        before in prop::sample::select(&["", "نص قبل ", "x\u{A0}", "١٢\u{3000}"][..]),
        after in prop::sample::select(&["", " وبعده نص", "،", ".pdf"][..]),
    ) {
        let text = format!("{before}{vocalized}{after}");
        let dir = TempDir::new().unwrap();
        let idx = ContentIndex::open_or_create(dir.path()).unwrap();
        idx.add_or_update("C:/a.txt", META, &text).unwrap();
        idx.commit().unwrap();
        let hits = idx.search(&bare, 5).unwrap();
        prop_assert_eq!(hits.len(), 1, "{:?} not found in {:?}", bare, text);
        let s = idx.snippets(&bare, &["C:/a.txt"]).unwrap().pop().flatten().unwrap();
        check_snippet(&text, &s)?;
        let words: Vec<&str> = s.original_ranges().map(|r| &text[r]).collect();
        let trimmed = vocalized.trim_start_matches('\u{0640}').trim_end_matches('\u{0640}');
        prop_assert!(words.contains(&trimmed), "{:?} highlighted, want {:?}", words, trimmed);
    }
}
