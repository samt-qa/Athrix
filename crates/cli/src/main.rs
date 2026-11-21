//! Athrix command-line interface. For now it only exposes the Arabic normalizer, as a smoke
//! test of the workspace; search commands come in M7 (task 7.1).

use std::process::ExitCode;

const USAGE: &str = "usage:
  athrix --version
  athrix normalize <text>   print <text> as the name index stores it";

fn main() -> ExitCode {
    athrix_core::logging::init("warn");
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{} {}", athrix_core::APP_NAME, athrix_core::VERSION);
            ExitCode::SUCCESS
        }
        Some("normalize") if args.len() > 1 => {
            println!("{}", athrix_text_ar::normalize_name(&args[1..].join(" ")));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
