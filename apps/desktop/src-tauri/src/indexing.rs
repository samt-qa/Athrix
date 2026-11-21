//! Background indexing: owns the current [`FileIndex`] and the [`IndexStatus`], and runs walks
//! on a dedicated thread (never the UI thread).
//!
//! **Overlapping requests are queued, latest wins.** If `start` is called while a walk is
//! running, the roots are remembered and a new walk starts as soon as the current one ends;
//! several requests made meanwhile collapse into the last one. Searches keep using the previous
//! index until a walk finishes and the new index is swapped in.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Instant;

use athrix_fs_index::{FileIndex, WalkOptions, walk};

use crate::model::{IndexState, IndexStatus};

/// Folder names never indexed (matched against each entry's name, case-insensitive on Windows).
pub const DEFAULT_EXCLUDED_NAMES: [&str; 4] = [
    ".git",
    "node_modules",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Path glob for the per-user temp folder (`AppData\Local\Temp`) under any root.
pub const DEFAULT_EXCLUDED_PATH_GLOB: &str = "**/AppData/Local/Temp";

/// Called with every new status (start, finish, error), from whichever thread changed it.
pub type StatusListener = Arc<dyn Fn(&IndexStatus) + Send + Sync>;

/// What [`Indexer::start`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// A walk started on a new thread.
    Started,
    /// A walk was already running; this one runs after it.
    Queued,
}

#[derive(Debug, Default)]
struct Progress {
    status: IndexStatus,
    /// Roots requested while a walk was running.
    pending: Option<Vec<PathBuf>>,
}

/// The index and its status, shared between commands and the indexing thread.
#[derive(Debug, Default)]
pub struct Indexer {
    index: RwLock<Option<Arc<FileIndex>>>,
    progress: Mutex<Progress>,
}

impl Indexer {
    /// An indexer with no index (`idle`).
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the current status.
    pub fn status(&self) -> IndexStatus {
        self.lock().status.clone()
    }

    /// The index searches should use, if any walk has finished.
    pub fn index(&self) -> Option<Arc<FileIndex>> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Starts indexing `roots` in the background and returns immediately. See the module docs
    /// for what happens when a walk is already running.
    pub fn start(self: &Arc<Self>, roots: Vec<PathBuf>, listener: StatusListener) -> StartOutcome {
        let started = {
            let mut p = self.lock();
            if p.status.state == IndexState::Indexing {
                p.pending = Some(roots);
                return StartOutcome::Queued;
            }
            p.status = self.indexing_status(&roots);
            p.status.clone()
        };
        listener(&started);

        let this = Arc::clone(self);
        let worker_listener = Arc::clone(&listener);
        let spawned = std::thread::Builder::new()
            .name("athrix-indexer".into())
            .spawn(move || this.run(roots, &worker_listener));
        if let Err(e) = spawned {
            let failed = {
                let mut p = self.lock();
                p.status.state = IndexState::Error;
                p.status.error = Some(format!("cannot start the indexing thread: {e}"));
                p.pending = None;
                p.status.clone()
            };
            listener(&failed);
        }
        StartOutcome::Started
    }

