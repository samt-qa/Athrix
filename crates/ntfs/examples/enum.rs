//! Enumerates an NTFS volume and prints entry count and timings.
//!
//! ```text
//! cargo run -p athrix-ntfs --example enum --release -- C: [--no-metadata] [--sample N]
//! cargo run -p athrix-ntfs --example enum --release -- --synthetic 6400000
//! ```
//!
//! `--synthetic N` needs no privileges: it encodes N fake `USN_RECORD_V2`s into 1 MiB buffers
//! (shaped like FSCTL_ENUM_USN_DATA output, children often before parents) and times the CPU
//! side only (record parsing + tree building), i.e. everything except the kernel calls.
//!
//! Needs an elevated (Administrator) shell; otherwise it prints the typed fallback error
//! (`AccessDenied`) and exits with code 2, which is what the app turns into "use the walk".
//!
//! Timing note (no criterion: a full-volume scan is a single, I/O-bound, multi-second run that
//! a statistical harness would only repeat against a warm cache). Run it twice and report the
//! second (warm) and the first after a reboot (cold) separately; compare with the jwalk walk of
//! the same drive (`athrix-fs-index` `walk` example). Phases are timed separately:
//! `enum` = FSCTL_ENUM_USN_DATA, `tree` = parent resolution + BFS ordering,
//! `mft` = sequential $MFT read + parse for sizes/mtimes.

use std::process::ExitCode;
use std::time::Instant;

use athrix_ntfs::record::split_output;
use athrix_ntfs::tree::TreeBuilder;
use athrix_ntfs::{EnumOptions, NtfsError, Volume, is_elevated};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut drive = None;
    let mut opts = EnumOptions::default();
    let mut sample = 5usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--no-metadata" => opts.metadata = false,
            "--sample" => sample = args.next().and_then(|n| n.parse().ok()).unwrap_or(5),
            "--synthetic" => {
                let n = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(1_000_000);
                synthetic(n);
                return ExitCode::SUCCESS;
            }
            _ => drive = Some(a),
        }
    }
    let drive = drive.unwrap_or_else(|| "C:".into());
    match is_elevated() {
        Ok(e) => println!("elevated: {e}"),
        Err(e) => println!("elevated: unknown ({e})"),
    }

    let total = Instant::now();
    let volume = match Volume::open(&drive) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    match volume.query_journal() {
        Ok(j) => println!(
            "journal: id {:#x}, usn {}..{}, max size {} MiB",
            j.journal_id,
            j.first_usn,
            j.next_usn,
            j.maximum_size >> 20
        ),
        Err(e) => println!("journal: {e}"),
    }
    let (tree, stats) = match volume.enumerate(&opts) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let elapsed = total.elapsed();

    let dirs = tree.iter().filter(|e| e.is_dir()).count();
    println!(
        "{}: {} entries ({} dirs, {} files), {} orphans dropped, {} USN records in {} calls",
        volume.name(),
        tree.len(),
        dirs,
        tree.len() - dirs,
        tree.orphan_count(),
        stats.records,
        stats.enum_calls
    );
    println!(
        "time: total {:.2?} = enum {:.2?} + tree {:.2?} + mft {:.2?} ({} MiB, {} matched, {} bad)",
        elapsed,
        stats.enum_time,
        stats.tree_time,
        stats.mft_time,
        stats.mft_bytes >> 20,
        stats.mft_matched,
        stats.mft_bad_records
    );
    if let Some(e) = &stats.metadata_error {
        println!("metadata unavailable: {e}");
    }
    println!(
        "memory: ~{} MiB ({} name bytes)",
        tree.heap_bytes() >> 20,
        tree.name_bytes()
    );
    let step = (tree.len() / sample.max(1)).max(1);
    for e in tree.iter().step_by(step).take(sample) {
        println!(
            "  {:>12} {:>12} {}",
            e.size.map_or("-".into(), |s| s.to_string()),
            e.modified.map_or("-".into(), |m| m.to_string()),
            tree.path(e.index).display()
        );
    }
    ExitCode::SUCCESS
}

fn fail(e: &NtfsError) -> ExitCode {
    eprintln!("error: {e}");
    eprintln!("debug: {e:?}");
    if e.should_fall_back() {
        eprintln!("fallback: use the directory walk for this volume");
        ExitCode::from(2)
    } else {
        ExitCode::FAILURE
    }
}

/// Encodes one `USN_RECORD_V2` (8-byte aligned) into `out`.
fn encode_v2(out: &mut Vec<u8>, file_ref: u64, parent_ref: u64, attributes: u32, name: &str) {
    let start = out.len();
    out.extend_from_slice(&[0; 60]);
    for u in name.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    let name_len = (out.len() - start - 60) as u16;
    out.resize(start + (out.len() - start).next_multiple_of(8), 0);
    let len = (out.len() - start) as u32;
    let r = &mut out[start..];
    r[0..4].copy_from_slice(&len.to_le_bytes());
    r[4..6].copy_from_slice(&2u16.to_le_bytes());
    r[8..16].copy_from_slice(&file_ref.to_le_bytes());
    r[16..24].copy_from_slice(&parent_ref.to_le_bytes());
    r[52..56].copy_from_slice(&attributes.to_le_bytes());
    r[56..58].copy_from_slice(&name_len.to_le_bytes());
    r[58..60].copy_from_slice(&60u16.to_le_bytes());
}

/// Times parsing + tree building for `n` synthetic records (~1 directory per 12 entries).
fn synthetic(n: u64) {
    const ROOT: u64 = (5 << 48) | 5;
    let file_ref = |rec: u64| (1u64 << 48) | (rec + 16);
    // Pre-encode buffers so only parsing and tree building are timed.
    let mut buffers = Vec::new();
    let mut buf = 0u64.to_le_bytes().to_vec();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let is_dir = i % 12 == 0;
        // Parent: a random earlier directory (no cycles: every entry is reachable).
        let parent = if i < 12 {
            ROOT
        } else {
            file_ref((state % i) / 12 * 12)
        };
        let name = format!("file-{i:x}_{}.txt", state % 1000);
        encode_v2(
            &mut buf,
            file_ref(i),
            parent,
            if is_dir { 0x10 } else { 0x20 },
            &name,
        );
        if buf.len() > (1 << 20) - 600 {
            buffers.push(std::mem::replace(&mut buf, 0u64.to_le_bytes().to_vec()));
        }
    }
    buffers.push(buf);

    let t = Instant::now();
    let mut builder = TreeBuilder::with_capacity(n as usize, 24);
    for b in &buffers {
        let (_, records) = split_output(b).expect("well-formed");
        for rec in records.flatten().flatten() {
            builder.push_record(&rec);
        }
    }
    let parse = t.elapsed();
    let t = Instant::now();
    let tree = builder.finish("C:\\");
    let finish = t.elapsed();
    println!(
        "synthetic: {n} records in {} buffers -> {} entries ({} orphans); parse {:.2?}, tree {:.2?}, ~{} MiB",
        buffers.len(),
        tree.len(),
        tree.orphan_count(),
        parse,
        finish,
        tree.heap_bytes() >> 20
    );
}
