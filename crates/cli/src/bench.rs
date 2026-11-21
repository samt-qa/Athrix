//! `athrix bench …`: the measurements behind `docs/PERF-AUDIT.md` (task 5.8).
//!
//! - `bench names <SNAPSHOT>`: loads a name snapshot (read only), reports load time, heap and
//!   process memory, then replays realistic typing sequences keystroke by keystroke through
//!   the same steps as the desktop app's name fast path (`athrix_query::parse` →
//!   `Query::compile` → search for `limit + 1` ids → path + metadata of each hit) and prints
//!   p50 / p95 / p99 per keystroke and overall.
//! - `bench content <ROOT>…`: walks the roots, extracts up to `--max-files` documents with
//!   `athrix-extract` on all cores, builds a content index in `--index-dir`, then times word,
//!   phrase, Arabic and name + content (`AND`) queries through `athrix_ranking::execute`, like
//!   the app's content path, then snippets for the first 30 content rows (what the app's
//!   `search` command returns), and reports each phase.
//!
//! Both are measuring tools only: nothing here changes what the product does.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use athrix_content_index::{ContentIndex, FileMeta, IndexOptions};
use athrix_extract::{Kind, Limits};
use athrix_fs_index::{FileIndex, Query, WalkOptions, walk};
use athrix_query::{PlanOptions, SearchPlan};
use athrix_ranking::{Closures, ContentHit, ExecOptions, NameHit, NameResults, execute};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{CliError, usage, value};

/// Results per query in the desktop app (`LIMIT` in `apps/desktop/src/App.tsx`).
const APP_LIMIT: usize = 200;
/// The app's name cap for name + content plans (`NAME_CAP` in the app's `content.rs`).
const NAME_CAP: usize = 20_000;
/// The app's content cap (`CONTENT_CAP`).
const CONTENT_CAP: usize = 10_000;
/// Rows whose snippets the app's `search` builds (`SNIPPET_ROWS` in the app's `content.rs`).
const SNIPPET_ROWS: usize = 30;

/// What a user types, one query per keystroke (every prefix of each string).
const TYPING: &[&str] = &[
    "report",
    "تقرير",
    "invoice",
    "فاتورة",
    "*.pdf",
    "ext:docx size:>1mb",
    "budget 2024",
    "type:image vacation",
    "fuzzy:rprt",
];

/// Process resident memory (working set on Windows), in bytes.
fn rss_bytes() -> Option<u64> {
    let pid = sysinfo::get_current_pid().ok()?;
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map(sysinfo::Process::memory)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `p` in `[0, 100]` of sorted `v` (nearest rank).
fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v[rank.clamp(1, v.len()) - 1]
}

fn summary(v: &mut [f64]) -> String {
    v.sort_by(f64::total_cmp);
    format!(
        "p50 {:>7.2} ms  p95 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms  (n={})",
        pct(v, 50.0),
        pct(v, 95.0),
        pct(v, 99.0),
        v.last().copied().unwrap_or(f64::NAN),
        v.len()
    )
}

/// Prefixes of `s` on char boundaries, one per keystroke, skipping whitespace-only tails.
fn keystrokes(s: &str) -> Vec<&str> {
    s.char_indices()
        .map(|(i, c)| &s[..i + c.len_utf8()])
        .filter(|p| !p.ends_with(' '))
        .collect()
}

/// `athrix bench <names|content> …`.
pub fn cmd_bench(args: &[String]) -> Result<std::process::ExitCode, CliError> {
    match args.first().map(String::as_str) {
        Some("names") => bench_names(&args[1..]),
        Some("content") => bench_content(&args[1..]),
        _ => Err(usage("bench needs `names` or `content`")),
    }
}

/// One keystroke the way the app's name fast path runs it. Returns the number of items.
fn name_keystroke(index: &FileIndex, text: &str, limit: usize) -> Option<usize> {
    let ast = athrix_query::parse(text).ok()?;
    let plan = athrix_query::plan(&ast, &PlanOptions::default()).ok()?;
    if plan.uses_content() {
        return None;
    }
    let query = Query::compile(&ast).ok()?;
    let cancel = AtomicBool::new(false);
    let ids = index.search(&query, limit + 1, &cancel).ok()?;
    let mut items = 0;
    for &id in ids.iter().take(limit) {
        // What `build_item` reads for each row.
        let path = index.path(id);
        let meta = index.meta(id);
        std::hint::black_box((path, meta, query.fuzzy_highlights(index.name(id))));
        items += 1;
    }
    if items == 0 {
        suggestion_probes(index, text, &query, limit, &cancel);
    }
    Some(items)
}

