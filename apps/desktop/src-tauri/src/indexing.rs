//! Background indexing: owns the live name index and the [`IndexStatus`], runs walks on a
//! dedicated thread (never the UI thread) and keeps the index current with a file watcher.
//!
//! **Overlapping requests are queued, latest wins.** If `start` is called while the indexing
//! thread is busy, the roots are remembered and a new walk starts as soon as the current one
//! ends; several requests made meanwhile collapse into the last one. Searches keep using the
//! previous index until a walk finishes and the new index is swapped in.
//!
//! **Snapshots (task 1.4).** With a [`SnapshotStore`] set, every successful walk is saved
//! (after the swap, on the indexing thread), and [`Indexer::start_from_snapshot`] first loads
//! the saved index of the same roots so searching works at once, then walks to refresh it.
//!
//! **Live updates (task 1.5).** A walked or loaded index becomes the base of an
//! [`athrix_fs_index::LiveIndex`]. When a walk starts, a [`Watcher`] is started for its roots
//! (or kept, if it already watches them). Its batches are applied to the live index of the
//! same roots, if there is one, and also remembered while the walk runs; once the walk's index
//! is installed, the remembered paths are applied to it too, so a change made during a long
//! walk is not lost whether or not the walk saw it. A [`WatchBatch::RescanNeeded`] queues a
//! new walk of the watched roots through the same queue (latest wins).
//!
//! **Generations and views.** Every snapshot the app publishes (walk, snapshot load, applied
//! batch, compaction) becomes an [`IndexView`] with a new 21-bit generation. Result ids carry
//! the generation (`generation << 32 | entry`) and are resolved against the view of that
//! generation, kept in a small ring of the [`RECENT_VIEWS`] latest views that share the
//! current base; ids of older views (or of a replaced base) are rejected as stale.

use std::collections::VecDeque;
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use athrix_fs_index::{
    EntryId, FileIndex, IndexSnapshot, LiveIndex, WalkOptions, WatchBatch, WatchConfig, Watcher,
    walk,
};

use crate::model::{IndexState, IndexStatus};
use crate::roots::path_key;

mod snapshot_store;

pub use snapshot_store::SnapshotStore;

/// Folder names never indexed (matched against each entry's name, case-insensitive on Windows).
///
/// Besides the usual noise (`.git`, `node_modules`), this skips system folders that only matter
/// when whole drives are indexed: the recycle bin, `System Volume Information`, and Windows'
/// update / reset staging folders (`$WinREAgent`, `$Windows.~BT`, `$Windows.~WS`, `$SysReset`),
/// which hold hundreds of thousands of copies of system files nobody searches for.
pub const DEFAULT_EXCLUDED_NAMES: [&str; 8] = [
    ".git",
    "node_modules",
    "$RECYCLE.BIN",
    "System Volume Information",
    "$WinREAgent",
    "$Windows.~BT",
    "$Windows.~WS",
    "$SysReset",
];

/// Path globs never indexed, under any root:
/// - the per-user temp folder (`AppData\Local\Temp`);
/// - the Windows component store (`Windows\WinSxS`): ~100k hard-linked copies of system files,
///   often a tenth of a system drive's entries, never what a user is looking for.
pub const DEFAULT_EXCLUDED_PATH_GLOBS: [&str; 2] = ["**/AppData/Local/Temp", "**/Windows/WinSxS"];

/// Pseudo file systems skipped when `/` is indexed (Linux): `/proc`, `/sys`, `/dev`. Not used on
/// Windows.
pub const UNIX_EXCLUDED_PREFIXES: [&str; 3] = ["/proc", "/sys", "/dev"];

/// Bits of an index generation. Generations wrap within this width so result ids
/// (`generation << 32 | entry`) stay below 2^53, the largest integer JavaScript holds exactly.
pub const GENERATION_BITS: u32 = 21;

/// Mask applied to generations, see [`GENERATION_BITS`].
pub const GENERATION_MASK: u32 = (1 << GENERATION_BITS) - 1;

/// How many of the latest views (sharing the current base) stay resolvable, so a result shown
/// just before a few batches were applied can still be opened.
pub const RECENT_VIEWS: usize = 16;

/// Minimum time between two status events caused by watched changes (entry count updates).
pub const LIVE_STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Changed paths remembered during a walk beyond which the walk's index is not patched but a
/// new walk is queued instead.
const MAX_WALK_BUFFER: usize = 1_000_000;

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

/// One published state of the live index: an immutable [`IndexSnapshot`] tagged with the
/// app generation that result ids carry, plus the id runs of each root for scoped search.
///
/// Ids are only meaningful with the view (snapshot) they came from. Derefs to the
/// [`IndexSnapshot`].
#[derive(Debug)]
pub struct IndexView {
    generation: u32,
    snapshot: Arc<IndexSnapshot>,
    /// Runs of the snapshot's base (shared by all views of the same base).
    runs: Arc<[IndexRoot]>,
    /// [`path_key`]s of the indexed roots (every entry, base or delta, lies under one).
    root_keys: Arc<[String]>,
}

