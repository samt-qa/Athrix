//! Athrix command-line interface: index roots (walk or NTFS enumeration), save and search
//! snapshots, and print names as the index normalizes them. Mostly a measuring and debugging
//! tool for now; the full CLI comes in M7 (task 7.1).

mod bench;
mod interpret;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use athrix_fs_index::ntfs::{IndexMethod, NtfsIndexError, RootMethod, index_roots, is_elevated};
use athrix_fs_index::{FileIndex, Query, WalkOptions};

const USAGE: &str = "usage:
  athrix --version
  athrix normalize <text>
      print <text> as the name index stores it
  athrix interpret <TEXT>... [--explain] [--today YYYY-MM-DD]
      read plain Arabic or English as filters («ملفات PDF من الأسبوع الماضي» →
      ext:pdf dm:lastweek); prints the query, or exits 1 when there is nothing to suggest
  athrix index <ROOT>... [--method auto|walk|ntfs] [--save FILE] [--exclude GLOB]...
               [--no-default-excludes]
      index the roots and print entries, time, method per root and memory.
      auto: NTFS enumeration for whole drives (C:\\) when elevated, walk otherwise;
      ntfs: NTFS only (fails when not elevated); walk: directory walk only.
      The app's default exclusions (.git, node_modules, $RECYCLE.BIN, ...) apply unless
      --no-default-excludes is given.
  athrix search <SNAPSHOT> <QUERY>... [--limit N]
      load a snapshot written by `index --save` and print the matching paths
  athrix bench names <SNAPSHOT> [--rounds N] [--limit N] [--query TEXT]...
      load a snapshot (read only) and time realistic typing, keystroke by keystroke
      (--query: type these texts instead of the built-in list)
  athrix bench content <ROOT>... --index-dir DIR [--max-files N] [--max-bytes N]
               [--names SNAPSHOT] [--rounds N] [--reuse]
      extract documents under the roots into a scratch content index, then time content
      queries (see docs/PERF-AUDIT.md)";

/// Same defaults as the desktop app (`apps/desktop/src-tauri/src/indexing.rs`), so CLI
/// measurements index what the app indexes.
const DEFAULT_EXCLUDED_NAMES: [&str; 8] = [
    ".git",
    "node_modules",
    "$RECYCLE.BIN",
    "System Volume Information",
    "$WinREAgent",
    "$Windows.~BT",
    "$Windows.~WS",
    "$SysReset",
];
const DEFAULT_EXCLUDED_PATH_GLOBS: [&str; 2] = ["**/AppData/Local/Temp", "**/Windows/WinSxS"];

