//! Listing, sweeping, prefix deletes, scoped search, paging and counts.

use athrix_content_index::{
    ContentIndex, FileMeta, PathFilter, RebuildReason, SCHEMA_VERSION, SearchOptions,
};
use std::collections::{BTreeMap, HashSet};
use tempfile::TempDir;

fn meta(i: i64) -> FileMeta {
    FileMeta {
        size: 10 + i as u64,
        mtime: i,
    }
}

const DOCS: &[&str] = &[
    "C:/docs/contract.txt",
    "C:/docs/sub/report.txt",
    "C:/docs2/other.txt",
    "C:/مستندات/عقد الإيجار.txt",
    "C:/مستندات/تقارير/تقرير ٢٠٢٦.docx",
    "D:/data/x.md",
];

/// Every doc contains «عقد» and «report»; each commit after two documents makes several
/// segments.
fn corpus() -> (TempDir, ContentIndex) {
    let dir = TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    for (i, p) in DOCS.iter().enumerate() {
        idx.add_or_update(
            p,
            meta(i as i64),
            &format!("عقد report {}", "كلمة ".repeat(i)),
        )
        .unwrap();
        if i % 2 == 1 {
            idx.commit().unwrap();
        }
    }
    idx.commit().unwrap();
    (dir, idx)
}

