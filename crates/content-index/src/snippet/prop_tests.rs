//! Property tests of the snippet builder on arbitrary text, terms and sizes: it never panics,
//! the fragment is a slice of the text, and highlights are sorted, inside the fragment, on
//! char boundaries and on words that really match.
//!
//! Default: 256 cases per property (fewer for the 64 KiB-window one). Raise with
//! `PROPTEST_CASES`.

use super::{FIRST_WINDOW, Snippet, make};
use athrix_text_ar::{Analyzer, tokenize};
use proptest::prelude::*;
use std::collections::BTreeSet;

/// Pieces of text: Arabic words (plain and diacritized), Latin, digits, punctuation, and
/// every kind of whitespace, including multi-byte ones (U+00A0 2 bytes, U+2003 / U+3000 3).
const PIECES: &[&str] = &[
    "عقد",
    "الْعَقْدِ",
    "الإيجار",
    "ايجار",
    "تقرير",
    "التَّقَارِيرَ",
    "فاتورة",
    "وَالْمَكْتَبَاتُ",
    "مكتبة",
    "Report",
    "report",
    "reports",
    "2026",
    "٢٠٢٦",
    "ﷺ",
    "\u{FEFB}",
    "ـــ",
    "\u{200C}",
    "\u{FEFF}",
    "،",
    ".",
    "-",
    "_",
    "😀",
    "中文",
    "É",
    " ",
    " ",
    " ",
    "\n",
    "\t",
    "\u{A0}",
    "\u{2003}",
    "\u{3000}",
    "\u{2028}",
    "\u{85}",
];

fn text(max_pieces: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            5 => prop::sample::select(PIECES).prop_map(str::to_owned),
            1 => any::<char>().prop_map(String::from),
        ],
        0..max_pieces,
    )
    .prop_map(|v| v.concat())
}

/// Query terms as the content query builds them: analyzed words.
fn terms() -> impl Strategy<Value = BTreeSet<String>> {
    prop::collection::vec(
        prop_oneof![
            prop::sample::select(PIECES).prop_map(str::to_owned),
            "\\PC{1,4}",
        ],
        0..4,
    )
    .prop_map(|words| {
        words
            .iter()
            .flat_map(|w| tokenize(w).map(|t| t.text).collect::<Vec<_>>())
            .collect()
    })
}

/// Checks every documented guarantee of a snippet.
fn check(
    text: &str,
    terms: &BTreeSet<String>,
    max: usize,
    s: &Snippet,
) -> Result<(), TestCaseError> {
    prop_assert!(s.offset + s.fragment.len() <= text.len());
    prop_assert!(text.is_char_boundary(s.offset));
    prop_assert_eq!(
        &text[s.offset..s.offset + s.fragment.len()],
        s.fragment.as_str()
    );
    let mut last_end = 0;
    for r in &s.highlights {
        prop_assert!(r.start < r.end, "empty highlight {r:?}");
        prop_assert!(
            r.start >= last_end,
            "highlights overlap or are unsorted: {:?}",
            s.highlights
        );
        prop_assert!(r.end <= s.fragment.len(), "{r:?} past the fragment");
        prop_assert!(s.fragment.is_char_boundary(r.start) && s.fragment.is_char_boundary(r.end));
        last_end = r.end;
        // The highlighted word analyzes to one of the query terms.
        let word = &s.fragment[r.clone()];
        prop_assert!(
            tokenize(word).any(|t| terms.contains(&t.text)),
            "{word:?} highlighted for {terms:?}"
        );
    }
    for (r, o) in s.highlights.iter().zip(s.original_ranges()) {
        prop_assert_eq!(&s.fragment[r.clone()], &text[o]);
    }
    // At most `max` bytes, unless the matched cluster alone is longer (one overlong word).
    let extent = match (s.highlights.first(), s.highlights.last()) {
        (Some(a), Some(b)) => b.end - a.start,
        _ => 0,
    };
    prop_assert!(
        s.fragment.len() <= max.max(extent),
        "{} > {max}",
        s.fragment.len()
    );
    Ok(())
}

proptest! {
    #[test]
    fn snippets_are_valid_for_any_text(
        text in text(80),
        terms in terms(),
        max in 0usize..200,
        stem: bool,
    ) {
        let analyzer = Analyzer { stem, ..Analyzer::default() };
        let s = make(&text, &terms, analyzer, max);
        check(&text, &terms, max, &s)?;
    }

    /// When the text holds a query term, the snippet highlights at least one word.
    #[test]
    fn a_present_term_is_highlighted(text in text(60), max in 1usize..300) {
        let Some(t) = tokenize(&text).next() else { return Ok(()); };
        let terms: BTreeSet<String> = [t.text.clone()].into();
        let s = make(&text, &terms, Analyzer::default(), max);
        check(&text, &terms, max, &s)?;
        prop_assert!(!s.highlights.is_empty(), "{:?} not highlighted in {:?}", t.text, text);
    }

    #[test]
    fn snippets_on_arbitrary_unicode(text in "\\PC{0,200}", q in "\\PC{0,12}", max in 0usize..120) {
        let terms: BTreeSet<String> = tokenize(&q).map(|t| t.text).collect();
        let s = make(&text, &terms, Analyzer::default(), max);
        check(&text, &terms, max, &s)?;
    }
}

proptest! {
    // Each case tokenizes 64 KiB: 24 cases by default, `PROPTEST_CASES` overrides.
    #![proptest_config(ProptestConfig {
        cases: if std::env::var_os("PROPTEST_CASES").is_some() {
            ProptestConfig::default().cases
        } else {
            24
        },
        ..ProptestConfig::default()
    })]

    /// Matches past the first 64 KiB window, with multi-byte whitespace and words right at
    /// the window edge, are still found and sliced correctly.
    #[test]
    fn matches_across_the_first_window(
        filler in prop::sample::select(&["س ", "ab\u{A0}", "x\u{3000}", "مكتب\u{2003}", "y"][..]),
        shift in 0usize..12,
        word in prop::sample::select(&["الْعَقْدِ", "Report", "٢٠٢٦"][..]),
        sep in prop::sample::select(&[" ", "\u{A0}", "\u{3000}", ""][..]),
        max in 1usize..200,
    ) {
        let reps = (FIRST_WINDOW - shift) / filler.len() + 1;
        let text = format!("{}{sep}{word} end", filler.repeat(reps));
        let terms: BTreeSet<String> = tokenize(word).map(|t| t.text).collect();
        let s = make(&text, &terms, Analyzer::default(), max);
        check(&text, &terms, max, &s)?;
        if !sep.is_empty() && !filler.ends_with('y') {
            prop_assert!(!s.highlights.is_empty(), "{word:?} after {} bytes", text.len() - word.len() - 4);
        }
    }
}