fn main() -> ExitCode {
    athrix_core::logging::init("warn");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{} {}", athrix_core::APP_NAME, athrix_core::VERSION);
            Ok(ExitCode::SUCCESS)
        }
        Some("normalize") if args.len() > 1 => {
            println!("{}", athrix_text_ar::normalize_name(&args[1..].join(" ")));
            Ok(ExitCode::SUCCESS)
        }
        Some("index") => cmd_index(&args[1..]),
        Some("search") => cmd_search(&args[1..]),
        Some("bench") => bench::cmd_bench(&args[1..]),
        Some("interpret") => interpret::cmd_interpret(&args[1..]),
        _ => Err(Usage(None).into()),
    };
    match result {
        Ok(code) => code,
        Err(CliError::Usage(Usage(msg))) => {
            if let Some(msg) = msg {
                eprintln!("error: {msg}\n");
            }
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
        Err(CliError::Other(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

struct Usage(Option<String>);

enum CliError {
    Usage(Usage),
    Other(String),
}

impl From<Usage> for CliError {
    fn from(u: Usage) -> Self {
        CliError::Usage(u)
    }
}

impl From<athrix_core::Error> for CliError {
    fn from(e: athrix_core::Error) -> Self {
        CliError::Other(e.to_string())
    }
}

fn usage(msg: impl Into<String>) -> CliError {
    Usage(Some(msg.into())).into()
}

fn value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<&'a str, CliError> {
    it.next()
        .map(String::as_str)
        .ok_or_else(|| usage(format!("{flag} needs a value")))
}

fn cmd_index(args: &[String]) -> Result<ExitCode, CliError> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut method = IndexMethod::Auto;
    let mut save: Option<PathBuf> = None;
    let mut excludes: Vec<String> = Vec::new();
    let mut defaults = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--method" => {
                method = match value(&mut it, "--method")? {
                    "auto" => IndexMethod::Auto,
                    "walk" => IndexMethod::Walk,
                    "ntfs" => IndexMethod::Ntfs,
                    other => return Err(usage(format!("unknown method {other:?}"))),
                }
            }
            "--save" => save = Some(value(&mut it, "--save")?.into()),
            "--exclude" => excludes.push(value(&mut it, "--exclude")?.into()),
            "--no-default-excludes" => defaults = false,
            flag if flag.starts_with("--") => {
                return Err(usage(format!("unknown option {flag}")));
            }
            root => roots.push(root.into()),
        }
    }
    if roots.is_empty() {
        return Err(usage("index needs at least one root"));
    }
    let mut opts = WalkOptions::new(&roots);
    if defaults {
        for g in DEFAULT_EXCLUDED_NAMES
            .iter()
            .chain(&DEFAULT_EXCLUDED_PATH_GLOBS)
        {
            opts = opts.exclude_glob(*g);
        }
    }
    for g in excludes {
        opts = opts.exclude_glob(g);
    }

    match is_elevated() {
        Ok(e) => println!("elevated: {e}"),
        Err(_) => println!("elevated: n/a"),
    }
    let start = Instant::now();
    let done = match index_roots(&opts, method) {
        Ok(d) => d,
        Err(NtfsIndexError::Ntfs(e)) => {
            eprintln!("error: {e}");
            if e.should_fall_back() {
                eprintln!(
                    "NTFS enumeration is not available here; run from an elevated \
                     (Administrator) terminal for whole NTFS drives such as C:\\, or use \
                     --method auto (falls back to the directory walk) or --method walk."
                );
                return Ok(ExitCode::from(2));
            }
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(CliError::Other(e.to_string())),
    };
    let total = start.elapsed();

    for r in &done.roots {
        println!(
            "{}: {} entries in {:.2?} via {}",
            r.root.display(),
            r.entries,
            r.elapsed,
            r.method.name()
        );
        match &r.method {
            RootMethod::Walk {
                fallback: Some(e), ..
            } => {
                println!("  ntfs unavailable, fell back to walk: {e}");
            }
            RootMethod::Walk { .. } => {}
            RootMethod::Ntfs {
                stats,
                push,
                push_time,
            } => {
                println!(
                    "  enum {:.2?} ({} records, {} calls) + tree {:.2?} + mft {:.2?} ({} MiB, \
                     {} matched, {} bad) + index {:.2?}",
                    stats.enum_time,
                    stats.records,
                    stats.enum_calls,
                    stats.tree_time,
                    stats.mft_time,
                    stats.mft_bytes >> 20,
                    stats.mft_matched,
                    stats.mft_bad_records,
                    push_time
                );
                println!(
                    "  excluded {} (+{} below them), {} NTFS metadata files skipped",
                    push.excluded, push.below_excluded, push.metadata_files
                );
                if let Some(e) = &stats.metadata_error {
                    println!("  sizes/dates unavailable: {e}");
                }
            }
        }
    }
    let index = &done.index;
    println!(
        "total: {} entries in {:.2?}, heap {} bytes ({:.1} MiB)",
        index.len(),
        total,
        index.heap_bytes(),
        index.heap_bytes() as f64 / (1024.0 * 1024.0)
    );
    if let Some(path) = save {
        let t = Instant::now();
        index.save(&path)?;
        println!("saved {} in {:.2?}", path.display(), t.elapsed());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_search(args: &[String]) -> Result<ExitCode, CliError> {
    let mut limit = 50usize;
    let mut positional: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--limit" => {
                limit = value(&mut it, "--limit")?
                    .parse()
                    .map_err(|_| usage("--limit needs a number"))?;
            }
            other => positional.push(other),
        }
    }
    let [snapshot, query @ ..] = positional.as_slice() else {
        return Err(usage("search needs a snapshot and a query"));
    };
    if query.is_empty() {
        return Err(usage("search needs a query"));
    }
    let query_text = query.join(" ");
    let t = Instant::now();
    let index = FileIndex::load(std::path::Path::new(snapshot))?;
    let load = t.elapsed();
    let query = Query::parse(&query_text).map_err(|e| CliError::Other(e.to_string()))?;
    let t = Instant::now();
    let hits = index.search(&query, limit, &AtomicBool::new(false))?;
    let search = t.elapsed();
    for &id in &hits {
        println!("{}", index.path(id).display());
    }
    eprintln!(
        "{} hit(s) (limit {limit}) among {} entries; load {load:.2?}, search {search:.2?}",
        hits.len(),
        index.len()
    );
    Ok(ExitCode::SUCCESS)
}
