//! Live file-system watching (task 1.5) on top of the [`notify`] crate: `ReadDirectoryChangesW`
//! on Windows, inotify on Linux, FSEvents on macOS.
//!
//! [`Watcher::start`] watches every root of a [`WalkOptions`] recursively. Each raw event is
//! filtered on the OS callback thread with the walk's own rules (outside the roots, or at or
//! below an excluded path such as `node_modules` or `**/AppData/Local/Temp`: dropped right
//! away), then a background thread **coalesces** the changed paths: a batch is delivered once
//! no new event arrived for [`WatchConfig::debounce`], or at the latest
//! [`WatchConfig::max_delay`] after its first event. A `git checkout`, an unzip or a build
//! thus arrives as one [`WatchBatch::Changed`] with each path once, which
//! [`LiveIndex::apply_paths`](crate::LiveIndex::apply_paths) reconciles against the disk.
//! Events only say *which* paths to look at; the kind of change (create, rename, delete,
//! modify) is re-read from the disk when applying, so lost ordering or merged renames do not
//! matter.
//!
//! When the platform reports that events were lost (an overflow / rescan flag) or the watcher
//! reports an error, the batch becomes [`WatchBatch::RescanNeeded`]: the caller should walk
//! again and call [`LiveIndex::replace_base`](crate::LiveIndex::replace_base).
//!
//! Known gap: notify 8 on Windows does not surface every `ReadDirectoryChangesW` overflow (a
//! completion with zero bytes is not flagged, and `ERROR_NOTIFY_ENUM_DIR` ends that root's
//! watch with only a log line). The NTFS USN journal reader (M2) closes this gap; until then an
//! occasional full re-walk (for example at startup) keeps the index honest.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use athrix_core::{Error, Result};
use notify::{EventKind, RecursiveMode, Watcher as _};

use crate::walk::{Scope, WalkOptions, path_key};

/// Debouncing settings for [`Watcher::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
    /// Quiet time that ends a batch (default 300 ms).
    pub debounce: Duration,
    /// Longest time a batch may stay open while events keep coming (default 2 s), so a
    /// long-running copy still shows progress.
    pub max_delay: Duration,
    /// More distinct paths than this in one batch turn it into
    /// [`WatchBatch::RescanNeeded`] (default 1,000,000), bounding memory.
    pub max_paths: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            debounce: Duration::from_millis(300),
            max_delay: Duration::from_secs(2),
            max_paths: 1_000_000,
        }
    }
}

/// What the watcher delivers to its callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchBatch {
    /// These paths (each once, sorted) were created, modified, renamed (old and new names) or
    /// deleted. Excluded paths never appear.
    Changed(Vec<PathBuf>),
    /// Events were lost or the watcher failed; the index may be stale anywhere under the
    /// roots. The reason is for logs.
    RescanNeeded(String),
}

enum Msg {
    Paths(Vec<PathBuf>),
    Rescan(String),
    Stop,
}

/// A running watcher. Dropping it (or calling [`Watcher::stop`]) stops the OS watch, discards
/// pending events and joins the background thread (waiting for a callback in progress).
#[derive(Debug)]
pub struct Watcher {
    os: Option<notify::RecommendedWatcher>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    watched: Vec<PathBuf>,
}

