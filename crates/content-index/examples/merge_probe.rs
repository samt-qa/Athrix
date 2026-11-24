//! Merges segments of a content index copy, to watch the memory of a merge from outside.
//!
//! `cargo run --release -p athrix-content-index --example merge_probe -- <index copy> [n]`
//!
//! Merges the `n` (default 2) biggest segments. Never point it at the app's own index.

use std::time::Instant;

use tantivy::{Index, IndexWriter, TantivyDocument};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().ok_or("usage: merge_probe <index copy> [n]")?;
    let n: usize = args.next().map_or(Ok(2), |s| s.parse())?;
    let index = Index::open_in_dir(&dir)?;
    let mut metas = index.searchable_segment_metas()?;
    metas.sort_by_key(|m| std::cmp::Reverse(m.num_docs()));
    for m in &metas {
        println!(
            "segment {} docs {} deleted {}",
            m.id().short_uuid_string(),
            m.num_docs(),
            m.num_deleted_docs()
        );
    }
    let ids: Vec<_> = metas.iter().take(n).map(|m| m.id()).collect();
    let mut writer: IndexWriter<TantivyDocument> = index.writer_with_num_threads(1, 64 << 20)?;
    println!("writer open; waiting 5 s");
    std::thread::sleep(std::time::Duration::from_secs(5));
    println!("merge of {} segments starts", ids.len());
    let started = Instant::now();
    let merged = writer.merge(&ids).wait()?;
    println!(
        "merge done in {:.1} s: {:?}",
        started.elapsed().as_secs_f64(),
        merged.map(|m| m.num_docs())
    );
    std::thread::sleep(std::time::Duration::from_secs(5));
    Ok(())
}
