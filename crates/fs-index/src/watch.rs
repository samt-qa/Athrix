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
//!
//! **Polled roots (removable drives).** An OS watch keeps a directory handle open on its root
//! for as long as it runs, and Windows refuses to "safely remove" a drive with an open handle.
//! Roots given to [`Watcher::start_with_polling`] are therefore not watched by the OS but
//! scanned every [`PollConfig::interval`] (with the walk's own exclusions) and compared with
//! the previous scan; the differences arrive through the same debouncing as OS events. No
//! handle stays open between scans, so ejecting only fails if it happens during a scan (a
//! retry a moment later succeeds). A scan never takes more than about a fifth of the time: the
//! wait after a scan is at least four times its duration. When a polled root disappears the
//! watcher reports [`WatchBatch::RescanNeeded`], like an OS watch whose drive went away.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use athrix_core::{Error, Result};
use notify::{EventKind, RecursiveMode, Watcher as _};

use crate::index::{EntryId, FileIndex};
use crate::walk::{Scope, WalkOptions, path_key};

/// Which roots [`Watcher::start_with_polling`] scans instead of watching, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollConfig {
    /// Roots (among the walk options' roots) to poll. Others are watched by the OS.
    pub roots: Vec<PathBuf>,
    /// Shortest time between two scans (default 20 s). The wait after a scan is at least four
    /// times its duration, so a large drive is scanned less often.
    pub interval: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        PollConfig {
            roots: Vec::new(),
            interval: Duration::from_secs(20),
        }
    }
}

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

/// A running watcher. Dropping it (or calling [`Watcher::stop`]) stops the OS watch and the
/// polling, discards pending events and joins the debounce thread (waiting for a callback in
/// progress). A scan of a polled root in progress is not waited for: it finishes in the
/// background and its result is dropped.
#[derive(Debug)]
pub struct Watcher {
    os: Option<notify::RecommendedWatcher>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    watched: Vec<PathBuf>,
    polled: Vec<PathBuf>,
    /// Dropped to stop the polling thread (which is not joined, see above).
    poll_stop: Option<Sender<()>>,
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
        Self::start_with_polling(opts, config, &PollConfig::default(), on_batch)
    }

    /// [`Watcher::start`], except that the roots of `opts` listed in `poll.roots` are scanned
    /// periodically instead of watched by the OS, so no handle stays open on them (see the
    /// module docs: removable drives can then be ejected while the app runs). A polled root
    /// that is missing at start is polled anyway and reported once it appears.
    ///
    /// # Errors
    /// As [`Watcher::start`]; also [`Error::Io`] if the polling thread cannot be started.
    pub fn start_with_polling<F>(
        opts: &WalkOptions,
        config: WatchConfig,
        poll: &PollConfig,
        on_batch: F,
    ) -> Result<Watcher>
    where
        F: FnMut(WatchBatch) + Send + 'static,
    {
        let scope = Arc::new(Scope::new(opts)?);
        let polled_keys: Vec<String> = poll.roots.iter().map(|r| path_key(r)).collect();
        let mut by_poll: Vec<PathBuf> = Vec::new();
        let mut by_os: Vec<PathBuf> = Vec::new();
        for (root, key) in &scope.roots {
            if polled_keys.contains(key) {
                by_poll.push(root.clone());
            } else {
                by_os.push(root.clone());
            }
        }
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
            polled: Vec::new(),
            poll_stop: None,
        };
        if !by_os.is_empty() {
            let filter = Arc::clone(&scope);
            let os_tx = tx.clone();
            let mut os = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Some(msg) = filter_event(&filter, res) {
                    let _ = os_tx.send(msg);
                }
            })
            .map_err(|e| notify_error(PathBuf::new(), e))?;
            for root in &by_os {
                match os.watch(root, RecursiveMode::Recursive) {
                    Ok(()) => watcher.watched.push(root.clone()),
                    Err(e) => {
                        tracing::warn!(root = %root.display(), error = %e, "cannot watch root");
                    }
                }
            }
            watcher.os = Some(os);
        }
        if !by_poll.is_empty() {
            let (stop_tx, stop_rx) = mpsc::channel();
            let roots = by_poll.clone();
            let interval = poll.interval;
            let poll_scope = Arc::clone(&scope);
            std::thread::Builder::new()
                .name("athrix-poll".into())
                .spawn(move || poll_loop(&poll_scope, &roots, interval, &tx, &stop_rx))
                .map_err(|e| Error::io("athrix-poll thread", e))?;
            tracing::info!(roots = ?by_poll, ?interval, "polling for changes (no open handle)");
            watcher.poll_stop = Some(stop_tx);
            watcher.watched.extend(by_poll.iter().cloned());
            watcher.polled = by_poll;
        }
        Ok(watcher)
    }

    /// The roots that are actually being watched (by the OS or by polling).
    pub fn watched_roots(&self) -> &[PathBuf] {
        &self.watched
    }

    /// The roots among [`Watcher::watched_roots`] that are polled, not watched by the OS.
    pub fn polled_roots(&self) -> &[PathBuf] {
        &self.polled
    }

    /// Stops watching (same as dropping).
    pub fn stop(self) {}
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // No more OS events or scans first, then end the debounce thread.
        drop(self.os.take());
        drop(self.poll_stop.take());
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

