//! The four tools: `search_files`, `search_content`, `file_info`, `read_text`. All read-only;
//! there is deliberately no tool that writes, deletes, moves, opens or runs anything.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use athrix_content_index::{ContentIndex, PathFilter};
use athrix_extract::{ExtractError, Kind, Limits, WorkerConfig, WorkerPool};
use athrix_fs_index::Query as NameQuery;
use athrix_query::{CalendarDate, InterpretOptions, PlanOptions, SearchPlan, interpret_with};
use athrix_ranking::{Closures, ContentHit, ExecOptions, Matched, NameHit, NameResults, execute};
use serde_json::{Value, json};

use crate::config::McpConfig;
use crate::names::{self, Names};
use crate::paths::Guard;

/// Folder of the content index inside the app data folder (the app's `CONTENT_DIR`).
pub const CONTENT_DIR: &str = "content";
/// Default `maxChars` of `read_text`.
pub const DEFAULT_READ_CHARS: usize = 20_000;
/// Highest `maxChars` of `read_text`.
pub const MAX_READ_CHARS: usize = 200_000;
/// Largest file `read_text` opens, in bytes.
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
/// Longest time one PDF may take in the worker.
const PDF_TIMEOUT: Duration = Duration::from_secs(20);
/// Name results fetched for a name + content query (the app's `NAME_CAP`).
const NAME_CAP: usize = 20_000;
/// Content results fetched at most (the app's `CONTENT_CAP`).
const CONTENT_CAP: usize = 10_000;

/// What a tool returns on success: a JSON value (sent as text and as structured content) and
/// what goes to the log (counts and the query or path, never file contents).
#[derive(Debug)]
pub struct ToolOutput {
    /// The result.
    pub value: Value,
    /// Number of results (or characters read).
    pub count: usize,
}

/// What the tools need from the server.
pub struct Ctx<'a> {
    /// The app data folder.
    pub data_dir: &'a Path,
    /// The settings read for this call.
    pub config: &'a McpConfig,
    /// The allowed folders.
    pub guard: &'a Guard,
    /// The cached name snapshot.
    pub names: &'a mut Names,
    /// The PDF worker pool, created on first use.
    pub worker: &'a OnceLock<WorkerPool>,
}

/// Note added to the search tool descriptions.
const STALE_NOTE: &str = "Results come from Athrix's saved index, which can be a few minutes \
behind the disk (files deleted since are left out; very new files may be missing). Only files \
under the folders the user allowed in Athrix → Settings → MCP are ever visible.";

const QUERY_HELP: &str = "Athrix query syntax (Arabic and English): plain words match file \
names (Arabic letter variants and diacritics are ignored); `\"exact phrase\"`; `-word` \
excludes; `a OR b`; `*.pdf` wildcards; filters `ext:pdf`, `type:document|image|video|audio|\
archive|folder`, `size:>1mb`, `dm:2025` / `dm:today` / `dm:>=2025-01-15` (modified), \
`path:x`, `infolder:x`, `content:word` (file text), `fuzzy:word`. Arabic filter names work too \
(`نوع:`, `حجم:`, `تاريخ:`, `مسار:`, `مجلد:`). When the query as written finds nothing, it is read as a natural-language request (Arabic or English, e.g. `ملفات PDF من الأسبوع الماضي`, `photos from last year`) and the rewritten query is reported in `interpretedAs`.";

/// The `tools/list` result.
pub fn list() -> Value {
    let read_only = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false
    });
    let limit = json!({
        "type": "integer",
        "minimum": 1,
        "maximum": crate::config::MAX_RESULTS_CAP,
        "description": "Most results to return (capped by the user's setting, default 50)."
    });
    json!({ "tools": [
        {
            "name": "search_files",
            "title": "Search files by name (Athrix)",
            "description": format!("Find files and folders by NAME and metadata on the user's \
                computer through Athrix's index. Returns path, name, kind, size and modification \
                time (UTC). {QUERY_HELP} {STALE_NOTE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Athrix query, e.g. `تقرير ext:pdf dm:2025`." },
                    "limit": limit
                },
                "required": ["query"]
            },
            "annotations": read_only
        },
        {
            "name": "search_content",
            "title": "Search inside documents (Athrix)",
            "description": format!("Full-text search INSIDE documents (txt, Office, PDF, \
                EPUB, e-mail… that Athrix indexed), Arabic-aware. Plain words match the text \
                (and names); filters work as in search_files. Returns matching files with a \
                short snippet, matches marked **like this**. Needs content indexing turned on \
                in Athrix. {QUERY_HELP} {STALE_NOTE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Words or phrases to find in file text, e.g. `\"عقد الإيجار\" ext:docx`." },
                    "limit": limit
                },
                "required": ["query"]
            },
            "annotations": read_only
        },
        {
            "name": "file_info",
            "title": "File details (Athrix)",
            "description": "Details of one file or folder under an allowed folder: kind, size, \
                created / modified times (UTC), extension, and whether read_text can read it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path, as returned by a search." }
                },
                "required": ["path"]
            },
            "annotations": read_only
        },
        {
            "name": "read_text",
            "title": "Read a document's text (Athrix)",
            "description": format!("The plain text of one document under an allowed folder \
                (txt/code, Word, Excel, PowerPoint, OpenDocument, EPUB, HTML, e-mail, PDF). \
                Works only when the user turned on \"allow reading file text\" in Athrix → \
                Settings → MCP. Binary files are refused. Returns at most maxChars characters \
                (default {DEFAULT_READ_CHARS}, at most {MAX_READ_CHARS})."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path, as returned by a search." },
                    "maxChars": { "type": "integer", "minimum": 1, "maximum": MAX_READ_CHARS }
                },
                "required": ["path"]
            },
            "annotations": read_only
        }
    ]})
}