/// A run of consecutive base entry ids that all belong to one root.
///
/// The walker (and a snapshot of it) stores each root followed by its whole subtree before the
/// next root, so a walked base has exactly one run per root, from the root entry up to the
/// next root entry. A base produced by compaction appends the entries added since the walk
/// after all walked entries, which gives further (short) runs for their roots at the end.
/// Search scopes rely on the runs to skip the parts of the base outside the scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRoot {
    /// First id of the run.
    pub start: EntryId,
    /// One past the last id of the run.
    pub end: EntryId,
    /// [`crate::roots::path_key`] of the run's root path.
    pub key: String,
}

impl IndexView {
    /// Tags `snapshot` with `generation` (masked to [`GENERATION_BITS`]). `roots` are the
    /// indexed roots (the walk's); the base runs are computed here (one pass over the base).
    pub fn new(generation: u32, snapshot: Arc<IndexSnapshot>, roots: &[PathBuf]) -> Self {
        let runs = base_runs(snapshot.base()).into();
        let root_keys = roots.iter().map(|r| path_key(r)).collect();
        Self {
            generation: generation & GENERATION_MASK,
            snapshot,
            runs,
            root_keys,
        }
    }

    /// A view of a plain index that is not watched (tests, tools): its roots are its root
    /// entries.
    pub fn from_index(generation: u32, index: FileIndex) -> Self {
        let roots: Vec<PathBuf> = (0..EntryId::try_from(index.len()).unwrap_or(EntryId::MAX))
            .filter(|&id| index.parent(id).is_none())
            .map(|id| PathBuf::from(index.name(id)))
            .collect();
        // No exclusion globs, so creating the live index cannot fail.
        let live = LiveIndex::new(index, &WalkOptions::new(roots.iter().cloned()))
            .expect("walk options without globs are valid");
        Self::new(generation, live.snapshot(), &roots)
    }

    /// The next view: same base runs if `snapshot` has the same base as `self`.
    fn successor(&self, generation: u32, snapshot: Arc<IndexSnapshot>) -> Self {
        let runs = if Arc::ptr_eq(snapshot.base(), self.snapshot.base()) {
            Arc::clone(&self.runs)
        } else {
            base_runs(snapshot.base()).into()
        };
        Self {
            generation: generation & GENERATION_MASK,
            snapshot,
            runs,
            root_keys: Arc::clone(&self.root_keys),
        }
    }

    /// The generation, `<= GENERATION_MASK`.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The snapshot this view shows.
    pub fn snapshot(&self) -> &Arc<IndexSnapshot> {
        &self.snapshot
    }

    /// The runs of base ids per root, in id order (see [`IndexRoot`]).
    pub fn roots(&self) -> &[IndexRoot] {
        &self.runs
    }

    /// [`path_key`]s of the indexed roots.
    pub fn root_keys(&self) -> &[String] {
        &self.root_keys
    }

    /// True if `entry` is a live entry of this view (what a result id of it can point at).
    pub fn has_entry(&self, entry: EntryId) -> bool {
        (entry as usize) < self.snapshot.id_bound() && self.snapshot.contains(entry)
    }
}

impl Deref for IndexView {
    type Target = IndexSnapshot;

    fn deref(&self) -> &IndexSnapshot {
        &self.snapshot
    }
}

/// Splits the ids of `index` into runs of entries of the same root (see [`IndexRoot`]).
/// Parents always precede their children, so the root of an entry is the root of its parent,
/// found in the current run or by binary search over the runs so far.
fn base_runs(index: &FileIndex) -> Vec<IndexRoot> {
    let len = EntryId::try_from(index.len()).unwrap_or(EntryId::MAX);
    let mut runs: Vec<IndexRoot> = Vec::new();
    // Root entry id of each run.
    let mut run_roots: Vec<EntryId> = Vec::new();
    for id in 0..len {
        let root = match index.parent(id) {
            None => id,
            Some(parent) => {
                let at = runs.partition_point(|r| r.start <= parent);
                match at.checked_sub(1) {
                    Some(i) => run_roots[i],
                    None => id,
                }
            }
        };
        match (runs.last_mut(), run_roots.last()) {
            (Some(run), Some(&r)) if r == root && run.end == id => run.end = id + 1,
            _ => {
                runs.push(IndexRoot {
                    start: id,
                    end: id + 1,
                    key: path_key(Path::new(index.name(root))),
                });
                run_roots.push(root);
            }
        }
    }
    runs
}

#[derive(Debug, Default)]
struct Progress {
    status: IndexStatus,
    /// Roots requested while the indexing thread was busy.
    pending: Option<Vec<PathBuf>>,
    /// The indexing thread is alive (loading a snapshot, walking or saving). Requests made
    /// meanwhile are queued in `pending`.
    running: bool,
    /// When the last status event caused by watched changes was emitted.
    last_live_emit: Option<Instant>,
    /// A delayed status event (throttling) is already scheduled.
    live_emit_scheduled: bool,
}

/// Paths changed while a walk runs, applied to the walk's index once it is installed.
#[derive(Debug, Default)]
struct WalkBuffer {
    paths: Vec<PathBuf>,
    overflowed: bool,
}

