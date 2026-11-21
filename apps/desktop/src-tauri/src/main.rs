//! Athrix desktop entry point.

// Prevents an extra console window on Windows in release builds. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    athrix_desktop_lib::run()
}