impl Watcher {
    /// Starts watching every root of `opts` recursively and calls `on_batch` on a background
    /// thread with coalesced batches. A root that cannot be watched (missing, no access) is
    /// skipped with a `tracing` warning; see [`Watcher::watched_roots`].
    ///
    /// # Errors
    /// [`Error::Config`] if an exclusion glob is invalid, [`Error::Io`] if the OS watcher
    /// cannot be created.
    pub fn start<F>(opts: &WalkOptions, config: WatchConfig, on_batch: F) -> Result<Watcher>
    where
        F: FnMut(WatchBatch) + Send + 'static,
    {
        let scope = Arc::new(Scope::new(opts)?);
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("athrix-watch".into())
            .spawn(move || debounce_loop(&rx, config, on_batch))
            .map_err(|e| Error::io("athrix-watch thread", e))?;
        let mut watcher = Watcher {
            os: None,
            tx: tx.clone(),
            thread: Some(thread),
            watched: Vec::new(),
        };
        let filter = Arc::clone(&scope);
        let mut os = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Some(msg) = filter_event(&filter, res) {
                let _ = tx.send(msg);
            }
        })
        .map_err(|e| notify_error(PathBuf::new(), e))?;
        for (root, _) in &scope.roots {
            match os.watch(root, RecursiveMode::Recursive) {
                Ok(()) => watcher.watched.push(root.clone()),
                Err(e) => {
                    tracing::warn!(root = %root.display(), error = %e, "cannot watch root");
                }
            }
        }
        watcher.os = Some(os);
        Ok(watcher)
    }

    /// The roots that are actually being watched.
    pub fn watched_roots(&self) -> &[PathBuf] {
        &self.watched
    }

    /// Stops watching (same as dropping).
    pub fn stop(self) {}
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // No more OS events first, then end the debounce thread.
        drop(self.os.take());
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            // Dropped from inside the callback: the thread ends on its own after it returns.
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

fn notify_error(path: PathBuf, e: notify::Error) -> Error {
    Error::io(path, std::io::Error::other(e))
}

/// Turns one raw notify result into a message, dropping what the walk would not index.
fn filter_event(scope: &Scope, res: notify::Result<notify::Event>) -> Option<Msg> {
    let event = match res {
        Ok(e) => e,
        Err(e) => return Some(Msg::Rescan(format!("watcher error: {e}"))),
    };
    if event.need_rescan() {
        return Some(Msg::Rescan("the platform reported lost events".into()));
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return None;
    }
    let paths: Vec<PathBuf> = event
        .paths
        .into_iter()
        .filter(|p| scope.admits(p, &path_key(p)))
        .collect();
    (!paths.is_empty()).then_some(Msg::Paths(paths))
}

