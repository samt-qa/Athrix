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
//! new walk of the watched roots through the same queue (latest wins). Roots on removable
//! drives are *polled* (scanned every [`POLL_INTERVAL`] or so) instead of watched by the OS: an
//! OS watch keeps a handle open on the drive, and Windows then refuses to "safely remove" it.
//!
//! **Generations and views.** Every snapshot the app publishes (walk, snapshot load, applied
//! batch, compaction) becomes an [`IndexView`] with a new 21-bit generation. Result ids carry
//! the generation (`generation << 32 | entry`) and are resolved against the view of that
//! generation, kept in a small ring of the [`RECENT_VIEWS`] latest views that share the
//! current base; ids of older views (or of a replaced base) are rejected as stale.
//!
//! **Drives coming and going (task 2.5).** The roots of the index may include *offline* roots:
//! roots whose drive is unplugged ([`Indexer::reconcile`], [`Indexer::set_offline_roots`]).
//! - Going offline is instant and walks nothing: the root's subtree is tombstoned in the live
//!   index (results disappear at once), the watcher is restarted without it, and the
//!   [`IndexObserver`] is told (the content index pauses that root but keeps its documents). A
//!   watched root that vanishes before the drive monitor notices (the watcher reports changes
//!   or an error under a root that no longer exists) is taken offline the same way instead of
//!   triggering a full re-walk.
//! - A root that comes back, a new root, or another volume on the same path is handled by an
//!   *update* job on the indexing thread: only those roots are walked (with `scanning` in the
//!   status), then the current entries minus the replaced roots and the new walk are
//!   concatenated ([`FileIndex::concat`]) and installed as a new base. Watched changes under a
//!   root being walked are only buffered (and applied after the install), never applied to
//!   the old live index, which would walk that drive inside its lock.
//! - Requests are queued like walks (latest wins): a pending full walk absorbs an update, and
//!   two updates merge (union of the roots to refresh).
//! - The snapshot may be of other roots than wanted at start-up (a drive unplugged since): if
//!   they overlap it is loaded anyway and the extra roots hidden, then the walk follows.

use std::collections::VecDeque;
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use athrix_fs_index::watch::PollConfig;
use athrix_fs_index::{
    ApplyStats, Change, EntryId, FileIndex, IndexSnapshot, LiveIndex, WalkOptions, WatchBatch,
    WatchConfig, Watcher, walk,
};

use crate::model::{IndexState, IndexStatus};
use crate::roots::{key_has_prefix, path_key};

mod snapshot_store;

pub use snapshot_store::SnapshotStore;

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

/// Follows the name index for other indexes built from it (the content index, see
/// [`crate::content`]).
pub trait IndexObserver: Send + Sync {
    /// A new base is current (a snapshot was loaded or a walk finished), after the status
    /// event. Called on the indexing thread; may take a while (the walk queue waits, searches
    /// do not).
    fn base_ready(&self, view: &Arc<IndexView>);

    /// The watcher reported changed paths; they were applied to the live index first. Called
    /// on the watcher's thread: keep it short. Paths under offline roots are never passed.
    fn files_changed(&self, batch: &WatchBatch);

    /// The offline roots are now `offline` (drives unplugged; their entries are hidden but
    /// their derived data should be kept, not deleted). Called on whichever thread noticed;
    /// keep it short.
    fn roots_offline(&self, offline: &[PathBuf]) {
        let _ = offline;
    }
}

/// Work for the indexing thread.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Job {
    /// Walk every (online) root from scratch.
    Full(Vec<PathBuf>),
    /// Bring the index to `roots`, walking only the roots it does not hold yet (new, back
    /// online) and those in `refresh` (another volume on their path).
    Update {
        roots: Vec<PathBuf>,
        refresh: Vec<PathBuf>,
    },
}

impl Job {
    fn roots(&self) -> &[PathBuf] {
        match self {
            Job::Full(roots) | Job::Update { roots, .. } => roots,
        }
    }

    /// `self` is queued and `next` is requested: the one job that does both (latest roots
    /// win; a full walk covers any update).
    fn merge(self, next: Job) -> Job {
        match (self, next) {
            (_, Job::Full(roots)) | (Job::Full(_), Job::Update { roots, .. }) => Job::Full(roots),
            (Job::Update { refresh: mut a, .. }, Job::Update { roots, refresh: b }) => {
                for r in b {
                    if !a.contains(&r) {
                        a.push(r);
                    }
                }
                Job::Update { roots, refresh: a }
            }
        }
    }
}

/// What an [`Job::Update`] has to do, given the live index now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateWork {
    /// Roots to walk (their old entries, if any, are replaced).
    add: Vec<PathBuf>,
    /// Roots of the live index that are no longer wanted.
    dropped: Vec<PathBuf>,
    /// There is no live index yet: walk everything.
    full: bool,
}

#[derive(Default)]
struct ObserverSlot(OnceLock<Arc<dyn IndexObserver>>);

