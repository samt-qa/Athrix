//! Small end-to-end benchmark: index 10k synthetic ~2 KB documents (Arabic with partial
//! diacritics + some English and digits), then run queries. A second part builds a 50k-doc
//! index (~1 KB each, 100 folders) and measures scoped search (a 10k-path set and a folder
//! prefix), paging, counts, listing every path, the retain sweep and a prefix delete.
//!
//! `cargo bench -p athrix-content-index --bench content_bench` (optionally `-- <docs>` for the
//! first part).
//!
//! Reference run (Windows dev machine, schema 2, other builds running alongside, so ±2×), 50k
//! documents, 300 mixed queries, top 20 + snippets, p50 / p95:
//!
//! | case | p50 | p95 |
//! |---|---|---|
//! | unscoped | 0.8–1.3 ms | 3–6 ms |
//! | within a 10k-path set | 1.4–2.2 ms | 3.0–4.8 ms |
//! | within a folder of 500 docs | 0.8–1.2 ms | 1.4–1.8 ms |
//! | page at offset 100 + total | 0.9–1.3 ms | 3.7–5.2 ms |
//! | count only | 0.06–0.08 ms | 2.1–2.7 ms |
//!
//! `PathFilter::paths` of 10k paths builds in 1.4–2.8 ms; `paths()` over 50k documents takes
//! 4–7 ms; `retain` removing 40k queues its deletes in 26–37 ms (+35–53 ms commit);
//! `delete_prefix` of a 500-doc folder 20–30 ms with the commit. The same 10k-path filter as
//! a `TermSetQuery` on the key field (first attempt) was p95 16 ms.

use athrix_content_index::{ContentIndex, FileMeta, PathFilter, SearchOptions};
use std::time::{Duration, Instant};

const ARABIC: &[&str] = &[
    "الْمَكْتَبَاتُ",
    "مكتبة",
    "التَّقَارِيرُ",
    "تقرير",
    "المعلمون",
    "المعلمين",
    "مدرسة",
    "المدارس",
    "وزارة",
    "الصحة",
    "مستشفى",
    "المؤتمر",
    "رئيس",
    "عقد",
    "الإيجار",
    "المستأجر",
    "والمؤجر",
    "سنة",
    "مدينة",
    "الرياض",
    "جدة",
    "الطلاب",
    "بالجامعة",
    "كتاب",
    "كتبها",
    "العربية",
    "اللغة",
    "الحكومة",
    "الشركة",
    "الموظفين",
    "الرواتب",
    "الميزانية",
    "مشروع",
    "المشاريع",
    "تطوير",
    "البرمجيات",
    "قاعدة",
    "البيانات",
    "نظام",
    "الأنظمة",
    "المعلومات",
    "الأمن",
    "السيبراني",
    "فاتورة",
    "الضريبة",
    "القيمة",
    "المضافة",
    "اجتماع",
    "محضر",
    "قرار",
    "مجلس",
    "الإدارة",
    "الأعضاء",
    "وسيارات",
    "النقل",
    "الطريق",
    "خطة",
    "استراتيجية",
    "التعليم",
    "الصِّحَّةُ",
    "الْعَامَّةُ",
    "إِنَّمَا",
    "الْأَعْمَالُ",
    "بِالنِّيَّاتِ",
    "في",
    "من",
    "على",
    "إلى",
    "عن",
    "هذا",
    "هذه",
    "التي",
    "الذي",
    "كان",
    "قال",
    "بين",
    "بعد",
    "قبل",
];
const ENGLISH: &[&str] = &[
    "report", "contract", "invoice", "meeting", "project", "database", "system", "security",
    "budget", "the", "and", "of", "Riyadh", "Annual", "Q4", "revenue",
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Skewed pick: low indexes are more frequent (roughly Zipf-like).
    fn pick<'a>(&mut self, words: &[&'a str]) -> &'a str {
        let u = (self.next() % 10_000) as f64 / 10_000.0;
        words[((u * u) * words.len() as f64) as usize]
    }
}

