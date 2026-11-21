//! End-to-end tests of the content scheduler with real files, a real content index and the
//! real name-index walk.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use athrix_content_index::ContentIndex;
use athrix_daemon::{
    CandidateFilter, ContentScheduler, ExtractFailure, Extractor, FailureKind, InProcessExtractor,
    NullProbe, Politeness, Progress, RunState, SchedulerConfig, SystemProbe, SystemState,
};
use athrix_extract::Kind;
use athrix_fs_index::{IndexSnapshot, LiveIndex, WalkOptions, walk};
use tempfile::TempDir;

const LONG: Duration = Duration::from_secs(60);

/// A temp dir without a leading dot in its name (the default filter skips dot-folders).
fn tmp() -> TempDir {
    tempfile::Builder::new()
        .prefix("athrix-daemon-")
        .tempdir()
        .unwrap()
}

/// Fast, deterministic settings. The temp dir lives under `AppData` on Windows, so the test
/// filter keeps only `node_modules` as an excluded folder name.
fn config() -> SchedulerConfig {
    SchedulerConfig {
        workers: 2,
        commit_every: 100,
        commit_interval: Duration::from_millis(200),
        filter: CandidateFilter {
            excluded_dir_names: vec!["node_modules".into()],
            excluded_root_dirs: Vec::new(),
            ..CandidateFilter::default()
        },
        priority_folders: Vec::new(),
        politeness: Politeness {
            probe_interval: Duration::from_millis(10),
            busy_samples: 1,
            ..Politeness::default()
        },
        progress_interval: Duration::from_millis(20),
        low_priority: false,
        ..SchedulerConfig::default()
    }
}

fn open_index() -> (TempDir, Arc<ContentIndex>) {
    let dir = tmp();
    let index = Arc::new(ContentIndex::open_or_create(dir.path().join("content")).unwrap());
    (dir, index)
}

fn start(index: &Arc<ContentIndex>, config: SchedulerConfig) -> ContentScheduler {
    ContentScheduler::builder(Arc::clone(index))
        .config(config)
        .probe(NullProbe)
        .start()
        .unwrap()
}