/// The app's "did you mean" probes after an empty result (`suggest.rs`): the words retyped on
/// the other keyboard layout, then the query with the `fuzzy:` switch, each a count-only
/// search, stopping at the first that finds something. Approximation: a token is swapped
/// whole unless it holds `:` (a filter); the app swaps the parsed terms only.
fn suggestion_probes(index: &FileIndex, text: &str, query: &Query, limit: usize, c: &AtomicBool) {
    let mut changed = false;
    let swapped: Vec<String> = text
        .split(' ')
        .map(
            |tok| match (tok.contains(':'), athrix_text_ar::swap_layout(tok)) {
                (false, Some(s)) => {
                    changed = true;
                    s
                }
                _ => tok.to_owned(),
            },
        )
        .collect();
    let layout = changed.then(|| swapped.join(" "));
    let fuzzy = (!query.is_ranked()).then(|| format!("fuzzy: {}", text.trim_start()));
    for candidate in [layout, fuzzy].into_iter().flatten() {
        let Ok(ast) = athrix_query::parse(&candidate) else {
            continue;
        };
        let Ok(q) = Query::compile(&ast) else {
            continue;
        };
        if index.search(&q, limit, c).is_ok_and(|ids| !ids.is_empty()) {
            return;
        }
    }
}

fn bench_names(args: &[String]) -> Result<std::process::ExitCode, CliError> {
    let mut rounds = 5usize;
    let mut limit = APP_LIMIT;
    let mut snapshot: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rounds" => {
                rounds = value(&mut it, "--rounds")?
                    .parse()
                    .map_err(|_| usage("--rounds needs a number"))?;
            }
            "--limit" => {
                limit = value(&mut it, "--limit")?
                    .parse()
                    .map_err(|_| usage("--limit needs a number"))?;
            }
            other => snapshot = Some(other.into()),
        }
    }
    let Some(snapshot) = snapshot else {
        return Err(usage("bench names needs a snapshot file"));
    };

    let rss0 = rss_bytes().unwrap_or(0);
    let t = Instant::now();
    let index = FileIndex::load(&snapshot)?;
    let load = t.elapsed();
    let rss1 = rss_bytes().unwrap_or(0);
    let n = index.len();
    let per_m = |b: u64| mib(b) * 1_000_000.0 / n.max(1) as f64;
    println!("entries: {n}");
    println!("load: {:.2} ms", ms(load));
    println!(
        "heap_bytes: {:.1} MiB ({:.1} MiB per 1M entries)",
        mib(index.heap_bytes() as u64),
        per_m(index.heap_bytes() as u64)
    );
    println!(
        "rss: before {:.1} MiB, after load {:.1} MiB, delta {:.1} MiB ({:.1} MiB per 1M entries)",
        mib(rss0),
        mib(rss1),
        mib(rss1.saturating_sub(rss0)),
        per_m(rss1.saturating_sub(rss0))
    );

    // Warm the rayon pool once (the app's pool is long-lived).
    let _ = name_keystroke(&index, "warmup", limit);

    let mut all = Vec::new();
    let mut first_keys = Vec::new();
    for word in TYPING {
        println!("\n== {word:?}");
        for key in keystrokes(word) {
            let mut times = Vec::with_capacity(rounds);
            let mut items = None;
            for _ in 0..rounds {
                let t = Instant::now();
                items = name_keystroke(&index, key, limit);
                times.push(ms(t.elapsed()));
            }
            times.sort_by(f64::total_cmp);
            let shown = items.map_or_else(|| "invalid/none".to_owned(), |i| i.to_string());
            println!(
                "  {key:<22} items {shown:>12}  p50 {:>7.2} ms  max {:>7.2} ms",
                pct(&times, 50.0),
                times.last().copied().unwrap_or(f64::NAN)
            );
            if key.chars().count() == 1 {
                first_keys.extend_from_slice(&times);
            }
            all.extend(times);
        }
    }
    println!("\nall keystrokes:   {}", summary(&mut all));
    println!("first keystroke:  {}", summary(&mut first_keys));
    println!("rss at end: {:.1} MiB", mib(rss_bytes().unwrap_or(0)));
    Ok(std::process::ExitCode::SUCCESS)
}

/// Default exclusions while picking documents (same intent as `index`).
fn corpus_walk_options(roots: &[PathBuf]) -> WalkOptions {
    let mut opts = WalkOptions::new(roots);
    for g in crate::DEFAULT_EXCLUDED_NAMES
        .iter()
        .chain(&crate::DEFAULT_EXCLUDED_PATH_GLOBS)
    {
        opts = opts.exclude_glob(*g);
    }
    opts
}