fn listed(idx: &ContentIndex) -> BTreeMap<String, FileMeta> {
    let mut out = BTreeMap::new();
    let n = idx
        .paths(|p, m| {
            assert!(out.insert(p.to_owned(), m).is_none(), "{p} listed twice");
        })
        .unwrap();
    assert_eq!(n as usize, out.len());
    out
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn hit_paths(hits: Vec<athrix_content_index::Hit>) -> Vec<String> {
    sorted(hits.into_iter().map(|h| h.path).collect())
}

#[test]
fn paths_lists_every_committed_document_with_its_meta() {
    let (_d, idx) = corpus();
    let all = listed(&idx);
    assert_eq!(all.len(), DOCS.len());
    for (i, p) in DOCS.iter().enumerate() {
        assert_eq!(all[*p], meta(i as i64), "{p}");
    }
    // Updates and deletes: one entry per live document, the latest meta.
    idx.add_or_update(DOCS[0], meta(99), "جديد").unwrap();
    idx.delete(DOCS[3]);
    assert_eq!(
        listed(&idx).len(),
        DOCS.len(),
        "uncommitted changes are not seen"
    );
    idx.commit().unwrap();
    let all = listed(&idx);
    assert_eq!(all.len(), DOCS.len() - 1);
    assert_eq!(all[DOCS[0]], meta(99));
    assert!(!all.contains_key(DOCS[3]));
}

#[test]
fn paths_on_an_empty_index() {
    let dir = TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    assert_eq!(idx.paths(|_, _| panic!("no documents")).unwrap(), 0);
}

#[test]
fn retain_drops_files_the_name_index_no_longer_has() {
    let (_d, idx) = corpus();
    let alive: HashSet<&str> = [DOCS[0], DOCS[3], DOCS[5]].into_iter().collect();
    let removed = idx.retain(|p, _| alive.contains(p)).unwrap();
    assert_eq!(removed, 3);
    idx.commit().unwrap();
    let left: Vec<String> = listed(&idx).into_keys().collect();
    assert_eq!(left, sorted(alive.iter().map(|s| s.to_string()).collect()));
    assert_eq!(idx.num_docs(), 3);
    // Meta is available to the predicate (e.g. drop files whose size changed).
    assert_eq!(idx.retain(|_, m| m.mtime != 5).unwrap(), 1);
    idx.commit().unwrap();
    assert_eq!(idx.num_docs(), 2);
}

#[test]
fn retain_handles_more_than_one_batch() {
    let dir = TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    for i in 0..10_000 {
        idx.add_or_update(&format!("C:/many/{i:05}.txt"), meta(i), "نص")
            .unwrap();
    }
    idx.commit().unwrap();
    let removed = idx
        .retain(|p, _| p.ends_with("0.txt")) // keep 1 in 10
        .unwrap();
    assert_eq!(removed, 9_000);
    idx.commit().unwrap();
    assert_eq!(idx.num_docs(), 1_000);
}

#[test]
fn delete_prefix_respects_folder_boundaries() {
    let (_d, idx) = corpus();
    assert_eq!(idx.delete_prefix("C:/docs").unwrap(), 2);
    idx.commit().unwrap();
    let left = listed(&idx);
    assert!(
        left.contains_key("C:/docs2/other.txt"),
        "C:/docs2 is not under C:/docs"
    );
    assert!(!left.contains_key("C:/docs/contract.txt"));
    assert!(!left.contains_key("C:/docs/sub/report.txt"));

    // Arabic folder, with a trailing separator.
    assert_eq!(idx.delete_prefix("C:/مستندات/تقارير/").unwrap(), 1);
    idx.commit().unwrap();
    assert!(listed(&idx).contains_key("C:/مستندات/عقد الإيجار.txt"));

    // A prefix that is a file path deletes that file.
    assert_eq!(idx.delete_prefix("C:/مستندات/عقد الإيجار.txt").unwrap(), 1);
    // Empty prefix: nothing.
    assert_eq!(idx.delete_prefix("").unwrap(), 0);
    idx.commit().unwrap();
    assert_eq!(listed(&idx).len(), 2);

    // A drive root.
    assert_eq!(idx.delete_prefix("C:/").unwrap(), 1);
    idx.commit().unwrap();
    assert_eq!(
        listed(&idx).into_keys().collect::<Vec<_>>(),
        ["D:/data/x.md"]
    );
}

#[test]
fn delete_prefix_also_removes_uncommitted_documents() {
    let (_d, idx) = corpus();
    idx.add_or_update("C:/docs/new.txt", meta(7), "عقد")
        .unwrap();
    assert_eq!(idx.delete_prefix("C:/docs").unwrap(), 2, "committed ones");
    idx.commit().unwrap();
    assert!(
        idx.search("عقد", 50)
            .unwrap()
            .iter()
            .all(|h| !h.path.starts_with("C:/docs/"))
    );
}

#[test]
fn search_within_a_set_of_paths() {
    let (_d, idx) = corpus();
    let filter = PathFilter::paths([DOCS[1], DOCS[4], "C:/not/indexed.txt"]);
    let hits = idx.search_within("عقد", 50, &filter).unwrap();
    assert_eq!(
        hit_paths(hits.clone()),
        sorted(vec![DOCS[1].into(), DOCS[4].into()])
    );
    // The scope does not change scores.
    let all = idx.search("عقد", 50).unwrap();
    for h in &hits {
        let same = all.iter().find(|a| a.path == h.path).unwrap();
        assert_eq!(same.score, h.score);
    }
    assert_eq!(idx.count_within("عقد", &filter).unwrap(), 2);
    assert_eq!(idx.count_within("غيرموجود", &filter).unwrap(), 0);
    let empty = PathFilter::paths(Vec::<&str>::new());
    assert!(idx.search_within("عقد", 50, &empty).unwrap().is_empty());
    assert_eq!(idx.count_within("عقد", &empty).unwrap(), 0);
}

#[test]
fn search_within_folders() {
    let (_d, idx) = corpus();
    let docs = PathFilter::prefix("C:/docs");
    assert_eq!(
        hit_paths(idx.search_within("report", 50, &docs).unwrap()),
        [DOCS[0], DOCS[1]]
    );
    let arabic = PathFilter::prefix("C:/مستندات");
    assert_eq!(idx.count_within("عقد", &arabic).unwrap(), 2);
    let drives = PathFilter::prefixes(["D:/", "C:/docs2"]);
    assert_eq!(
        hit_paths(idx.search_within("عقد", 50, &drives).unwrap()),
        [DOCS[2], DOCS[5]]
    );
    assert!(PathFilter::prefix("").is_empty());
}

#[test]
fn paging_and_totals() {
    let (_d, idx) = corpus();
    let all = idx.search("عقد", 50).unwrap();
    assert_eq!(all.len(), DOCS.len());
    let mut paged = Vec::new();
    for page in 0..3 {
        let opts = SearchOptions {
            limit: 2,
            offset: page * 2,
            count_total: true,
            ..SearchOptions::default()
        };
        let p = idx.search_with("عقد", &opts).unwrap();
        assert_eq!(p.total, Some(DOCS.len() as u64));
        assert_eq!(p.hits.len(), 2);
        paged.extend(p.hits);
    }
    assert_eq!(paged, all);
    let past_end = SearchOptions {
        limit: 10,
        offset: 100,
        ..SearchOptions::default()
    };
    let p = idx.search_with("عقد", &past_end).unwrap();
    assert!(p.hits.is_empty());
    assert_eq!(p.total, None);

    assert_eq!(idx.count("عقد").unwrap(), DOCS.len() as u64);
    assert_eq!(idx.count("كلمة").unwrap(), DOCS.len() as u64 - 1);
    assert_eq!(idx.count("عقد -كلمة").unwrap(), 1);
    assert_eq!(idx.count("").unwrap(), 0);

    // Scoped page with total.
    let scope = PathFilter::prefix("C:/");
    let opts = SearchOptions {
        limit: 1,
        offset: 1,
        filter: Some(&scope),
        count_total: true,
    };
    let p = idx.search_with("report", &opts).unwrap();
    assert_eq!(p.total, Some(5));
    assert_eq!(p.hits.len(), 1);
}

#[test]
fn schema_1_index_is_rebuilt() {
    assert_eq!(SCHEMA_VERSION, 3);
    let dir = TempDir::new().unwrap();
    drop(ContentIndex::open_or_create(dir.path()).unwrap());
    std::fs::write(
        dir.path().join("athrix-content-index.version"),
        format!("schema=1 analyzer={}\n", athrix_text_ar::ANALYZER_VERSION),
    )
    .unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    assert!(matches!(
        idx.rebuilt(),
        Some(RebuildReason::Outdated { .. })
    ));
}

#[cfg(windows)]
mod windows {
    use super::*;

    #[test]
    fn identity_ignores_case_and_separator() {
        let (_d, idx) = corpus();
        // Re-adding with another case / separator replaces the document.
        idx.add_or_update(r"c:\DOCS\Contract.TXT", meta(50), "عقد جديد")
            .unwrap();
        idx.commit().unwrap();
        let all = listed(&idx);
        assert_eq!(all.len(), DOCS.len());
        assert_eq!(
            all[r"c:\DOCS\Contract.TXT"],
            meta(50),
            "keeps the last form"
        );
        assert!(idx.is_current(r"c:\DOCS\Contract.TXT", meta(50)).unwrap());
        // The key hashes the exact path: another spelling is found but not current, so the
        // indexer re-extracts once and the stored form follows the name index.
        assert!(!idx.is_current("C:/docs/contract.txt", meta(50)).unwrap());
        idx.delete("C:/DOCS/CONTRACT.txt");
        idx.commit().unwrap();
        assert_eq!(idx.num_docs() as usize, DOCS.len() - 1);
    }

    #[test]
    fn prefixes_and_sets_ignore_case_and_separator() {
        let (_d, idx) = corpus();
        let filter = PathFilter::paths([r"C:\DOCS\SUB\REPORT.TXT", r"c:\مستندات\عقد الإيجار.TXT"]);
        assert_eq!(idx.count_within("عقد", &filter).unwrap(), 2);
        assert_eq!(
            idx.count_within("عقد", &PathFilter::prefix(r"c:\Docs"))
                .unwrap(),
            2
        );
        assert_eq!(idx.delete_prefix(r"c:\DOCS\").unwrap(), 2);
        assert_eq!(idx.delete_prefix(r"C:\مستندات").unwrap(), 2);
        idx.commit().unwrap();
        assert_eq!(idx.num_docs(), 2);
    }
}

#[cfg(not(windows))]
mod portable {
    use super::*;

    #[test]
    fn identity_is_byte_exact() {
        let (_d, idx) = corpus();
        idx.add_or_update("C:/DOCS/contract.txt", meta(50), "عقد")
            .unwrap();
        idx.commit().unwrap();
        assert_eq!(listed(&idx).len(), DOCS.len() + 1, "another file");
        idx.delete("c:/docs/contract.txt");
        idx.commit().unwrap();
        assert_eq!(idx.num_docs() as usize, DOCS.len() + 1, "no such file");
    }

    #[test]
    fn prefixes_and_sets_are_case_sensitive_and_backslash_is_a_character() {
        let (_d, idx) = corpus();
        assert_eq!(
            idx.count_within("عقد", &PathFilter::prefix("C:/DOCS"))
                .unwrap(),
            0
        );
        assert_eq!(
            idx.count_within("عقد", &PathFilter::prefix(r"C:\docs"))
                .unwrap(),
            0
        );
        assert_eq!(
            idx.count_within("عقد", &PathFilter::paths(["c:/docs/contract.txt"]))
                .unwrap(),
            0
        );
        assert_eq!(idx.delete_prefix("C:/Docs").unwrap(), 0);
        idx.add_or_update(r"/home/a\b.txt", meta(1), "عقد").unwrap();
        idx.commit().unwrap();
        assert_eq!(
            idx.count_within("عقد", &PathFilter::prefix("/home/a"))
                .unwrap(),
            0
        );
        assert_eq!(
            idx.count_within("عقد", &PathFilter::prefix("/home"))
                .unwrap(),
            1
        );
    }
}
