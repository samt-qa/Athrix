//! Memory of each name-index stage the desktop app goes through at start-up, measured on a
//! real snapshot and real drives.
//!
//! Usage: `cargo run --release -p athrix-cli --example mem_probe -- <names.snap copy> <root>...`
//!
//! Prints the private bytes after each stage and the peak during it (sampled every 20 ms).
//! Never pass the app's own snapshot path: copy it first.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use athrix_fs_index::{FileIndex, LiveIndex, WalkOptions, walk};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// Private bytes (commit charge) of this process.
fn private_bytes(sys: &mut System, pid: sysinfo::Pid) -> u64 {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map_or(0, sysinfo::Process::virtual_memory)
}

struct Sampler {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Sampler {
    fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (Arc::clone(&peak), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            let pid = sysinfo::get_current_pid().unwrap();
            let mut sys = System::new();
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(private_bytes(&mut sys, pid), Ordering::Relaxed);
                thread::sleep(Duration::from_millis(20));
            }
        });
        Sampler {
            peak,
            stop,
            handle: Some(handle),
        }
    }

    /// Runs `f`, printing the private bytes after it and the peak during it.
    fn stage<T>(&self, name: &str, f: impl FnOnce() -> T) -> T {
        let pid = sysinfo::get_current_pid().unwrap();
        let mut sys = System::new();
        self.peak
            .store(private_bytes(&mut sys, pid), Ordering::Relaxed);
        let t = Instant::now();
        let out = f();
        let elapsed = t.elapsed();
        thread::sleep(Duration::from_millis(60));
        let after = private_bytes(&mut sys, pid);
        let peak = self.peak.load(Ordering::Relaxed).max(after);
        println!(
            "{name:<34} after {:>7.0} MiB   peak {:>7.0} MiB   {:>6.1} s",
            mib(after),
            mib(peak),
            elapsed.as_secs_f64()
        );
        out
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let snap = PathBuf::from(args.next().expect("snapshot copy path"));
    let roots: Vec<PathBuf> = args.map(PathBuf::from).collect();
    assert!(!roots.is_empty(), "give at least one root");
    let opts = WalkOptions::new(roots.iter().cloned()).with_default_exclusions();
    let s = Sampler::start();

    s.stage("baseline", || ());
    let loaded = s.stage("load snapshot", || FileIndex::load(&snap).unwrap());
    println!("  entries: {}", loaded.len());
    let live = s.stage("LiveIndex::new (lookup)", || {
        LiveIndex::new(loaded, &opts).unwrap()
    });
    let snapshot = live.snapshot();
    let merged = s.stage("to_file_index (compaction copy)", || {
        snapshot.to_file_index()
    });
    s.stage("drop compaction copy", || drop(merged));
    let walked = s.stage("walk all roots (old index held)", || walk(&opts).unwrap());
    println!("  walked entries: {}", walked.len());
    let live2 = s.stage("LiveIndex::new of walk", || {
        LiveIndex::new(walked, &opts).unwrap()
    });
    s.stage("drop old live index", || drop((snapshot, live)));
    let snap2 = live2.snapshot();
    let out = std::env::temp_dir().join("athrix-mem-probe.snap");
    s.stage("save snapshot", || snap2.save(&out).unwrap());
    let _ = std::fs::remove_file(&out);
    s.stage("end", || ());
}
