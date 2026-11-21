//! End-to-end live watching: a real OS watcher feeding a `LiveIndex` over a temp directory.
//!
//! Waits poll the index with a timeout instead of sleeping a fixed time.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use athrix_fs_index::{LiveIndex, Query, WalkOptions, WatchBatch, WatchConfig, Watcher, walk};

const TIMEOUT: Duration = Duration::from_secs(20);

fn config(debounce_ms: u64) -> WatchConfig {
    WatchConfig {
        debounce: Duration::from_millis(debounce_ms),
        max_delay: Duration::from_secs(10),
        ..WatchConfig::default()
    }
}

fn names(live: &LiveIndex, text: &str) -> BTreeSet<String> {
    let snap = live.snapshot();
    snap.search(
        &Query::parse(text).unwrap(),
        usize::MAX,
        &AtomicBool::new(false),
    )
    .unwrap()
    .into_iter()
    .map(|id| snap.name(id).to_owned())
    .collect()
}

fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| (*s).to_owned()).collect()
}

/// Polls until `text` finds exactly `expected`, or panics after [`TIMEOUT`].
fn wait_for(live: &LiveIndex, text: &str, expected: &[&str]) {
    let expected = set(expected);
    let start = Instant::now();
    loop {
        let got = names(live, text);
        if got == expected {
            return;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "`{text}`: expected {expected:?}, still {got:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A live index over `dir` with a watcher applying its batches.
fn live_over(dir: &Path, debounce_ms: u64) -> (Arc<LiveIndex>, Watcher) {
    let opts = WalkOptions::new([dir])
        .exclude_glob("node_modules")
        .exclude_glob("**/AppData/Local/Temp");
    let live = Arc::new(LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap());
    let writer = Arc::clone(&live);
    let rewalk = opts.clone();
    let watcher = Watcher::start(&opts, config(debounce_ms), move |batch| match batch {
        WatchBatch::Changed(paths) => {
            writer.apply_paths(&paths);
        }
        WatchBatch::RescanNeeded(_) => writer.replace_base(walk(&rewalk).unwrap()),
    })
    .unwrap();
    assert_eq!(watcher.watched_roots(), [dir.to_path_buf()]);
    (live, watcher)
}

#[test]
fn create_rename_and_delete_files_and_folders_show_up_in_search() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs").join("old_report.txt"), b"x").unwrap();
    let (live, _watcher) = live_over(root, 100);
    wait_for(&live, "report", &["old_report.txt"]);

    // Files.
    fs::write(root.join("docs").join("تقرير جديد.pdf"), b"x").unwrap();
    wait_for(&live, "تقرير", &["تقرير جديد.pdf"]);
    fs::rename(
        root.join("docs").join("old_report.txt"),
        root.join("docs").join("renamed_report.txt"),
    )
    .unwrap();
    wait_for(&live, "report", &["renamed_report.txt"]);
    fs::remove_file(root.join("docs").join("renamed_report.txt")).unwrap();
    wait_for(&live, "report", &[]);

    // A folder created with contents: everything inside is picked up.
    let project = root.join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src").join("main.rs"), b"fn main() {}").unwrap();
    wait_for(&live, "main", &["main.rs"]);
    wait_for(&live, "infolder:project", &["src", "main.rs"]);

    // A folder renamed: the subtree moves with it.
    fs::rename(&project, root.join("moved")).unwrap();
    wait_for(&live, "infolder:moved", &["src", "main.rs"]);
    wait_for(&live, "project", &[]);
    let snap = live.snapshot();
    let id = snap
        .search(&Query::parse("main").unwrap(), 1, &AtomicBool::new(false))
        .unwrap()[0];
    assert_eq!(
        snap.path(id),
        root.join("moved").join("src").join("main.rs")
    );

    // A folder deleted: the subtree goes.
    fs::remove_dir_all(root.join("moved")).unwrap();
    wait_for(&live, "main | moved | src", &[]);

    // The live result equals a fresh walk.
    let opts = WalkOptions::new([root]);
    let fresh = walk(&opts).unwrap();
    let walked: BTreeSet<String> = (0..fresh.len() as u32)
        .map(|i| fresh.name(i).to_owned())
        .collect();
    assert_eq!(names(&live, ""), walked);
}

#[test]
fn excluded_paths_produce_no_batches() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let opts = WalkOptions::new([&root]).exclude_glob("node_modules");
    let batches = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let (tx, rx) = mpsc::channel::<()>();
    let sink = Arc::clone(&batches);
    let marker = root.join("marker.txt");
    let marker_seen = marker.clone();
    let _watcher = Watcher::start(&opts, config(100), move |batch| {
        if let WatchBatch::Changed(paths) = batch {
            let done = paths.contains(&marker_seen);
            sink.lock().unwrap().extend(paths);
            if done {
                let _ = tx.send(());
            }
        }
    })
    .unwrap();
    let nm = root.join("node_modules").join("pkg");
    fs::create_dir_all(&nm).unwrap();
    for i in 0..50 {
        fs::write(nm.join(format!("f{i}.js")), b"x").unwrap();
    }
    // Something not excluded, written last, ends the test once it is seen.
    fs::write(&marker, b"x").unwrap();
    rx.recv_timeout(TIMEOUT).expect("marker never reported");
    let seen = batches.lock().unwrap();
    assert!(
        seen.iter()
            .all(|p| !p.components().any(|c| c.as_os_str() == "node_modules")),
        "{seen:?}"
    );
}

#[test]
fn a_burst_of_changes_is_delivered_as_one_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::create_dir_all(root.join("burst")).unwrap();
    let opts = WalkOptions::new([&root]);
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    let _watcher = Watcher::start(&opts, config(500), move |batch| {
        if let WatchBatch::Changed(paths) = batch {
            let _ = tx.send(paths);
        }
    })
    .unwrap();
    // Like a checkout: many creates, rewrites and deletes in quick succession.
    for i in 0..200 {
        fs::write(root.join("burst").join(format!("file_{i}.txt")), b"one").unwrap();
    }
    for i in 0..200 {
        fs::write(root.join("burst").join(format!("file_{i}.txt")), b"two").unwrap();
    }
    for i in 0..100 {
        fs::remove_file(root.join("burst").join(format!("file_{i}.txt"))).unwrap();
    }
    let first = rx.recv_timeout(TIMEOUT).expect("no batch");
    let files = first
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .count();
    assert_eq!(files, 200, "every file once: {first:?}");
    // Nothing else trails behind the burst.
    assert!(rx.recv_timeout(Duration::from_millis(1500)).is_err());
}

#[test]
fn stopping_the_watcher_stops_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (live, watcher) = live_over(root, 50);
    fs::write(root.join("before.txt"), b"x").unwrap();
    wait_for(&live, "before", &["before.txt"]);
    watcher.stop();
    let generation = live.generation();
    fs::write(root.join("after.txt"), b"x").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(names(&live, "after").is_empty());
    assert_eq!(live.generation(), generation);
}
