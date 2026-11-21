//! Throughput measurements. Ignored by default; run with
//! `cargo test -p athrix-extract --release -- --ignored --nocapture perf`.

use std::time::Instant;

use super::fixtures::*;
use crate::{Kind, Limits, extract_bytes};

fn report(what: &str, bytes: usize, chars: usize, f: impl Fn()) {
    f(); // warm-up
    let runs = 5;
    let start = Instant::now();
    for _ in 0..runs {
        f();
    }
    let secs = start.elapsed().as_secs_f64() / f64::from(runs);
    println!(
        "{what}: {:.1} MB in {:.1} ms -> {:.0} MB/s ({chars} chars out)",
        bytes as f64 / 1e6,
        secs * 1e3,
        bytes as f64 / 1e6 / secs
    );
}

#[test]
#[ignore = "throughput measurement; run in release"]
fn perf_docx_5mb() {
    let para = format!("{AR} — {EN}، وهذا نص عربي طويل للاختبار رقم ");
    // Incompressible-ish: vary every paragraph so the zip is ~5 MB.
    let paras: Vec<String> = (0..22_000)
        .map(|i| format!("{para}{i} {:x}", i * 7919))
        .collect();
    let refs: Vec<&str> = paras.iter().map(String::as_str).collect();
    let bytes = docx(&refs, true);
    let limits = Limits {
        max_output_chars: usize::MAX,
        ..Limits::default()
    };
    let chars = extract_bytes(Kind::Docx, &bytes, &limits)
        .unwrap()
        .text
        .chars()
        .count();
    report("docx (stored)", bytes.len(), chars, || {
        extract_bytes(Kind::Docx, &bytes, &limits).unwrap();
    });
    let deflated = docx(&refs, false);
    report("docx (deflated)", deflated.len(), chars, || {
        extract_bytes(Kind::Docx, &deflated, &limits).unwrap();
    });
}

#[test]
#[ignore = "throughput measurement; run in release"]
fn perf_text_10mb() {
    let line = format!("{AR} {EN} {AR2} 0123456789\n");
    let src = line.repeat(10_000_000 / line.len());
    let limits = Limits {
        max_output_chars: usize::MAX / 8,
        ..Limits::default()
    };
    let chars = src.chars().count();
    report("text utf-8", src.len(), chars, || {
        extract_bytes(Kind::Text, src.as_bytes(), &limits).unwrap();
    });
    let (cp, _, _) = encoding_rs::WINDOWS_1256.encode(&src);
    report("text windows-1256 (detected)", cp.len(), chars, || {
        extract_bytes(Kind::Text, &cp, &limits).unwrap();
    });
    let html = format!("<html><body>{}</body></html>", src.replace('\n', "<p>"));
    report("html", html.len(), chars, || {
        extract_bytes(Kind::Html, html.as_bytes(), &limits).unwrap();
    });
}
