//! Memory of the content scheduler's backlog for a real name-index snapshot.
//!
//! Usage: `cargo run --release -p athrix-daemon --example queue_probe -- <names.snap copy>`
//!
//! Loads the snapshot, starts a paused scheduler on a scratch content index, queues the
//! snapshot like the app does at start-up, and prints the private bytes before and after.
//! Never pass the app's own snapshot path: copy it first.

use std::sync::Arc;

use athrix_content_index::ContentIndex;
use athrix_daemon::ContentScheduler;
use athrix_fs_index::{FileIndex, LiveIndex, WalkOptions};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

fn private_mib() -> f64 {
    let pid = sysinfo::get_current_pid().unwrap();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map_or(0, sysinfo::Process::virtual_memory) as f64 / (1024.0 * 1024.0)
}

fn main() {
    let snap = std::env::args().nth(1).expect("snapshot copy path");
    let base = FileIndex::load(snap.as_ref()).unwrap();
    let live = LiveIndex::new(base, &WalkOptions::new(Vec::<std::path::PathBuf>::new())).unwrap();
    let snapshot = live.snapshot();
    let dir = tempfile::tempdir().unwrap();
    // Optional second argument: a COPY of a real content index.
    let content = std::env::args()
        .nth(2)
        .map_or_else(|| dir.path().to_path_buf(), std::path::PathBuf::from);
    let index = Arc::new(ContentIndex::open_or_create(&content).unwrap());
    println!(
        "content docs {} keys {} rebuilt {:?}",
        index.num_docs(),
        index.current_keys().unwrap().len(),
        index.rebuilt()
    );
    let scheduler = ContentScheduler::builder(index).start().unwrap();
    scheduler.pause();
    let before = private_mib();
    let t = std::time::Instant::now();
    let queued = scheduler.enqueue_snapshot(&snapshot);
    let first = private_mib();
    println!(
        "queued {queued} candidates in {:.1} s: {before:.0} -> {first:.0} MiB (+{:.0} MiB, {:.0} B each)",
        t.elapsed().as_secs_f64(),
        first - before,
        (first - before) * 1024.0 * 1024.0 / queued.max(1) as f64
    );
    // The app queues the whole snapshot again after every full walk (each start-up).
    let again = scheduler.enqueue_snapshot(&snapshot);
    println!("queued again ({again}): {:.0} MiB", private_mib());
    let _ = scheduler.shutdown();
}