    /// The indexing thread: walks `roots`, then any roots queued meanwhile.
    fn run(&self, mut roots: Vec<PathBuf>, listener: &StatusListener) {
        loop {
            let started = Instant::now();
            let opts = walk_options(&roots);
            let result = catch_unwind(AssertUnwindSafe(|| walk(&opts)));
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

            let (finished, next) = {
                let mut p = self.lock();
                match result {
                    Ok(Ok(index)) => {
                        let entries = index.len() as u64;
                        tracing::info!(entries, elapsed_ms, "indexing finished");
                        *self.index.write().unwrap_or_else(PoisonError::into_inner) =
                            Some(Arc::new(index));
                        p.status.state = IndexState::Ready;
                        p.status.entries = entries;
                        p.status.elapsed_ms = Some(elapsed_ms);
                        p.status.error = None;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "indexing failed");
                        p.status.state = IndexState::Error;
                        p.status.error = Some(e.to_string());
                    }
                    Err(_) => {
                        tracing::error!("indexing thread panicked");
                        p.status.state = IndexState::Error;
                        p.status.error = Some("indexing stopped unexpectedly".into());
                    }
                }
                let finished = p.status.clone();
                let next = p.pending.take();
                if let Some(next) = &next {
                    p.status = self.indexing_status(next);
                }
                (finished, next.map(|n| (n, p.status.clone())))
            };
            listener(&finished);
            match next {
                Some((next_roots, restarted)) => {
                    listener(&restarted);
                    roots = next_roots;
                }
                None => return,
            }
        }
    }

    /// Status for a walk of `roots` that is about to start; keeps the current entry count since
    /// searches still use the previous index.
    fn indexing_status(&self, roots: &[PathBuf]) -> IndexStatus {
        IndexStatus {
            state: IndexState::Indexing,
            entries: self.index().map_or(0, |ix| ix.len() as u64),
            roots: roots
                .iter()
                .map(|r| r.to_string_lossy().into_owned())
                .collect(),
            elapsed_ms: None,
            error: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Walk options for `roots` with the default exclusions from the IPC contract.
pub fn walk_options(roots: &[PathBuf]) -> WalkOptions {
    let mut opts = WalkOptions::new(roots.iter().cloned());
    for name in DEFAULT_EXCLUDED_NAMES {
        opts = opts.exclude_glob(name);
    }
    opts.exclude_glob(DEFAULT_EXCLUDED_PATH_GLOB)
}

/// The default root: the user's home folder (`USERPROFILE` on Windows, `HOME` elsewhere).
///
/// Read from the environment instead of `std::env::home_dir`, which is deprecated on the MSRV
/// (Rust 1.85) and would fail clippy with `-D warnings` there.
pub fn default_roots() -> Result<Vec<PathBuf>, String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|p| !p.is_empty())
        .map(|p| vec![PathBuf::from(p)])
        .ok_or_else(|| "cannot find the user's home folder".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in [
            "docs",
            ".git/objects",
            "node_modules/pkg",
            "AppData/Local/Temp/x",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        for f in [
            "docs/تقرير.pdf",
            ".git/HEAD",
            "node_modules/pkg/a.js",
            "AppData/Local/Temp/x/t.tmp",
        ] {
            fs::write(p.join(f), b"x").unwrap();
        }
        dir
    }

    fn listener() -> (StatusListener, mpsc::Receiver<IndexStatus>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let l: StatusListener = Arc::new(move |s: &IndexStatus| {
            let _ = tx.lock().unwrap().send(s.clone());
        });
        (l, rx)
    }

    fn next(rx: &mpsc::Receiver<IndexStatus>) -> IndexStatus {
        rx.recv_timeout(Duration::from_secs(30))
            .expect("status event")
    }

    #[test]
    fn indexes_in_background_with_default_exclusions() {
        let dir = tree();
        let indexer = Arc::new(Indexer::new());
        assert_eq!(indexer.status().state, IndexState::Idle);
        let (l, rx) = listener();
        let root = dir.path().to_path_buf();
        assert_eq!(indexer.start(vec![root.clone()], l), StartOutcome::Started);
        assert_eq!(next(&rx).state, IndexState::Indexing);
        let done = next(&rx);
        assert_eq!(done.state, IndexState::Ready, "{done:?}");
        // root, docs, docs/تقرير.pdf, AppData, AppData/Local (Temp, .git, node_modules excluded)
        assert_eq!(done.entries, 5, "{done:?}");
        assert!(done.elapsed_ms.is_some());
        assert_eq!(done.roots, vec![root.to_string_lossy().into_owned()]);
        assert_eq!(indexer.index().unwrap().len(), 5);
        assert_eq!(indexer.status(), done);
    }

    #[test]
    fn a_second_start_while_running_is_queued_and_latest_wins() {
        let a = tree();
        let b = tree();
        let c = tree();
        let indexer = Arc::new(Indexer::new());
        let (l, rx) = listener();
        // Pretend a walk of `a` is running, then ask for `b` and `c` meanwhile.
        indexer.lock().status = indexer.indexing_status(&[a.path().to_path_buf()]);
        let queued =
            |d: &tempfile::TempDir| indexer.start(vec![d.path().to_path_buf()], Arc::clone(&l));
        assert_eq!(queued(&b), StartOutcome::Queued);
        assert_eq!(queued(&c), StartOutcome::Queued);
        assert!(rx.try_recv().is_err(), "queuing emits nothing");
        // Run the "running" walk on this thread: it must then pick up `c` (not `b`).
        indexer.run(vec![a.path().to_path_buf()], &l);
        let events: Vec<_> = rx.try_iter().collect();
        let states: Vec<_> = events.iter().map(|s| s.state).collect();
        assert_eq!(
            states,
            [IndexState::Ready, IndexState::Indexing, IndexState::Ready]
        );
        let root = |d: &tempfile::TempDir| vec![d.path().to_string_lossy().into_owned()];
        assert_eq!(events[0].roots, root(&a));
        assert_eq!(events[1].roots, root(&c));
        assert_eq!(events[1].entries, 5, "previous index stays searchable");
        assert_eq!(events[2].roots, root(&c));
        assert!(indexer.lock().pending.is_none());
    }

    #[test]
    fn missing_roots_give_an_empty_ready_index() {
        // The walker skips unreadable roots instead of failing.
        let indexer = Arc::new(Indexer::new());
        let (l, rx) = listener();
        indexer.start(vec![PathBuf::from("Z:/definitely/missing/athrix")], l);
        assert_eq!(next(&rx).state, IndexState::Indexing);
        let done = next(&rx);
        assert_eq!(done.state, IndexState::Ready);
        assert_eq!(done.entries, 0);
    }
}
