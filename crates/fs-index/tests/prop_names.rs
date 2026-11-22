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

/// Names drawn from a small pool, so most entries share their name with others (as on a real
/// disk), with folders and files of every kind, sizes and both times.
fn shared_names_index(seed: u64, entries: usize) -> athrix_fs_index::FileIndex {
    const POOL: &[&str] = &[
        "index.js",
        "Index.JS",
        "report.pdf",
        "Report 2026.PDF",
        "تقرير_مالي.docx",
        "التقرير السنوي.xlsx",
        "cover.jpg",
        "META-INF",
        "MonthlyReportSummary.txt",
        "notes",
        "README.md",
        "فاتورة.pdf",
        "clip.mp4",
        "archive.tar.gz",
        "",
    ];
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut b = IndexBuilder::new();
    let root = b.push(
        None,
        "/r",
        EntryMeta {
            is_dir: true,
            ..Default::default()
        },
    );
    let mut dirs = vec![root];
    for i in 0..entries {
        let r = next();
        let parent = dirs[(r as usize >> 8) % dirs.len()];
        let name = if r % 7 == 0 {
            format!("{} {}", POOL[(r >> 20) as usize % POOL.len()], i % 50)
        } else {
            POOL[(r >> 20) as usize % POOL.len()].to_owned()
        };
        let is_dir = r % 5 == 0;
        let t = |k: u64| (r >> k) % 3 != 0;
        let id = b.push(
            Some(parent),
            &name,
            EntryMeta {
                is_dir,
                size: if is_dir { 0 } else { (r >> 33) % (4 << 20) },
                modified: t(40).then_some(NOW - ((r >> 41) % 800) as i64 * 86_400),
                created: t(50).then_some(NOW - ((r >> 51) % 800) as i64 * 86_400),
                ..Default::default()
            },
        );
        if is_dir {
            dirs.push(id);
        }
    }
    b.finish()
}

const SHARED_QUERIES: &[&str] = &[
    "report",
    "index",
    "تقرير",
    "*.pdf",
    "ext:pdf",
    "ext:pdf;docx size:>1mb",
    "type:image",
    "type:folder|image",
    "type:folder",
    "case:Report",
    "ww:notes",
    "report !ext:pdf",
    "notes | size:<1kb",
    "dc:lastmonth",
    "dc:thisyear dm:<2026-06",
    "parent:notes report",
    "index dc:>2026-01",
    "fuzzy:rprt",
    "fuzzy:mrs",
    "fuzzy: report 2026",
    "fuzzy:rprt ext:pdf",
    "fuzzy:rprt size:>1kb",
    "fuzzy:idx | notes",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Sharing name-only work between entries of the same name (the per-search memo) gives
    /// exactly the per-entry results, for every limit, ranked or not, and across many
    /// searches on one index (memo reuse and epoch wrap-around).
    #[test]
    fn shared_name_work_equals_per_entry_evaluation(seed in any::<u64>(), big in any::<bool>()) {
        let ix = shared_names_index(seed, if big { 40_000 } else { 3_000 });
        prop_assert!(ix.distinct_names() < ix.len() / 4);
        let no = AtomicBool::new(false);
        for round in 0..3 {
            for text in SHARED_QUERIES {
                let q = compile(text).expect("parses");
                let ids = || 0..ix.len() as EntryId;
                let expected: Vec<EntryId> = if q.is_ranked() {
                    let mut all: Vec<(u32, EntryId)> = ids()
                        .filter_map(|id| ix.score(&q, id).map(|s| (s, id)))
                        .collect();
                    all.sort_unstable_by_key(|&(s, id)| (std::cmp::Reverse(s), id));
                    all.into_iter().map(|(_, id)| id).collect()
                } else {
                    ids().filter(|&id| ix.is_match(&q, id)).collect()
                };
                for limit in [1, 7, 200, usize::MAX] {
                    let got = ix.search(&q, limit, &no).unwrap();
                    let want = &expected[..limit.min(expected.len())];
                    prop_assert_eq!(&got[..], want, "{} limit {} round {}", text, limit, round);
                }
                let unordered = ix.search(&q.unordered(), usize::MAX, &no).unwrap();
                let mut sorted = unordered.clone();
                sorted.sort_unstable();
                let mut want = expected.clone();
                want.sort_unstable();
                prop_assert_eq!(sorted, want, "{} unordered", text);
            }
        }
    }
}
