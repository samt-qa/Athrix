//! `--search-in <folder>`: Explorer's "Search here with Athrix" (task 5.4).
//!
//! The installer registers a context-menu verb on folders, drives and folder backgrounds that
//! runs `"<install dir>\Athrix.exe" --search-in "%V"`. The folder arrives either in this
//! process's argv (first launch) or in the single-instance callback (Athrix already running).
//! Either way [`handle_args`] shows the main window and hands the folder to the frontend, which
//! turns it into an `infolder:"<path>"` query.
//!
//! Delivery (see `docs/IPC-CONTRACT.md`): until the frontend has called
//! [`take_pending_search_in`] once, the folder is kept as pending (the webview may not be
//! listening yet); after that, each new folder is emitted to the main window as
//! [`SEARCH_IN_EVENT`]. The frontend therefore calls `take_pending_search_in` once at start-up
//! and listens to the event; it never sees the same folder twice.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::popup::{MAIN_LABEL, show_main};

/// The command-line flag the context-menu verb passes.
pub const SEARCH_IN_FLAG: &str = "--search-in";
/// Event emitted to the main window with a [`SearchInPayload`].
pub const SEARCH_IN_EVENT: &str = "app://search-in";

/// Payload of [`SEARCH_IN_EVENT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchInPayload {
    /// The folder to search in (absolute, an existing directory).
    pub path: String,
}

/// Pending `--search-in` folder and whether the frontend has started taking them.
#[derive(Debug, Default)]
pub struct SearchInState {
    inner: Mutex<SearchInInner>,
}

#[derive(Debug, Default)]
struct SearchInInner {
    /// Set once the frontend called `take_pending_search_in`: later folders are emitted.
    frontend_ready: bool,
    /// The latest folder not yet delivered (only before `frontend_ready`).
    pending: Option<String>,
}

impl SearchInState {
    /// Either keeps `path` as pending (frontend not ready) or returns it for emitting.
    fn offer(&self, path: String) -> Option<String> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.frontend_ready {
            Some(path)
        } else {
            inner.pending = Some(path);
            None
        }
    }

    /// Marks the frontend ready and returns (and clears) the pending folder.
    fn take(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.frontend_ready = true;
        inner.pending.take()
    }
}

/// The raw value of `--search-in` in `args` (the program path, if present, is just skipped).
/// Accepts `--search-in <value>` and `--search-in=<value>`; the last occurrence wins. The value
/// is cleaned by [`clean_path_arg`]. `None` when the flag is missing or has no value.
pub fn parse_search_in<I, S>(args: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        if arg == SEARCH_IN_FLAG {
            if let Some(value) = args.get(i + 1) {
                found = clean_path_arg(&value.to_string_lossy());
                i += 1;
            }
        } else if let Some(value) = arg
            .strip_prefix(SEARCH_IN_FLAG)
            .and_then(|r| r.strip_prefix('='))
        {
            found = clean_path_arg(value);
        }
        i += 1;
    }
    found
}

/// Undoes what quoting does to a folder path on the command line. Explorer passes `"%V"`; for a
/// drive root that is `"C:\"`, whose `\"` the Windows argv rules read as an escaped quote, so
/// the value arrives as `C:"`. Trailing quotes are dropped and a bare drive (`C:`) becomes its
/// root (`C:\`). Surrounding whitespace is trimmed. `None` when nothing is left.
pub fn clean_path_arg(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Some(PathBuf::from(format!("{trimmed}\\")));
    }
    Some(PathBuf::from(trimmed))
}

/// `path` made absolute against `cwd` (a relative path only comes from a hand-typed command)
/// and kept only if it is an existing directory.
pub fn resolve_dir(path: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let path = match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path.to_path_buf(),
    };
    path.is_dir().then_some(path)
}

