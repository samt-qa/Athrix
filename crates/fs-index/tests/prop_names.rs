//! Property tests of plain name matching through the public API: compiling any query never
//! panics, and a word of a (diacritized, variant-spelled) Arabic name finds the name.
//!
//! Fuzzy matching has its own file (`prop_fuzzy.rs`). Default: 256 cases per property;
//! `PROPTEST_CASES` raises it.

use athrix_fs_index::{EntryId, EntryMeta, IndexBuilder, Query};
use athrix_text_ar::normalize_name;
use proptest::prelude::*;
use std::sync::atomic::AtomicBool;

/// Saturday 2026-11-21 12:00 UTC, UTC+3.
const NOW: i64 = 1_795_262_400;
const OFFSET: i32 = 3 * 3600;

fn compile(q: &str) -> Option<Query> {
    Query::parse_at(q, NOW, OFFSET).ok()
}

/// Arabic letters (no tatweel).
fn letter() -> impl Strategy<Value = char> {
    prop_oneof![(0x0621u32..=0x063A), (0x0641u32..=0x064A)]
        .prop_map(|u| char::from_u32(u).expect("Arabic letter"))
}

/// (bare word, the same word as it may be written in a file name: harakat, tatweel, and the
/// alef / yaa / digit variants that name normalization folds).
fn word() -> impl Strategy<Value = (String, String)> {
    prop::collection::vec(
        (
            letter(),
            prop::option::of(prop::sample::select(
                &[
                    '\u{064E}', '\u{064F}', '\u{0650}', '\u{0651}', '\u{0652}', '\u{0640}',
                ][..],
            )),
            any::<bool>(),
        ),
        2..8,
    )
    .prop_map(|v| {
        let bare: String = v.iter().map(|(c, _, _)| *c).collect();
        let written: String = v
            .iter()
            .flat_map(|(c, m, swap)| {
                // Spelling variants the name index folds (nass Search level).
                let c = match (*c, swap) {
                    ('ا', true) => 'أ',
                    ('ي', true) => 'ى',
                    (c, _) => c,
                };
                std::iter::once(c).chain(*m)
            })
            .collect();
        (bare, written)
    })
}

proptest! {
    #[test]
    fn compiling_any_query_never_panics(q in "\\PC{0,60}") {
        if let Some(q) = compile(&q) {
            let _ = q.matches_name(&normalize_name("تقرير_2026.pdf"));
            let _ = q.fuzzy_highlights("تقرير_2026.pdf");
        }
    }

    #[test]
    fn a_word_of_the_name_matches_it(
        (bare, written) in word(),
        before in prop::sample::select(&["", "مسودة ", "IMG_", "٢٠٢٦-", "(1) "][..]),
        after in prop::sample::select(&["", ".pdf", "_نهائي.docx", " - نسخة", "١"][..]),
    ) {
        let name = format!("{before}{written}{after}");
        let normalized = normalize_name(&name);
        let q = compile(&bare).expect("a bare Arabic word parses");
        prop_assert_eq!(q.matches_name(&normalized), Some(true), "{:?} in {:?} ({:?})", bare, name, normalized);
        // The written (variant, diacritized) spelling finds it too.
        let q = compile(&written).expect("parses");
        prop_assert_eq!(q.matches_name(&normalized), Some(true));
    }

    /// `matches_name` agrees with searching a one-entry index.
    #[test]
    fn matches_name_agrees_with_search(
        (_, written) in word(),
        (bare, _) in word(),
        ext in prop::sample::select(&["", ".pdf", ".DOCX"][..]),
    ) {
        let name = format!("{written}{ext}");
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/r", EntryMeta { is_dir: true, ..Default::default() });
        b.push(Some(root), &name, EntryMeta::default());
        let ix = b.finish();
        let q = compile(&bare).expect("parses");
        let found: Vec<EntryId> = ix.search(&q, 10, &AtomicBool::new(false)).unwrap();
        let by_name = q.matches_name(&normalize_name(&name)).expect("name-only query");
        let root_matches = q.matches_name(&normalize_name("/r")).expect("name-only query");
        prop_assert_eq!(found.len(), usize::from(by_name) + usize::from(root_matches),
            "{:?} vs {:?}: {:?}", bare, name, found);
    }
}