/// Whether `name` is one of the tools.
pub fn exists(name: &str) -> bool {
    matches!(
        name,
        "search_files" | "search_content" | "file_info" | "read_text"
    )
}

/// Runs tool `name` with `args`. `Err` is a tool error message (English) for the assistant.
pub fn call(ctx: &mut Ctx<'_>, name: &str, args: &Value) -> Result<ToolOutput, String> {
    match name {
        "search_files" => search(ctx, args, false),
        "search_content" => search(ctx, args, true),
        "file_info" => file_info(ctx, args),
        "read_text" => read_text(ctx, args),
        _ => Err(format!("unknown tool {name}")),
    }
}

fn str_arg<'v>(args: &'v Value, name: &str) -> Result<&'v str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string argument `{name}`"))
}

fn uint_arg(args: &Value, name: &str) -> Result<Option<usize>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| *n >= 1)
            .map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX)))
            .ok_or_else(|| format!("`{name}` must be a positive integer")),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn search(ctx: &mut Ctx<'_>, args: &Value, content_mode: bool) -> Result<ToolOutput, String> {
    let text = str_arg(args, "query")?.trim();
    if text.is_empty() {
        return Err("the query is empty".to_owned());
    }
    let max = ctx.config.max_results as usize;
    let limit = uint_arg(args, "limit")?.unwrap_or(max).min(max);
    let literal = run_query(ctx, text, limit, content_mode);
    if literal.as_ref().is_ok_and(|out| out.count > 0) {
        return literal;
    }
    // Nothing (or a query error): try it as a natural-language request («ملفات PDF من الأسبوع
    // الماضي», "photos from last year") rewritten into the filter syntax.
    let options = InterpretOptions { today: today() };
    if let Some(found) = interpret_with(text, &options) {
        if found.query.trim() != text {
            if let Ok(mut out) = run_query(ctx, &found.query, limit, content_mode) {
                out.value["interpretedAs"] = json!(found.query);
                return Ok(out);
            }
        }
    }
    literal
}