/// Collects messages into batches (see the module docs) until `Stop` or disconnection.
fn debounce_loop<F: FnMut(WatchBatch)>(rx: &Receiver<Msg>, config: WatchConfig, mut on_batch: F) {
    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut rescan: Option<String> = None;
    // (first event, last event) of the open batch.
    let mut open: Option<(Instant, Instant)> = None;
    loop {
        let msg = match open {
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            },
            Some((first, last)) => {
                let deadline = (last + config.debounce).min(first + config.max_delay);
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => {
                        let batch = match rescan.take() {
                            Some(reason) => {
                                pending.clear();
                                WatchBatch::RescanNeeded(reason)
                            }
                            None => {
                                let mut paths: Vec<PathBuf> = pending.drain().collect();
                                paths.sort();
                                WatchBatch::Changed(paths)
                            }
                        };
                        open = None;
                        on_batch(batch);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        };
        let now = Instant::now();
        match msg {
            Msg::Stop => return,
            Msg::Paths(paths) => {
                if rescan.is_none() {
                    pending.extend(paths);
                    if pending.len() > config.max_paths {
                        pending.clear();
                        rescan = Some(format!("more than {} changed paths", config.max_paths));
                    }
                }
            }
            Msg::Rescan(reason) => {
                pending.clear();
                rescan.get_or_insert(reason);
            }
        }
        open = Some(open.map_or((now, now), |(first, _)| (first, now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(config: WatchConfig) -> (Sender<Msg>, Receiver<WatchBatch>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let t = std::thread::spawn(move || {
            debounce_loop(&rx, config, move |b| {
                let _ = out_tx.send(b);
            });
        });
        (tx, out_rx, t)
    }

    fn cfg(debounce_ms: u64, max_delay_ms: u64) -> WatchConfig {
        WatchConfig {
            debounce: Duration::from_millis(debounce_ms),
            max_delay: Duration::from_millis(max_delay_ms),
            max_paths: 1000,
        }
    }

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn a_burst_becomes_one_deduplicated_sorted_batch() {
        let (tx, out, t) = run(cfg(200, 10_000));
        for i in (0..500).rev() {
            tx.send(Msg::Paths(vec![PathBuf::from(format!("f{:03}", i % 250))]))
                .unwrap();
        }
        let WatchBatch::Changed(paths) = out.recv_timeout(WAIT).unwrap() else {
            panic!("expected changes");
        };
        assert_eq!(paths.len(), 250);
        assert!(paths.windows(2).all(|w| w[0] < w[1]));
        assert!(out.recv_timeout(Duration::from_millis(400)).is_err());
        tx.send(Msg::Stop).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn max_delay_flushes_a_never_ending_stream() {
        let (tx, out, t) = run(cfg(150, 300));
        let start = Instant::now();
        let mut i = 0;
        while out.try_recv().is_err() {
            assert!(start.elapsed() < WAIT, "no batch while events keep coming");
            tx.send(Msg::Paths(vec![PathBuf::from(format!("p{i}"))]))
                .unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(tx);
        t.join().unwrap();
    }

    #[test]
    fn rescan_wins_over_paths_and_overflow_becomes_rescan() {
        let (tx, out, t) = run(cfg(100, 10_000));
        tx.send(Msg::Paths(vec![PathBuf::from("a")])).unwrap();
        tx.send(Msg::Rescan("lost".into())).unwrap();
        tx.send(Msg::Paths(vec![PathBuf::from("b")])).unwrap();
        assert_eq!(
            out.recv_timeout(WAIT).unwrap(),
            WatchBatch::RescanNeeded("lost".into())
        );
        let many: Vec<PathBuf> = (0..1001).map(|i| PathBuf::from(format!("x{i}"))).collect();
        tx.send(Msg::Paths(many)).unwrap();
        assert!(matches!(
            out.recv_timeout(WAIT).unwrap(),
            WatchBatch::RescanNeeded(_)
        ));
        // Back to normal afterwards.
        tx.send(Msg::Paths(vec![PathBuf::from("c")])).unwrap();
        assert_eq!(
            out.recv_timeout(WAIT).unwrap(),
            WatchBatch::Changed(vec![PathBuf::from("c")])
        );
        tx.send(Msg::Stop).unwrap();
        t.join().unwrap();
    }

    #[test]
    fn filter_drops_excluded_outside_and_access_events() {
        let root = std::env::temp_dir().join("athrix-filter-test");
        let opts = WalkOptions::new([&root])
            .exclude_glob("node_modules")
            .exclude_glob("**/AppData/Local/Temp");
        let scope = Scope::new(&opts).unwrap();
        let ev = |kind, paths: Vec<PathBuf>| {
            let mut e = notify::Event::new(kind);
            e.paths = paths;
            Ok(e)
        };
        let create = EventKind::Create(notify::event::CreateKind::Any);
        let kept = filter_event(
            &scope,
            ev(
                create,
                vec![
                    root.join("a.txt"),
                    root.join("node_modules").join("x.js"),
                    root.join("AppData").join("Local").join("Temp").join("t"),
                    root.join("AppData").join("Local").join("keep"),
                    PathBuf::from("/elsewhere/y"),
                ],
            ),
        );
        let Some(Msg::Paths(paths)) = kept else {
            panic!("expected paths");
        };
        assert_eq!(
            paths,
            [
                root.join("a.txt"),
                root.join("AppData").join("Local").join("keep")
            ]
        );
        assert!(filter_event(&scope, ev(create, vec![root.join("node_modules")])).is_none());
        let access = EventKind::Access(notify::event::AccessKind::Any);
        assert!(filter_event(&scope, ev(access, vec![root.join("a.txt")])).is_none());
        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert!(matches!(
            filter_event(&scope, Ok(rescan)),
            Some(Msg::Rescan(_))
        ));
        assert!(matches!(
            filter_event(&scope, Err(notify::Error::generic("boom"))),
            Some(Msg::Rescan(_))
        ));
    }
}