fn snapshot(root: &Path) -> Arc<IndexSnapshot> {
    let opts = WalkOptions::new([root]);
    let base = walk(&opts).unwrap();
    LiveIndex::new(base, &opts).unwrap().snapshot()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + LONG;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn hits(index: &ContentIndex, query: &str) -> Vec<String> {
    let mut v: Vec<String> = index
        .search(query, 100)
        .unwrap()
        .into_iter()
        .map(|h| h.path)
        .collect();
    v.sort();
    v
}

fn key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn docx(text: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p><w:sectPr/></w:body></w:document>"#
    );
    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
    let types = r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#;
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in [
        ("[Content_Types].xml", types),
        ("_rels/.rels", rels),
        ("word/document.xml", document.as_str()),
    ] {
        w.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(data.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn write(path: &Path, data: impl AsRef<[u8]>) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, data).unwrap();
}

#[test]
fn initial_scan_indexes_documents_and_search_finds_them() {
    let docs = tmp();
    let root = docs.path();
    write(&root.join("notes.txt"), "the quarterly zephyrine report");
    let (cp1256, _, _) = encoding_rs::WINDOWS_1256.encode("تقرير الميزانية السنوية");
    write(&root.join("arabic.txt"), &cp1256);
    write(&root.join("sub/letter.docx"), docx("contract bravissimo"));
    // Not candidates:
    write(&root.join("~$letter.docx"), docx("owner file bravissimo"));
    write(&root.join(".hidden.txt"), "zephyrine hidden");
    write(
        &root.join("node_modules/pkg/readme.md"),
        "zephyrine dependency",
    );
    write(&root.join("program.exe"), "MZ zephyrine");

    let (_ix, index) = open_index();
    let sched = start(&index, config());
    let snap = snapshot(root);
    assert_eq!(sched.enqueue_snapshot(&snap), 3);
    assert!(sched.wait_idle(LONG));

    let p = sched.progress();
    assert_eq!(p.indexed, 3, "{p:?}");
    assert_eq!(p.failed.total(), 0);
    assert_eq!(p.state, RunState::Idle);
    assert!(p.commits >= 1);
    assert_eq!(hits(&index, "zephyrine"), [key(&root.join("notes.txt"))]);
    assert_eq!(hits(&index, "الميزانية"), [key(&root.join("arabic.txt"))]);
    assert_eq!(
        hits(&index, "bravissimo"),
        [key(&root.join("sub").join("letter.docx"))]
    );

    // A second scan finds everything current.
    assert_eq!(sched.enqueue_snapshot(&snap), 3);
    assert!(sched.wait_idle(LONG));
    let p = sched.progress();
    assert_eq!((p.indexed, p.up_to_date), (3, 3));
    sched.shutdown();
}

#[test]
fn modify_delete_and_folder_rename_update_the_index() {
    let docs = tmp();
    let root = docs.path();
    let a = root.join("a.txt");
    write(&a, "alpha original");
    write(&root.join("dir/one.md"), "galaxy one");
    write(&root.join("dir/deep/two.md"), "galaxy two");

    let (_ix, index) = open_index();
    let sched = start(&index, config());
    sched.enqueue_snapshot(&snapshot(root));
    assert!(sched.wait_idle(LONG));
    assert_eq!(hits(&index, "original").len(), 1);
    assert_eq!(hits(&index, "galaxy").len(), 2);

    // Modify: new size, so a new version.
    write(&a, "alpha rewritten with more words");
    sched.paths_changed(&[&a]);
    assert!(sched.wait_idle(LONG));
    assert!(hits(&index, "original").is_empty());
    assert_eq!(hits(&index, "rewritten"), [key(&a)]);

    // Folder rename: delete of the old subtree + listing of the new folder.
    let new_dir = root.join("renamed");
    fs::rename(root.join("dir"), &new_dir).unwrap();
    sched.paths_changed(&[root.join("dir"), new_dir.clone()]);
    assert!(sched.wait_idle(LONG));
    assert_eq!(
        hits(&index, "galaxy"),
        [
            key(&new_dir.join("deep").join("two.md")),
            key(&new_dir.join("one.md")),
        ]
    );

    // Delete.
    fs::remove_file(&a).unwrap();
    sched.paths_changed(&[&a]);
    assert!(sched.wait_idle(LONG));
    assert!(hits(&index, "rewritten").is_empty());
    assert!(sched.progress().deleted >= 3);
    sched.shutdown();
}

/// Records the order files are extracted in.
#[derive(Default)]
struct Recorder(Mutex<Vec<PathBuf>>);

impl Extractor for Recorder {
    fn supports(&self, kind: Kind) -> bool {
        kind == Kind::Text
    }
    fn extract(&self, path: &Path, _: Kind) -> Result<String, ExtractFailure> {
        self.0.lock().unwrap().push(path.to_path_buf());
        Ok("recorded".into())
    }
}

#[test]
fn priority_order_changes_then_user_folders_then_recent_then_small() {
    let docs = tmp();
    let root = docs.path();
    let names = [
        "Documents/old.txt",
        "other/old-small.txt",
        "other/new-big.txt",
        "other/new-small.txt",
        "other/changed.txt",
    ];
    for n in names {
        write(&root.join(n), "x");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let day = 86_400;

    let (_ix, index) = open_index();
    let rec = Arc::new(Recorder::default());
    let sched = ContentScheduler::builder(Arc::clone(&index))
        .config(SchedulerConfig {
            workers: 1,
            start_paused: true,
            priority_folders: vec![root.join("Documents")],
            ..config()
        })
        .extractor(Arc::clone(&rec) as Arc<dyn Extractor>)
        .probe(NullProbe)
        .start()
        .unwrap();
    // Metadata is given explicitly here; the content index only compares it for equality.
    sched.enqueue_files([
        (root.join("other/old-small.txt"), 10, now - 400 * day),
        (root.join("other/new-big.txt"), 50 << 20, now),
        (root.join("Documents/old.txt"), 10, now - 1000 * day),
        (root.join("other/new-small.txt"), 10, now),
        (root.join("other/changed.txt"), 10, now - 2000 * day),
    ]);
    sched.paths_changed(&[root.join("other/changed.txt")]); // same path: deduped, promoted
    assert_eq!(sched.progress().queued, 5);
    sched.resume();
    assert!(sched.wait_idle(LONG));

    let order: Vec<PathBuf> = rec.0.lock().unwrap().clone();
    let expected: Vec<PathBuf> = [
        "other/changed.txt",
        "Documents/old.txt",
        "other/new-small.txt",
        "other/new-big.txt",
        "other/old-small.txt",
    ]
    .iter()
    .map(|n| root.join(n))
    .collect();
    assert_eq!(order, expected);
    assert_eq!(sched.progress().indexed, 5);
}

fn many_files(root: &Path, n: usize) {
    for i in 0..n {
        write(
            &root.join(format!("d{}/f{i}.txt", i % 20)),
            format!("file number {i} مستند رقم {i} with some shared words"),
        );
    }
}

#[test]
fn pause_and_resume() {
    let docs = tmp();
    many_files(docs.path(), 30);
    let (_ix, index) = open_index();
    let sched = start(&index, config());
    sched.pause();
    sched.enqueue_snapshot(&snapshot(docs.path()));
    std::thread::sleep(Duration::from_millis(150));
    let p = sched.progress();
    assert_eq!(p.indexed, 0);
    assert_eq!(p.queued, 30);
    assert!(matches!(p.state, RunState::Paused(r) if r.user));
    assert!(!sched.wait_idle(Duration::from_millis(50)));

    sched.resume();
    assert!(sched.wait_idle(LONG));
    assert_eq!(sched.progress().indexed, 30);
    assert_eq!(index.num_docs(), 30);
}

struct FakeProbe(Arc<Mutex<SystemState>>);

impl SystemProbe for FakeProbe {
    fn sample(&mut self) -> SystemState {
        *self.0.lock().unwrap()
    }
}

#[test]
fn fake_probe_drives_auto_pause_on_battery_and_when_busy() {
    let docs = tmp();
    many_files(docs.path(), 20);
    let (_ix, index) = open_index();
    let state = Arc::new(Mutex::new(SystemState {
        on_battery: true,
        cpu_load: 0.0,
    }));
    let sched = ContentScheduler::builder(Arc::clone(&index))
        .config(config())
        .probe(FakeProbe(Arc::clone(&state)))
        .start()
        .unwrap();
    wait_until("battery pause", || sched.pause_reasons().battery);
    sched.enqueue_snapshot(&snapshot(docs.path()));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sched.progress().indexed, 0);

    // Back on AC but the machine is busy: still paused.
    *state.lock().unwrap() = SystemState {
        on_battery: false,
        cpu_load: 0.95,
    };
    wait_until("busy pause", || {
        let r = sched.pause_reasons();
        r.busy && !r.battery
    });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sched.progress().indexed, 0);

    // Calm again: runs to the end.
    state.lock().unwrap().cpu_load = 0.1;
    assert!(sched.wait_idle(LONG));
    assert!(!sched.pause_reasons().any());
    assert_eq!(sched.progress().indexed, 20);
}

