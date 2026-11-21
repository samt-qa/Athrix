//! Live-update cost over ~1,000,000 synthetic entries (task 1.5): applying a batch of changes
//! to a `LiveIndex` and searching a snapshot that carries a delta.
//!
//! Run with `cargo bench -p athrix-fs-index --bench live`. Budget: a batch of ~100 changes on a
//! 1M-entry index well under 100 ms, and search p95 with a delta no worse than ~2x the plain
//! index. The synthetic generator is copied from `benches/index.rs` so all benches use the
//! same data. Changes go through `LiveIndex::apply_changes` (no disk access): the synthetic
//! tree does not exist on disk.

// `criterion_group!` generates an undocumented public function.
#![allow(missing_docs)]

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use athrix_fs_index::{
    Change, EntryId, EntryMeta, FileIndex, IndexBuilder, LiveIndex, Query, WalkOptions,
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

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

const QUERIES: &[&str] = &[
    "تقرير",
    "مُحاضَرة",
    "report 2026",
    "^img",
    "pdf$",
    "*.pdf",
    "فاتورة_*",
    "zzqxv",
    // Filters (query language, task 3.x): metadata, booleans and lazily built paths.
    "ext:pdf",
    "تقرير ext:pdf",
    "type:image;video",
    "size:>10mb",
    "dm:2020",
    "report | invoice",
    "!report ext:md",
    "report path:src",
    "infolder:مشروع",
    "path:zzqxv",
];

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// The synthetic root (it does not exist on disk).
const ROOT: &str = "D:\\Data";

/// Folder and file paths of the base, for building realistic changes.
struct Targets {
    dirs: Vec<PathBuf>,
    files: Vec<(PathBuf, EntryMeta)>,
}

fn targets(ix: &FileIndex) -> Targets {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    while dirs.len() < 2_000 || files.len() < 20_000 {
        let id = rng.below(ix.len()) as EntryId;
        if ix.meta(id).is_dir {
            if dirs.len() < 2_000 {
                dirs.push(ix.path(id));
            }
        } else if files.len() < 20_000 {
            files.push((ix.path(id), ix.meta(id)));
        }
    }
    Targets { dirs, files }
}

/// A batch of `n` changes: 60% new files in existing folders, 20% modified files (new size),
/// 20% deleted files. `round` varies names and picks so batches differ.
fn batch(t: &Targets, n: usize, round: usize) -> Vec<Change> {
    let mut rng = Rng(0x5151_0000 + round as u64);
    let file = |size| EntryMeta {
        size,
        modified: Some(1_800_000_000),
        ..Default::default()
    };
    (0..n)
        .map(|i| match i % 5 {
            0..=2 => Change::Upsert {
                path: t.dirs[rng.below(t.dirs.len())].join(format!("جديد new_{round}_{i}.txt")),
                meta: file(i as u64),
            },
            3 => {
                let (path, meta) = &t.files[rng.below(t.files.len())];
                Change::Upsert {
                    path: path.clone(),
                    meta: EntryMeta {
                        size: meta.size + 1,
                        ..*meta
                    },
                }
            }
            _ => Change::Remove {
                path: t.files[rng.below(t.files.len())].0.clone(),
            },
        })
        .collect()
}

fn new_live(base: &Arc<FileIndex>) -> LiveIndex {
    let live = LiveIndex::new(Arc::clone(base), &WalkOptions::new([ROOT])).unwrap();
    // Measure the overlay itself, not compaction.
    live.set_compaction_thresholds(usize::MAX, usize::MAX);
    live
}

/// A live index whose delta already holds about `entries` changes.
fn with_delta(base: &Arc<FileIndex>, t: &Targets, entries: usize) -> LiveIndex {
    let live = new_live(base);
    for round in 0..entries.div_ceil(1000) {
        live.apply_changes(&batch(t, 1000, 1_000_000 + round));
    }
    live
}

fn print_summary(base: &Arc<FileIndex>, t: &Targets) {
    let start = Instant::now();
    let live = new_live(base);
    eprintln!(
        "\n[live] {} entries, LiveIndex::new (path lookup table) {:?}",
        base.len(),
        start.elapsed()
    );
    let depth = |p: &PathBuf| p.components().count();
    eprintln!(
        "[live] changed paths: avg depth {:.0} components, avg length {:.0} bytes (deep synthetic tree)",
        t.files.iter().map(|f| depth(&f.0)).sum::<usize>() as f64 / t.files.len() as f64,
        t.files.iter().map(|f| f.0.as_os_str().len()).sum::<usize>() as f64 / t.files.len() as f64,
    );
    for delta in [0, 10_000, 30_000] {
        let mut times = Vec::new();
        for round in 0..30 {
            let live = with_delta(base, t, delta);
            let changes = batch(t, 100, round);
            let s = Instant::now();
            black_box(live.apply_changes(&changes));
            times.push(s.elapsed());
        }
        times.sort();
        eprintln!(
            "[live] apply 100 changes, delta {:>6}: p50 {:>9.3?}  p95 {:>9.3?}  max {:>9.3?}",
            delta,
            percentile(&times, 0.5),
            percentile(&times, 0.95),
            times[times.len() - 1],
        );
    }
    // Removing a whole folder: one random folder of the sample.
    let s = Instant::now();
    let stats = live.apply_changes(&[Change::Remove {
        path: t.dirs[0].clone(),
    }]);
    eprintln!(
        "[live] remove a folder subtree of {} entries: {:?}",
        stats.removed,
        s.elapsed()
    );

    let live = with_delta(base, t, 30_000);
    let snap = live.snapshot();
    eprintln!(
        "[live] snapshot: {} delta entries, {} removed base entries",
        snap.delta_entries(),
        snap.removed_entries()
    );
    let s = Instant::now();
    let merged = black_box(snap.to_file_index());
    eprintln!("[live] compaction into a new base: {:?}", s.elapsed());
    let s = Instant::now();
    black_box(LiveIndex::new(Arc::new(merged), &WalkOptions::new([ROOT])).unwrap());
    eprintln!(
        "[live] lookup table rebuild after compaction: {:?}",
        s.elapsed()
    );

    let cancel = AtomicBool::new(false);
    let measure = |f: &dyn Fn(&Query) -> usize, q: &str| {
        let mut times = Vec::with_capacity(200);
        let mut hits = 0;
        for _ in 0..200 {
            let t = Instant::now();
            hits = f(&Query::parse(q).unwrap());
            times.push(t.elapsed());
        }
        times.sort();
        (hits, percentile(&times, 0.5), percentile(&times, 0.95))
    };
    for limit in [100, usize::MAX] {
        for q in QUERIES {
            let (bh, b50, b95) = measure(&|q| base.search(q, limit, &cancel).unwrap().len(), q);
            let (lh, l50, l95) = measure(&|q| snap.search(q, limit, &cancel).unwrap().len(), q);
            eprintln!(
                "[live] limit {:>5} {:<18} base hits {:>7} p50 {:>9.3?} p95 {:>9.3?} | delta hits {:>7} p50 {:>9.3?} p95 {:>9.3?} ({:.2}x)",
                if limit == usize::MAX {
                    "all".to_string()
                } else {
                    limit.to_string()
                },
                q,
                bh,
                b50,
                b95,
                lh,
                l50,
                l95,
                l95.as_secs_f64() / b95.as_secs_f64().max(1e-9),
            );
        }
    }
}

fn benches(c: &mut Criterion) {
    let base = Arc::new(build(&synthetic(ENTRIES)));
    let t = targets(&base);
    print_summary(&base, &t);

    let mut g = c.benchmark_group("live_apply_1m");
    g.sample_size(20);
    let mut round = 0;
    for delta in [0, 30_000] {
        g.bench_function(format!("batch100/delta{delta}"), |b| {
            b.iter_batched(
                || {
                    round += 1;
                    (with_delta(&base, &t, delta), batch(&t, 100, round))
                },
                |(live, changes)| {
                    black_box(live.apply_changes(&changes));
                    live
                },
                BatchSize::PerIteration,
            )
        });
    }
    g.finish();

    let cancel = AtomicBool::new(false);
    let snap = with_delta(&base, &t, 30_000).snapshot();
    let mut g = c.benchmark_group("live_search_1m");
    for q in ["تقرير", "*.pdf", "ext:pdf", "report path:src", "zzqxv"] {
        g.bench_function(format!("base/limit100/{q}"), |b| {
            b.iter(|| {
                base.search(&Query::parse(black_box(q)).unwrap(), 100, &cancel)
                    .unwrap()
            })
        });
        g.bench_function(format!("delta30k/limit100/{q}"), |b| {
            b.iter(|| {
                snap.search(&Query::parse(black_box(q)).unwrap(), 100, &cancel)
                    .unwrap()
            })
        });
        g.bench_function(format!("delta30k/all/{q}"), |b| {
            b.iter(|| {
                snap.search(&Query::parse(black_box(q)).unwrap(), usize::MAX, &cancel)
                    .unwrap()
            })
        });
    }
    g.finish();
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
