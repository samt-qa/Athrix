//! Property tests of fuzzy name matching (`fuzzy:`) through the public API: highlights are
//! valid byte ranges of the displayed name, and an exact substring always matches.
//!
//! Kept apart from `prop_names.rs` because the fuzzy matcher changes often. Default: 256
//! cases per property; `PROPTEST_CASES` raises it.

use athrix_fs_index::{EntryMeta, IndexBuilder, Query};
use proptest::prelude::*;
use std::sync::atomic::AtomicBool;

const NOW: i64 = 1_795_262_400;

fn compile(q: &str) -> Option<Query> {
    Query::parse_at(q, NOW, 0).ok()
}

/// File-name-like text: Arabic (with harakat and tatweel), Latin of both cases, digits,
/// separators, multi-byte spaces and the odd arbitrary character.
fn name() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => prop::sample::select(&[
                'a', 'b', 'r', 'e', 'p', 'o', 't', 'M', 'R', 'S', 'ا', 'أ', 'ل', 'ت', 'ق', 'ر', 'ي',
                'ى', 'ة', 'ه', '\u{064E}', '\u{0651}', 'ـ', '_', '-', '.', ' ', '\u{A0}', '\u{3000}',
                '٢', '0', '😀', 'İ', 'ß',
            ][..]),
            1 => any::<char>(),
        ],
        0..40,
    )
    .prop_map(|v| v.into_iter().collect())
}

fn fuzzy_word() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9اأتقريىةهل]{1,8}"
}

fn check_ranges(name: &str, ranges: &[std::ops::Range<usize>]) -> Result<(), TestCaseError> {
    let mut last_end: Option<usize> = None;
    for r in ranges {
        prop_assert!(r.start < r.end, "empty range {r:?} in {name:?}");
        prop_assert!(r.end <= name.len(), "{r:?} past the end of {name:?}");
        prop_assert!(
            name.is_char_boundary(r.start) && name.is_char_boundary(r.end),
            "{r:?} not on char boundaries of {name:?}"
        );
        if let Some(e) = last_end {
            prop_assert!(r.start > e, "ranges not sorted and merged: {ranges:?}");
        }
        last_end = Some(r.end);
    }
    Ok(())
}

proptest! {
    #[test]
    fn fuzzy_highlights_are_valid_ranges(name in name(), w in fuzzy_word(), w2 in fuzzy_word(), or: bool) {
        let text = if or { format!("fuzzy:{w} | fuzzy:{w2}") } else { format!("fuzzy:{w}") };
        let q = compile(&text).expect("fuzzy query parses");
        check_ranges(&name, &q.fuzzy_highlights(&name))?;
        let switch = compile(&format!("fuzzy: {w} {w2}")).expect("switch parses");
        check_ranges(&name, &switch.fuzzy_highlights(&name))?;
    }

    #[test]
    fn fuzzy_highlights_on_arbitrary_unicode(name in "\\PC{0,50}", w in "\\PC{1,8}") {
        if let Some(q) = compile(&format!("fuzzy:\"{}\"", w.replace('"', ""))) {
            check_ranges(&name, &q.fuzzy_highlights(&name))?;
        }
    }

    /// A word that occurs literally in a name is a fuzzy match of it, and is highlighted.
    #[test]
    fn an_exact_substring_matches_fuzzily(
        prefix in "[a-z ]{0,8}",
        w in "[a-z]{2,8}",
        suffix in "[a-z .]{0,8}",
    ) {
        let name = format!("{prefix}{w}{suffix}");
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/r", EntryMeta { is_dir: true, ..Default::default() });
        let id = b.push(Some(root), &name, EntryMeta::default());
        let ix = b.finish();
        let q = compile(&format!("fuzzy:{w}")).expect("parses");
        prop_assert!(ix.is_match(&q, id), "fuzzy:{} does not match {:?}", w, name);
        let found = ix.search(&q, 10, &AtomicBool::new(false)).unwrap();
        prop_assert!(found.contains(&id));
        prop_assert!(ix.score(&q, id).is_some());
        let hl = q.fuzzy_highlights(&name);
        check_ranges(&name, &hl)?;
        prop_assert!(!hl.is_empty(), "no highlight of {:?} in {:?}", w, name);
    }
}
