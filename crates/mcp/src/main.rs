//! `athrix-mcp`: the Athrix MCP server over stdio. See the library docs (`athrix_mcp`).
//!
//! ```text
//! athrix-mcp [--data-dir <dir>] [--log-file <file> | --no-log]
//! ```
//!
//! AI clients start it themselves, e.g. in Claude Desktop's `claude_desktop_config.json`:
//! `{"mcpServers":{"athrix":{"command":"C:\\…\\athrix-mcp.exe"}}}`.

use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use athrix_mcp::config::default_data_dir;
use athrix_mcp::log::CallLog;
use athrix_mcp::{Server, ServerOptions};

const USAGE: &str = "athrix-mcp: Athrix MCP server (stdio). Read-only file search for AI \
assistants, off until enabled in Athrix → Settings → MCP.

usage: athrix-mcp [--data-dir <dir>] [--log-file <file> | --no-log]
  --data-dir   Athrix data folder (default: %APPDATA%\\com.samt.athrix)
  --log-file   where to log tool calls (default: <data-dir>\\logs\\mcp.log)
  --no-log     do not log tool calls
  --version    print the version";

fn main() -> ExitCode {
    let mut data_dir: Option<PathBuf> = None;
    let mut log_file: Option<PathBuf> = None;
    let mut no_log = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => data_dir = args.next().map(PathBuf::from),
            Some("--log-file") => log_file = args.next().map(PathBuf::from),
            Some("--no-log") => no_log = true,
            Some("--version" | "-V") => {
                println!("athrix-mcp {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("athrix-mcp: unknown argument {arg:?}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(data_dir) = data_dir.or_else(default_data_dir) else {
        eprintln!("athrix-mcp: cannot find the Athrix data folder; pass --data-dir");
        return ExitCode::from(2);
    };
    let log_file = if no_log {
        None
    } else {
        Some(log_file.unwrap_or_else(|| CallLog::default_file(&data_dir)))
    };
    let mut server = Server::new(ServerOptions { data_dir, log_file });
    let stdin = BufReader::new(std::io::stdin().lock());
    let stdout = std::io::stdout().lock();
    match server.run(stdin, stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "athrix-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