/// The live index, the watcher and their bookkeeping. Locked only briefly, never while
/// applying a batch or walking (lock order: `core`, then `views`).
#[derive(Debug, Default)]
struct Core {
    /// The live index searches see (through `views`), and the roots it was walked with.
    live: Option<Arc<LiveIndex>>,
    live_roots: Vec<PathBuf>,
    /// Bumped whenever `live` is replaced; results of applying a batch to an older live index
    /// are not published.
    session: u64,
    /// The running watcher, the roots it watches and its token (bumped on every restart, so
    /// batches of a stopped watcher are ignored).
    watcher: Option<Watcher>,
    watch_roots: Vec<PathBuf>,
    watch_token: u64,
    /// Set while a walk runs.
    during_walk: Option<WalkBuffer>,
}

/// The listener of the last `start` call, used for live updates and rescans.
#[derive(Default)]
struct ListenerSlot(Mutex<Option<StatusListener>>);

impl std::fmt::Debug for ListenerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ListenerSlot")
    }
}

/// The index and its status, shared between commands, the indexing thread and the watcher.
#[derive(Debug, Default)]
pub struct Indexer {
    /// The latest views, oldest first (at most [`RECENT_VIEWS`], all of the current base).
    views: RwLock<VecDeque<Arc<IndexView>>>,
    core: Mutex<Core>,
    progress: Mutex<Progress>,
    /// Last generation handed out (before masking).
    generation: AtomicU32,
    /// Where snapshots are loaded from and saved to; unset means no snapshots.
    snapshots: OnceLock<SnapshotStore>,
    listener: ListenerSlot,
    watch_config: WatchConfig,
}

impl Indexer {
    /// An indexer with no index (`idle`) and no snapshot store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets where snapshots are stored. From then on every successful walk is saved there, and
    /// [`Indexer::start_from_snapshot`] loads from it. Only the first call has an effect.
    pub fn set_snapshot_store(&self, store: SnapshotStore) {
        if self.snapshots.set(store).is_err() {
            tracing::warn!("the snapshot store is already set; keeping the first one");
        }
    }

    /// A snapshot of the current status.
    pub fn status(&self) -> IndexStatus {
        self.lock().status.clone()
    }

    /// The current view searches should use, if a walk finished or a snapshot was loaded.
    pub fn index(&self) -> Option<Arc<IndexView>> {
        self.read_views().back().cloned()
    }

    /// The view of generation `generation`, if it is one of the recent ones (see
    /// [`RECENT_VIEWS`]); `None` means a result id of that generation is stale.
    pub fn view(&self, generation: u32) -> Option<Arc<IndexView>> {
        self.read_views()
            .iter()
            .rev()
            .find(|v| v.generation == generation)
            .cloned()
    }

    fn read_views(&self) -> RwLockReadGuard<'_, VecDeque<Arc<IndexView>>> {
        self.views.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The roots being watched for changes (empty if none).
    pub fn watched_roots(&self) -> Vec<PathBuf> {
        let core = self.core();
        match &core.watcher {
            Some(w) => w.watched_roots().to_vec(),
            None => Vec::new(),
        }
    }

    /// Starts indexing `roots` in the background and returns immediately. See the module docs
    /// for what happens when a walk is already running.
    pub fn start(self: &Arc<Self>, roots: Vec<PathBuf>, listener: StatusListener) -> StartOutcome {
        self.launch(roots, listener, false)
    }

    /// Like [`Indexer::start`], but the indexing thread first loads the stored snapshot of
    /// `roots`, if there is a valid one, and makes it current (`ready`) before walking. Used
    /// at app start-up.
    pub fn start_from_snapshot(
        self: &Arc<Self>,
        roots: Vec<PathBuf>,
        listener: StatusListener,
    ) -> StartOutcome {
        self.launch(roots, listener, true)
    }

