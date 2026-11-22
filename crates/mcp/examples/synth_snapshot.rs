//! Writes a synthetic Athrix data folder for measuring the MCP server's memory:
//! `<data>/index/names.snap` with `entries` entries, of which about `percent`% lie under the
//! allowed folder `<data>/tree/allowed`, and an enabled `<data>/mcp.json` allowing that folder.
//!
//! ```text
//! cargo run --release -p athrix-mcp-server --example synth_snapshot -- <data-dir> [entries] [percent]
//! ```
//!
//! Then run `athrix-mcp --data-dir <data-dir> --no-log`, send a `search_files` call and watch the
//! process memory.

use std::path::PathBuf;

use athrix_fs_index::{EntryMeta, IndexBuilder};
use athrix_mcp::McpConfig;
use athrix_mcp::config::CONFIG_FILE;

/// Files per synthetic folder.
const PER_FOLDER: usize = 200;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(data) = args.next().map(PathBuf::from) else {
        eprintln!("usage: synth_snapshot <data-dir> [entries] [percent-allowed]");
        std::process::exit(2);
    };
    let entries: usize = args
        .next()
        .and_then(|a| a.to_str()?.parse().ok())
        .unwrap_or(5_000_000);
    let percent: usize = args
        .next()
        .and_then(|a| a.to_str()?.parse().ok())
        .unwrap_or(5);

    let tree = data.join("tree");
    let allowed = tree.join("allowed");
    std::fs::create_dir_all(&allowed).expect("create the allowed folder");
    std::fs::create_dir_all(tree.join("other")).expect("create the other folder");

    let dir = EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    };
    let mut b = IndexBuilder::with_capacity(entries, 40);
    let root = b.push(None, &tree.to_string_lossy(), dir);
    let allowed_id = b.push(Some(root), "allowed", dir);
    let other_id = b.push(Some(root), "other", dir);
    let inside = entries * percent / 100;
    let mut folder = None;
    let mut i = 0usize;
    while b.len() < entries {
        let parent = if b.len() < inside {
            allowed_id
        } else {
            other_id
        };
        if i % PER_FOLDER == 0 || folder.is_none_or(|(p, _)| p != parent) {
            let f = b.push(
                Some(parent),
                &format!("مجلد المشروع {}", i / PER_FOLDER),
                dir,
            );
            folder = Some((parent, f));
        }
        let (_, f) = folder.expect("set above");
        let meta = EntryMeta {
            size: (i as u64 * 7919) % 50_000_000,
            modified: Some(1_600_000_000 + (i as i64 % 100_000_000)),
            ..EntryMeta::default()
        };
        let name = match i % 3 {
            0 => format!("تقرير الميزانية السنوية {i}.pdf"),
            1 => format!("IMG_{i:08}_holiday-photo.jpg"),
            _ => format!("notes-{i}-مسودة.docx"),
        };
        b.push(Some(f), &name, meta);
        i += 1;
    }
    let index = b.finish();
    let snap_dir = data.join("index");
    std::fs::create_dir_all(&snap_dir).expect("create the index folder");
    index
        .save(&snap_dir.join("names.snap"))
        .expect("save the snapshot");
    McpConfig {
        enabled: true,
        allowed_folders: vec![allowed.to_string_lossy().into_owned()],
        allow_content_read: false,
        max_results: 50,
    }
    .save(&data.join(CONFIG_FILE))
    .expect("save mcp.json");
    println!(
        "{} entries ({} under {}), {} heap bytes",
        index.len(),
        inside,
        allowed.display(),
        index.heap_bytes()
    );
}
