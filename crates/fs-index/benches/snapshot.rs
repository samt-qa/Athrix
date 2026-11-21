//! Index snapshot save/load time and file size over ~1,000,000 synthetic entries (task 1.4).
//!
//! Run with `cargo bench -p athrix-fs-index --bench snapshot`. Target from
//! `docs/01-RESEARCH-AND-FEATURES.md` §8.3: the index is ready < 300 ms after startup.
//! The synthetic generator is copied from `benches/index.rs` so both benches use the same data.

// `criterion_group!` generates an undocumented public function.
#![allow(missing_docs)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use athrix_fs_index::{EntryId, EntryMeta, FileIndex, IndexBuilder};
use criterion::{Criterion, criterion_group, criterion_main};

const ENTRIES: usize = 1_000_000;

const ARABIC: &[&str] = &[
    "مُحَاضَرَة",
    "محاضرة",
    "تقرير",
    "تَقْرِير",
    "فاتورة",
    "أحمد",
    "إحصاء",
    "الفِقْه",
    "مشروع",
    "صُورَة",
    "عقد",
    "ميزانية",
    "دَرْس",
    "كتاب",
    "الاجتماع",
    "مـــلف",
];
const LATIN: &[&str] = &[
    "report", "Invoice", "IMG", "notes", "project", "backup", "final", "draft", "Meeting",
    "budget", "photo", "README", "src", "lib", "test", "data",
];
const EXTS: &[&str] = &[
    "pdf", "docx", "xlsx", "jpg", "png", "txt", "mp4", "zip", "rs", "js", "md", "pptx", "json",
];
const SEPS: &[&str] = &["_", " ", "-", "."];

/// xorshift64*: deterministic and dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

type Entry = (Option<EntryId>, String, EntryMeta);

/// Generates a nested tree of `n` entries (~8% folders) with mixed Arabic/Latin names.
fn synthetic(n: usize) -> Vec<Entry> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<Entry> = Vec::with_capacity(n);
    let dir = EntryMeta {
        is_dir: true,
        ..Default::default()
    };
    out.push((None, "D:\\Data".into(), dir));
    let mut dirs: Vec<EntryId> = vec![0];
    while out.len() < n {
        let id = out.len() as EntryId;
        // Prefer recent folders so the tree gets deep, but sometimes go back up.
        let parent = if rng.below(4) == 0 {
            dirs[rng.below(dirs.len())]
        } else {
            dirs[dirs.len() - 1 - rng.below(dirs.len().min(32))]
        };
        let mut name = String::new();
        for w in 0..1 + rng.below(3) {
            if w > 0 {
                name.push_str(rng.pick(SEPS));
            }
            name.push_str(if rng.below(2) == 0 {
                rng.pick(ARABIC)
            } else {
                rng.pick(LATIN)
            });
        }
        if rng.below(3) == 0 {
            name.push_str(rng.pick(SEPS));
            name.push_str(&(2010 + rng.below(17)).to_string());
        }
        if rng.below(5) == 0 {
            name.push_str(&format!(" ({})", rng.below(100)));
        }
        let is_dir = rng.below(100) < 8;
        if is_dir {
            dirs.push(id);
            out.push((Some(parent), name, dir));
        } else {
            name.push('.');
            name.push_str(rng.pick(EXTS));
            let meta = EntryMeta {
                size: rng.next() % 50_000_000,
                modified: Some(1_500_000_000 + (rng.next() % 300_000_000) as i64),
                ..Default::default()
            };
            out.push((Some(parent), name, meta));
        }
    }
    out
}

fn build(entries: &[Entry]) -> FileIndex {
    let mut b = IndexBuilder::with_capacity(entries.len(), 24);
    for (parent, name, meta) in entries {
        b.push(*parent, name, *meta);
    }
    b.finish()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, Duration) {
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            black_box(f());
            t.elapsed()
        })
        .collect();
    times.sort();
    (percentile(&times, 0.5), times[times.len() - 1])
}

fn benches(c: &mut Criterion) {
    let ix = build(&synthetic(ENTRIES));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.athrix");
    ix.save(&path).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    let (save_p50, save_max) = time(10, || ix.save(&path).unwrap());
    let (load_p50, load_max) = time(10, || FileIndex::load(&path).unwrap());
    eprintln!(
        "
[snapshot] {} entries, file {:.1} MB (index heap {:.1} MB), save p50 {save_p50:?} max {save_max:?}, load p50 {load_p50:?} max {load_max:?}",
        ix.len(),
        size as f64 / 1e6,
        ix.heap_bytes() as f64 / 1e6,
    );

    let mut g = c.benchmark_group("snapshot_1m");
    g.sample_size(10);
    g.bench_function("save", |b| b.iter(|| ix.save(black_box(&path)).unwrap()));
    g.bench_function("load", |b| {
        b.iter(|| FileIndex::load(black_box(&path)).unwrap())
    });
    g.finish();
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