/// Handles the arguments of a launch: when they carry a valid `--search-in` folder, shows the
/// main window and delivers the folder (see the module docs). Other arguments are ignored; an
/// invalid folder is logged and ignored. Returns whether a folder was accepted.
pub fn handle_args<R: Runtime, I, S>(app: &AppHandle<R>, args: I, cwd: Option<&Path>) -> bool
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let Some(raw) = parse_search_in(args) else {
        return false;
    };
    let Some(dir) = resolve_dir(&raw, cwd) else {
        tracing::warn!(path = %raw.display(), "--search-in: not an existing folder; ignored");
        return false;
    };
    let path = dir.to_string_lossy().into_owned();
    tracing::info!(%path, "search in folder (Explorer context menu)");
    show_main(app);
    if let Some(path) = app.state::<SearchInState>().offer(path) {
        let payload = SearchInPayload { path };
        if let Err(err) = app.emit_to(MAIN_LABEL, SEARCH_IN_EVENT, payload) {
            tracing::warn!("couldn't emit {SEARCH_IN_EVENT}: {err}");
        }
    }
    true
}

/// `take_pending_search_in`: the folder from a `--search-in` launch that arrived before the
/// frontend was listening, or `null`. The first call also switches later folders to
/// [`SEARCH_IN_EVENT`].
#[tauri::command]
pub fn take_pending_search_in(state: tauri::State<'_, SearchInState>) -> Option<String> {
    state.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Option<PathBuf> {
        parse_search_in(args.iter().copied())
    }

    #[test]
    fn finds_the_flag_and_its_value() {
        assert_eq!(
            parse(&["Athrix.exe", "--search-in", r"C:\Users\me\Docs"]),
            Some(PathBuf::from(r"C:\Users\me\Docs"))
        );
        assert_eq!(
            parse(&["Athrix.exe", r"--search-in=D:\مشاريع"]),
            Some(PathBuf::from(r"D:\مشاريع"))
        );
    }

    #[test]
    fn missing_flag_or_value_is_none() {
        assert_eq!(parse(&["Athrix.exe"]), None);
        assert_eq!(parse(&["Athrix.exe", "--search-in"]), None);
        assert_eq!(parse(&["Athrix.exe", "--search-in", "  "]), None);
        assert_eq!(parse(&["Athrix.exe", "--search-in="]), None);
        assert_eq!(parse(&["Athrix.exe", "--search-inx", "C:\\"]), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn last_occurrence_wins_and_other_args_are_skipped() {
        assert_eq!(
            parse(&[
                "x",
                "--other",
                "--search-in",
                "A",
                "--search-in",
                "B",
                "--x"
            ]),
            Some(PathBuf::from("B"))
        );
    }

    #[test]
    fn drive_root_quote_mangling_is_undone() {
        // `"C:\"` reaches argv as `C:"`.
        assert_eq!(clean_path_arg("C:\""), Some(PathBuf::from("C:\\")));
        assert_eq!(clean_path_arg("d:"), Some(PathBuf::from("d:\\")));
        assert_eq!(
            clean_path_arg(" \"E:\\Some Folder\" "),
            Some(PathBuf::from("E:\\Some Folder"))
        );
        assert_eq!(clean_path_arg("\"\""), None);
    }

    #[test]
    fn only_existing_directories_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            resolve_dir(dir.path(), None),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(resolve_dir(&file, None), None);
        assert_eq!(resolve_dir(&dir.path().join("missing"), None), None);
        // Relative paths resolve against the launch's working directory.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(
            resolve_dir(Path::new("sub"), Some(dir.path())),
            Some(dir.path().join("sub"))
        );
    }

    #[test]
    fn pending_until_the_frontend_takes_it_then_emitted() {
        let state = SearchInState::default();
        assert_eq!(state.offer("A".into()), None);
        assert_eq!(state.offer("B".into()), None); // the latest wins
        assert_eq!(state.take(), Some("B".into()));
        assert_eq!(state.take(), None);
        // Ready: new folders go straight to the event.
        assert_eq!(state.offer("C".into()), Some("C".into()));
        assert_eq!(state.take(), None);
    }
}