impl std::fmt::Debug for ObserverSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObserverSlot")
    }
}

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
    /// Work requested while the indexing thread was busy.
    pending: Option<Job>,
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
    /// Roots whose drive is unplugged: their entries are tombstoned, they are not watched.
    offline: Vec<PathBuf>,
    /// Roots being (or about to be) walked that the live index does not hold: watched changes
    /// under them are buffered, not applied to the live index.
    unheld: Vec<PathBuf>,
}

impl Core {
    /// The paths of `paths` that the live index can take now: under one of its roots that is
    /// online and held.
    fn applicable(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        let keys: Vec<String> = self
            .live_roots
            .iter()
            .filter(|r| !self.offline.contains(r) && !self.unheld.contains(r))
            .map(|r| path_key(r))
            .collect();
        paths
            .iter()
            .filter(|p| {
                let k = path_key(p);
                keys.iter().any(|root| key_has_prefix(&k, root))
            })
            .cloned()
            .collect()
    }

    /// What an update to `roots` (refreshing `refresh`) would do; `None` if nothing.
    fn update_work(&self, roots: &[PathBuf], refresh: &[PathBuf]) -> Option<UpdateWork> {
        let online = roots.iter().filter(|r| !self.offline.contains(r));
        if self.live.is_none() {
            return Some(UpdateWork {
                add: online.cloned().collect(),
                dropped: Vec::new(),
                full: true,
            });
        }
        let add: Vec<PathBuf> = online
            .filter(|r| {
                !self.live_roots.contains(r) || refresh.contains(r) || self.unheld.contains(r)
            })
            .cloned()
            .collect();
        let dropped: Vec<PathBuf> = self
            .live_roots
            .iter()
            .filter(|r| !roots.contains(r))
            .cloned()
            .collect();
        (!add.is_empty() || !dropped.is_empty() || self.live_roots != roots).then_some(UpdateWork {
            add,
            dropped,
            full: false,
        })
    }
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
    observer: ObserverSlot,
    watch_config: WatchConfig,
    /// Picks the watched roots to poll instead of watching them (see [`PollPolicy`]) and how
    /// often; unset means [`crate::volumes::roots_on_removable_drives`] every
    /// [`POLL_INTERVAL`].
    poll_policy: OnceLock<(PollPolicy, Duration)>,
}

/// Picks, among the roots about to be watched, those to poll instead (no handle kept open, so
/// the drive can be ejected safely; see [`athrix_fs_index::watch`]).
pub type PollPolicy = fn(&[PathBuf]) -> Vec<PathBuf>;

