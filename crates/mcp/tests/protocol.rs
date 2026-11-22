//! End-to-end tests of the MCP server: JSON-RPC lines in, lines out, over a temporary data
//! folder holding a real name snapshot and content index.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use athrix_content_index::{ContentIndex, FileMeta};
use athrix_fs_index::{WalkOptions, walk};
use athrix_mcp::config::{CONFIG_FILE, McpConfig};
use athrix_mcp::{Server, ServerOptions};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A data folder and a file tree: `files/allowed/…` (allowed) and `files/secret/…` (not).
struct Fixture {
    _tmp: TempDir,
    data: PathBuf,
    allowed: PathBuf,
    secret: PathBuf,
    log: PathBuf,
}

const ALLOWED_FILES: &[(&str, &str)] = &[
    ("report-2025.txt", "ميزانية المشروع للعام القادم"),
    ("تقرير.md", "budget notes and plans"),
    ("sub/deep-report.txt", "nothing here"),
];
const SECRET_FILES: &[(&str, &str)] = &[("report-secret.txt", "ميزانية سرية جدا")];

impl Fixture {
    fn new() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("files");
        let allowed = root.join("allowed");
        let secret = root.join("secret");
        for (dir, files) in [(&allowed, ALLOWED_FILES), (&secret, SECRET_FILES)] {
            for (name, text) in files {
                let p = dir.join(name);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, text).unwrap();
            }
        }
        fs::write(allowed.join("blob.bin"), [0u8, 1, 2, 3, 255, 254]).unwrap();
        let data = tmp.path().join("data");

        // Name snapshot of the whole tree (the allowed folder and the secret one).
        let index = walk(&WalkOptions::new([root.clone()])).unwrap();
        fs::create_dir_all(data.join("index")).unwrap();
        index.save(&data.join("index").join("names.snap")).unwrap();

        // Content index of every text file.
        let content = ContentIndex::open_or_create(data.join("content")).unwrap();
        for (dir, files) in [(&allowed, ALLOWED_FILES), (&secret, SECRET_FILES)] {
            for (name, text) in files {
                let p = dir.join(name);
                let meta = FileMeta { size: 1, mtime: 1 };
                content
                    .add_or_update(&p.to_string_lossy(), meta, text)
                    .unwrap();
            }
        }
        content.commit().unwrap();
        drop(content);

        let log = tmp.path().join("mcp.log");
        Fixture {
            data,
            allowed,
            secret,
            log,
            _tmp: tmp,
        }
    }

    fn configure(&self, enabled: bool, folders: &[&Path], read: bool) {
        McpConfig {
            enabled,
            allowed_folders: folders
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            allow_content_read: read,
            max_results: 50,
        }
        .save(&self.data.join(CONFIG_FILE))
        .unwrap();
    }

    fn enable(&self, read: bool) {
        self.configure(true, &[&self.allowed], read);
    }

    fn server(&self) -> Server {
        Server::new(ServerOptions::new(
            self.data.clone(),
            Some(self.log.clone()),
        ))
    }
}

fn request(server: &mut Server, id: u64, method: &str, params: Value) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    let answer = server
        .handle_line(&line.to_string())
        .expect("a request is answered");
    assert!(!answer.contains('\n'), "one message per line");
    let v: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], id);
    v
}

/// `tools/call`; returns (isError, the result).
fn call(server: &mut Server, tool: &str, args: Value) -> (bool, Value) {
    let v = request(
        server,
        7,
        "tools/call",
        json!({ "name": tool, "arguments": args }),
    );
    let result = v["result"].clone();
    assert!(result.is_object(), "tool calls answer with a result: {v}");
    (result["isError"] == true, result)
}

fn text(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap().to_owned()
}