fn bench_content(args: &[String]) -> Result<std::process::ExitCode, CliError> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut max_files = 20_000usize;
    let mut max_bytes = 4u64 << 20;
    let mut rounds = 20usize;
    let mut index_dir: Option<PathBuf> = None;
    let mut names: Option<PathBuf> = None;
    let mut reuse = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--max-files" => {
                max_files = value(&mut it, "--max-files")?
                    .parse()
                    .map_err(|_| usage("--max-files needs a number"))?;
            }
            "--max-bytes" => {
                max_bytes = value(&mut it, "--max-bytes")?
                    .parse()
                    .map_err(|_| usage("--max-bytes needs a number"))?;
            }
            "--rounds" => {
                rounds = value(&mut it, "--rounds")?
                    .parse()
                    .map_err(|_| usage("--rounds needs a number"))?;
            }
            "--index-dir" => index_dir = Some(value(&mut it, "--index-dir")?.into()),
            "--names" => names = Some(value(&mut it, "--names")?.into()),
            "--reuse" => reuse = true,
            flag if flag.starts_with("--") => {
                return Err(usage(format!("unknown option {flag}")));
            }
            root => roots.push(root.into()),
        }
    }
    let Some(index_dir) = index_dir else {
        return Err(usage(
            "bench content needs --index-dir DIR (a scratch directory)",
        ));
    };
    if roots.is_empty() {
        return Err(usage("bench content needs at least one root"));
    }

    let t = Instant::now();
    let walked = walk(&corpus_walk_options(&roots))?;
    println!(
        "walk: {} entries in {:.2} s",
        walked.len(),
        t.elapsed().as_secs_f64()
    );
    let name_index = match &names {
        Some(p) => {
            let t = Instant::now();
            let ix = FileIndex::load(p)?;
            println!(
                "names: {} entries from {} in {:.2} s",
                ix.len(),
                p.display(),
                t.elapsed().as_secs_f64()
            );
            ix
        }
        None => walked,
    };

    let content = if reuse {
        ContentIndex::open_or_create(&index_dir)
            .map_err(|e| CliError::Other(format!("content index: {e}")))?
    } else {
        let content = ContentIndex::recreate(&index_dir, IndexOptions::default())
            .map_err(|e| CliError::Other(format!("content index: {e}")))?;
        build_corpus(&content, &name_index, &roots, max_files, max_bytes)?;
        content
    };
    println!("documents: {}", content.num_docs());
    query_content(&content, &name_index, rounds)?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// Picks the content candidates under `roots` from `index`, extracts them on all cores and
/// adds them to `content`.
fn build_corpus(
    content: &ContentIndex,
    index: &FileIndex,
    roots: &[PathBuf],
    max_files: usize,
    max_bytes: u64,
) -> Result<(), CliError> {
    let mut files: Vec<(PathBuf, FileMeta)> = Vec::new();
    let mut by_kind: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for id in 0..index.len() {
        let Ok(id) = u32::try_from(id) else { break };
        let meta = index.meta(id);
        if meta.is_dir || meta.size == 0 || meta.size > max_bytes {
            continue;
        }
        let Some(kind) = Kind::from_path(Path::new(index.name(id))) else {
            continue;
        };
        if !kind.is_supported() {
            continue;
        }
        let path = index.path(id);
        if !roots.iter().any(|r| path.starts_with(r)) {
            continue;
        }
        *by_kind.entry(kind.name()).or_default() += 1;
        files.push((
            path,
            FileMeta {
                size: meta.size,
                mtime: meta.modified.unwrap_or(0),
            },
        ));
    }
    println!("candidates: {} {by_kind:?}", files.len());
    // An even sample over the whole tree rather than the first folders.
    if files.len() > max_files {
        let step = files.len() as f64 / max_files as f64;
        files = (0..max_files)
            .map(|i| files[(i as f64 * step) as usize].clone())
            .collect();
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = AtomicUsize::new(0);
    let stats = Mutex::new((0usize, 0usize, 0u64, Duration::ZERO));
    let limits = Limits::default();
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, meta)) = files.get(i) else {
                        break;
                    };
                    let te = Instant::now();
                    let res = athrix_extract::extract(path, &limits);
                    let took = te.elapsed();
                    let mut st = stats.lock().unwrap_or_else(|e| e.into_inner());
                    st.3 += took;
                    match res {
                        Ok(x) => {
                            st.0 += 1;
                            st.2 += x.text.len() as u64;
                            drop(st);
                            let _ = content.add_or_update(&path.to_string_lossy(), *meta, &x.text);
                        }
                        Err(_) => st.1 += 1,
                    }
                }
            });
        }
    });
    let extract_wall = t.elapsed();
    let t = Instant::now();
    content
        .commit()
        .map_err(|e| CliError::Other(format!("commit: {e}")))?;
    let commit = t.elapsed();
    let (ok, failed, text_bytes, cpu) = *stats.lock().unwrap_or_else(|e| e.into_inner());
    println!(
        "extract+index: {ok} ok, {failed} failed, {:.1} MiB text, wall {:.1} s \
         ({threads} threads, extract cpu {:.1} s), commit {:.1} s, {:.0} docs/s",
        mib(text_bytes),
        extract_wall.as_secs_f64(),
        cpu.as_secs_f64(),
        commit.as_secs_f64(),
        ok as f64 / (extract_wall + commit).as_secs_f64()
    );
    Ok(())
}