#[test]
fn failures_are_counted_remembered_and_retried_only_after_a_change() {
    let docs = tmp();
    let root = docs.path();
    let bad = root.join("broken.docx");
    write(&bad, b"this is not a zip file at all");
    write(&root.join("good.txt"), "fine content");

    let (_ix, index) = open_index();
    let sched = start(&index, config());
    sched.enqueue_snapshot(&snapshot(root));
    assert!(sched.wait_idle(LONG));
    let p = sched.progress();
    assert_eq!(p.indexed, 1);
    assert_eq!(p.failed.total(), 1);
    assert_eq!(p.failed.iter().count(), 1);
    let failures = sched.failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].path, key(&bad));
    assert!(
        matches!(
            failures[0].kind,
            FailureKind::Corrupt | FailureKind::Unsupported
        ),
        "{:?}",
        failures[0]
    );

    // Same version again: skipped without extracting.
    sched.enqueue_snapshot(&snapshot(root));
    assert!(sched.wait_idle(LONG));
    let p = sched.progress();
    assert_eq!(p.failed.total(), 1);
    assert_eq!(p.known_failures, 1);

    // Fixed file (new size): retried and indexed, forgotten as a failure.
    write(&bad, docx("repaired marvellous"));
    sched.paths_changed(&[&bad]);
    assert!(sched.wait_idle(LONG));
    assert_eq!(hits(&index, "marvellous"), [key(&bad)]);
    assert!(sched.failures().is_empty());
    assert_eq!(sched.progress().indexed, 2);
}

