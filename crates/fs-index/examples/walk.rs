//! Walks real folders and runs one query, printing timings.
//!
//! `cargo run -p athrix-fs-index --release --example walk -- <query> <root>...`

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use athrix_fs_index::{Query, WalkOptions, walk};

fn main() -> athrix_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let query = args.next().unwrap_or_default();
    let roots: Vec<String> = args.collect();
    let t = Instant::now();
    let index = walk(&WalkOptions::new(&roots))?;
    println!(
        "indexed {} entries in {:?}, heap {:.1} MB",
        index.len(),
        t.elapsed(),
        index.heap_bytes() as f64 / 1e6
    );
    let t = Instant::now();
    let hits = index.search(&Query::parse(&query)?, 20, &AtomicBool::new(false))?;
    println!("query {query:?}: {} hits in {:?}", hits.len(), t.elapsed());
    for id in hits {
        println!("  {}", index.path(id).display());
    }
    Ok(())
}
