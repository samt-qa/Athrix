//! Finds duplicates under a folder and prints timings (read-only: files are only opened for
//! reading). `cargo run --release -p athrix-dedupe --example scan -- <folder> [cache.sqlite3]`
//! runs twice: a cold scan, then a second one that should be served by the cache.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use athrix_dedupe::{Candidate, HashCache, Options, find_duplicates};

fn walk(dir: &Path, out: &mut Vec<Candidate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let Ok(ty) = e.file_type() else { continue };
        let path = e.path();
        if ty.is_dir() {
            walk(&path, out);
        } else if ty.is_file() {
            if let Ok(m) = e.metadata() {
                out.push(Candidate {
                    key: path.to_string_lossy().to_lowercase(),
                    path,
                    size: m.len(),
                });
            }
        }
    }
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().expect("usage: scan <folder> [cache]"));
    let cache_path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("athrix-dedupe-scan.sqlite3"));
    let t = Instant::now();
    let mut candidates = Vec::new();
    walk(&root, &mut candidates);
    let total: u64 = candidates.iter().map(|c| c.size).sum();
    println!(
        "walked {} files, {:.2} GB in {:?}",
        candidates.len(),
        total as f64 / 1e9,
        t.elapsed()
    );
    let cache = HashCache::open(&cache_path).expect("cache");
    let options = Options {
        max_time: Duration::from_secs(600),
        max_bytes: u64::MAX,
        threads: 0,
    };
    for run in ["first", "second"] {
        let t = Instant::now();
        let r = find_duplicates(&candidates, Some(&cache), &options, &AtomicBool::new(false))
            .expect("not cancelled");
        println!(
            "{run}: {:?}; {} groups, {} files, wasted {:.2} MB, read {:.2} MB, cache hits {}, \
             skipped {}, incomplete {}",
            t.elapsed(),
            r.groups.len(),
            r.groups.iter().map(|g| g.members.len()).sum::<usize>(),
            r.wasted() as f64 / 1e6,
            r.bytes_read as f64 / 1e6,
            r.cache_hits,
            r.skipped,
            r.incomplete
        );
    }
}