/// The query sets of the content measurement (app syntax).
const CONTENT_QUERIES: &[(&str, &[&str])] = &[
    (
        "word",
        &[
            "content:report",
            "content:invoice",
            "content:the",
            "content:function",
            "content:customer",
            "content:error",
        ],
    ),
    (
        "phrase",
        &[
            "content:\"annual report\"",
            "content:\"best regards\"",
            "content:\"terms and conditions\"",
            "content:\"return value\"",
        ],
    ),
    (
        "arabic",
        &[
            "content:تقرير",
            "content:التقارير",
            "content:وزارة",
            "content:\"بسم الله\"",
            "content:المملكة",
            "content:عقد",
        ],
    ),
    (
        "name+content",
        &[
            "report content:summary",
            "*.docx content:project",
            "ext:txt content:error",
            "تقرير content:وزارة",
            "readme content:install",
        ],
    ),
    (
        "plain words (searchContent on)",
        &["report", "invoice 2024", "تقرير"],
    ),
];

fn query_content(content: &ContentIndex, names: &FileIndex, rounds: usize) -> Result<(), CliError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut all = Vec::new();
    for (group, queries) in CONTENT_QUERIES {
        let both = group.starts_with("plain");
        let mut times = Vec::new();
        let mut search_times = Vec::new();
        let mut snippet_times = Vec::new();
        println!(
            "\n== {group}   (end-to-end = search without snippets + snippets of {SNIPPET_ROWS} rows)"
        );
        for text in *queries {
            let mut q_times = Vec::with_capacity(rounds);
            let mut s_times = Vec::with_capacity(rounds);
            let mut n_times = Vec::with_capacity(rounds);
            let mut hits = 0;
            for _ in 0..rounds {
                let t = Instant::now();
                let run = content_query(content, names, text, both, now)?;
                q_times.push(ms(t.elapsed()));
                s_times.push(ms(run.search));
                n_times.push(ms(run.snippets));
                hits = run.hits;
            }
            for v in [&mut q_times, &mut s_times, &mut n_times] {
                v.sort_by(f64::total_cmp);
            }
            println!(
                "  {text:<34} hits {hits:>4}  p50 {:>7.2} ms  p95 {:>7.2} ms  max {:>7.2} ms  \
                 (search p95 {:>7.2} ms, snippets p95 {:>6.2} ms)",
                pct(&q_times, 50.0),
                pct(&q_times, 95.0),
                q_times.last().copied().unwrap_or(f64::NAN),
                pct(&s_times, 95.0),
                pct(&n_times, 95.0),
            );
            times.extend_from_slice(&q_times);
            search_times.extend_from_slice(&s_times);
            snippet_times.extend_from_slice(&n_times);
        }
        println!("  {group} end-to-end: {}", summary(&mut times));
        println!("  {group} search:     {}", summary(&mut search_times));
        println!("  {group} snippets:   {}", summary(&mut snippet_times));
        all.extend(times);
    }
    println!("\nall content queries: {}", summary(&mut all));

    // Where the time goes, straight on the index: matching alone (count), top-N hits (no
    // snippets; the executor's first fetch for the app's limit is max(4 × 201, 200) = 804,
    // and an AND deepens up to 10,000), then snippets of the best 30 hits.
    println!(
        "\n== raw ContentIndex (p50 of {rounds}): count | top 20 | top 804 | top 10000 | \
         snippets of top {SNIPPET_ROWS}"
    );
    for q in RAW_CONTENT {
        let p50 = |f: &dyn Fn() -> bool| -> f64 {
            let mut v: Vec<f64> = (0..rounds)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(f());
                    ms(t.elapsed())
                })
                .collect();
            v.sort_by(f64::total_cmp);
            pct(&v, 50.0)
        };
        let n = content.count(q).unwrap_or(0);
        let count = p50(&|| content.count(q).is_ok());
        let top = |k: usize| p50(&|| content.search(q, k).is_ok());
        let shown: Vec<String> = content
            .search(q, SNIPPET_ROWS)
            .map(|h| h.into_iter().map(|h| h.path).collect())
            .unwrap_or_default();
        let snippets = p50(&|| content.snippets(q, &shown).is_ok());
        println!(
            "  {q:<22} matches {n:>6}  {count:>8.2} | {:>8.2} | {:>8.2} | {:>8.2} | {snippets:>8.2} ms",
            top(20),
            top(804),
            top(10_000)
        );
    }
    Ok(())
}