/// Runs the Athrix query `text` (see [`search`]).
fn run_query(
    ctx: &mut Ctx<'_>,
    text: &str,
    limit: usize,
    content_mode: bool,
) -> Result<ToolOutput, String> {
    let ast = athrix_query::parse(text).map_err(|e| e.to_string())?;
    let plan = athrix_query::plan(
        &ast,
        &PlanOptions {
            plain_words_search_content: content_mode,
        },
    )
    .map_err(|e| e.to_string())?;
    if matches!(plan, SearchPlan::Empty) {
        return Ok(ToolOutput {
            value: json!({ "results": [], "note": "empty query" }),
            count: 0,
        });
    }

    let guard = ctx.guard;
    let data_dir = ctx.data_dir;
    let names = &mut *ctx.names;
    let mut content: Option<ContentIndex> = None;
    let scope = PathFilter::prefixes(guard.folders().map(|p| p.to_string_lossy().into_owned()));
    let results = {
        let mut name_fn = |q: &athrix_query::Query, limit: usize| -> Result<NameResults, String> {
            let compiled = NameQuery::compile(q).map_err(|e| e.message().to_owned())?;
            let (index, mask) = names.get(data_dir, guard)?;
            let ids = names::search(index, mask, &compiled, limit);
            Ok(NameResults {
                hits: ids
                    .into_iter()
                    .map(|id| NameHit {
                        path: index.path(id).to_string_lossy().into_owned(),
                        mtime: index.meta(id).modified,
                    })
                    .collect(),
                ranked: compiled.is_ranked(),
            })
        };
        let content_ref = &mut content;
        let mut content_fn = |q: &str, limit: usize| -> Result<Vec<ContentHit<()>>, String> {
            let index = open_content(content_ref, data_dir)?;
            let hits = index
                .search_within(q, limit, &scope)
                .map_err(|e| e.to_string())?;
            Ok(hits
                .into_iter()
                .filter(|h| guard.admits_lexically(&h.path))
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
        let mut options = ExecOptions::new(now_secs());
        // Headroom for hits dropped by the disk check below (deleted since the snapshot).
        options.limit = limit.saturating_mul(2).max(limit + 10);
        options.name_cap = NAME_CAP;
        options.content_cap = CONTENT_CAP;
        options.exclusion_cap = CONTENT_CAP;
        execute(&plan, &options, None, &mut sources)?
    };

    let mut rows: Vec<(PathBuf, Matched)> = Vec::new();
    for hit in results.hits {
        if rows.len() >= limit {
            break;
        }
        if let Ok(path) = guard.check(&hit.path) {
            rows.push((path, hit.matched));
        }
    }

    let snippets = match (&content, content_text(&plan)) {
        (Some(index), Some(q)) if content_mode => {
            let paths: Vec<String> = rows
                .iter()
                .filter(|(_, m)| m.has_content())
                .map(|(p, _)| p.to_string_lossy().into_owned())
                .collect();
            let found = index.snippets(&q, &paths).unwrap_or_default();
            paths.into_iter().zip(found).collect::<Vec<_>>()
        }
        _ => Vec::new(),
    };

    let items: Vec<Value> = rows
        .iter()
        .map(|(path, matched)| {
            let mut row = describe(path, false);
            if content_mode || matches!(plan, SearchPlan::NameAndContent { .. }) {
                row["matched"] = json!(match matched {
                    Matched::Name => "name",
                    Matched::Content => "content",
                    Matched::Both => "both",
                });
            }
            let p = path.to_string_lossy();
            if let Some((_, Some(s))) = snippets.iter().find(|(sp, _)| *sp == p) {
                row["snippet"] = json!(mark(s));
            }
            row
        })
        .collect();
    let count = items.len();
    let mut value = json!({ "results": items });
    if results.truncated.names || results.truncated.content || count == limit {
        value["more"] = json!(true);
    }
    Ok(ToolOutput { value, count })
}

fn open_content<'c>(
    slot: &'c mut Option<ContentIndex>,
    data_dir: &Path,
) -> Result<&'c ContentIndex, String> {
    if slot.is_none() {
        let dir = data_dir.join(CONTENT_DIR);
        let index = ContentIndex::open_read_only(&dir).map_err(|e| {
            format!(
                "the content index is not available ({e}); turn on content indexing in Athrix \
                 and let it finish"
            )
        })?;
        *slot = Some(index);
    }
    Ok(slot.as_ref().expect("opened above"))
}

/// The content-query text of the plan's positive content part (what snippets are built for).
fn content_text(plan: &SearchPlan) -> Option<String> {
    match plan {
        SearchPlan::NameAndContent { content, .. } | SearchPlan::NameOrContent { content, .. }
            if content.has_positive() =>
        {
            Some(content.to_query_text())
        }
        _ => None,
    }
}

