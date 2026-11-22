//! `athrix-mcp`: a [Model Context Protocol](https://modelcontextprotocol.io) server that lets AI
//! assistants (Claude Desktop, Claude Code, …) search the user's files through Athrix.
//!
//! - **Off by default.** The desktop app writes `<app_data>/mcp.json` ([`config`]); unless it
//!   says `enabled` with at least one allowed folder, every tool call is refused with a message
//!   telling the user where to turn it on.
//! - **Read-only.** Four tools ([`tools`]): `search_files`, `search_content`, `file_info`,
//!   `read_text` (the last only with `allowContentRead`). Nothing writes, deletes, moves, opens
//!   or runs files, and the indexes are opened read-only.
//! - **Allowed folders only.** Every result and every read passes the folder guard
//!   ([`paths::Guard`]): normalized, compared case-insensitively on Windows, `..` resolved,
//!   UNC / device / `\\?\` paths refused, links and junctions resolved and checked again.
//! - **No network.** It talks to the AI client over stdio only ([`server`]); the client starts
//!   it, so the desktop app need not be running.
//! - **Logged.** Each call (tool, query or path, result count, time; never contents) goes to
//!   `<app_data>/logs/mcp.log` ([`log`]).
//!
//! Data: the app's name-index snapshot ([`names`], possibly a few minutes behind the disk) and
//! its Tantivy content index (`<app_data>/content`, opened read-only per call so the app keeps
//! writing). PDFs are read through the isolated `athrix-extract-worker` when it sits beside the
//! binary; this crate never parses PDFs in-process.

pub mod config;
pub mod log;
pub mod names;
pub mod paths;
pub mod server;
pub mod tools;

pub use config::McpConfig;
pub use server::{Server, ServerOptions};