/// Content-query texts timed straight on the content index.
const RAW_CONTENT: &[&str] = &[
    "report",
    "the",
    "function",
    "\"return value\"",
    "تقرير",
    "عقد",
    "\"بسم الله\"",
];

/// Timings of one [`content_query`].
struct QueryRun {
    /// Rows returned.
    hits: usize,
    /// `execute` (no snippets).
    search: Duration,
    /// Snippets of the first [`SNIPPET_ROWS`] content rows.
    snippets: Duration,
}

/// One content query the way the app's `search` command runs it: plan, then `execute` over a
/// name source (the whole name index) and the content index (hits without snippets), then
/// snippets for the content rows among the first [`SNIPPET_ROWS`].
fn content_query(
    content: &ContentIndex,
    names: &FileIndex,
    text: &str,
    plain_words_search_content: bool,
    now: i64,
) -> Result<QueryRun, CliError> {
    let ast = athrix_query::parse(text).map_err(|e| CliError::Other(format!("{e:?}")))?;
    let plan = athrix_query::plan(
        &ast,
        &PlanOptions {
            plain_words_search_content,
        },
    )
    .map_err(|e| CliError::Other(format!("{e:?}")))?;
    if matches!(plan, SearchPlan::Name(_) | SearchPlan::Empty) {
        return Err(CliError::Other(format!("{text:?} is not a content query")));
    }
    let cancel = AtomicBool::new(false);
    let mut name_fn = |q: &athrix_query::Query, limit: usize| -> Result<NameResults, String> {
        let compiled = Query::compile(q).map_err(|e| e.message().to_owned())?;
        let ids = names
            .search(&compiled, limit, &cancel)
            .map_err(|e| e.to_string())?;
        Ok(NameResults {
            hits: ids
                .into_iter()
                .map(|id| NameHit {
                    path: names.path(id).to_string_lossy().into_owned(),
                    mtime: names.meta(id).modified,
                })
                .collect(),
            ranked: compiled.is_ranked(),
        })
    };
    let mut content_fn = |q: &str, limit: usize| -> Result<Vec<ContentHit<()>>, String> {
        let hits = content.search(q, limit).map_err(|e| e.to_string())?;
        Ok(hits
            .into_iter()
            .map(|h| ContentHit {
                path: h.path,
                score: h.score,
                mtime: None,
                snippet: (),
            })
            .collect())
    };
    let mut sources = Closures {
        names: &mut name_fn,
        content: &mut content_fn,
        verify: None,
    };
    let mut options = ExecOptions::new(now);
    options.limit = APP_LIMIT + 1;
    options.name_cap = NAME_CAP;
    options.content_cap = CONTENT_CAP;
    options.exclusion_cap = CONTENT_CAP;
    let t = Instant::now();
    let results = execute(&plan, &options, None, &mut sources).map_err(CliError::Other)?;
    let search = t.elapsed();

    // The app's snippets on first paint: the first SNIPPET_ROWS rows that matched content.
    let t = Instant::now();
    let shown: Vec<&str> = results
        .hits
        .iter()
        .take(APP_LIMIT)
        .take(SNIPPET_ROWS)
        .filter(|h| h.matched.has_content())
        .map(|h| h.path.as_str())
        .collect();
    if let Some(text) = plan_content_text(&plan) {
        std::hint::black_box(
            content
                .snippets(&text, &shown)
                .map_err(|e| CliError::Other(format!("snippets: {e}")))?,
        );
    }
    Ok(QueryRun {
        hits: results.hits.len().min(APP_LIMIT),
        search,
        snippets: t.elapsed(),
    })
}

/// The content-query text of a plan's positive content part (what the app builds snippets
/// for), `None` when it has none.
fn plan_content_text(plan: &SearchPlan) -> Option<String> {
    match plan {
        SearchPlan::NameAndContent { content, .. } | SearchPlan::NameOrContent { content, .. }
            if content.has_positive() =>
        {
            Some(content.to_query_text())
        }
        _ => None,
    }
}