/// The snippet fragment on one line, matches wrapped in `**`.
fn mark(s: &athrix_content_index::Snippet) -> String {
    let mut out = String::with_capacity(s.fragment.len() + 8 * s.highlights.len());
    let mut at = 0;
    for r in &s.highlights {
        if r.start < at || r.end > s.fragment.len() {
            continue;
        }
        out.push_str(&s.fragment[at..r.start]);
        out.push_str("**");
        out.push_str(&s.fragment[r.clone()]);
        out.push_str("**");
        at = r.end;
    }
    out.push_str(&s.fragment[at..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The JSON row of an existing, already-checked path.
fn describe(path: &Path, detailed: bool) -> Value {
    let meta = std::fs::metadata(path).ok();
    let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
    let name = path
        .file_name()
        .map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
    let mut row = json!({
        "path": path.to_string_lossy(),
        "name": name,
        "kind": if is_dir { "folder" } else { "file" },
    });
    if let Some(m) = &meta {
        if !is_dir {
            row["size"] = json!(m.len());
        }
        if let Some(t) = m.modified().ok().and_then(iso_time) {
            row["modified"] = json!(t);
        }
        if detailed {
            if let Some(t) = m.created().ok().and_then(iso_time) {
                row["created"] = json!(t);
            }
            row["readOnly"] = json!(m.permissions().readonly());
        }
    }
    row
}

/// The (year, month, day) of `days` since 1970-01-01 (Howard Hinnant's civil-from-days).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Today's date (UTC: the server has no time-zone data; a day off near midnight at worst).
fn today() -> Option<CalendarDate> {
    let (y, m, d) = civil(now_secs().div_euclid(86_400));
    Some(CalendarDate {
        year: u16::try_from(y).ok()?,
        month: Some(u8::try_from(m).ok()?),
        day: Some(u8::try_from(d).ok()?),
    })
}

/// `t` as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn iso_time(t: SystemTime) -> Option<String> {
    let secs = i64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    Some(iso_secs(secs))
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn iso_secs(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn file_info(ctx: &mut Ctx<'_>, args: &Value) -> Result<ToolOutput, String> {
    let path = ctx
        .guard
        .check(str_arg(args, "path")?)
        .map_err(|e| e.to_string())?;
    let mut row = describe(&path, true);
    let kind = if path.is_file() {
        Kind::from_path(&path)
    } else {
        None
    };
    if let Some(ext) = path.extension() {
        row["extension"] = json!(ext.to_string_lossy().to_lowercase());
    }
    row["readableText"] = json!(ctx.config.allow_content_read && kind.is_some());
    Ok(ToolOutput {
        value: row,
        count: 1,
    })
}

fn read_text(ctx: &mut Ctx<'_>, args: &Value) -> Result<ToolOutput, String> {
    if !ctx.config.allow_content_read {
        return Err(
            "reading file text is turned off. The user can allow it in Athrix → Settings → MCP \
             («السماح بقراءة نص الملفات»)."
                .to_owned(),
        );
    }
    let path = ctx
        .guard
        .check(str_arg(args, "path")?)
        .map_err(|e| e.to_string())?;
    let max_chars = uint_arg(args, "maxChars")?
        .unwrap_or(DEFAULT_READ_CHARS)
        .min(MAX_READ_CHARS);
    let meta = std::fs::metadata(&path).map_err(|e| format!("cannot read the file: {e}"))?;
    if !meta.is_file() {
        return Err("this is a folder, not a file".to_owned());
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!(
            "the file is too large to read ({} bytes, limit {MAX_READ_BYTES})",
            meta.len()
        ));
    }
    let Some(kind) = Kind::from_path(&path) else {
        return Err("unsupported file type (binary or unknown format): no text to read".to_owned());
    };
    let limits = Limits {
        max_input_bytes: MAX_READ_BYTES,
        max_output_chars: max_chars,
        ..Limits::default()
    };
    let extracted = if kind == Kind::Pdf {
        let pool = ctx.worker.get_or_init(pdf_pool);
        let program_present = pool
            .config()
            .program
            .clone()
            .or_else(|| athrix_extract::worker::default_program().ok())
            .is_some_and(|p| p.is_file());
        if !program_present {
            return Err(
                "PDF reading is not available: the PDF worker (athrix-extract-worker) is not \
                 next to athrix-mcp"
                    .to_owned(),
            );
        }
        pool.extract_isolated(&path, &limits)
    } else {
        athrix_extract::extract(&path, &limits)
    }
    .map_err(|e| match e {
        ExtractError::Unsupported(why) => format!("unsupported file: {why}"),
        other => format!("cannot extract the text: {other}"),
    })?;
    if looks_binary(&extracted.text) {
        return Err("the file looks binary: no text to return".to_owned());
    }
    let chars = extracted.text.chars().count();
    Ok(ToolOutput {
        value: json!({
            "path": path.to_string_lossy(),
            "format": extracted.kind.name(),
            "truncated": extracted.truncated,
            "chars": chars,
            "text": extracted.text,
        }),
        count: chars,
    })
}

fn pdf_pool() -> WorkerPool {
    let mut config = WorkerConfig::default();
    config.timeout = PDF_TIMEOUT;
    config.max_workers = 1;
    WorkerPool::new(config)
}

/// Many replacement or control characters: decoded garbage rather than text.
fn looks_binary(text: &str) -> bool {
    let mut total = 0usize;
    let mut bad = 0usize;
    for c in text.chars().take(8192) {
        total += 1;
        if c == '\u{FFFD}' || (c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}')) {
            bad += 1;
        }
    }
    total > 0 && bad * 10 > total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates() {
        assert_eq!(iso_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_secs(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_secs(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(iso_secs(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn binary_detection() {
        assert!(!looks_binary("مرحبا\nhello\tworld"));
        assert!(looks_binary("\u{0}\u{1}\u{2}abc\u{FFFD}\u{FFFD}"));
        assert!(!looks_binary(""));
    }
}