/// The polling thread: a baseline scan of every root, then a scan every `interval` (or four
/// times the last scan's duration, if longer) compared with the previous one. Ends when `stop`
/// is dropped (checked between roots and while waiting) or the debounce thread is gone.
fn poll_loop(
    scope: &Scope,
    roots: &[PathBuf],
    interval: Duration,
    tx: &Sender<Msg>,
    stop: &Receiver<()>,
) {
    let stopped = || !matches!(stop.try_recv(), Err(TryRecvError::Empty));
    let mut scans: Vec<Option<FileIndex>> = Vec::with_capacity(roots.len());
    let started = Instant::now();
    for root in roots {
        if stopped() {
            return;
        }
        scans.push(scan(scope, root));
    }
    let mut wait = interval.max(started.elapsed() * 4);
    loop {
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
        let started = Instant::now();
        for (root, previous) in roots.iter().zip(scans.iter_mut()) {
            let now = scan(scope, root);
            if stopped() {
                return;
            }
            let msg = match (previous.as_ref(), now.as_ref()) {
                (Some(old), Some(new)) => {
                    let changed = diff_scans(old, new);
                    (!changed.is_empty()).then_some(Msg::Paths(changed))
                }
                (Some(_), None) => Some(Msg::Rescan(format!(
                    "polled folder {} is gone",
                    root.display()
                ))),
                (None, Some(_)) => Some(Msg::Paths(vec![root.clone()])),
                (None, None) => None,
            };
            *previous = now;
            if let Some(msg) = msg {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
        wait = interval.max(started.elapsed() * 4);
    }
}

/// Scans `root` with the walk's rules; `None` if it is not there (before or after the scan:
/// a drive pulled out mid-scan gives no partial result).
fn scan(scope: &Scope, root: &Path) -> Option<FileIndex> {
    if !root.is_dir() {
        return None;
    }
    let index = scope.walk_subtree(root);
    root.is_dir().then_some(index)
}

/// Per-entry keys of a scan: a hash of the entry's names from its root down (parents come
/// before their children in a walk, so one pass suffices).
fn entry_keys(index: &FileIndex) -> Vec<u64> {
    let mut keys: Vec<u64> = Vec::with_capacity(index.len());
    for id in 0..index.len() as EntryId {
        let mut h = DefaultHasher::new();
        if let Some(parent) = index.parent(id) {
            keys[parent as usize].hash(&mut h);
        }
        index.name(id).hash(&mut h);
        keys.push(h.finish());
    }
    keys
}

/// The paths that differ between two scans of the same root: entries that appeared,
/// disappeared, or whose kind, size or modification time changed. Below an entry that
/// appeared or disappeared nothing more is listed (applying that entry handles its subtree).
pub(crate) fn diff_scans(old: &FileIndex, new: &FileIndex) -> Vec<PathBuf> {
    let old_keys = entry_keys(old);
    let new_keys = entry_keys(new);
    let mut unseen: HashMap<u64, EntryId> = old_keys
        .iter()
        .enumerate()
        .map(|(id, &k)| (k, id as EntryId))
        .collect();
    let mut changed = Vec::new();
    // Entries of `new` that appeared (their descendants are not listed).
    let mut appeared = vec![false; new.len()];
    for (id, key) in new_keys.iter().enumerate() {
        let id = id as EntryId;
        match unseen.remove(key) {
            Some(old_id) => {
                if old.meta(old_id) != new.meta(id) {
                    changed.push(new.path(id));
                }
            }
            None => {
                let under_new = new.parent(id).is_some_and(|p| appeared[p as usize]);
                appeared[id as usize] = true;
                if !under_new {
                    changed.push(new.path(id));
                }
            }
        }
    }
    let mut gone = vec![false; old.len()];
    let mut gone_ids: Vec<EntryId> = unseen.into_values().collect();
    gone_ids.sort_unstable();
    for id in gone_ids {
        gone[id as usize] = true;
        if !old.parent(id).is_some_and(|p| gone[p as usize]) {
            changed.push(old.path(id));
        }
    }
    changed.sort();
    changed.dedup();
    changed
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
    fn default_exclusions_drop_windows_system_noise_events() {
        let root = std::env::temp_dir().join("athrix-noise-test");
        let scope = Scope::new(&WalkOptions::new([&root]).with_default_exclusions()).unwrap();
        let mut e = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any));
        let windows = root.join("Windows");
        e.paths = vec![
            windows.join("Prefetch").join("APP.EXE-1234.pf"),
            windows
                .join("appcompat")
                .join("Programs")
                .join("Amcache.hve"),
            windows.join("System32").join("kept.dll"),
        ];
        let Some(Msg::Paths(paths)) = filter_event(&scope, Ok(e)) else {
            panic!("expected paths");
        };
        assert_eq!(paths, [windows.join("System32").join("kept.dll")]);
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

    #[test]
    fn diff_scans_lists_created_modified_and_deleted_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        for d in ["keep", "old/deep", "node_modules"] {
            std::fs::create_dir_all(p.join(d)).unwrap();
        }
        for f in [
            "keep/same.txt",
            "keep/grows.txt",
            "old/deep/x.txt",
            "gone.txt",
        ] {
            std::fs::write(p.join(f), b"x").unwrap();
        }
        let scope = Scope::new(&WalkOptions::new([p]).exclude_glob("node_modules")).unwrap();
        let before = scan(&scope, p).unwrap();
        assert!(diff_scans(&before, &before).is_empty());

        std::fs::write(p.join("keep").join("grows.txt"), b"longer").unwrap();
        std::fs::remove_dir_all(p.join("old")).unwrap();
        std::fs::remove_file(p.join("gone.txt")).unwrap();
        std::fs::create_dir_all(p.join("new").join("sub")).unwrap();
        std::fs::write(p.join("new").join("sub").join("n.txt"), b"x").unwrap();
        std::fs::write(p.join("node_modules").join("ignored.js"), b"x").unwrap();
        let after = scan(&scope, p).unwrap();

        let changed = diff_scans(&before, &after);
        // Folders whose listing changed (`keep`, the root) may be reported too; the entries
        // themselves must be, and nothing below a new or deleted folder, nor excluded ones.
        for expected in [
            p.join("keep").join("grows.txt"),
            p.join("old"),
            p.join("gone.txt"),
            p.join("new"),
        ] {
            assert!(changed.contains(&expected), "{expected:?} in {changed:?}");
        }
        for not in [
            p.join("keep").join("same.txt"),
            p.join("old").join("deep"),
            p.join("new").join("sub"),
            p.join("node_modules").join("ignored.js"),
        ] {
            assert!(!changed.contains(&not), "{not:?} in {changed:?}");
        }
    }

    #[test]
    fn a_polled_root_reports_changes_and_its_disappearance() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("usb");
        std::fs::create_dir_all(&root).unwrap();
        let (out_tx, out) = mpsc::channel();
        let poll = PollConfig {
            roots: vec![root.clone()],
            interval: Duration::from_millis(100),
        };
        let watcher = Watcher::start_with_polling(
            &WalkOptions::new([&root]),
            cfg(50, 1000),
            &poll,
            move |b| {
                let _ = out_tx.send(b);
            },
        )
        .unwrap();
        assert_eq!(watcher.watched_roots(), std::slice::from_ref(&root));
        assert_eq!(watcher.polled_roots(), std::slice::from_ref(&root));
        // Let the baseline scan happen first.
        std::thread::sleep(Duration::from_millis(50));
        let file = root.join("تقرير.txt");
        std::fs::write(&file, b"x").unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            match out.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(WatchBatch::Changed(paths)) if paths.contains(&file) => break,
                Ok(_) => {}
                Err(e) => panic!("no batch with the new file: {e}"),
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
        let deadline = Instant::now() + WAIT;
        loop {
            match out.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(WatchBatch::RescanNeeded(reason)) => {
                    assert!(reason.contains("gone"), "{reason}");
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("the removal was not reported: {e}"),
            }
        }
        let stopping = Instant::now();
        drop(watcher);
        assert!(stopping.elapsed() < Duration::from_secs(5));
    }
}