fn make_doc(rng: &mut Rng, target: usize) -> String {
    let mut s = String::with_capacity(target + 64);
    while s.len() < target {
        match rng.next() % 20 {
            0 => s.push_str(rng.pick(ENGLISH)),
            1 => s.push_str(&(rng.next() % 3000).to_string()),
            2 => s.push_str("، "),
            _ => s.push_str(rng.pick(ARABIC)),
        }
        s.push(' ');
    }
    s
}

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|d| {
            d.flatten()
                .map(|e| e.metadata().map_or(0, |m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn make_queries(rng: &mut Rng, docs: &[String], n: usize) -> Vec<String> {
    let mut queries: Vec<String> = Vec::new();
    for i in 0..n {
        let q = match i % 5 {
            0 => rng.pick(ARABIC).to_owned(),
            1 => format!("{} {}", rng.pick(ARABIC), rng.pick(ARABIC)),
            2 => {
                let d = &docs[(rng.next() as usize) % docs.len()];
                let words: Vec<&str> = d.split(' ').filter(|w| w.len() > 2).collect();
                let k = (rng.next() as usize) % (words.len() - 1);
                format!("\"{} {}\"", words[k], words[k + 1])
            }
            3 => rng.pick(ENGLISH).to_owned(),
            _ => "كلمةغيرموجودة".to_owned(),
        };
        queries.push(q);
    }
    queries
}

/// Runs `f` once per query; prints p50 / p95 / max and returns the summed result.
fn timed(label: &str, queries: &[String], mut f: impl FnMut(&str) -> usize) -> usize {
    let mut lat = Vec::new();
    let mut total = 0;
    for q in queries {
        let t = Instant::now();
        total += f(q);
        lat.push(t.elapsed());
    }
    lat.sort();
    println!(
        "{label:<44} p50 {:>10.3?}  p95 {:>10.3?}  max {:>10.3?}  ({total})",
        percentile(&lat, 0.5),
        percentile(&lat, 0.95),
        lat[lat.len() - 1]
    );
    total
}

fn scoped(rng: &mut Rng) {
    const N: usize = 50_000;
    let docs: Vec<String> = (0..N).map(|_| make_doc(rng, 1024)).collect();
    let path = |i: usize| format!("C:/bench/مجلد{:02}/doc{i:05}.txt", i % 100);
    let dir = tempfile::TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    let t = Instant::now();
    for (i, d) in docs.iter().enumerate() {
        let meta = FileMeta {
            size: d.len() as u64,
            mtime: i as i64,
        };
        idx.add_or_update(&path(i), meta, d).unwrap();
    }
    idx.commit().unwrap();
    println!(
        "
--- scoped: {N} documents of ~1 KB, indexed in {:.2?}",
        t.elapsed()
    );

    let queries = make_queries(rng, &docs, 300);
    // 10k paths from the "name side": every 5th document.
    let set: Vec<String> = (0..N).step_by(5).map(path).collect();
    let t = Instant::now();
    let set_filter = PathFilter::paths(&set);
    println!("PathFilter::paths(10k)  built in {:.3?}", t.elapsed());
    let folder = PathFilter::prefix("C:/bench/مجلد07"); // 500 documents

    timed("unscoped top 20", &queries, |q| {
        idx.search(q, 20).unwrap().len()
    });
    timed("within 10k-path set, top 20", &queries, |q| {
        idx.search_within(q, 20, &set_filter).unwrap().len()
    });
    timed("within folder (500 docs), top 20", &queries, |q| {
        idx.search_within(q, 20, &folder).unwrap().len()
    });
    timed("page 6 (offset 100) + total", &queries, |q| {
        let opts = SearchOptions {
            limit: 20,
            offset: 100,
            count_total: true,
            ..SearchOptions::default()
        };
        idx.search_with(q, &opts).unwrap().hits.len()
    });
    timed("count", &queries, |q| idx.count(q).unwrap() as usize);
    timed("count within 10k-path set", &queries, |q| {
        idx.count_within(q, &set_filter).unwrap() as usize
    });

    let t = Instant::now();
    let mut bytes = 0;
    let n = idx.paths(|p, _| bytes += p.len()).unwrap();
    println!("paths()                 {n} paths in {:.2?}", t.elapsed());
    let keep: std::collections::HashSet<&str> = set.iter().map(String::as_str).collect();
    let t = Instant::now();
    let removed = idx.retain(|p, _| keep.contains(p)).unwrap();
    let queued = t.elapsed();
    idx.commit().unwrap();
    println!(
        "retain (keep 10k)       removed {removed} in {queued:.2?} + commit {:.2?}",
        t.elapsed() - queued
    );
    let t = Instant::now();
    let removed = idx.delete_prefix("C:/bench/مجلد05").unwrap();
    idx.commit().unwrap();
    println!(
        "delete_prefix (folder)  removed {removed} in {:.2?} incl. commit",
        t.elapsed()
    );
}

fn main() {
    let n: usize = std::env::args()
        .skip(1)
        .find_map(|a| a.parse().ok())
        .unwrap_or(10_000);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let docs: Vec<String> = (0..n).map(|_| make_doc(&mut rng, 2048)).collect();
    let bytes: usize = docs.iter().map(String::len).sum();

    // Tokenizer alone.
    let t = Instant::now();
    let tokens: usize = docs
        .iter()
        .map(|d| athrix_text_ar::tokenize(d).count())
        .sum();
    let tok_time = t.elapsed();

    let dir = tempfile::TempDir::new().unwrap();
    let idx = ContentIndex::open_or_create(dir.path()).unwrap();
    let t = Instant::now();
    for (i, d) in docs.iter().enumerate() {
        let meta = FileMeta {
            size: d.len() as u64,
            mtime: i as i64,
        };
        idx.add_or_update(&format!("C:/bench/doc{i:05}.txt"), meta, d)
            .unwrap();
    }
    idx.commit().unwrap();
    let index_time = t.elapsed();
    let size = dir_size(dir.path());

    // Queries: single words, two-word AND, phrases taken from documents, English, misses.
    let queries = make_queries(&mut rng, &docs, 300);
    let mut lat = Vec::new();
    let mut total_hits = 0;
    for q in &queries {
        let t = Instant::now();
        total_hits += idx.search(q, 20).unwrap().len();
        lat.push(t.elapsed());
    }
    lat.sort();

    let mb = bytes as f64 / (1 << 20) as f64;
    println!("documents        {n} ({mb:.1} MiB of text, {tokens} tokens)");
    println!("tokenizer        {:.0} MiB/s", mb / tok_time.as_secs_f64());
    println!(
        "indexing         {:.2} s  →  {:.0} docs/s, {:.1} MiB/s (incl. commit)",
        index_time.as_secs_f64(),
        n as f64 / index_time.as_secs_f64(),
        mb / index_time.as_secs_f64()
    );
    println!(
        "index on disk    {:.1} MiB ({:.0}% of the text; text stored for snippets)",
        size as f64 / (1 << 20) as f64,
        100.0 * size as f64 / bytes as f64
    );
    println!(
        "query (top 20 + snippets, {} queries, {} hits)  p50 {:?}  p95 {:?}  max {:?}",
        queries.len(),
        total_hits,
        percentile(&lat, 0.5),
        percentile(&lat, 0.95),
        lat[lat.len() - 1]
    );
    drop(idx);
    scoped(&mut rng);
}
