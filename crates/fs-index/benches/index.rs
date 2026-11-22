//! Index build and name-search latency over ~1,000,000 synthetic entries (task 1.9).
//!
//! Run with `cargo bench -p athrix-fs-index`. Besides criterion's statistics, the bench prints
//! the index heap size and p50/p95 latencies so they can be compared with the targets in
//! `docs/01-RESEARCH-AND-FEATURES.md` §10 (first results p95 < 30 ms, < 150 MB for 1M names).

// `criterion_group!` generates an undocumented public function.
#![allow(missing_docs)]

use std::hint::black_box;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use athrix_fs_index::{EntryId, EntryMeta, FileIndex, IndexBuilder, Query};
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
    // Fuzzy terms (task 3.7): ranked, so every query scans the whole index.
    "fuzzy:rprt",
    "fuzzy:reprot",
    "fuzzy:mrs",
    "fuzzy:تقرر",
    "fuzzy:managmnet",
    "fuzzy:zzqxv",
    "fuzzy: تقرير 2026",
];

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Prints memory and p50/p95/max latency per query (criterion reports means, not p95).
fn print_summary(entries: &[Entry], ix: &FileIndex) {
    let name_bytes: usize = entries.iter().map(|e| e.1.len()).sum();
    let start = Instant::now();
    let _ = black_box(build(entries));
    let build_time = start.elapsed();
    eprintln!(
        "\n[fs-index] {} entries, {:.1} MB of raw names, index heap {:.1} MB, build {:?}, {} rayon threads",
        ix.len(),
        name_bytes as f64 / 1e6,
        ix.heap_bytes() as f64 / 1e6,
        build_time,
        rayon::current_num_threads(),
    );
    // Path filters rebuild paths, so their cost depends on depth and path length.
    let (mut depth_sum, mut depth_max, mut path_bytes) = (0usize, 0usize, 0usize);
    for id in 0..ix.len() as EntryId {
        let (mut d, mut cur) = (0, ix.parent(id));
        path_bytes += ix.normalized_name(id).len();
        while let Some(p) = cur {
            d += 1;
            path_bytes += ix.normalized_name(p).len() + 1;
            cur = ix.parent(p);
        }
        depth_sum += d;
        depth_max = depth_max.max(d);
    }
    eprintln!(
        "[fs-index] depth avg {:.1} max {}, normalized path avg {:.0} bytes",
        depth_sum as f64 / ix.len() as f64,
        depth_max,
        path_bytes as f64 / ix.len() as f64,
    );
    let cancel = AtomicBool::new(false);
    for limit in [100, usize::MAX] {
        for q in QUERIES {
            let mut times = Vec::with_capacity(200);
            let mut hits = 0;
            for _ in 0..200 {
                let t = Instant::now();
                // Parse inside the loop: the UI parses on every keystroke too.
                let r = ix
                    .search(&Query::parse(q).unwrap(), limit, &cancel)
                    .unwrap();
                times.push(t.elapsed());
                hits = r.len();
            }
            times.sort();
            eprintln!(
                "[fs-index] limit {:>5} {:<20} hits {:>7}  p50 {:>9.3?}  p95 {:>9.3?}  max {:>9.3?}",
                if limit == usize::MAX {
                    "all".to_string()
                } else {
                    limit.to_string()
                },
                q,
                hits,
                percentile(&times, 0.5),
                percentile(&times, 0.95),
                times[times.len() - 1],
            );
        }
    }
}

/// The "did you mean" probe (`search_suggest` in the app): how many entries a fuzzy rewrite
/// finds, up to the app's limit, with [`Query::unordered`] (stops at the limit anywhere)
/// against the ranked search it replaced (scores the whole index).
fn print_suggest_probe(ix: &FileIndex) {
    let cancel = AtomicBool::new(false);
    for q in [
        "fuzzy: rprt",
        "fuzzy: reprot",
        "fuzzy: managmnet",
        "fuzzy: تقرر",
        "fuzzy: zzqxv",
        "fuzzy: budget 2024",
        "fuzzy: فاتوره الكهرباء",
    ] {
        let query = Query::parse(q).unwrap();
        for (label, query) in [("ranked", query.clone()), ("unordered", query.unordered())] {
            let mut times = Vec::with_capacity(100);
            let mut hits = 0;
            for _ in 0..100 {
                let t = Instant::now();
                hits = ix.search(&query, 200, &cancel).unwrap().len();
                times.push(t.elapsed());
            }
            times.sort();
            eprintln!(
                "[fs-index] probe {label:<9} {q:<24} hits {hits:>4}  p50 {:>9.3?}  p95 {:>9.3?}",
                percentile(&times, 0.5),
                percentile(&times, 0.95),
            );
        }
    }
}

fn benches(c: &mut Criterion) {
    let entries = synthetic(ENTRIES);
    let ix = build(&entries);
    print_summary(&entries, &ix);
    print_suggest_probe(&ix);

    let mut g = c.benchmark_group("build");
    g.sample_size(10);
    g.bench_function("1m_entries", |b| b.iter(|| build(black_box(&entries))));
    g.finish();

    let cancel = AtomicBool::new(false);
    let mut g = c.benchmark_group("search_1m");
    for q in QUERIES {
        g.bench_function(format!("limit100/{q}"), |b| {
            b.iter(|| {
                ix.search(&Query::parse(black_box(q)).unwrap(), 100, &cancel)
                    .unwrap()
            })
        });
        g.bench_function(format!("all/{q}"), |b| {
            b.iter(|| {
                ix.search(&Query::parse(black_box(q)).unwrap(), usize::MAX, &cancel)
                    .unwrap()
            })
        });
    }
    g.finish();
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
