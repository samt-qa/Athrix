//! `athrix interpret "<text>"`: shows how a natural-language query («ملفات PDF من الأسبوع
//! الماضي أكبر من 5 ميجا», "excel files from yesterday bigger than 2mb") is read as filters,
//! with the same rules as the desktop app's suggestion (`athrix_query::interpret_with`).
//!
//! Prints the suggested query on stdout and exits 0, or prints nothing and exits 1 when there
//! is nothing to suggest. `--explain` adds one line per piece of the input on stderr.
//! `--today YYYY-MM-DD` replaces the local date used for relative phrases («قبل 3 أيام»).

use std::process::ExitCode;

use athrix_query::{CalendarDate, InterpretOptions, PartRole, interpret_with};
use chrono::Datelike;

use crate::{CliError, usage, value};

/// Runs `athrix interpret`.
pub fn cmd_interpret(args: &[String]) -> Result<ExitCode, CliError> {
    let mut explain = false;
    let mut today: Option<CalendarDate> = None;
    let mut words: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--explain" => explain = true,
            "--today" => {
                let v = value(&mut it, "--today")?;
                today = Some(parse_day(v).ok_or_else(|| usage("--today needs YYYY-MM-DD"))?);
            }
            "--" => words.extend(it.by_ref().map(String::as_str)),
            flag if flag.starts_with("--") => {
                return Err(usage(format!("unknown option {flag}")));
            }
            w => words.push(w),
        }
    }
    if words.is_empty() {
        return Err(usage("interpret needs some text"));
    }
    let text = words.join(" ");
    let options = InterpretOptions::with_today(today.unwrap_or_else(local_today));
    let Some(found) = interpret_with(&text, &options) else {
        if explain {
            eprintln!("nothing to suggest (no filter cue, or the text already uses filters)");
        }
        return Ok(ExitCode::FAILURE);
    };
    println!("{}", found.query);
    if explain {
        for p in &found.parts {
            let piece = p.span.slice(&text);
            match p.role {
                PartRole::Ignored => eprintln!("  {:<9} {piece}", p.role.as_str()),
                _ => eprintln!("  {:<9} {piece}  →  {}", p.role.as_str(), p.output),
            }
        }
        eprintln!("  confidence {:.2}", found.confidence);
    }
    Ok(ExitCode::SUCCESS)
}

fn local_today() -> CalendarDate {
    let d = chrono::Local::now().date_naive();
    CalendarDate {
        year: u16::try_from(d.year()).unwrap_or(1970),
        month: u8::try_from(d.month()).ok(),
        day: u8::try_from(d.day()).ok(),
    }
}

fn parse_day(s: &str) -> Option<CalendarDate> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(CalendarDate {
        year: u16::try_from(d.year()).ok()?,
        month: u8::try_from(d.month()).ok(),
        day: u8::try_from(d.day()).ok(),
    })
}