fn result_names(result: &Value) -> Vec<String> {
    result["structuredContent"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn handshake_lists_read_only_tools() {
    let f = Fixture::new();
    let mut s = f.server();
    let init = request(
        &mut s,
        1,
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(init["result"]["serverInfo"]["name"], "athrix");
    // An unknown version gets ours.
    let other = request(
        &mut s,
        2,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    );
    assert_eq!(
        other["result"]["protocolVersion"],
        athrix_mcp::server::PROTOCOL_VERSIONS[0]
    );
    assert_eq!(
        s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        None
    );
    assert_eq!(request(&mut s, 3, "ping", json!({}))["result"], json!({}));

    let list = request(&mut s, 4, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["search_files", "search_content", "file_info", "read_text"]
    );
    for t in tools {
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert_eq!(t["annotations"]["destructiveHint"], false);
        assert_eq!(t["inputSchema"]["type"], "object");
    }
}

#[test]
fn protocol_errors() {
    let f = Fixture::new();
    let mut s = f.server();
    let parse: Value = serde_json::from_str(&s.handle_line("{not json").unwrap()).unwrap();
    assert_eq!(parse["error"]["code"], -32700);
    assert_eq!(parse["id"], Value::Null);
    let unknown = request(&mut s, 1, "resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    let tool = request(
        &mut s,
        2,
        "tools/call",
        json!({ "name": "delete_file", "arguments": {} }),
    );
    assert_eq!(tool["error"]["code"], -32602);
    let bad: Value = serde_json::from_str(
        &s.handle_line(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(bad["error"]["code"], -32600);
    // Batches are answered as arrays; notifications inside get no answer.
    let batch = s
        .handle_line(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/cancelled"}]"#)
        .unwrap();
    let batch: Value = serde_json::from_str(&batch).unwrap();
    assert_eq!(batch.as_array().unwrap().len(), 1);
    // Responses from the client are ignored.
    assert_eq!(
        s.handle_line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#),
        None
    );
    assert_eq!(s.handle_line("   "), None);
    let bom = s
        .handle_line("\u{feff}{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"ping\"}")
        .unwrap();
    assert!(bom.contains("\"result\""), "{bom}");
}

#[test]
fn run_streams_lines() {
    let f = Fixture::new();
    let mut s = f.server();
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n"
    );
    let mut out = Vec::new();
    s.run(input.as_bytes(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("2025-03-26") && lines[1].contains("search_files"));
}

#[test]
fn disabled_or_folderless_config_refuses_every_tool() {
    let f = Fixture::new();
    let mut s = f.server();
    // No mcp.json at all.
    for tool in ["search_files", "search_content", "file_info", "read_text"] {
        let (is_error, r) = call(&mut s, tool, json!({ "query": "report", "path": "x" }));
        assert!(is_error, "{tool}");
        assert!(text(&r).contains("Settings → MCP"), "{tool}");
    }
    // Present but off; on without folders; on with only a folder that does not exist.
    f.configure(false, &[&f.allowed], true);
    assert!(call(&mut s, "search_files", json!({ "query": "report" })).0);
    f.configure(true, &[], true);
    assert!(call(&mut s, "search_files", json!({ "query": "report" })).0);
    let gone = f.allowed.join("missing");
    f.configure(true, &[&gone], true);
    assert!(call(&mut s, "search_files", json!({ "query": "report" })).0);
    // Turning it on applies at once, without restarting the server.
    f.enable(false);
    assert!(!call(&mut s, "search_files", json!({ "query": "report" })).0);
}

#[test]
fn name_search_sees_only_allowed_folders() {
    let f = Fixture::new();
    f.enable(false);
    let mut s = f.server();
    let (is_error, r) = call(&mut s, "search_files", json!({ "query": "report" }));
    assert!(!is_error, "{r}");
    let mut names = result_names(&r);
    names.sort();
    assert_eq!(names, ["deep-report.txt", "report-2025.txt"]);
    // Arabic names, and every row has its fields.
    let (_, r) = call(&mut s, "search_files", json!({ "query": "تقرير" }));
    assert_eq!(result_names(&r), ["تقرير.md"]);
    let row = &r["structuredContent"]["results"][0];
    assert_eq!(row["kind"], "file");
    assert!(row["size"].as_u64().unwrap() > 0);
    assert!(row["modified"].as_str().unwrap().ends_with('Z'));
    assert!(Path::new(row["path"].as_str().unwrap()).starts_with(&f.allowed));
    // The secret folder is not even visible as a folder.
    let (_, r) = call(&mut s, "search_files", json!({ "query": "secret" }));
    assert!(result_names(&r).is_empty());
    // Ranked (fuzzy) queries go through the same mask.
    let (_, r) = call(&mut s, "search_files", json!({ "query": "fuzzy:reprot" }));
    let mut names = result_names(&r);
    names.sort();
    assert_eq!(names, ["deep-report.txt", "report-2025.txt"]);
    // limit is honoured.
    let (_, r) = call(
        &mut s,
        "search_files",
        json!({ "query": "report", "limit": 1 }),
    );
    assert_eq!(result_names(&r).len(), 1);
    // A deleted file drops out although the snapshot still has it.
    fs::remove_file(f.allowed.join("report-2025.txt")).unwrap();
    let (_, r) = call(&mut s, "search_files", json!({ "query": "report" }));
    assert_eq!(result_names(&r), ["deep-report.txt"]);
    // Query errors are tool errors.
    let (is_error, _) = call(&mut s, "search_files", json!({ "query": "size:>>" }));
    assert!(is_error);
    let (is_error, _) = call(&mut s, "search_files", json!({ "nope": 1 }));
    assert!(is_error);
}

#[test]
fn idle_server_frees_the_name_index_and_reloads_it() {
    let f = Fixture::new();
    f.enable(false);
    let mut s = Server::new(ServerOptions {
        idle_release: Some(Duration::from_millis(200)),
        ..ServerOptions::new(f.data.clone(), None)
    });
    assert!(!s.names_loaded());
    let (_, r) = call(&mut s, "search_files", json!({ "query": "report" }));
    assert_eq!(result_names(&r).len(), 2);
    assert!(s.names_loaded());
    let started = Instant::now();
    while s.names_loaded() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!s.names_loaded(), "released after the idle period");
    let (_, r) = call(&mut s, "search_files", json!({ "query": "report" }));
    assert_eq!(result_names(&r).len(), 2, "loaded again on demand");

    // Switched off: it stays.
    let mut kept = Server::new(ServerOptions {
        idle_release: None,
        ..ServerOptions::new(f.data.clone(), None)
    });
    call(&mut kept, "search_files", json!({ "query": "report" }));
    std::thread::sleep(Duration::from_millis(300));
    assert!(kept.names_loaded());
}

#[test]
fn content_search_returns_snippets_from_allowed_files_only() {
    let f = Fixture::new();
    f.enable(false);
    let mut s = f.server();
    let (is_error, r) = call(&mut s, "search_content", json!({ "query": "ميزانية" }));
    assert!(!is_error, "{r}");
    assert_eq!(result_names(&r), ["report-2025.txt"]);
    let row = &r["structuredContent"]["results"][0];
    assert!(
        row["snippet"].as_str().unwrap().contains("**ميزانية**"),
        "{row}"
    );
    assert_eq!(row["matched"], "content");
    // `content:` works from search_files too.
    let (_, r) = call(&mut s, "search_files", json!({ "query": "content:budget" }));
    assert_eq!(result_names(&r), ["تقرير.md"]);
    // A natural-language request is rewritten when the literal query finds nothing.
    let (is_error, r) = call(
        &mut s,
        "search_files",
        json!({ "query": "مستندات تحتوي على ميزانية" }),
    );
    assert!(!is_error, "{r}");
    assert_eq!(result_names(&r), ["report-2025.txt"]);
    assert_eq!(
        r["structuredContent"]["interpretedAs"],
        "type:doc content:ميزانية"
    );
    // Allowing the secret folder as well shows it.
    f.configure(true, &[&f.allowed, &f.secret], false);
    let (_, r) = call(&mut s, "search_content", json!({ "query": "ميزانية" }));
    assert_eq!(result_names(&r).len(), 2);
}

#[test]
fn content_search_without_a_content_index_says_so() {
    let f = Fixture::new();
    f.enable(false);
    fs::remove_dir_all(f.data.join("content")).unwrap();
    let mut s = f.server();
    let (is_error, r) = call(&mut s, "search_content", json!({ "query": "ميزانية" }));
    assert!(is_error);
    assert!(text(&r).contains("content index"));
    assert!(!f.data.join("content").exists(), "never created");
}

#[test]
fn file_info_and_traversal() {
    let f = Fixture::new();
    f.enable(false);
    let mut s = f.server();
    let inside = f.allowed.join("report-2025.txt");
    let (is_error, r) = call(&mut s, "file_info", json!({ "path": inside }));
    assert!(!is_error, "{r}");
    assert_eq!(r["structuredContent"]["extension"], "txt");
    assert_eq!(r["structuredContent"]["readableText"], false);
    let (is_error, _) = call(&mut s, "file_info", json!({ "path": f.allowed }));
    assert!(!is_error, "the allowed folder itself");

    let secret = f.secret.join("report-secret.txt");
    let sep = std::path::MAIN_SEPARATOR;
    let dotdot = format!(
        "{}{sep}..{sep}secret{sep}report-secret.txt",
        f.allowed.display()
    );
    let mut attempts = vec![
        secret.to_string_lossy().into_owned(),
        dotdot,
        f.allowed.parent().unwrap().to_string_lossy().into_owned(),
        "report-2025.txt".to_owned(),
        format!("{}{sep}missing.txt", f.allowed.display()),
    ];
    if cfg!(windows) {
        attempts.push(format!(r"\\?\{}", inside.display()));
        attempts.push(format!(r"\\.\{}", inside.display()));
        attempts.push(format!(
            r"\\localhost\c$\{}",
            &inside.to_string_lossy()[3..]
        ));
        attempts.push(format!("{}:stream", inside.display()));
        // A sibling whose name starts like the allowed folder.
        attempts.push(format!("{}2", f.allowed.display()));
    }
    for path in attempts {
        let (is_error, r) = call(&mut s, "file_info", json!({ "path": path }));
        assert!(is_error, "{path} must be refused: {r}");
        assert!(!text(&r).contains("ميزانية"));
    }
}

#[test]
fn read_text_needs_its_switch_and_stays_inside() {
    let f = Fixture::new();
    f.enable(false);
    let mut s = f.server();
    let inside = f.allowed.join("report-2025.txt");
    let (is_error, r) = call(&mut s, "read_text", json!({ "path": inside }));
    assert!(is_error && text(&r).contains("Settings → MCP"));

    f.enable(true);
    let (is_error, r) = call(&mut s, "read_text", json!({ "path": inside }));
    assert!(!is_error, "{r}");
    assert_eq!(
        r["structuredContent"]["text"],
        "ميزانية المشروع للعام القادم"
    );
    let (_, r) = call(
        &mut s,
        "read_text",
        json!({ "path": inside, "maxChars": 7 }),
    );
    assert_eq!(r["structuredContent"]["text"], "ميزانية");
    assert_eq!(r["structuredContent"]["truncated"], true);

    let (is_error, r) = call(
        &mut s,
        "read_text",
        json!({ "path": f.secret.join("report-secret.txt") }),
    );
    assert!(is_error && !text(&r).contains("سرية"));
    let (is_error, _) = call(
        &mut s,
        "read_text",
        json!({ "path": f.allowed.join("blob.bin") }),
    );
    assert!(is_error, "binary files are refused");
    let (is_error, _) = call(&mut s, "read_text", json!({ "path": f.allowed }));
    assert!(is_error, "folders are refused");
}

/// A link inside the allowed folder pointing at the secret one: listed by the walker as an
/// entry, but nothing through it passes the guard.
#[test]
fn links_out_of_an_allowed_folder_are_refused() {
    let f = Fixture::new();
    let link = f.allowed.join("link");
    if !make_dir_link(&f.secret, &link) {
        eprintln!("cannot create a directory link here; skipping");
        return;
    }
    f.enable(true);
    let mut s = f.server();
    let through = link.join("report-secret.txt");
    assert!(through.is_file(), "the link works");
    let (is_error, r) = call(&mut s, "read_text", json!({ "path": through }));
    assert!(is_error && !text(&r).contains("سرية"), "{r}");
    let (is_error, _) = call(&mut s, "file_info", json!({ "path": link }));
    assert!(is_error);
}

#[cfg(windows)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    // A junction needs no privilege (unlike a symbolic link).
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(unix)]
fn make_dir_link(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

#[cfg(not(any(windows, unix)))]
fn make_dir_link(_: &Path, _: &Path) -> bool {
    false
}

#[test]
fn calls_are_logged_without_contents() {
    let f = Fixture::new();
    f.enable(true);
    let mut s = f.server();
    call(&mut s, "search_content", json!({ "query": "ميزانية" }));
    call(
        &mut s,
        "read_text",
        json!({ "path": f.allowed.join("report-2025.txt") }),
    );
    let log = fs::read_to_string(&f.log).unwrap();
    assert!(log.contains("tool=search_content") && log.contains("tool=read_text"));
    assert!(log.contains("results=1"));
    assert!(!log.contains("المشروع"), "no file text in the log: {log}");
}
