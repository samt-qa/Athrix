//! The Model Context Protocol over stdio: JSON-RPC 2.0, one message per line.
//!
//! Handled: `initialize` (version negotiation), `ping`, `tools/list`, `tools/call`, and the
//! notifications `notifications/initialized` / `notifications/cancelled` (ignored: every call
//! is short and synchronous). Responses the client sends (we never send requests) and other
//! notifications are ignored; other methods get `-32601`. A JSON array is answered as a batch
//! (older protocol versions). Only protocol messages go to stdout.
//!
//! Safety gate: before every tool call the settings file is read again. Unless it is enabled
//! with at least one usable allowed folder, the call returns a tool error telling the user
//! where to turn it on; no index is touched.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use athrix_extract::WorkerPool;
use serde_json::{Value, json};

use crate::config::{CONFIG_FILE, McpConfig};
use crate::log::CallLog;
use crate::names::Names;
use crate::paths::Guard;
use crate::tools::{self, Ctx};

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The server's name in `initialize`.
pub const SERVER_NAME: &str = "athrix";

/// JSON-RPC error codes.
mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
}

/// Message shown when the server is off (Arabic first, then English).
pub const DISABLED_MESSAGE: &str = "خادم MCP في أثركس مُعطَّل أو لا توجد مجلدات مسموح بها. \
فعِّله من أثركس ← الإعدادات ← MCP / المساعدات الذكية، وأضف مجلدًا واحدًا على الأقل.\n\
The Athrix MCP server is disabled or has no allowed folders. Turn it on in Athrix → Settings → \
MCP and allow at least one folder.";

const INSTRUCTIONS: &str = "Athrix searches the user's local files (Arabic-first) through its \
own index. Use search_files for file names and metadata, search_content for text inside \
documents, file_info for one path, read_text to read a document (only if the user allowed it). \
Everything is read-only and limited to the folders the user allowed in Athrix → Settings → MCP; \
the index may lag the disk by a few minutes.";

/// Where the server finds its data.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// The app data folder (`%APPDATA%\com.samt.athrix` by default).
    pub data_dir: PathBuf,
    /// Where calls are logged (`None`: not logged).
    pub log_file: Option<PathBuf>,
}

/// The MCP server state. Feed it lines with [`Server::handle_line`] or run it over streams
/// with [`Server::run`].
pub struct Server {
    options: ServerOptions,
    log: CallLog,
    names: Names,
    worker: OnceLock<WorkerPool>,
}

impl Server {
    /// A server over `options`.
    pub fn new(options: ServerOptions) -> Server {
        Server {
            log: CallLog::new(options.log_file.clone()),
            options,
            names: Names::default(),
            worker: OnceLock::new(),
        }
    }

    /// Reads messages from `input` line by line until it ends, writing each answer as one line
    /// to `output`.
    pub fn run(&mut self, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if let Some(answer) = self.handle_line(&line) {
                output.write_all(answer.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// Handles one line; returns the answer to send (one line of JSON), if any.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        // Some shells put a byte-order mark before the first line.
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            return None;
        }
        let answer = match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) if batch.is_empty() => {
                Some(error(Value::Null, code::INVALID_REQUEST, "empty batch"))
            }
            Ok(Value::Array(batch)) => {
                let answers: Vec<Value> =
                    batch.into_iter().filter_map(|m| self.handle(m)).collect();
                (!answers.is_empty()).then_some(Value::Array(answers))
            }
            Ok(message) => self.handle(message),
            Err(e) => Some(error(
                Value::Null,
                code::PARSE_ERROR,
                &format!("parse error: {e}"),
            )),
        }?;
        Some(answer.to_string())
    }

    /// Handles one message; `None` for notifications and responses.
    pub fn handle(&mut self, message: Value) -> Option<Value> {
        let Value::Object(obj) = message else {
            return Some(error(
                Value::Null,
                code::INVALID_REQUEST,
                "not a JSON-RPC object",
            ));
        };
        let id = obj.get("id").cloned();
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(error(
                id.unwrap_or(Value::Null),
                code::INVALID_REQUEST,
                "jsonrpc must be \"2.0\"",
            ));
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response to a request of ours (we send none): ignore. Anything else is invalid.
            return if obj.contains_key("result") || obj.contains_key("error") {
                None
            } else {
                Some(error(
                    id.unwrap_or(Value::Null),
                    code::INVALID_REQUEST,
                    "missing method",
                ))
            };
        };
        let Some(id) = id.filter(|i| i.is_string() || i.is_number()) else {
            // A notification (`notifications/initialized`, `notifications/cancelled`, …).
            return None;
        };
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => ok(id, initialize(&params)),
            "ping" => ok(id, json!({})),
            "tools/list" => ok(id, tools::list()),
            "tools/call" => match self.call_tool(&params) {
                Ok(result) => ok(id, result),
                Err(message) => error(id, code::INVALID_PARAMS, &message),
            },
            other => error(
                id,
                code::METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            ),
        })
    }

    /// `tools/call`: `Err` for protocol errors (unknown tool, bad params); tool failures are
    /// results with `isError: true`.
    fn call_tool(&mut self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call needs a tool name")?;
        if !tools::exists(name) {
            return Err(format!("unknown tool: {name}"));
        }
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return Err("arguments must be an object".to_owned()),
        };
        let subject = args
            .get("query")
            .or_else(|| args.get("path"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let started = Instant::now();

        let config = McpConfig::load(&self.options.data_dir.join(CONFIG_FILE));
        let guard = if config.is_active() {
            Guard::new(&config.allowed_folders)
        } else {
            Guard::default()
        };
        if !config.is_active() || guard.is_empty() {
            self.log.record(
                name,
                &subject,
                "refused=disabled",
                started.elapsed().as_millis(),
            );
            return Ok(tool_error(DISABLED_MESSAGE));
        }

        let mut ctx = Ctx {
            data_dir: &self.options.data_dir,
            config: &config,
            guard: &guard,
            names: &mut self.names,
            worker: &self.worker,
        };
        let outcome = tools::call(&mut ctx, name, &args);
        let millis = started.elapsed().as_millis();
        Ok(match outcome {
            Ok(out) => {
                self.log
                    .record(name, &subject, &format!("results={}", out.count), millis);
                let text = serde_json::to_string_pretty(&out.value).unwrap_or_default();
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": out.value,
                    "isError": false
                })
            }
            Err(message) => {
                self.log.record(name, &subject, "error", millis);
                tool_error(&message)
            }
        })
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": "Athrix file search",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": INSTRUCTIONS
    })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
