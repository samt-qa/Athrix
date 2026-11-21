//! Unit tests. They live inside the crate so the fuzz tests can call the
//! parsers without the `catch_unwind` guard and see real panics.

// Fixture builders favour readability over allocation-free string building.
#![allow(clippy::format_collect)]

mod fixtures;
mod formats;
mod fuzz;
#[cfg(feature = "pdf")]
mod pdf;
mod perf;

/// Property-test case count: `PROPTEST_CASES` when set, else `default`.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
