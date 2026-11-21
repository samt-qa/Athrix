//! The NTFS adapter must produce the same index (paths, metadata, search results) as a walk of
//! the same tree. A real folder is walked, and the same folder is turned into a synthetic
//! [`VolumeTree`] (records in reverse order, as MFT order allows children before parents), so
//! no privileges are needed and the test runs on every OS.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

use athrix_fs_index::ntfs::{VolumeTree, push_volume_tree};
use athrix_fs_index::{EntryId, FileIndex, IndexBuilder, Query, WalkOptions, walk};
use athrix_ntfs::FILE_ATTRIBUTE_DIRECTORY;
use athrix_ntfs::tree::TreeBuilder;

const ROOT_REF: u64 = (5 << 48) | 5;

fn make_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    for d in [
        "docs/عربي",
        "docs/sub/deep",
        "node_modules/pkg",
        "build/out",
        "keep/.git/objects",
        "Users/me/AppData/Local/Temp/x",
        "Users/me/AppData/Roaming",
        "Windows/WinSxS/amd64",
        "Windows/System32",
        "$RECYCLE.BIN/S-1-5",
    ] {
        fs::create_dir_all(p.join(d)).unwrap();
    }
    for (i, f) in [
        "docs/report.pdf",
        "docs/Report-Final.PDF",
        "docs/عربي/مُحَاضَرَة.docx",
        "docs/عربي/المحاضرة الأولى.pdf",
        "docs/sub/deep/notes.txt",
        "docs/sub/cache.tmp",
        "node_modules/pkg/index.js",
        "build/out/app.exe",
        "keep/readme.md",
        "keep/.git/HEAD",
        "Users/me/AppData/Local/Temp/x/tmp1.pdf",
        "Users/me/AppData/Roaming/settings.json",
        "Windows/WinSxS/amd64/a.dll",
        "Windows/System32/notepad.exe",
        "$RECYCLE.BIN/S-1-5/old report.pdf",
    ]
    .iter()
    .enumerate()
    {
        fs::write(p.join(f), vec![b'x'; i * 7 + 1]).unwrap();
    }
    dir
}

/// Builds the volume tree of `root` the way the enumeration would see it, with metadata.
fn volume_tree(root: &Path) -> VolumeTree {
    // (record, parent ref, attributes, name), collected depth-first.
    let mut recs: Vec<(u64, u64, u32, String)> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), ROOT_REF)];
    let mut next = 100u64;
    while let Some((dir, dir_ref)) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let is_dir = e.file_type().unwrap().is_dir();
            let file_ref = (1 << 48) | next;
            next += 1;
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0x20
            };
            recs.push((
                file_ref,
                dir_ref,
                attrs,
                e.file_name().to_string_lossy().into(),
            ));
            if is_dir {
                stack.push((e.path(), file_ref));
            }
        }
    }
    let mut b = TreeBuilder::new();
    for (file_ref, parent, attrs, name) in recs.iter().rev() {
        b.push(*file_ref, *parent, *attrs, name);
    }
    let mut tree = b.finish(&root.to_string_lossy());
    assert_eq!(tree.orphan_count(), 0);
    for i in 0..tree.len() as u32 {
        let md = fs::symlink_metadata(tree.path(i)).unwrap();
        let mtime = md
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        tree.set_metadata(i, Some(if md.is_dir() { 0 } else { md.len() }), Some(mtime));
    }
    tree.mark_metadata();
    tree
}

fn adapter_index(tree: &VolumeTree, root: &Path, opts: &WalkOptions) -> FileIndex {
    let mut b = IndexBuilder::new();
    push_volume_tree(&mut b, tree, root, opts).unwrap();
    let ix = b.finish();
    for id in 1..ix.len() as EntryId {
        assert!(ix.parent(id).unwrap() < id, "parents precede children");
    }
    ix
}

/// Every entry as `(path, normalized name, is_dir, is_symlink, size, modified)`.
type Row = (String, String, bool, bool, u64, Option<i64>);

fn rows(ix: &FileIndex) -> BTreeSet<Row> {
    (0..ix.len() as EntryId)
        .map(|id| {
            let m = ix.meta(id);
            (
                ix.path(id).to_string_lossy().into_owned(),
                ix.normalized_name(id).to_owned(),
                m.is_dir,
                m.is_symlink,
                m.size,
                m.modified,
            )
        })
        .collect()
}

fn hits(ix: &FileIndex, query: &str) -> BTreeSet<String> {
    ix.search(
        &Query::parse(query).unwrap(),
        10_000,
        &AtomicBool::new(false),
    )
    .unwrap()
    .into_iter()
    .map(|id| ix.path(id).to_string_lossy().into_owned())
    .collect()
}

const QUERIES: &[&str] = &[
    "محاضرة",
    "المحاضره",
    "*.pdf",
    "report",
    "ext:pdf",
    "notes | readme",
    "fuzzy:rprt",
    "\"old report\"",
    "docs",
    "a.dll",
    "settings",
];

fn assert_equivalent(root: &Path, opts: &WalkOptions, tree: &VolumeTree) {
    let walked = walk(&opts.clone()).unwrap();
    let adapted = adapter_index(tree, root, opts);
    assert_eq!(walked.len(), adapted.len());
    assert_eq!(rows(&walked), rows(&adapted));
    for q in QUERIES {
        assert_eq!(hits(&walked, q), hits(&adapted, q), "query {q:?}");
    }
}

#[test]
fn adapter_matches_walk_without_exclusions() {
    let dir = make_tree();
    let tree = volume_tree(dir.path());
    let opts = WalkOptions::new([dir.path()]);
    assert_equivalent(dir.path(), &opts, &tree);
    // Something was found for most queries (the comparison is not vacuous).
    let ix = adapter_index(&tree, dir.path(), &opts);
    assert_eq!(hits(&ix, "محاضرة").len(), 2);
    assert_eq!(hits(&ix, "*.pdf").len(), 5);
}

#[test]
fn adapter_matches_walk_with_the_app_exclusions() {
    let dir = make_tree();
    let tree = volume_tree(dir.path());
    let mut opts = WalkOptions::new([dir.path()]);
    for name in [
        ".git",
        "node_modules",
        "$RECYCLE.BIN",
        "System Volume Information",
        "$WinREAgent",
        "$Windows.~BT",
        "$Windows.~WS",
        "$SysReset",
        "**/AppData/Local/Temp",
        "**/Windows/WinSxS",
        "*.tmp",
    ] {
        opts = opts.exclude_glob(name);
    }
    opts = opts.exclude_prefix(dir.path().join("build").join("out"));
    assert_equivalent(dir.path(), &opts, &tree);

    let ix = adapter_index(&tree, dir.path(), &opts);
    let prefix = dir.path().to_string_lossy().into_owned();
    let all: Vec<String> = rows(&ix)
        .into_iter()
        .map(|r| r.0[prefix.len()..].to_owned())
        .collect();
    for gone in [
        "node_modules",
        ".git",
        "Temp",
        "WinSxS",
        "RECYCLE",
        "cache.tmp",
        "app.exe",
    ] {
        assert!(!all.iter().any(|p| p.contains(gone)), "{gone} in {all:?}");
    }
    for kept in ["Roaming", "System32", "notes.txt", "keep"] {
        assert!(all.iter().any(|p| p.contains(kept)), "{kept} missing");
    }
}