    fn launch(
        self: &Arc<Self>,
        roots: Vec<PathBuf>,
        listener: StatusListener,
        use_snapshot: bool,
    ) -> StartOutcome {
        *self
            .listener
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&listener));
        let started = {
            let mut p = self.lock();
            if p.running {
                p.pending = Some(roots);
                return StartOutcome::Queued;
            }
            p.running = true;
            p.status = self.indexing_status(&roots);
            p.status.clone()
        };
        listener(&started);

        let this = Arc::clone(self);
        let worker_listener = Arc::clone(&listener);
        let spawned = std::thread::Builder::new()
            .name("athrix-indexer".into())
            .spawn(move || this.run(roots, &worker_listener, use_snapshot));
        if let Err(e) = spawned {
            let failed = {
                let mut p = self.lock();
                p.status.state = IndexState::Error;
                p.status.error = Some(format!("cannot start the indexing thread: {e}"));
                p.pending = None;
                p.running = false;
                p.status.clone()
            };
            listener(&failed);
        }
        StartOutcome::Started
    }

    /// The indexing thread: optionally loads the snapshot, walks `roots` (saving each result),
    /// then walks any roots queued meanwhile.
    fn run(
        self: &Arc<Self>,
        mut roots: Vec<PathBuf>,
        listener: &StatusListener,
        use_snapshot: bool,
    ) {
        if use_snapshot {
            self.load_snapshot(&mut roots, listener);
        }
        loop {
            self.begin_walk(&roots);
            let started = Instant::now();
            let opts = walk_options(&roots);
            let result = catch_unwind(AssertUnwindSafe(|| walk(&opts)));
            let elapsed_ms = millis_since(started);
            let result = match result {
                Ok(Ok(index)) => self.install(index, &roots).map_err(Some),
                Ok(Err(e)) => Err(Some(e.to_string())),
                Err(_) => Err(None),
            };
            if result.is_err() {
                self.core().during_walk = None;
            }

            let (finished, walked) = {
                let mut p = self.lock();
                let walked = match result {
                    Ok(view) => {
                        let entries = view.len() as u64;
                        tracing::info!(
                            entries,
                            elapsed_ms,
                            generation = view.generation(),
                            "indexing finished"
                        );
                        p.status.state = IndexState::Ready;
                        p.status.entries = entries;
                        p.status.elapsed_ms = Some(elapsed_ms);
                        p.status.error = None;
                        true
                    }
                    Err(Some(e)) => {
                        tracing::warn!(error = %e, "indexing failed");
                        p.status.state = IndexState::Error;
                        p.status.error = Some(e);
                        false
                    }
                    Err(None) => {
                        tracing::error!("indexing thread panicked");
                        p.status.state = IndexState::Error;
                        p.status.error = Some("indexing stopped unexpectedly".into());
                        false
                    }
                };
                (p.status.clone(), walked)
            };
            listener(&finished);
            // Saved after the swap and the `ready` event, so neither search nor the UI waits.
            if walked {
                self.save_snapshot(&roots);
            }

            let next = {
                let mut p = self.lock();
                match p.pending.take() {
                    Some(next) => {
                        p.status = self.indexing_status(&next);
                        Some((next, p.status.clone()))
                    }
                    None => {
                        p.running = false;
                        None
                    }
                }
            };
            match next {
                Some((next_roots, restarted)) => {
                    listener(&restarted);
                    roots = next_roots;
                }
                None => return,
            }
        }
    }

    /// Loads the stored snapshot of `roots`, if any, makes it current and reports `ready`, then
    /// reports `indexing` for the walk that follows (of `roots`, or of roots queued meanwhile,
    /// latest wins). A missing or unusable snapshot changes nothing.
    fn load_snapshot(self: &Arc<Self>, roots: &mut Vec<PathBuf>, listener: &StatusListener) {
        let Some(store) = self.snapshots.get() else {
            return;
        };
        let started = Instant::now();
        let loaded = catch_unwind(AssertUnwindSafe(|| store.load(roots))).unwrap_or_else(|_| {
            tracing::error!("loading the index snapshot panicked; ignoring it");
            None
        });
        let Some(index) = loaded else {
            return;
        };
        let view = match self.install(index, roots) {
            Ok(view) => view,
            Err(e) => {
                tracing::warn!(error = %e, "cannot use the index snapshot; ignoring it");
                return;
            }
        };
        let entries = view.len() as u64;
        tracing::info!(
            entries,
            elapsed_ms = millis_since(started),
            generation = view.generation(),
            "index snapshot loaded"
        );
        let (ready, indexing) = {
            let mut p = self.lock();
            p.status.state = IndexState::Ready;
            p.status.entries = entries;
            p.status.elapsed_ms = None;
            p.status.error = None;
            let ready = p.status.clone();
            if let Some(next) = p.pending.take() {
                *roots = next;
            }
            p.status = self.indexing_status(roots);
            (ready, p.status.clone())
        };
        listener(&ready);
        listener(&indexing);
    }

    /// Saves the current live index as the snapshot of `roots`, if a store is set and the live
    /// index is of those roots. Failures are only logged.
    fn save_snapshot(&self, roots: &[PathBuf]) {
        let Some(store) = self.snapshots.get() else {
            return;
        };
        let snapshot = {
            let core = self.core();
            match &core.live {
                Some(live) if core.live_roots == roots => live.snapshot(),
                _ => return,
            }
        };
        let started = Instant::now();
        match catch_unwind(AssertUnwindSafe(|| store.save(&snapshot, roots))) {
            Ok(Ok(())) => tracing::info!(
                elapsed_ms = millis_since(started),
                path = %store.snapshot_path().display(),
                "index snapshot saved"
            ),
            Ok(Err(e)) => tracing::warn!(error = %e, "cannot save the index snapshot"),
            Err(_) => tracing::error!("saving the index snapshot panicked"),
        }
    }

    /// Before a walk of `roots`: starts remembering changed paths and makes sure a watcher
    /// watches `roots` (restarting it if it watched other roots).
    fn begin_walk(self: &Arc<Self>, roots: &[PathBuf]) {
        let (old, token) = {
            let mut core = self.core();
            core.during_walk = Some(WalkBuffer::default());
            if core.watcher.is_some() && core.watch_roots == roots {
                return;
            }
            core.watch_token += 1;
            core.watch_roots = roots.to_vec();
            (core.watcher.take(), core.watch_token)
        };
        // Stopping joins the old watcher's thread, whose callback may be waiting for `core`.
        drop(old);
        self.start_watcher(roots, token);
    }

    fn start_watcher(self: &Arc<Self>, roots: &[PathBuf], token: u64) {
        let weak = Arc::downgrade(self);
        let started = Watcher::start(&walk_options(roots), self.watch_config, move |batch| {
            if let Some(this) = weak.upgrade() {
                this.on_batch(token, batch);
            }
        });
        match started {
            Ok(watcher) => {
                tracing::info!(roots = ?watcher.watched_roots(), "watching for changes");
                let stale = {
                    let mut core = self.core();
                    if core.watch_token == token {
                        core.watcher = Some(watcher);
                        None
                    } else {
                        Some(watcher)
                    }
                };
                drop(stale);
            }
            Err(e) => tracing::warn!(error = %e, "cannot watch the indexed folders for changes"),
        }
    }

    /// Handles one batch of the watcher with token `token` (on the watcher's thread).
    fn on_batch(self: &Arc<Self>, token: u64, batch: WatchBatch) {
        match batch {
            WatchBatch::Changed(paths) => {
                let target = {
                    let mut core = self.core();
                    if core.watch_token != token {
                        return;
                    }
                    if let Some(buffer) = core.during_walk.as_mut() {
                        if buffer.paths.len() + paths.len() > MAX_WALK_BUFFER {
                            buffer.overflowed = true;
                            buffer.paths = Vec::new();
                        } else if !buffer.overflowed {
                            buffer.paths.extend(paths.iter().cloned());
                        }
                    }
                    match &core.live {
                        Some(live) if core.live_roots == core.watch_roots => {
                            Some((Arc::clone(live), core.session))
                        }
                        _ => None,
                    }
                };
                if let Some((live, session)) = target {
                    self.apply(&live, session, &paths, true);
                }
            }
            WatchBatch::RescanNeeded(reason) => {
                let roots = {
                    let core = self.core();
                    if core.watch_token != token {
                        return;
                    }
                    core.watch_roots.clone()
                };
                tracing::warn!(%reason, "file watching lost track of changes; indexing again");
                self.rescan(roots);
            }
        }
    }

    /// Queues a new walk of `roots` (latest wins).
    fn rescan(self: &Arc<Self>, roots: Vec<PathBuf>) {
        let listener = self
            .listener
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(listener) = listener {
            let outcome = self.start(roots, listener);
            tracing::info!(?outcome, "re-indexing requested by the watcher");
        }
    }

    /// Applies changed paths to `live` and publishes the result if `live` is still current.
    /// With `report`, a changed entry count is reported (throttled).
    fn apply(self: &Arc<Self>, live: &LiveIndex, session: u64, paths: &[PathBuf], report: bool) {
        let started = Instant::now();
        let Ok(stats) = catch_unwind(AssertUnwindSafe(|| live.apply_paths(paths))) else {
            tracing::error!("applying file changes panicked");
            return;
        };
        if !stats.changed() {
            return;
        }
        let view = {
            let core = self.core();
            if core.session != session {
                return;
            }
            self.publish(&core, live.snapshot())
        };
        tracing::debug!(
            upserted = stats.upserted,
            removed = stats.removed,
            skipped = stats.skipped,
            compacted = stats.compacted,
            entries = view.len(),
            generation = view.generation(),
            elapsed_ms = millis_since(started),
            "file changes applied"
        );
        if report {
            self.report_entries(view.len() as u64);
        }
    }

    /// Records the entry count after applied changes and emits the status, at most once per
    /// [`LIVE_STATUS_INTERVAL`] (a skipped update is emitted when the interval ends).
    fn report_entries(self: &Arc<Self>, entries: u64) {
        let now = Instant::now();
        let action = {
            let mut p = self.lock();
            if p.status.entries == entries {
                return;
            }
            p.status.entries = entries;
            match p.last_live_emit {
                Some(last) if now.duration_since(last) < LIVE_STATUS_INTERVAL => {
                    if p.live_emit_scheduled {
                        return;
                    }
                    p.live_emit_scheduled = true;
                    Err(LIVE_STATUS_INTERVAL - now.duration_since(last))
                }
                _ => {
                    p.last_live_emit = Some(now);
                    Ok(p.status.clone())
                }
            }
        };
        match action {
            Ok(status) => self.emit(&status),
            Err(delay) => {
                let weak = Arc::downgrade(self);
                let spawned = std::thread::Builder::new()
                    .name("athrix-status".into())
                    .spawn(move || {
                        std::thread::sleep(delay);
                        if let Some(this) = weak.upgrade() {
                            let status = {
                                let mut p = this.lock();
                                p.live_emit_scheduled = false;
                                p.last_live_emit = Some(Instant::now());
                                p.status.clone()
                            };
                            this.emit(&status);
                        }
                    });
                if spawned.is_err() {
                    self.lock().live_emit_scheduled = false;
                }
            }
        }
    }

    fn emit(&self, status: &IndexStatus) {
        let listener = self
            .listener
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(listener) = listener {
            listener(status);
        }
    }

    /// Makes `index` (walked or loaded, of `roots`) the base of a new live index, publishes it
    /// under a new generation, then applies the paths changed during the walk, if any. Returns
    /// the current view.
    pub(crate) fn install(
        self: &Arc<Self>,
        index: FileIndex,
        roots: &[PathBuf],
    ) -> Result<Arc<IndexView>, String> {
        let live =
            Arc::new(LiveIndex::new(index, &walk_options(roots)).map_err(|e| e.to_string())?);
        let (view, buffered, session) = {
            let mut core = self.core();
            core.session += 1;
            core.live = Some(Arc::clone(&live));
            core.live_roots = roots.to_vec();
            let buffered = core.during_walk.take();
            let view = self.publish_new_base(&core, live.snapshot(), roots);
            (view, buffered, core.session)
        };
        if let Some(buffer) = buffered {
            if buffer.overflowed {
                self.rescan(roots.to_vec());
            } else if !buffer.paths.is_empty() {
                self.apply(&live, session, &buffer.paths, false);
            }
        }
        Ok(self.index().unwrap_or(view))
    }

    /// Publishes `snapshot` (of the current live index; `_core` proves the lock is held) as a
    /// new view. A snapshot of another base drops the older views.
    fn publish(
        &self,
        _core: &MutexGuard<'_, Core>,
        snapshot: Arc<IndexSnapshot>,
    ) -> Arc<IndexView> {
        let last = self.index();
        let view = match &last {
            Some(last) if Arc::ptr_eq(last.snapshot(), &snapshot) => return Arc::clone(last),
            Some(last) => last.successor(self.next_generation(), snapshot),
            None => IndexView::new(self.next_generation(), snapshot, &[]),
        };
        self.push_view(view)
    }

    fn publish_new_base(
        &self,
        _core: &MutexGuard<'_, Core>,
        snapshot: Arc<IndexSnapshot>,
        roots: &[PathBuf],
    ) -> Arc<IndexView> {
        self.push_view(IndexView::new(self.next_generation(), snapshot, roots))
    }

    fn push_view(&self, view: IndexView) -> Arc<IndexView> {
        let view = Arc::new(view);
        let mut views = self.views.write().unwrap_or_else(PoisonError::into_inner);
        if views
            .back()
            .is_some_and(|last| !Arc::ptr_eq(last.snapshot.base(), view.snapshot.base()))
        {
            // Ids of another base mean nothing in this one; drop them (and their memory).
            views.clear();
        }
        views.push_back(Arc::clone(&view));
        while views.len() > RECENT_VIEWS {
            views.pop_front();
        }
        view
    }

    fn next_generation(&self) -> u32 {
        self.generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            & GENERATION_MASK
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

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Walk options for `roots` with the default exclusions from the IPC contract.
pub fn walk_options(roots: &[PathBuf]) -> WalkOptions {
    let mut opts = WalkOptions::new(roots.iter().cloned());
    for name in DEFAULT_EXCLUDED_NAMES {
        opts = opts.exclude_glob(name);
    }
    for glob in DEFAULT_EXCLUDED_PATH_GLOBS {
        opts = opts.exclude_glob(glob);
    }
    if cfg!(unix) {
        for prefix in UNIX_EXCLUDED_PREFIXES {
            opts = opts.exclude_prefix(prefix);
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_fs_index::Query;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

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

    fn with_store(dir: &tempfile::TempDir) -> Arc<Indexer> {
        let indexer = Indexer::new();
        indexer.set_snapshot_store(SnapshotStore::new(dir.path().join("index")));
        Arc::new(indexer)
    }

    /// Runs the indexing thread's work on this thread, as `launch` would, and returns the
    /// events emitted after the initial `indexing` one.
    fn run_here(indexer: &Arc<Indexer>, roots: &[PathBuf], use_snapshot: bool) -> Vec<IndexStatus> {
        {
            let mut p = indexer.lock();
            p.running = true;
            p.status = indexer.indexing_status(roots);
        }
        let (l, rx) = listener();
        indexer.run(roots.to_vec(), &l, use_snapshot);
        assert!(!indexer.lock().running);
        rx.try_iter().collect()
    }

    fn states(events: &[IndexStatus]) -> Vec<IndexState> {
        events.iter().map(|s| s.state).collect()
    }

    /// Polls `cond` for up to 20 s.
    fn eventually(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn finds(view: &IndexView, text: &str) -> bool {
        !view
            .search(&Query::parse(text).unwrap(), 10, &AtomicBool::new(false))
            .unwrap()
            .is_empty()
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
        {
            let mut p = indexer.lock();
            p.running = true;
            p.status = indexer.indexing_status(&[a.path().to_path_buf()]);
        }
        let queued =
            |d: &tempfile::TempDir| indexer.start(vec![d.path().to_path_buf()], Arc::clone(&l));
        assert_eq!(queued(&b), StartOutcome::Queued);
        assert_eq!(queued(&c), StartOutcome::Queued);
        assert!(rx.try_recv().is_err(), "queuing emits nothing");
        // Run the "running" walk on this thread: it must then pick up `c` (not `b`).
        indexer.run(vec![a.path().to_path_buf()], &l, false);
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            states(&events),
            [IndexState::Ready, IndexState::Indexing, IndexState::Ready]
        );
        let root = |d: &tempfile::TempDir| vec![d.path().to_string_lossy().into_owned()];
        assert_eq!(events[0].roots, root(&a));
        assert_eq!(events[1].roots, root(&c));
        assert_eq!(events[1].entries, 5, "previous index stays searchable");
        assert_eq!(events[2].roots, root(&c));
        assert_eq!(indexer.watched_roots(), [c.path().to_path_buf()]);
        let p = indexer.lock();
        assert!(p.pending.is_none());
        assert!(!p.running);
    }

    #[test]
    fn whole_drive_exclusions_skip_system_staging_folders() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for f in [
            "Windows/WinSxS/amd64_x/a.dll",
            "Windows/System32/c.dll",
            "$WinREAgent/Scratch/b.bin",
            "$Windows.~BT/Sources/s.wim",
            "Users/me/Documents/ملف.txt",
        ] {
            fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            fs::write(p.join(f), b"x").unwrap();
        }
        let ix = walk(&walk_options(&[p.to_path_buf()])).unwrap();
        let names: Vec<&str> = (0..ix.len() as u32).map(|id| ix.name(id)).collect();
        for skipped in ["WinSxS", "a.dll", "$WinREAgent", "b.bin", "$Windows.~BT"] {
            assert!(!names.contains(&skipped), "{skipped} in {names:?}");
        }
        for kept in ["Windows", "System32", "c.dll", "ملف.txt"] {
            assert!(names.contains(&kept), "{kept} missing from {names:?}");
        }
    }

    #[test]
    fn index_roots_are_contiguous_id_blocks() {
        let a = tree();
        let b = tree();
        let roots = [a.path().to_path_buf(), b.path().to_path_buf()];
        let view = IndexView::from_index(1, walk(&walk_options(&roots)).unwrap());
        let blocks = view.roots();
        assert_eq!(blocks.len(), 2);
        assert_eq!((blocks[0].start, blocks[0].end), (0, 5));
        assert_eq!((blocks[1].start, blocks[1].end), (5, 10));
        assert_eq!(blocks[1].key, path_key(b.path()));
        for block in blocks {
            for id in block.start..block.end {
                let mut top = id;
                while let Some(parent) = view.parent(top) {
                    top = parent;
                }
                assert_eq!(top, block.start, "entry {id} belongs to its block's root");
            }
        }
    }

    #[test]
    fn a_compacted_base_gets_extra_runs_for_entries_added_after_the_walk() {
        let a = tree();
        let b = tree();
        let roots = [a.path().to_path_buf(), b.path().to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        let added = a.path().join("docs").join("added-later.txt");
        fs::write(&added, b"x").unwrap();
        assert!(live.apply_paths(&[&added]).changed());
        live.compact();
        let view = IndexView::new(1, live.snapshot(), &roots);
        assert!(!view.has_changes(), "compacted into the base");
        let runs: Vec<(u32, u32, &str)> = view
            .roots()
            .iter()
            .map(|r| (r.start, r.end, r.key.as_str()))
            .collect();
        let (ka, kb) = (path_key(a.path()), path_key(b.path()));
        assert_eq!(
            runs,
            [
                (0, 5, ka.as_str()),
                (5, 10, kb.as_str()),
                (10, 11, ka.as_str())
            ]
        );
        assert_eq!(view.path(10), added);
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

    #[test]
    fn snapshot_is_used_at_startup_then_replaced_by_a_walk() {
        let data = tree();
        let snaps = tempfile::tempdir().unwrap();
        let roots = vec![data.path().to_path_buf()];

        // A first run walks and saves the snapshot.
        let first = with_store(&snaps);
        let events = run_here(&first, &roots, true);
        assert_eq!(states(&events), [IndexState::Ready], "no snapshot yet");
        drop(first);
        let store = SnapshotStore::new(snaps.path().join("index"));
        assert!(store.snapshot_path().is_file());
        assert_eq!(store.load(&roots).unwrap().len(), 5);

        // The disk changes; the next start-up shows the snapshot, then the fresh walk.
        fs::write(data.path().join("docs/جديد.txt"), b"x").unwrap();
        let second = with_store(&snaps);
        let (l, rx) = listener();
        assert_eq!(
            second.start_from_snapshot(roots.clone(), l),
            StartOutcome::Started
        );
        let events: Vec<_> = (0..4).map(|_| next(&rx)).collect();
        assert_eq!(
            states(&events),
            [
                IndexState::Indexing,
                IndexState::Ready,
                IndexState::Indexing,
                IndexState::Ready
            ]
        );
        assert_eq!(events[0].entries, 0);
        assert_eq!(events[1].entries, 5, "snapshot is searchable");
        assert_eq!(events[1].elapsed_ms, None, "no walk finished yet");
        assert_eq!(
            events[2].entries, 5,
            "snapshot stays searchable during the walk"
        );
        assert_eq!(events[3].entries, 6, "the walk replaced it");
        assert!(events[3].elapsed_ms.is_some());
        let view = second.index().unwrap();
        assert_eq!(view.len(), 6);
        assert_eq!(view.generation(), 2, "snapshot = generation 1, walk = 2");
        assert!(second.view(1).is_none(), "the snapshot's ids are stale");
        // The walk's result is saved for the next start-up (wait for the thread to finish).
        assert!(eventually(|| !second.lock().running));
        assert_eq!(store.load(&roots).unwrap().len(), 6);
    }

    #[test]
    fn a_corrupt_snapshot_falls_back_to_a_walk() {
        let data = tree();
        let snaps = tempfile::tempdir().unwrap();
        let roots = vec![data.path().to_path_buf()];
        run_here(&with_store(&snaps), &roots, false);
        let store = SnapshotStore::new(snaps.path().join("index"));
        fs::write(store.snapshot_path(), b"ATHRXIDX not really").unwrap();

        let indexer = with_store(&snaps);
        let events = run_here(&indexer, &roots, true);
        assert_eq!(states(&events), [IndexState::Ready]);
        assert!(events[0].elapsed_ms.is_some(), "came from a walk");
        assert_eq!(indexer.index().unwrap().generation(), 1);
        assert_eq!(
            store.load(&roots).unwrap().len(),
            5,
            "rewritten by the walk"
        );
    }

    #[test]
    fn a_snapshot_of_other_roots_is_ignored() {
        let a = tree();
        let b = tree();
        let snaps = tempfile::tempdir().unwrap();
        run_here(&with_store(&snaps), &[a.path().to_path_buf()], false);

        let roots_b = vec![b.path().to_path_buf()];
        let indexer = with_store(&snaps);
        let events = run_here(&indexer, &roots_b, true);
        assert_eq!(states(&events), [IndexState::Ready]);
        assert!(events[0].elapsed_ms.is_some(), "came from a walk");
        assert_eq!(indexer.index().unwrap().generation(), 1);
        // Now the store holds `b`'s snapshot.
        let store = SnapshotStore::new(snaps.path().join("index"));
        assert!(store.load(&[a.path().to_path_buf()]).is_none());
        assert!(store.load(&roots_b).is_some());
    }

    #[test]
    fn without_a_store_nothing_is_loaded_or_saved() {
        let data = tree();
        let indexer = Arc::new(Indexer::new());
        let events = run_here(&indexer, &[data.path().to_path_buf()], true);
        assert_eq!(states(&events), [IndexState::Ready]);
    }

    #[test]
    fn generations_increase_and_wrap_within_21_bits() {
        let indexer = Arc::new(Indexer::new());
        let install = || {
            indexer
                .install(FileIndex::default(), &[])
                .unwrap()
                .generation()
        };
        assert_eq!(install(), 1);
        assert_eq!(install(), 2);
        indexer
            .generation
            .store(GENERATION_MASK - 1, Ordering::Relaxed);
        assert_eq!(install(), GENERATION_MASK);
        assert_eq!(install(), 0);
        assert_eq!(indexer.index().unwrap().generation(), 0);
        indexer.generation.store(u32::MAX, Ordering::Relaxed);
        assert_eq!(install(), 0);
    }

    #[test]
    fn applied_batches_get_new_generations_and_recent_views_stay_resolvable() {
        let data = tree();
        let roots = vec![data.path().to_path_buf()];
        let indexer = Arc::new(Indexer::new());
        let first = indexer
            .install(walk(&walk_options(&roots)).unwrap(), &roots)
            .unwrap();
        let session = indexer.core().session;
        let live = indexer.core().live.clone().unwrap();
        let mut generations = vec![first.generation()];
        for i in 0..RECENT_VIEWS + 2 {
            let p = data.path().join(format!("batch-{i}.txt"));
            fs::write(&p, b"x").unwrap();
            indexer.apply(&live, session, &[p], false);
            generations.push(indexer.index().unwrap().generation());
        }
        assert!(generations.windows(2).all(|w| w[1] == w[0] + 1));
        // The latest RECENT_VIEWS generations resolve, older ones are gone.
        let (old, recent) = generations.split_at(generations.len() - RECENT_VIEWS);
        assert!(recent.iter().all(|&g| indexer.view(g).is_some()));
        assert!(old.iter().all(|&g| indexer.view(g).is_none()));
        // A view keeps resolving its own ids after later batches.
        let older = indexer.view(recent[0]).unwrap();
        assert_eq!(
            older.len() + RECENT_VIEWS - 1,
            indexer.index().unwrap().len()
        );

        // An apply to a superseded live index is not published.
        indexer
            .install(walk(&walk_options(&roots)).unwrap(), &roots)
            .unwrap();
        let current = indexer.index().unwrap().generation();
        let p = data.path().join("late.txt");
        fs::write(&p, b"x").unwrap();
        indexer.apply(&live, session, &[p], false);
        assert_eq!(indexer.index().unwrap().generation(), current);
        assert!(
            indexer.view(recent[0]).is_none(),
            "a new base drops old views"
        );
    }

    #[test]
    fn watched_changes_reach_the_index_and_report_the_entry_count() {
        let data = tree();
        let root = data.path().to_path_buf();
        let indexer = Arc::new(Indexer::new());
        let (l, rx) = listener();
        indexer.start(vec![root.clone()], l);
        assert_eq!(next(&rx).state, IndexState::Indexing);
        assert_eq!(next(&rx).entries, 5);
        assert!(eventually(|| !indexer.lock().running));
        assert_eq!(indexer.watched_roots(), std::slice::from_ref(&root));
        let walked = indexer.index().unwrap();

        let created = root.join("docs").join("watched-new.txt");
        fs::write(&created, b"x").unwrap();
        assert!(
            eventually(|| finds(&indexer.index().unwrap(), "watched-new")),
            "the created file is found"
        );
        let status = next(&rx);
        assert_eq!(status.state, IndexState::Ready);
        assert_eq!(status.entries, 6, "{status:?}");
        assert!(
            indexer.view(walked.generation()).is_some(),
            "the walk's view is still resolvable"
        );

        fs::remove_file(&created).unwrap();
        assert!(
            eventually(|| !finds(&indexer.index().unwrap(), "watched-new")),
            "the deleted file is gone"
        );
        assert!(eventually(|| indexer.status().entries == 5));
    }
}
