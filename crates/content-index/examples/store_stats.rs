//! How much stored preview text a smaller `stored_text_limit` would keep.
//!
//! Usage: `cargo run --release -p athrix-content-index --example store_stats -- <index dir>`
//!
//! Reads every stored preview of a content index (read only) and prints the total bytes and
//! the bytes kept with each candidate limit.

use tantivy::schema::Value;
use tantivy::{Index, TantivyDocument};

const LIMITS: [usize; 5] = [16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10];

fn main() {
    let dir = std::env::args().nth(1).expect("content index directory");
    let index = Index::open_in_dir(&dir).unwrap();
    let preview = index.schema().get_field("preview").unwrap();
    let searcher = index.reader().unwrap().searcher();
    let (mut docs, mut total, mut over) = (0u64, 0u64, [0u64; LIMITS.len()]);
    let mut kept = [0u64; LIMITS.len()];
    for segment in searcher.segment_readers() {
        let store = segment.get_store_reader(64).unwrap();
        for doc in store.iter::<TantivyDocument>(segment.alive_bitset()) {
            let doc = doc.unwrap();
            let len = doc
                .get_first(preview)
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            docs += 1;
            total += len as u64;
            for (i, &l) in LIMITS.iter().enumerate() {
                kept[i] += len.min(l) as u64;
                if len > l {
                    over[i] += 1;
                }
            }
        }
    }
    println!("{docs} documents, {} MiB of preview text", total >> 20);
    for (i, l) in LIMITS.iter().enumerate() {
        println!(
            "limit {:>3} KiB: keeps {:>6} MiB ({:.0}%), cuts {} documents",
            l >> 10,
            kept[i] >> 20,
            100.0 * kept[i] as f64 / total.max(1) as f64,
            over[i]
        );
    }
}
