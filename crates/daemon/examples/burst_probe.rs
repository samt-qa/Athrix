//! Peak memory of the content scheduler on a folder of big text files.
//!
//! Usage: `cargo run --release -p athrix-daemon --example burst_probe -- <folder>`
//!
//! Walks the folder, indexes it into a scratch content index with the default scheduler
//! settings, and prints the highest private bytes seen (sampled every 50 ms) and the time.
//! A run of big files is extracted faster than one thread indexes them: this shows how much
//! text waits in memory meanwhile.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use athrix_content_index::ContentIndex;
use athrix_daemon::ContentScheduler;
use athrix_fs_index::{LiveIndex, WalkOptions, walk};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

fn private_mib(sys: &mut System) -> u64 {
    let pid = sysinfo::get_current_pid().unwrap();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map_or(0, sysinfo::Process::virtual_memory) / (1024 * 1024)
}

fn main() {
    let root = std::env::args().nth(1).expect("folder of big text files");
    let options = WalkOptions::new([&root]);
    let base = walk(&options).unwrap();
    let live = LiveIndex::new(base, &options).unwrap();
    let snapshot = live.snapshot();
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(ContentIndex::open_or_create(dir.path()).unwrap());
    let scheduler = ContentScheduler::builder(Arc::clone(&index))
        .start()
        .unwrap();

    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (Arc::clone(&peak), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut sys = System::new();
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(private_mib(&mut sys), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let before = private_mib(&mut System::new());
    let started = Instant::now();
    let queued = scheduler.enqueue_snapshot(&snapshot);
    assert!(scheduler.wait_idle(Duration::from_secs(1800)));
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let p = scheduler.progress();
    println!(
        "{queued} files, {} indexed, {} MiB read in {:.1} s: {before} MiB before, peak {} MiB",
        p.indexed,
        p.bytes / (1024 * 1024),
        elapsed.as_secs_f64(),
        peak.load(Ordering::Relaxed)
    );
    let _ = scheduler.shutdown();
}