/// How often polled roots (removable drives) are scanned for changes, at most.
pub const POLL_INTERVAL: Duration = Duration::from_secs(20);

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

    /// Sets the observer told about new bases and watched changes. Only the first call has
    /// an effect.
    pub fn set_observer(&self, observer: Arc<dyn IndexObserver>) {
        if self.observer.0.set(observer).is_err() {
            tracing::warn!("the index observer is already set; keeping the first one");
        }
    }

    /// Tells the observer that `view` is the current base.
    fn notify_base(&self, view: &Arc<IndexView>) {
        if let Some(observer) = self.observer.0.get() {
            if catch_unwind(AssertUnwindSafe(|| observer.base_ready(view))).is_err() {
                tracing::error!("the index observer panicked on a new base");
            }
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
        self.launch(Job::Full(roots), listener, false)
    }

    /// Brings the index to `roots` without walking what it already holds (task 2.5):
    /// - `offline` (a subset of `roots`) are the roots whose drive is unplugged: newly offline
    ///   ones are hidden at once (see [`Indexer::set_offline_roots`]);
    /// - roots that are new, back online, or listed in `refresh` (another volume on their
    ///   path) are walked in the background, alone, and merged in;
    /// - roots of the index that are not in `roots` are dropped.
    ///
    /// Returns `None` when nothing needs walking or rebuilding, else what the queued update did
    /// (it waits behind a running walk, see the module docs).
    pub fn reconcile(
        self: &Arc<Self>,
        roots: Vec<PathBuf>,
        offline: Vec<PathBuf>,
        refresh: Vec<PathBuf>,
        listener: StatusListener,
    ) -> Option<StartOutcome> {
        self.set_listener(&listener);
        let mut refresh: Vec<PathBuf> = refresh
            .into_iter()
            .filter(|r| roots.contains(r) && !offline.contains(r))
            .collect();
        for back in self.change_offline(&offline, &refresh) {
            if roots.contains(&back) && !refresh.contains(&back) {
                refresh.push(back);
            }
        }
        let idle = !self.lock().running;
        if idle && self.core().update_work(&roots, &refresh).is_none() {
            return None;
        }
        Some(self.launch(Job::Update { roots, refresh }, listener, false))
    }

    /// Sets which roots are offline (drives unplugged), without walking anything: the entries
    /// of newly offline roots are tombstoned at once, the watcher drops them, the observer is
    /// told, and the status (`offline`) is emitted. Roots no longer offline are remembered as
    /// not held: the next [`Indexer::reconcile`] walks them. Also used before the first start.
    pub fn set_offline_roots(self: &Arc<Self>, offline: &[PathBuf]) {
        self.change_offline(offline, &[]);
    }

    /// The roots currently offline.
    pub fn offline_roots(&self) -> Vec<PathBuf> {
        self.core().offline.clone()
    }

    fn set_listener(&self, listener: &StatusListener) {
        *self
            .listener
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(listener));
    }

    /// See [`Indexer::set_offline_roots`]; `refresh` roots the live index holds online are
    /// hidden too (another volume took their path). Returns the roots that came back online.
    fn change_offline(self: &Arc<Self>, offline: &[PathBuf], refresh: &[PathBuf]) -> Vec<PathBuf> {
        let (hide, came_back, restart, changed) = {
            let mut core = self.core();
            let newly: Vec<PathBuf> = offline
                .iter()
                .filter(|r| !core.offline.contains(r))
                .cloned()
                .collect();
            let came_back: Vec<PathBuf> = core
                .offline
                .iter()
                .filter(|r| !offline.contains(r))
                .cloned()
                .collect();
            let swapped: Vec<PathBuf> = refresh
                .iter()
                .filter(|r| core.live_roots.contains(r) && !core.offline.contains(r))
                .cloned()
                .collect();
            for r in came_back.iter().chain(&swapped) {
                if !core.unheld.contains(r) {
                    core.unheld.push(r.clone());
                }
            }
            core.unheld.retain(|r| !offline.contains(r));
            core.offline = offline.to_vec();
            let restart = if core.watch_roots.iter().any(|r| offline.contains(r)) {
                core.watch_roots.retain(|r| !offline.contains(r));
                core.watch_token += 1;
                Some((
                    core.watcher.take(),
                    core.watch_roots.clone(),
                    core.watch_token,
                ))
            } else {
                None
            };
            let changed = !newly.is_empty() || !came_back.is_empty();
            let mut hide = newly;
            hide.extend(swapped);
            (hide, came_back, restart, changed)
        };
        if !hide.is_empty() {
            tracing::info!(roots = ?hide, "drive gone or replaced; hiding its entries");
        }
        if !came_back.is_empty() {
            tracing::info!(roots = ?came_back, "drive back; it will be indexed again");
        }
        if let Some((old, roots, token)) = restart {
            // Joins the old watcher's thread (unless called from it, see `Watcher`'s drop).
            drop(old);
            if !roots.is_empty() {
                self.start_watcher(&roots, token);
            }
        }
        let hidden = !hide.is_empty() && self.hide_roots(&hide);
        if changed {
            if let Some(observer) = self.observer.0.get() {
                if catch_unwind(AssertUnwindSafe(|| observer.roots_offline(offline))).is_err() {
                    tracing::error!("the index observer panicked on offline roots");
                }
            }
        }
        if changed || hidden {
            let status = {
                let mut p = self.lock();
                let gone = root_strings(offline);
                p.status.roots.retain(|r| !gone.contains(r));
                p.status.offline = gone;
                if let Some(view) = self.index() {
                    p.status.entries = view.len() as u64;
                }
                p.status.clone()
            };
            self.emit(&status);
        }
        came_back
    }

    /// Tombstones the subtrees of `roots` (those the live index has) and publishes the result.
    /// Returns true if anything was hidden.
    fn hide_roots(self: &Arc<Self>, roots: &[PathBuf]) -> bool {
        let target = {
            let core = self.core();
            core.live.as_ref().map(|live| {
                let changes: Vec<Change> = roots
                    .iter()
                    .filter(|r| core.live_roots.contains(r))
                    .map(|r| Change::Remove { path: r.clone() })
                    .collect();
                (Arc::clone(live), core.session, changes)
            })
        };
        let Some((live, session, changes)) = target else {
            return false;
        };
        if changes.is_empty() {
            return false;
        }
        let started = Instant::now();
        let Ok(stats) = catch_unwind(AssertUnwindSafe(|| live.apply_changes(&changes))) else {
            tracing::error!("hiding a root panicked");
            return false;
        };
        self.publish_applied(&live, session, stats, started, false)
    }

    /// Roots of `paths`' watcher (token `token`) that vanished: a path of the batch lies under
    /// them (or `all`) and they no longer exist.
    fn vanished_roots(&self, token: u64, paths: &[PathBuf], all: bool) -> Option<Vec<PathBuf>> {
        let roots = {
            let core = self.core();
            if core.watch_token != token {
                return None;
            }
            core.watch_roots.clone()
        };
        // Outside the lock: a stat of a disconnected network drive can block.
        Some(
            roots
                .into_iter()
                .filter(|r| {
                    let key = path_key(r);
                    (all || paths.iter().any(|p| key_has_prefix(&path_key(p), &key))) && !r.exists()
                })
                .collect(),
        )
    }

    /// Watched roots that disappeared before the drive monitor noticed: offline from now on.
    fn roots_vanished(self: &Arc<Self>, gone: &[PathBuf]) {
        let offline = {
            let core = self.core();
            let mut offline = core.offline.clone();
            for g in gone {
                if !offline.contains(g) {
                    offline.push(g.clone());
                }
            }
            offline
        };
        tracing::info!(roots = ?gone, "watched folder vanished (drive removed?)");
        self.change_offline(&offline, &[]);
    }

    /// Like [`Indexer::start`], but the indexing thread first loads the stored snapshot of
    /// `roots`, if there is a valid one, and makes it current (`ready`) before walking. Used
    /// at app start-up.
    pub fn start_from_snapshot(
        self: &Arc<Self>,
        roots: Vec<PathBuf>,
        listener: StatusListener,
    ) -> StartOutcome {
        self.launch(Job::Full(roots), listener, true)
    }

    fn launch(
        self: &Arc<Self>,
        job: Job,
        listener: StatusListener,
        use_snapshot: bool,
    ) -> StartOutcome {
        self.set_listener(&listener);
        let started = {
            let mut p = self.lock();
            if p.running {
                p.pending = Some(match p.pending.take() {
                    Some(queued) => queued.merge(job),
                    None => job,
                });
                return StartOutcome::Queued;
            }
            p.running = true;
            p.status = self.indexing_status(&job);
            p.status.clone()
        };
        listener(&started);

        let this = Arc::clone(self);
        let worker_listener = Arc::clone(&listener);
        let spawned = std::thread::Builder::new()
            .name("athrix-indexer".into())
            .spawn(move || this.run(job, &worker_listener, use_snapshot));
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

    /// The indexing thread: optionally loads the snapshot, runs `job` (saving each walked
    /// result), then the work queued meanwhile.
    fn run(self: &Arc<Self>, mut job: Job, listener: &StatusListener, use_snapshot: bool) {
        if use_snapshot {
            self.load_snapshot(&mut job, listener);
        }
        loop {
            let started = Instant::now();
            let result = self.run_job(&job);
            let elapsed_ms = millis_since(started);
            if result.is_err() {
                let mut core = self.core();
                core.during_walk = None;
            }

            let (finished, walked) = {
                let offline = root_strings(&self.core().offline);
                let mut p = self.lock();
                p.status.scanning = Vec::new();
                p.status.offline = offline;
                let walked = match result {
                    Ok(Some(view)) => {
                        let installed = Arc::clone(&view);
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
                        Some(installed)
                    }
                    Ok(None) => {
                        // An update that found nothing to do (the queue moved on meanwhile).
                        p.status.state = IndexState::Ready;
                        p.status.error = None;
                        None
                    }
                    Err(Some(e)) => {
                        tracing::warn!(error = %e, "indexing failed");
                        p.status.state = IndexState::Error;
                        p.status.error = Some(e);
                        None
                    }
                    Err(None) => {
                        tracing::error!("indexing thread panicked");
                        p.status.state = IndexState::Error;
                        p.status.error = Some("indexing stopped unexpectedly".into());
                        None
                    }
                };
                (p.status.clone(), walked)
            };
            listener(&finished);
            // Saved after the swap and the `ready` event, so neither search nor the UI waits.
            if let Some(view) = walked {
                self.save_snapshot(job.roots());
                // The current view (changes made during the walk may already be applied).
                self.notify_base(&self.index().unwrap_or(view));
            }

            let next = {
                let mut p = self.lock();
                loop {
                    match p.pending.take() {
                        Some(Job::Update { roots, refresh })
                            if self.core().update_work(&roots, &refresh).is_none() => {}
                        Some(next) => {
                            p.status = self.indexing_status(&next);
                            break Some((next, p.status.clone()));
                        }
                        None => {
                            p.running = false;
                            break None;
                        }
                    }
                }
            };
            match next {
                Some((next_job, restarted)) => {
                    listener(&restarted);
                    job = next_job;
                }
                None => return,
            }
        }
    }

    /// Runs one job; `Ok(None)` = an update with nothing to do.
    fn run_job(self: &Arc<Self>, job: &Job) -> Result<Option<Arc<IndexView>>, Option<String>> {
        match job {
            Job::Full(roots) => self.walk_all(roots).map(Some),
            Job::Update { roots, refresh } => self.walk_new(roots, refresh),
        }
    }

    /// Walks every online root of `roots` and installs the result (offline roots stay in the
    /// index's roots, empty).
    fn walk_all(self: &Arc<Self>, roots: &[PathBuf]) -> Result<Arc<IndexView>, Option<String>> {
        let online = self.online(roots);
        self.begin_walk(&online, &online);
        let opts = walk_options(&online);
        match catch_unwind(AssertUnwindSafe(|| walk(&opts))) {
            Ok(Ok(index)) => self.install_walked(index, roots, &online).map_err(Some),
            Ok(Err(e)) => Err(Some(e.to_string())),
            Err(_) => Err(None),
        }
    }

    /// The update of [`Indexer::reconcile`]: walks only the roots to add, then installs the
    /// current entries minus the replaced / dropped roots plus that walk.
    fn walk_new(
        self: &Arc<Self>,
        roots: &[PathBuf],
        refresh: &[PathBuf],
    ) -> Result<Option<Arc<IndexView>>, Option<String>> {
        let Some(work) = self.core().update_work(roots, refresh) else {
            return Ok(None);
        };
        if work.full {
            return self.walk_all(roots).map(Some);
        }
        tracing::info!(add = ?work.add, dropped = ?work.dropped, "updating the indexed roots");
        let online = self.online(roots);
        self.begin_walk(&online, &work.add);
        let walked = if work.add.is_empty() {
            FileIndex::default()
        } else {
            let opts = walk_options(&work.add);
            match catch_unwind(AssertUnwindSafe(|| walk(&opts))) {
                Ok(Ok(index)) => index,
                Ok(Err(e)) => return Err(Some(e.to_string())),
                Err(_) => return Err(None),
            }
        };
        // The latest entries (changes applied during the walk included).
        let Some(snapshot) = self.core().live.as_ref().map(|l| l.snapshot()) else {
            return self.walk_all(roots).map(Some);
        };
        let replaced: Vec<String> = work
            .dropped
            .iter()
            .chain(&work.add)
            .map(|r| path_key(r))
            .collect();
        let merged = catch_unwind(AssertUnwindSafe(|| {
            let current = snapshot.to_file_index();
            drop(snapshot);
            FileIndex::concat(&[&current, &walked], |part, root| {
                part == 1 || !replaced.contains(&path_key(Path::new(root)))
            })
        }))
        .map_err(|_| None)?;
        self.install_walked(merged, roots, &work.add)
            .map(Some)
            .map_err(Some)
    }

    /// The roots of `roots` that are not offline.
    fn online(&self, roots: &[PathBuf]) -> Vec<PathBuf> {
        let core = self.core();
        roots
            .iter()
            .filter(|r| !core.offline.contains(r))
            .cloned()
            .collect()
    }

    /// Loads the stored snapshot of the job's roots (or of overlapping roots: the extra ones are
    /// hidden), if there is a valid one, makes it current and reports `ready`, then reports
    /// `indexing` for the job that follows (`job`, or the work queued meanwhile merged into
    /// it). A missing or unusable snapshot changes nothing.
    fn load_snapshot(self: &Arc<Self>, job: &mut Job, listener: &StatusListener) {
        let Some(store) = self.snapshots.get() else {
            return;
        };
        let started = Instant::now();
        let wanted = job.roots().to_vec();
        let loaded = catch_unwind(AssertUnwindSafe(|| store.load_overlapping(&wanted)))
            .unwrap_or_else(|_| {
                tracing::error!("loading the index snapshot panicked; ignoring it");
                None
            });
        let Some((index, saved)) = loaded else {
            return;
        };
        let view = match self.install_walked(index, &saved, &[]) {
            Ok(view) => view,
            Err(e) => {
                tracing::warn!(error = %e, "cannot use the index snapshot; ignoring it");
                return;
            }
        };
        let extra: Vec<PathBuf> = saved
            .iter()
            .filter(|r| !wanted.contains(r))
            .cloned()
            .collect();
        if !extra.is_empty() {
            tracing::info!(roots = ?extra, "the snapshot has roots not indexed now; hiding them");
            self.hide_roots(&extra);
        }
        let view = self.index().unwrap_or(view);
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
                *job = job.clone().merge(next);
            }
            p.status = self.indexing_status(job);
            (ready, p.status.clone())
        };
        listener(&ready);
        listener(&indexing);
        self.notify_base(&self.index().unwrap_or(view));
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

    /// Before a walk of `walked`: starts remembering changed paths, marks the walked roots the
    /// live index does not hold as not held, and makes sure a watcher watches `watch` (the
    /// online roots; restarting it if it watched other roots).
    fn begin_walk(self: &Arc<Self>, watch: &[PathBuf], walked: &[PathBuf]) {
        let (old, token) = {
            let mut core = self.core();
            core.during_walk = Some(WalkBuffer::default());
            for r in walked {
                if !core.live_roots.contains(r) && !core.unheld.contains(r) {
                    core.unheld.push(r.clone());
                }
            }
            if core.watcher.is_some() && core.watch_roots == watch {
                return;
            }
            core.watch_token += 1;
            core.watch_roots = watch.to_vec();
            (core.watcher.take(), core.watch_token)
        };
        // Stopping joins the old watcher's thread, whose callback may be waiting for `core`.
        drop(old);
        if !watch.is_empty() {
            self.start_watcher(watch, token);
        }
    }

    /// Sets which watched roots are polled instead of watched by the OS, and how often (the
    /// default polls the roots on removable drives every [`POLL_INTERVAL`]). Only the first
    /// call has an effect.
    pub fn set_poll_policy(&self, policy: PollPolicy, interval: Duration) {
        let _ = self.poll_policy.set((policy, interval));
    }

    fn start_watcher(self: &Arc<Self>, roots: &[PathBuf], token: u64) {
        let weak = Arc::downgrade(self);
        let (policy, interval) = self.poll_policy.get().copied().unwrap_or((
            crate::volumes::roots_on_removable_drives as PollPolicy,
            POLL_INTERVAL,
        ));
        // An OS watch holds a handle on its root: Windows then refuses to eject the drive.
        let poll = PollConfig {
            roots: policy(roots),
            interval,
        };
        let started = Watcher::start_with_polling(
            &walk_options(roots),
            self.watch_config,
            &poll,
            move |batch| {
                if let Some(this) = weak.upgrade() {
                    this.on_batch(token, batch);
                }
            },
        );
        match started {
            Ok(watcher) => {
                tracing::info!(
                    roots = ?watcher.watched_roots(),
                    polled = ?watcher.polled_roots(),
                    "watching for changes"
                );
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
                // A drive pulled out before the monitor noticed: hide it, never "delete" it.
                let Some(gone) = self.vanished_roots(token, &paths, false) else {
                    return;
                };
                let paths: Vec<PathBuf> = if gone.is_empty() {
                    paths
                } else {
                    let keys: Vec<String> = gone.iter().map(|g| path_key(g)).collect();
                    paths
                        .into_iter()
                        .filter(|p| {
                            let k = path_key(p);
                            !keys.iter().any(|g| key_has_prefix(&k, g))
                        })
                        .collect()
                };
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
                        Some(live) => {
                            let applicable = core.applicable(&paths);
                            (!applicable.is_empty())
                                .then(|| (Arc::clone(live), core.session, applicable))
                        }
                        None => None,
                    }
                };
                if !gone.is_empty() {
                    self.roots_vanished(&gone);
                }
                if let Some((live, session, applicable)) = target {
                    self.apply(&live, session, &applicable, true);
                }
                if !paths.is_empty() {
                    if let Some(observer) = self.observer.0.get() {
                        observer.files_changed(&WatchBatch::Changed(paths));
                    }
                }
            }
            WatchBatch::RescanNeeded(reason) => {
                let Some(gone) = self.vanished_roots(token, &[], true) else {
                    return;
                };
                if !gone.is_empty() {
                    // The watcher failed because its drive went away: not a reason to re-walk.
                    tracing::info!(%reason, "file watching stopped on a removed drive");
                    self.roots_vanished(&gone);
                    return;
                }
                let roots = {
                    let core = self.core();
                    if core.watch_token != token {
                        return;
                    }
                    if core.live_roots.is_empty() {
                        core.watch_roots.clone()
                    } else {
                        core.live_roots.clone()
                    }
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
        self.publish_applied(live, session, stats, started, report);
    }

    /// Publishes the snapshot of `live` after changes (`stats`) if `live` is still current.
    /// Returns true if a new view was published.
    fn publish_applied(
        self: &Arc<Self>,
        live: &LiveIndex,
        session: u64,
        stats: ApplyStats,
        started: Instant,
        report: bool,
    ) -> bool {
        if !stats.changed() {
            return false;
        }
        let view = {
            let core = self.core();
            if core.session != session {
                return false;
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
        true
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

    /// Makes `index` (a walk of all of `roots`) the current index; see
    /// [`Indexer::install_walked`]. (Tests only: the indexing thread knows what it walked.)
    #[cfg(test)]
    pub(crate) fn install(
        self: &Arc<Self>,
        index: FileIndex,
        roots: &[PathBuf],
    ) -> Result<Arc<IndexView>, String> {
        self.install_walked(index, roots, roots)
    }

    /// Makes `index` (walked or loaded, of `roots`; `walked` are the roots this walk read) the
    /// base of a new live index, hides its offline roots, publishes it under a new generation,
    /// then applies the paths changed during the walk, if any. Returns the current view.
    fn install_walked(
        self: &Arc<Self>,
        index: FileIndex,
        roots: &[PathBuf],
        walked: &[PathBuf],
    ) -> Result<Arc<IndexView>, String> {
        let live =
            Arc::new(LiveIndex::new(index, &walk_options(roots)).map_err(|e| e.to_string())?);
        let (view, buffered, session) = {
            let mut core = self.core();
            // A walk that raced an unplug (or a snapshot) may hold entries of an offline root.
            let hidden: Vec<Change> = core
                .offline
                .iter()
                .filter(|r| roots.contains(r))
                .map(|r| Change::Remove { path: r.clone() })
                .collect();
            if !hidden.is_empty() {
                live.apply_changes(&hidden);
            }
            core.session += 1;
            core.live = Some(Arc::clone(&live));
            core.live_roots = roots.to_vec();
            core.unheld
                .retain(|r| !walked.contains(r) && roots.contains(r));
            let buffered = core.during_walk.take().map(|b| {
                let paths = core.applicable(&b.paths);
                (b.overflowed, paths)
            });
            let view = self.publish_new_base(&core, live.snapshot(), roots);
            (view, buffered, core.session)
        };
        if let Some((overflowed, paths)) = buffered {
            if overflowed {
                self.rescan(roots.to_vec());
            } else if !paths.is_empty() {
                self.apply(&live, session, &paths, false);
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

    /// Status for `job`, about to start; keeps the current entry count since searches still use
    /// the previous index. `roots` are the job's online roots, `scanning` those it walks.
    fn indexing_status(&self, job: &Job) -> IndexStatus {
        let (online, offline, scanning) = {
            let core = self.core();
            let online: Vec<PathBuf> = job
                .roots()
                .iter()
                .filter(|r| !core.offline.contains(r))
                .cloned()
                .collect();
            let scanning = match job {
                Job::Full(_) => online.clone(),
                Job::Update { roots, refresh } => core
                    .update_work(roots, refresh)
                    .map_or_else(Vec::new, |w| if w.full { online.clone() } else { w.add }),
            };
            (online, core.offline.clone(), scanning)
        };
        IndexStatus {
            state: IndexState::Indexing,
            entries: self.index().map_or(0, |ix| ix.len() as u64),
            roots: root_strings(&online),
            elapsed_ms: None,
            error: None,
            offline: root_strings(&offline),
            scanning: root_strings(&scanning),
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

fn root_strings(roots: &[PathBuf]) -> Vec<String> {
    roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect()
}

/// Walk options for `roots` with the default exclusions from the IPC contract (the shared list
/// of [`athrix_fs_index::exclude`], also used by the CLI and the content daemon).
pub fn walk_options(roots: &[PathBuf]) -> WalkOptions {
    WalkOptions::new(roots.iter().cloned()).with_default_exclusions()
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
            p.status = indexer.indexing_status(&Job::Full(roots.to_vec()));
        }
        let (l, rx) = listener();
        indexer.run(Job::Full(roots.to_vec()), &l, use_snapshot);
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
            p.status = indexer.indexing_status(&Job::Full(vec![a.path().to_path_buf()]));
        }
        let queued =
            |d: &tempfile::TempDir| indexer.start(vec![d.path().to_path_buf()], Arc::clone(&l));
        assert_eq!(queued(&b), StartOutcome::Queued);
        assert_eq!(queued(&c), StartOutcome::Queued);
        assert!(rx.try_recv().is_err(), "queuing emits nothing");
        // Run the "running" walk on this thread: it must then pick up `c` (not `b`).
        indexer.run(Job::Full(vec![a.path().to_path_buf()]), &l, false);
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

    #[test]
    fn a_polled_root_keeps_the_index_current_and_goes_offline_when_removed() {
        let fixed = drive_with("fixed-file.txt");
        let usb_parent = tempfile::tempdir().unwrap();
        let usb = usb_parent.path().join("usb");
        fs::create_dir_all(&usb).unwrap();
        fs::write(usb.join("usb-file.txt"), b"x").unwrap();
        let roots = vec![fixed.path().to_path_buf(), usb.clone()];
        let indexer = Indexer::new();
        // The second root stands for a removable drive.
        indexer.set_poll_policy(
            |roots| {
                roots
                    .iter()
                    .filter(|r| r.ends_with("usb"))
                    .cloned()
                    .collect()
            },
            Duration::from_millis(100),
        );
        let indexer = Arc::new(indexer);
        let events = run_here(&indexer, &roots, false);
        assert_eq!(states(&events), [IndexState::Ready]);
        assert!(eventually(|| indexer.watched_roots().len() == 2));
        assert_eq!(
            indexer.core().watcher.as_ref().unwrap().polled_roots(),
            std::slice::from_ref(&usb)
        );

        fs::write(usb.join("usb-new.txt"), b"x").unwrap();
        assert!(
            eventually(|| finds(&indexer.index().unwrap(), "usb-new")),
            "a change on the polled root is found"
        );

        // Pulled out before the drive monitor noticed: offline, not deleted, no re-walk.
        fs::remove_dir_all(&usb).unwrap();
        assert!(eventually(|| indexer.offline_roots() == [usb.clone()]));
        assert!(!finds(&indexer.index().unwrap(), "usb-file"));
        assert!(finds(&indexer.index().unwrap(), "fixed-file"));
        assert_eq!(indexer.watched_roots(), [fixed.path().to_path_buf()]);
    }

    /// A folder holding one file named `name`.
    fn drive_with(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(name), b"x").unwrap();
        dir
    }

    /// Waits for a walk to start, then for the `ready` status that ends it.
    fn settled(rx: &mpsc::Receiver<IndexStatus>) -> IndexStatus {
        while next(rx).state != IndexState::Indexing {}
        loop {
            let s = next(rx);
            if s.state == IndexState::Ready {
                assert!(s.scanning.is_empty(), "{s:?}");
                return s;
            }
        }
    }

    #[test]
    fn queued_jobs_merge_and_a_full_walk_wins() {
        let p = |s: &str| PathBuf::from(s);
        let update = |roots: &[&str], refresh: &[&str]| Job::Update {
            roots: roots.iter().map(|r| p(r)).collect(),
            refresh: refresh.iter().map(|r| p(r)).collect(),
        };
        match update(&["/a"], &["/a"]).merge(update(&["/a", "/b"], &["/b"])) {
            Job::Update { roots, refresh } => {
                assert_eq!(roots, [p("/a"), p("/b")], "latest roots");
                assert_eq!(refresh, [p("/a"), p("/b")], "both refreshes kept");
            }
            Job::Full(_) => panic!("two updates stay an update"),
        }
        assert!(matches!(
            Job::Full(vec![p("/a")]).merge(update(&["/b"], &[])),
            Job::Full(r) if r == [p("/b")]
        ));
        assert!(matches!(
            update(&["/a"], &[]).merge(Job::Full(vec![p("/c")])),
            Job::Full(r) if r == [p("/c")]
        ));
    }

    #[test]
    fn an_unplugged_root_is_hidden_at_once_and_back_after_walking_it_alone() {
        let fixed = drive_with("fixed-file.txt");
        let usb = drive_with("usb-file.txt");
        let roots = vec![fixed.path().to_path_buf(), usb.path().to_path_buf()];
        let indexer = Arc::new(Indexer::new());
        let events = run_here(&indexer, &roots, false);
        assert_eq!(states(&events), [IndexState::Ready]);
        let view = indexer.index().unwrap();
        assert!(finds(&view, "usb-file") && finds(&view, "fixed-file"));

        // Unplugged: hidden without a walk, the root stays listed, offline.
        indexer.set_offline_roots(&roots[1..]);
        let view = indexer.index().unwrap();
        assert!(!finds(&view, "usb-file"), "hidden at once");
        assert!(finds(&view, "fixed-file"));
        let status = indexer.status();
        assert_eq!(status.state, IndexState::Ready);
        assert_eq!(status.roots, [fixed.path().to_string_lossy().into_owned()]);
        assert_eq!(status.offline, [usb.path().to_string_lossy().into_owned()]);
        assert_eq!(indexer.watched_roots(), [fixed.path().to_path_buf()]);
        // Nothing to walk while it stays unplugged.
        let (l, rx) = listener();
        assert_eq!(
            indexer.reconcile(
                roots.clone(),
                roots[1..].to_vec(),
                Vec::new(),
                Arc::clone(&l)
            ),
            None
        );

        // Plugged back (with a new file): walked alone, then searchable again.
        fs::write(usb.path().join("usb-new.txt"), b"x").unwrap();
        assert!(
            indexer
                .reconcile(roots.clone(), Vec::new(), Vec::new(), l)
                .is_some()
        );
        let done = settled(&rx);
        assert!(done.offline.is_empty(), "{done:?}");
        let view = indexer.index().unwrap();
        assert!(finds(&view, "usb-file") && finds(&view, "usb-new"));
        assert!(
            finds(&view, "fixed-file"),
            "the fixed drive kept its entries"
        );
        assert_eq!(view.len(), indexer.status().entries as usize);
        assert!(eventually(|| indexer.watched_roots().len() == 2));
    }

    #[test]
    fn reconcile_walks_added_roots_and_drops_removed_ones() {
        let a = drive_with("alpha-file.txt");
        let b = drive_with("beta-file.txt");
        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        let indexer = Arc::new(Indexer::new());
        run_here(&indexer, std::slice::from_ref(&pa), false);
        let (l, rx) = listener();
        assert_eq!(
            indexer.reconcile(vec![pa.clone()], Vec::new(), Vec::new(), Arc::clone(&l)),
            None,
            "nothing changed"
        );

        assert!(
            indexer
                .reconcile(
                    vec![pa.clone(), pb.clone()],
                    Vec::new(),
                    Vec::new(),
                    Arc::clone(&l)
                )
                .is_some()
        );
        let done = settled(&rx);
        assert_eq!(done.roots.len(), 2);
        let view = indexer.index().unwrap();
        assert!(finds(&view, "alpha-file") && finds(&view, "beta-file"));

        assert!(
            indexer
                .reconcile(vec![pb.clone()], Vec::new(), Vec::new(), Arc::clone(&l))
                .is_some()
        );
        let done = settled(&rx);
        assert_eq!(done.roots, [pb.to_string_lossy().into_owned()]);
        let view = indexer.index().unwrap();
        assert!(!finds(&view, "alpha-file"), "dropped root");
        assert!(finds(&view, "beta-file"));

        // Another volume on the same path (`refresh`): walked again, its new content shows.
        fs::remove_file(pb.join("beta-file.txt")).unwrap();
        fs::write(pb.join("gamma-file.txt"), b"x").unwrap();
        assert!(
            indexer
                .reconcile(vec![pb.clone()], Vec::new(), vec![pb.clone()], l)
                .is_some()
        );
        settled(&rx);
        assert!(eventually(|| {
            let view = indexer.index().unwrap();
            finds(&view, "gamma-file") && !finds(&view, "beta-file")
        }));
    }
}