/// Slow extraction, so shutdown happens mid-run.
struct Slow(InProcessExtractor);

impl Extractor for Slow {
    fn supports(&self, kind: Kind) -> bool {
        self.0.supports(kind)
    }
    fn extract(&self, path: &Path, kind: Kind) -> Result<String, ExtractFailure> {
        std::thread::sleep(Duration::from_millis(20));
        self.0.extract(path, kind)
    }
}

#[test]
fn shutdown_commits_what_was_extracted() {
    let docs = tmp();
    many_files(docs.path(), 60);
    let (_ix, index) = open_index();
    let reports = Arc::new(Mutex::new(Vec::<Progress>::new()));
    let sink = Arc::clone(&reports);
    let sched = ContentScheduler::builder(Arc::clone(&index))
        .config(SchedulerConfig {
            workers: 1,
            commit_every: u64::MAX,
            commit_interval: Duration::from_secs(3600),
            ..config()
        })
        .extractor(Arc::new(Slow(InProcessExtractor::default())))
        .probe(NullProbe)
        .on_progress(move |p| sink.lock().unwrap().push(*p))
        .start()
        .unwrap();
    sched.enqueue_snapshot(&snapshot(docs.path()));
    wait_until("a few documents", || sched.progress().indexed >= 3);
    assert_eq!(index.num_docs(), 0, "nothing committed before shutdown");

    let last = sched.shutdown();
    assert_eq!(last.state, RunState::Stopped);
    assert!(last.indexed >= 3 && last.indexed < 60, "{last:?}");
    assert_eq!(index.num_docs(), last.indexed);
    assert_eq!(last.commits, 1);

    let reports = reports.lock().unwrap();
    assert!(reports.len() >= 2, "progress was reported");
    assert!(reports.iter().any(|p| p.state == RunState::Running));
    assert_eq!(reports.last().unwrap().state, RunState::Stopped);
}

#[test]
fn throughput_on_2000_text_files() {
    let docs = tmp();
    let n = 2000;
    let t0 = Instant::now();
    for i in 0..n {
        let body = format!(
            "Document {i}. تقرير رقم {i} عن المشروع والميزانية. \
             Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor. {}",
            "كلمة عربية وكلمات أخرى للاختبار ".repeat(20)
        );
        write(&docs.path().join(format!("d{}/f{i}.txt", i % 50)), body);
    }
    let generated = t0.elapsed();
    let snap = snapshot(docs.path());

    let (_ix, index) = open_index();
    let workers = SchedulerConfig::default().workers;
    let sched = start(
        &index,
        SchedulerConfig {
            workers,
            commit_every: 1_000,
            commit_interval: Duration::from_secs(10),
            ..config()
        },
    );
    let t1 = Instant::now();
    assert_eq!(sched.enqueue_snapshot(&snap), n);
    assert!(sched.wait_idle(LONG));
    let took = t1.elapsed();
    let p = sched.progress();
    assert_eq!(p.indexed, n as u64);
    assert_eq!(index.num_docs(), n as u64);
    eprintln!(
        "throughput: {n} text files ({} KiB) with {workers} worker(s) in {:.2?} = {:.0} docs/s \
         ({} commits; generating the files took {:.2?})",
        p.bytes / 1024,
        took,
        n as f64 / took.as_secs_f64(),
        p.commits,
        generated,
    );
}
