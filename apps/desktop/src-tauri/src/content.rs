//! Content search (tasks 4.5 and 4.7): the full-text index of what is inside files, kept
//! current in the background, and the searches that combine it with the name index.
//!
//! # Pieces
//!
//! - **Content index** (`athrix-content-index`) in `<app_data>/content/`, opened when the
//!   name index first becomes ready. If it had to be rebuilt (damaged, or built by another
//!   version) it starts empty, and enqueuing the snapshot re-extracts every file.
//! - **Scheduler** (`athrix-daemon`): started after the name index is ready (snapshot loaded
//!   or walk finished; see [`IndexObserver::base_ready`]). Every new base enqueues its
//!   snapshot (files already indexed in the same version are skipped cheaply), and every
//!   watcher batch is forwarded after the name index applied it. A watcher rescan re-walks
//!   the name index, whose new base enqueues again. [`ContentService::shutdown`] stops it on
//!   app exit (commits what was extracted).
//! - **Setting** "content indexing enabled" in `<app_data>/content-indexing.json`
//!   (`{"version":1,"enabled":true}`, default enabled). Disabling stops the scheduler; what is
//!   already indexed stays searchable.
//! - **Frecency** (`athrix-ranking`) in `<app_data>/frecency.txt`: every opened result is
//!   recorded; it boosts the order of content searches. Saved every
//!   [`FRECENCY_SAVE_INTERVAL`] when changed, and on exit.
//! - **Status** ([`ContentStatus`]) for `content_status` and the `content://status` event,
//!   at most one event per [`STATUS_INTERVAL`] (the latest state is always sent at the end of
//!   the interval).
//!
//! # Search
//!
//! Content plans ([`SearchPlan::NameAndContent`], [`SearchPlan::NameOrContent`]) run through
//! [`athrix_ranking::execute`] with `PlanSources` over one [`IndexView`] (respecting the
//! search scope) and the content index. Content hits are mapped back to entry ids of that
//! view; a hit whose file is no longer in the name index (deleted, or under a root no longer
//! indexed) is dropped.
//!
//! ## Path → id
//!
//! The content index keys documents by the name index's path string. Mapping a hit back to an
//! entry uses a `PathLookup` built once per name-index base: a sorted table of
//! `(hash of the path key, base id)` for every base *file whose format the extractor knows*
//! (`Kind::from_path`, the same cheap test the scheduler uses before anything else), so
//! folders and the many files without text never cost anything. That is 12 bytes per
//! content-candidate file (a few MB for a few hundred thousand documents), the same per-entry
//! cost as the live index's own child table, and much less than a `path → id` map of strings
//! (100+ bytes per entry). A lookup is a binary search plus one path rebuild to rule out hash
//! collisions. Entries added since the base (the live delta, bounded by compaction) are
//! resolved through a small `path key → id` map built per view, only when a hit is not in the
//! base. Building the table costs one path rebuild per candidate file; it happens on the
//! indexing thread when a base becomes ready (or on the first content search after a
//! compaction).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athrix_content_index::{ContentIndex, PathFilter, Snippet};
use athrix_daemon::{ContentScheduler, Extractor, NullProbe, Progress, RunState, SchedulerConfig};
use athrix_extract::Kind;
use athrix_fs_index::{EntryId, FileIndex, IndexSnapshot, Query as NameQuery, WatchBatch};
use athrix_query::{Query as AstQuery, SearchPlan};
use athrix_ranking::{
    ContentHit, ExecOptions, FileFrecencyStore, FrecencyConfig, FrecencyStore, MemoryFrecencyStore,
    NameHit, NameResults, Sources, execute,
};
use serde::{Deserialize, Serialize};

use crate::indexing::{IndexObserver, IndexView};
use crate::model::{ContentSnippet, ContentState, ContentStatus, PauseReason};
use crate::query::{CompiledQuery, utf16_offset};
use crate::results::{SearchFailure, SearchHits, SearchScope, build_row, search_ids};
use crate::roots::path_key;

/// Folder of the content index inside the app data folder.
pub const CONTENT_DIR: &str = "content";

/// File of the frecency store inside the app data folder.
pub const FRECENCY_FILE: &str = "frecency.txt";

/// File of the "content indexing enabled" setting inside the app data folder.
pub const CONTENT_SETTING_FILE: &str = "content-indexing.json";

/// Minimum time between two `content://status` events.
pub const STATUS_INTERVAL: Duration = Duration::from_millis(500);

/// How often changed frecency data is saved.
pub const FRECENCY_SAVE_INTERVAL: Duration = Duration::from_secs(60);

/// Name results fetched to intersect with content (`name AND content`). Beyond it, content
/// hits are checked one by one against the name query ([`Sources::name_matches`]), so the
/// cap only bounds time and memory, not correctness.
const NAME_CAP: usize = 20_000;

/// Most content hits fetched while filtering by scope / name index.
const CONTENT_CAP: usize = 10_000;

const SETTING_VERSION: u32 = 1;

/// The extractor the content scheduler uses.
///
/// This is the single construction point: PDFs go through the isolated worker pool
/// ([`crate::pdf_worker`]), every other format is extracted in-process.
pub fn content_extractor() -> Arc<dyn Extractor> {
    crate::pdf_worker::content_extractor()
}

/// Receives every [`ContentStatus`] to emit (throttled).
pub type StatusSink = Arc<dyn Fn(&ContentStatus) + Send + Sync>;

/// Scheduler settings of a [`ContentService`].
#[derive(Debug, Clone)]
pub struct ContentOptions {
    /// Passed to the scheduler as is.
    pub scheduler: SchedulerConfig,
    /// Sample battery and CPU load for auto-pause (off in tests: never pauses by itself).
    pub system_probe: bool,
}

impl Default for ContentOptions {
    fn default() -> Self {
        ContentOptions {
            scheduler: SchedulerConfig::default(),
            system_probe: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingFile {
    version: u32,
    enabled: bool,
}

#[derive(Debug)]
struct Paths {
    content_dir: PathBuf,
    setting_file: PathBuf,
}

/// The frecency store: in memory until [`ContentService::configure`] points it at a file.
#[derive(Debug)]
enum Frecency {
    Memory(MemoryFrecencyStore),
    File(FileFrecencyStore),
}

impl FrecencyStore for Frecency {
    fn record_open(&mut self, path: &str, at: i64) {
        match self {
            Frecency::Memory(m) => m.record_open(path, at),
            Frecency::File(f) => f.record_open(path, at),
        }
    }

    fn frecency(&self, path: &str, now: i64) -> f64 {
        match self {
            Frecency::Memory(m) => m.frecency(path, now),
            Frecency::File(f) => f.frecency(path, now),
        }
    }
}

#[derive(Debug, Default)]
struct EmitState {
    last: Option<Instant>,
    scheduled: bool,
}

#[derive(Default)]
struct SinkSlot(OnceLock<StatusSink>);

impl std::fmt::Debug for SinkSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SinkSlot")
    }
}

/// `path key → id` of the live files added since the base.
type DeltaMap = HashMap<String, EntryId>;

/// The content index, its scheduler, the frecency store and the status events. Shared by the
/// commands (through `AppState`) and the name indexer (as its [`IndexObserver`]).
#[derive(Debug)]
pub struct ContentService {
    me: Weak<ContentService>,
    options: ContentOptions,
    enabled: AtomicBool,
    user_paused: AtomicBool,
    shut_down: AtomicBool,
    paths: OnceLock<Paths>,
    index: RwLock<Option<Arc<ContentIndex>>>,
    error: Mutex<Option<String>>,
    scheduler: RwLock<Option<Arc<ContentScheduler>>>,
    /// Serializes starting and stopping the scheduler (never held by searches).
    lifecycle: Mutex<()>,
    frecency: RwLock<Frecency>,
    lookup: Mutex<Option<Arc<PathLookup>>>,
    delta: Mutex<Option<(Weak<IndexSnapshot>, Arc<DeltaMap>)>>,
    sink: SinkSlot,
    emit: Mutex<EmitState>,
}

impl ContentService {
    /// A service with `options`, enabled, not configured (no index, frecency in memory).
    pub fn new(options: ContentOptions) -> Arc<Self> {
        Arc::new_cyclic(|me| ContentService {
            me: me.clone(),
            options,
            enabled: AtomicBool::new(true),
            user_paused: AtomicBool::new(false),
            shut_down: AtomicBool::new(false),
            paths: OnceLock::new(),
            index: RwLock::new(None),
            error: Mutex::new(None),
            scheduler: RwLock::new(None),
            lifecycle: Mutex::new(()),
            frecency: RwLock::new(Frecency::Memory(MemoryFrecencyStore::new(
                FrecencyConfig::default(),
            ))),
            lookup: Mutex::new(None),
            delta: Mutex::new(None),
            sink: SinkSlot::default(),
            emit: Mutex::new(EmitState::default()),
        })
    }

    /// Points the service at the app data folder `app_data`: loads the enabled setting and
    /// the frecency file, and remembers where the content index lives (opened later, when the
    /// name index is ready). Starts the periodic frecency saver. Only the first call has an
    /// effect.
    pub fn configure(&self, app_data: &Path) {
        let paths = Paths {
            content_dir: app_data.join(CONTENT_DIR),
            setting_file: app_data.join(CONTENT_SETTING_FILE),
        };
        let enabled = read_enabled(&paths.setting_file);
        if self.paths.set(paths).is_err() {
            tracing::warn!("the content service is already configured");
            return;
        }
        self.enabled.store(enabled, Ordering::Relaxed);
        match FileFrecencyStore::open(app_data.join(FRECENCY_FILE), FrecencyConfig::default()) {
            Ok(store) => *self.write_frecency() = Frecency::File(store),
            Err(e) => tracing::warn!(error = %e, "cannot read the frecency file; starting empty"),
        }
        let weak = self.me.clone();
        let spawned = std::thread::Builder::new()
            .name("athrix-frecency".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(FRECENCY_SAVE_INTERVAL);
                    match weak.upgrade() {
                        Some(me) if !me.shut_down.load(Ordering::Relaxed) => me.save_frecency(),
                        _ => return,
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot start the frecency saver; saving on exit only");
        }
    }

    /// Sets where status events go (only the first call has an effect).
    pub fn set_status_sink(&self, sink: StatusSink) {
        if self.sink.0.set(sink).is_err() {
            tracing::warn!("the content status sink is already set");
        }
    }

    /// Whether content indexing is enabled (the persisted setting).
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The content index, if it is open.
    pub fn index(&self) -> Option<Arc<ContentIndex>> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn scheduler(&self) -> Option<Arc<ContentScheduler>> {
        self.scheduler
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The current status.
    pub fn status(&self) -> ContentStatus {
        match self.scheduler() {
            Some(s) => self.status_of(Some(&s.progress())),
            None => self.status_of(None),
        }
    }

    /// The status for scheduler counters `progress` (`None`: no scheduler running).
    fn status_of(&self, progress: Option<&Progress>) -> ContentStatus {
        let indexed = self.index().map_or(0, |ix| ix.num_docs());
        let mut status = ContentStatus {
            state: ContentState::Idle,
            paused_by: Vec::new(),
            processed: 0,
            total: 0,
            indexed,
            failed: 0,
            docs_per_second: 0.0,
            error: None,
        };
        if let Some(p) = progress {
            status.processed = p.processed();
            status.total = p.total();
            status.failed = p.failed.total();
            status.docs_per_second = p.docs_per_sec;
        }
        if !self.enabled() {
            status.state = ContentState::Disabled;
            status.docs_per_second = 0.0;
            return status;
        }
        if let Some(e) = self.lock(&self.error).clone() {
            status.state = ContentState::Error;
            status.error = Some(e);
            return status;
        }
        status.state = match progress.map(|p| p.state) {
            Some(RunState::Running) => ContentState::Running,
            Some(RunState::Idle) => ContentState::Idle,
            Some(RunState::Stopped) => ContentState::Stopped,
            Some(RunState::Paused(r)) => {
                if r.user {
                    status.paused_by.push(PauseReason::User);
                }
                if r.battery {
                    status.paused_by.push(PauseReason::Battery);
                }
                if r.busy {
                    status.paused_by.push(PauseReason::Busy);
                }
                ContentState::Paused
            }
            None if self.shut_down.load(Ordering::Relaxed) => ContentState::Stopped,
            None if self.user_paused.load(Ordering::Relaxed) => {
                status.paused_by.push(PauseReason::User);
                ContentState::Paused
            }
            None => ContentState::Idle,
        };
        if status.state != ContentState::Running {
            status.docs_per_second = 0.0;
        }
        status
    }

    /// Emits the status through the sink, at most once per [`STATUS_INTERVAL`]: a call inside
    /// the interval schedules one emission at its end (with the state of that moment).
    /// `now` is the status to send if it goes out at once (computed when `None`).
    fn notify(&self, now: Option<ContentStatus>) {
        let Some(sink) = self.sink.0.get() else {
            return;
        };
        let at = Instant::now();
        let delay = {
            let mut e = self.lock(&self.emit);
            match e.last {
                Some(last) if at.duration_since(last) < STATUS_INTERVAL => {
                    if e.scheduled {
                        return;
                    }
                    e.scheduled = true;
                    STATUS_INTERVAL - at.duration_since(last)
                }
                _ => {
                    e.last = Some(at);
                    Duration::ZERO
                }
            }
        };
        if delay.is_zero() {
            let status = now.unwrap_or_else(|| self.status());
            sink(&status);
            return;
        }
        let weak = self.me.clone();
        let sink = Arc::clone(sink);
        let spawned = std::thread::Builder::new()
            .name("athrix-content-status".into())
            .spawn(move || {
                std::thread::sleep(delay);
                let Some(me) = weak.upgrade() else {
                    return;
                };
                {
                    let mut e = me.lock(&me.emit);
                    e.scheduled = false;
                    e.last = Some(Instant::now());
                }
                sink(&me.status());
            });
        if spawned.is_err() {
            self.lock(&self.emit).scheduled = false;
        }
    }

    /// Opens the content index if needed. `None` when not configured or it cannot be opened
    /// (the error is kept for the status).
    fn ensure_index(&self) -> Option<Arc<ContentIndex>> {
        if let Some(ix) = self.index() {
            return Some(ix);
        }
        let paths = self.paths.get()?;
        match ContentIndex::open_or_create(&paths.content_dir) {
            Ok(ix) => {
                if let Some(reason) = ix.rebuilt() {
                    tracing::warn!(
                        ?reason,
                        "the content index was rebuilt; every file will be indexed again"
                    );
                }
                let ix = Arc::new(ix);
                *self.index.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&ix));
                *self.lock(&self.error) = None;
                Some(ix)
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot open the content index");
                *self.lock(&self.error) = Some(format!("cannot open the content index: {e}"));
                None
            }
        }
    }

    /// Starts the scheduler if it is not running (caller holds `lifecycle`).
    fn ensure_scheduler(&self) -> Option<Arc<ContentScheduler>> {
        if let Some(s) = self.scheduler() {
            return Some(s);
        }
        let index = self.ensure_index()?;
        let mut config = self.options.scheduler.clone();
        config.start_paused = config.start_paused || self.user_paused.load(Ordering::Relaxed);
        let weak = self.me.clone();
        let mut builder = ContentScheduler::builder(index)
            .config(config)
            .extractor(content_extractor())
            .on_progress(move |p| {
                // Runs on the scheduler's control thread: never touch `scheduler` here (a
                // shutdown joins this thread).
                if let Some(me) = weak.upgrade() {
                    let status = me.status_of(Some(p));
                    me.notify(Some(status));
                }
            });
        if !self.options.system_probe {
            builder = builder.probe(NullProbe);
        }
        match builder.start() {
            Ok(s) => {
                let s = Arc::new(s);
                *self
                    .scheduler
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&s));
                tracing::info!("content indexing started");
                Some(s)
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot start content indexing");
                *self.lock(&self.error) = Some(format!("cannot start content indexing: {e}"));
                None
            }
        }
    }

    /// Stops the scheduler, if running (caller holds `lifecycle`): workers finish their file,
    /// what was extracted is committed.
    fn stop_scheduler(&self) {
        let taken = self
            .scheduler
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(s) = taken {
            // A status read may hold a clone for a moment; the last holder shuts it down.
            match Arc::try_unwrap(s) {
                Ok(s) => {
                    let p = s.shutdown();
                    tracing::info!(indexed = p.indexed, "content indexing stopped");
                }
                Err(s) => drop(s),
            }
        }
    }

    /// Queues every content candidate of `view` (enabled and not shut down only), starting
    /// the scheduler first if needed, and prepares the path lookup of its base.
    fn index_view(&self, view: &IndexView) {
        {
            let _turn = self.lock(&self.lifecycle);
            if self.shut_down.load(Ordering::Relaxed) || !self.enabled() {
                return;
            }
            if self.scheduler().is_none() {
                // First start of this session: drop documents of files the name index no
                // longer has (deleted while the app was closed, or under a root no longer
                // indexed), before the writer thread exists.
                self.prune(view);
            }
            if let Some(s) = self.ensure_scheduler() {
                let started = Instant::now();
                let queued = s.enqueue_snapshot(view.snapshot());
                tracing::info!(
                    queued,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "content candidates queued"
                );
            }
        }
        self.notify(None);
        self.lookup_for(view);
    }

    /// Deletes the content documents whose file is not in `view` and commits.
    fn prune(&self, view: &IndexView) {
        let Some(index) = self.ensure_index() else {
            return;
        };
        let lookup = self.lookup_for(view);
        let mut delta = None;
        let started = Instant::now();
        let removed = index.retain(|path, _| {
            self.find_file(view, &lookup, &mut delta, &path_key(Path::new(path)))
                .is_some()
        });
        match removed.and_then(|n| index.commit().map(|()| n)) {
            Ok(removed) => tracing::info!(
                removed,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "content documents of missing files removed"
            ),
            Err(e) => tracing::warn!(error = %e, "cannot prune the content index"),
        }
    }

    /// The live file entry of `view` whose [`path_key`] is `key`: the base through `lookup`,
    /// then the delta (its map is built into `delta` on first need).
    fn find_file(
        &self,
        view: &IndexView,
        lookup: &PathLookup,
        delta: &mut Option<Arc<DeltaMap>>,
        key: &str,
    ) -> Option<EntryId> {
        let mut found = lookup
            .candidates(key_hash(key))
            .iter()
            .copied()
            .find(|&id| view.contains(id) && path_key(&view.path(id)) == key);
        if found.is_none() && view.delta_entries() > 0 {
            found = delta
                .get_or_insert_with(|| self.delta_for(view))
                .get(key)
                .copied();
        }
        found.filter(|&id| !view.meta(id).is_dir)
    }

    /// Feeds a watcher batch to the scheduler (if running).
    fn forward_batch(&self, batch: &WatchBatch) {
        if let Some(s) = self.scheduler() {
            // `false` = rescan needed: the name indexer re-walks, and its new base is queued
            // again through `base_ready`.
            let _ = s.watch_batch(batch);
        }
    }

    /// Pauses content indexing at the user's request.
    pub fn pause(&self) {
        self.user_paused.store(true, Ordering::Relaxed);
        if let Some(s) = self.scheduler() {
            s.pause();
        }
        self.notify(None);
    }

    /// Ends a user pause (auto-pause on battery or high CPU may still hold).
    pub fn resume(&self) {
        self.user_paused.store(false, Ordering::Relaxed);
        if let Some(s) = self.scheduler() {
            s.resume();
        }
        self.notify(None);
    }

    /// Turns content indexing on or off and saves the setting. Turning it off stops the
    /// scheduler (blocks until the workers finish their files); turning it on starts it with
    /// `current` (the current name-index view), if any. A save failure is logged only.
    pub fn set_enabled(&self, enabled: bool, current: Option<Arc<IndexView>>) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if let Some(paths) = self.paths.get() {
            if let Err(e) = write_enabled(&paths.setting_file, enabled) {
                tracing::warn!(error = %e, "cannot save the content-indexing setting");
            }
        }
        if enabled {
            if let Some(view) = current {
                self.index_view(&view);
                return;
            }
        } else {
            let _turn = self.lock(&self.lifecycle);
            self.stop_scheduler();
        }
        self.notify(None);
    }

    /// Stops content indexing for good (app exit) and saves the frecency data.
    pub fn shutdown(&self) {
        {
            let _turn = self.lock(&self.lifecycle);
            self.shut_down.store(true, Ordering::Relaxed);
            self.stop_scheduler();
        }
        self.save_frecency();
    }

    /// Records that the file at `path` (the name index's form) was opened now.
    pub fn record_open(&self, path: &str) {
        self.write_frecency().record_open(path, now_secs());
    }

    /// Saves the frecency file if it changed (logged on failure).
    pub fn save_frecency(&self) {
        if let Frecency::File(f) = &mut *self.write_frecency() {
            if let Err(e) = f.save_if_dirty() {
                tracing::warn!(error = %e, "cannot save the frecency file");
            }
        }
    }

    /// Waits until the scheduler has nothing left to do (tests).
    #[cfg(test)]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        self.scheduler().is_some_and(|s| s.wait_idle(timeout))
    }

    /// Runs a content plan on `view` (only under `scope` if given) and the content index,
    /// returning up to `limit` items. `name_query` gives the name highlights (the plan's name
    /// part, compiled). Items carry `matched` and the content snippet.
    pub fn search(
        &self,
        view: &IndexView,
        plan: &SearchPlan,
        name_query: Option<&CompiledQuery>,
        limit: usize,
        scope: Option<&SearchScope>,
        cancel: &AtomicBool,
    ) -> Result<SearchHits, SearchFailure> {
        let lookup = self.lookup_for(view);
        let index = self.index();
        let mut sources = PlanSources {
            service: self,
            view,
            scope,
            cancel,
            content: index.as_deref(),
            lookup: &lookup,
            delta: None,
            scope_filter: scope.map(|s| PathFilter::prefix(&s.path().to_string_lossy())),
            resolved: HashMap::new(),
            compiled: Vec::new(),
        };
        let mut options = ExecOptions::new(now_secs());
        options.limit = limit.saturating_add(1);
        options.name_cap = NAME_CAP;
        options.content_cap = CONTENT_CAP;
        options.exclusion_cap = CONTENT_CAP;
        let results = {
            let frecency = self.frecency.read().unwrap_or_else(PoisonError::into_inner);
            let store: &dyn FrecencyStore = &*frecency;
            execute(plan, &options, Some(store), &mut sources)?
        };
        let truncated = results.hits.len() > limit;
        let mut items = Vec::with_capacity(results.hits.len().min(limit));
        for hit in results.hits.into_iter().take(limit) {
            if cancel.load(Ordering::Relaxed) {
                return Err(SearchFailure::Cancelled);
            }
            let Some(id) = sources.resolve(&hit.path) else {
                continue;
            };
            let mut item = build_row(view, view.generation(), id, |name| {
                name_query.map_or_else(Vec::new, |q| q.highlights(name))
            });
            item.matched = hit.matched.into();
            item.snippet = hit.snippet.map(ui_snippet);
            items.push(item);
        }
        Ok(SearchHits { items, truncated })
    }

    /// The path lookup of `view`'s base, built if the cached one is of another base.
    fn lookup_for(&self, view: &IndexView) -> Arc<PathLookup> {
        let base = view.snapshot().base();
        let mut slot = self.lock(&self.lookup);
        if let Some(l) = slot.as_ref().filter(|l| l.is_of(base)) {
            return Arc::clone(l);
        }
        // Drop the old table before building the new one (peak memory).
        *slot = None;
        let started = Instant::now();
        let built = Arc::new(PathLookup::build(base));
        tracing::debug!(
            files = built.ids.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "content path lookup built"
        );
        *slot = Some(Arc::clone(&built));
        built
    }

    /// `path key → id` of the live delta entries (files) of `view`, cached for the last view.
    fn delta_for(&self, view: &IndexView) -> Arc<DeltaMap> {
        let snapshot = view.snapshot();
        let mut slot = self.lock(&self.delta);
        if let Some((weak, map)) = slot.as_ref() {
            if std::ptr::eq(weak.as_ptr(), Arc::as_ptr(snapshot)) {
                return Arc::clone(map);
            }
        }
        let mut map = HashMap::new();
        for id in snapshot.base().len()..snapshot.id_bound() {
            let Ok(id) = EntryId::try_from(id) else {
                break;
            };
            // `contains` masks the hidden anchor copies of base folders.
            if snapshot.contains(id) && !snapshot.meta(id).is_dir {
                map.insert(path_key(&snapshot.path(id)), id);
            }
        }
        let map = Arc::new(map);
        *slot = Some((Arc::downgrade(snapshot), Arc::clone(&map)));
        map
    }

    fn write_frecency(&self) -> std::sync::RwLockWriteGuard<'_, Frecency> {
        self.frecency
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock<'a, T>(&self, m: &'a Mutex<T>) -> MutexGuard<'a, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl IndexObserver for ContentService {
    fn base_ready(&self, view: &Arc<IndexView>) {
        self.index_view(view);
    }

    fn files_changed(&self, batch: &WatchBatch) {
        self.forward_batch(batch);
    }
}

/// Converts a content-index snippet (byte ranges) to the UI's (UTF-16 ranges).
fn ui_snippet(s: Snippet) -> ContentSnippet {
    let highlights = s
        .highlights
        .iter()
        .map(|r| {
            (
                utf16_offset(&s.fragment, r.start),
                utf16_offset(&s.fragment, r.end),
            )
        })
        .filter(|(a, b)| a < b)
        .collect();
    ContentSnippet {
        text: s.fragment,
        highlights,
    }
}

/// Sorted `(hash of path key, base id)` of the content-candidate files of one base (see the
/// module docs).
#[derive(Debug)]
struct PathLookup {
    base: Weak<FileIndex>,
    hashes: Vec<u64>,
    ids: Vec<EntryId>,
}

impl PathLookup {
    fn build(base: &Arc<FileIndex>) -> Self {
        let mut pairs: Vec<(u64, EntryId)> = Vec::new();
        for id in 0..base.len() {
            let Ok(id) = EntryId::try_from(id) else {
                break;
            };
            if base.meta(id).is_dir || Kind::from_path(Path::new(base.name(id))).is_none() {
                continue;
            }
            pairs.push((key_hash(&path_key(&base.path(id))), id));
        }
        pairs.sort_unstable();
        let (hashes, ids) = pairs.into_iter().unzip();
        PathLookup {
            base: Arc::downgrade(base),
            hashes,
            ids,
        }
    }

    fn is_of(&self, base: &Arc<FileIndex>) -> bool {
        std::ptr::eq(self.base.as_ptr(), Arc::as_ptr(base))
    }

    /// Base ids whose path key hashes to `hash`.
    fn candidates(&self, hash: u64) -> &[EntryId] {
        let start = self.hashes.partition_point(|&h| h < hash);
        let end = start + self.hashes[start..].partition_point(|&h| h == hash);
        &self.ids[start..end]
    }
}

fn key_hash(key: &str) -> u64 {
    let mut h = DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

/// The two searches of [`execute`] over one view and the content index, with the hit → entry
/// mapping.
struct PlanSources<'a> {
    service: &'a ContentService,
    view: &'a IndexView,
    scope: Option<&'a SearchScope>,
    cancel: &'a AtomicBool,
    content: Option<&'a ContentIndex>,
    lookup: &'a PathLookup,
    delta: Option<Arc<DeltaMap>>,
    /// The scope as a content-index filter, so hits outside it are never fetched.
    scope_filter: Option<PathFilter>,
    /// `path key → entry` (or `None`: not in the view), for every path seen.
    resolved: HashMap<String, Option<EntryId>>,
    compiled: Vec<(AstQuery, NameQuery)>,
}

impl PlanSources<'_> {
    fn check(&self) -> Result<(), SearchFailure> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(SearchFailure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// The compiled form of a name query of the plan (compiled once per search).
    fn compiled(&mut self, query: &AstQuery) -> Result<usize, SearchFailure> {
        if let Some(i) = self.compiled.iter().position(|(q, _)| q == query) {
            return Ok(i);
        }
        let compiled =
            NameQuery::compile(query).map_err(|e| SearchFailure::Failed(e.message().to_owned()))?;
        self.compiled.push((query.clone(), compiled));
        Ok(self.compiled.len() - 1)
    }

    /// The live file entry of the view at `path` (the name index's form), if any.
    fn resolve(&mut self, path: &str) -> Option<EntryId> {
        let key = path_key(Path::new(path));
        if let Some(&found) = self.resolved.get(&key) {
            return found;
        }
        let found = self
            .service
            .find_file(self.view, self.lookup, &mut self.delta, &key);
        self.resolved.insert(key, found);
        found
    }
}

impl Sources for PlanSources<'_> {
    type Snippet = Snippet;
    type Error = SearchFailure;

    fn search_names(
        &mut self,
        query: &AstQuery,
        limit: usize,
    ) -> Result<NameResults, SearchFailure> {
        self.check()?;
        let i = self.compiled(query)?;
        let compiled = &self.compiled[i].1;
        let ids = search_ids(self.view, compiled, limit, self.scope, self.cancel)?;
        let ranked = compiled.is_ranked();
        let mut hits = Vec::with_capacity(ids.len());
        for (n, id) in ids.into_iter().enumerate() {
            if n % 4096 == 0 {
                self.check()?;
            }
            let path = self.view.path(id);
            self.resolved.insert(path_key(&path), Some(id));
            hits.push(NameHit {
                path: path.to_string_lossy().into_owned(),
                mtime: self.view.meta(id).modified,
            });
        }
        Ok(NameResults { hits, ranked })
    }

    fn search_content(
        &mut self,
        text: &str,
        limit: usize,
    ) -> Result<Vec<ContentHit<Snippet>>, SearchFailure> {
        self.check()?;
        let Some(index) = self.content else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Hits outside the scope or no longer in the name index are dropped, so fetch more
        // until `limit` survive or the index has no more.
        let mut fetch = limit.clamp(1, CONTENT_CAP);
        loop {
            let hits = match &self.scope_filter {
                Some(filter) => index.search_within(text, fetch, filter),
                None => index.search(text, fetch),
            }
            .map_err(|e| SearchFailure::Failed(format!("content search failed: {e}")))?;
            self.check()?;
            let exhausted = hits.len() < fetch;
            let mut out = Vec::with_capacity(hits.len().min(limit));
            for h in hits {
                if self.scope.is_some_and(|s| !s.contains(Path::new(&h.path))) {
                    continue;
                }
                let Some(id) = self.resolve(&h.path) else {
                    continue;
                };
                out.push(ContentHit {
                    path: h.path,
                    score: h.score,
                    mtime: self.view.meta(id).modified,
                    snippet: h.snippet,
                });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            if exhausted || fetch >= CONTENT_CAP {
                return Ok(out);
            }
            fetch = fetch.saturating_mul(4).min(CONTENT_CAP);
        }
    }

    fn name_matches(
        &mut self,
        query: &AstQuery,
        path: &str,
    ) -> Result<Option<bool>, SearchFailure> {
        self.check()?;
        let Some(id) = self.resolve(path) else {
            return Ok(Some(false));
        };
        if self.scope.is_some_and(|s| !s.contains(Path::new(path))) {
            return Ok(Some(false));
        }
        let i = self.compiled(query)?;
        Ok(Some(self.view.is_match(&self.compiled[i].1, id)))
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The persisted setting; missing or unreadable means enabled (logged).
fn read_enabled(file: &Path) -> bool {
    let bytes = match fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return true,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the content-indexing setting; enabling it");
            return true;
        }
    };
    match serde_json::from_slice::<SettingFile>(&bytes) {
        Ok(f) if f.version == SETTING_VERSION => f.enabled,
        Ok(f) => {
            tracing::warn!(
                version = f.version,
                "content-indexing setting has another version; enabling it"
            );
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "content-indexing setting is corrupt; enabling it");
            true
        }
    }
}

/// Writes atomically: a temporary file next to `file`, then a rename over it.
fn write_enabled(file: &Path, enabled: bool) -> io::Result<()> {
    let body = SettingFile {
        version: SETTING_VERSION,
        enabled,
    };
    let json = serde_json::to_vec_pretty(&body).map_err(io::Error::other)?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, file)
}

/// Fast, deterministic scheduler settings for tests: no auto-pause, normal priority, and no
/// excluded folder names (temp folders live under `AppData` on Windows).
#[cfg(test)]
pub(crate) fn test_options() -> ContentOptions {
    use athrix_daemon::CandidateFilter;
    ContentOptions {
        scheduler: SchedulerConfig {
            workers: 2,
            commit_every: 100,
            commit_interval: Duration::from_millis(200),
            filter: CandidateFilter {
                excluded_dir_names: Vec::new(),
                excluded_root_dirs: Vec::new(),
                ..CandidateFilter::default()
            },
            priority_folders: Vec::new(),
            progress_interval: Duration::from_millis(20),
            low_priority: false,
            ..SchedulerConfig::default()
        },
        system_probe: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::walk_options;
    use athrix_fs_index::{LiveIndex, walk};

    const LONG: Duration = Duration::from_secs(60);

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("athrix-content-")
            .tempdir()
            .unwrap()
    }

    /// A walked view of `root` (generation 1).
    fn view_of(root: &Path) -> Arc<IndexView> {
        let roots = vec![root.to_path_buf()];
        let opts = walk_options(&roots);
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();
        Arc::new(IndexView::new(1, live.snapshot(), &roots))
    }

    type Seen = Arc<Mutex<Vec<(Instant, ContentStatus)>>>;

    fn recording_sink(service: &ContentService) -> Seen {
        let seen: Seen = Arc::default();
        let sink_seen = Arc::clone(&seen);
        service.set_status_sink(Arc::new(move |s: &ContentStatus| {
            sink_seen.lock().unwrap().push((Instant::now(), s.clone()));
        }));
        seen
    }

    #[test]
    fn status_events_are_throttled_and_the_last_state_is_sent() {
        let service = ContentService::new(test_options());
        let seen = recording_sink(&service);
        // A burst: the first goes out at once, the rest collapse into one at the interval's end.
        for i in 0..50 {
            if i == 25 {
                service.pause();
            } else {
                service.notify(None);
            }
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
        std::thread::sleep(STATUS_INTERVAL * 2);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "{seen:?}");
            let gap = seen[1].0.duration_since(seen[0].0);
            assert!(
                gap >= STATUS_INTERVAL - Duration::from_millis(20),
                "{gap:?}"
            );
            // The trailing event carries the state at its time: paused by the user.
            assert_eq!(seen[1].1.state, ContentState::Paused);
            assert_eq!(seen[1].1.paused_by, vec![PauseReason::User]);
        }
        // Well after the interval: out at once again.
        std::thread::sleep(STATUS_INTERVAL + Duration::from_millis(100));
        service.resume();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2].1.state, ContentState::Idle);
    }

    #[test]
    fn indexes_after_the_name_index_and_reports_progress() {
        let data = tmp();
        fs::write(data.path().join("a.txt"), "ميزانية المشروع").unwrap();
        fs::write(data.path().join("b.md"), "notes").unwrap();
        fs::write(data.path().join("c.png"), "not text").unwrap();
        let app = tmp();
        let service = ContentService::new(test_options());
        service.configure(app.path());
        let seen = recording_sink(&service);
        // Not started until the name index is ready.
        assert_eq!(service.status().state, ContentState::Idle);
        assert!(service.index().is_none());

        // Paused by the user before it started: it starts paused.
        service.pause();
        service.base_ready(&view_of(data.path()));
        assert!(app.path().join(CONTENT_DIR).is_dir());
        let status = service.status();
        assert_eq!(status.state, ContentState::Paused, "{status:?}");
        assert_eq!(status.paused_by, vec![PauseReason::User]);
        assert!(!service.wait_idle(Duration::from_millis(100)));

        service.resume();
        assert!(service.wait_idle(LONG));
        let status = service.status();
        assert_eq!(status.state, ContentState::Idle, "{status:?}");
        assert_eq!(status.indexed, 2);
        assert_eq!(status.processed, status.total);
        assert!(status.processed >= 2);
        assert_eq!(status.failed, 0);
        assert!(!seen.lock().unwrap().is_empty());

        service.shutdown();
        assert_eq!(service.status().state, ContentState::Stopped);
        // A base after shutdown does not restart anything.
        service.base_ready(&view_of(data.path()));
        assert!(service.scheduler().is_none());
    }

    #[test]
    fn the_enabled_setting_is_persisted_and_disabling_stops_indexing() {
        let data = tmp();
        fs::write(data.path().join("a.txt"), "ميزانية").unwrap();
        let app = tmp();
        let view = view_of(data.path());
        let service = ContentService::new(test_options());
        service.configure(app.path());
        assert!(service.enabled());
        service.base_ready(&view);
        assert!(service.scheduler().is_some());

        service.set_enabled(false, Some(Arc::clone(&view)));
        assert!(service.scheduler().is_none());
        assert_eq!(service.status().state, ContentState::Disabled);
        let saved = fs::read_to_string(app.path().join(CONTENT_SETTING_FILE)).unwrap();
        assert!(saved.contains("false"), "{saved}");
        // Disabled: a new base starts nothing.
        service.base_ready(&view);
        assert!(service.scheduler().is_none());
        service.shutdown();
        drop(service);

        // The next start reads the setting.
        let again = ContentService::new(test_options());
        again.configure(app.path());
        assert!(!again.enabled());
        assert_eq!(again.status().state, ContentState::Disabled);
        again.set_enabled(true, Some(Arc::clone(&view)));
        assert!(again.wait_idle(LONG));
        assert_eq!(again.status().state, ContentState::Idle);
        assert_eq!(again.status().indexed, 1);
        again.shutdown();

        // A corrupt or missing file means enabled.
        fs::write(app.path().join(CONTENT_SETTING_FILE), "{oops").unwrap();
        assert!(read_enabled(&app.path().join(CONTENT_SETTING_FILE)));
        assert!(read_enabled(&app.path().join("missing.json")));
    }

    #[test]
    fn frecency_is_saved_and_loaded() {
        let app = tmp();
        let service = ContentService::new(test_options());
        service.configure(app.path());
        service.record_open("/data/a.txt");
        service.shutdown();
        let text = fs::read_to_string(app.path().join(FRECENCY_FILE)).unwrap();
        // Stored by path key (`\` and lower case on Windows).
        assert!(text.contains("a.txt"), "{text}");

        let again = ContentService::new(test_options());
        again.configure(app.path());
        let score = again
            .frecency
            .read()
            .unwrap()
            .frecency("/data/a.txt", now_secs());
        assert!(score > 0.9, "{score}");
    }

    #[test]
    fn the_path_lookup_finds_content_candidates_only() {
        let data = tmp();
        fs::create_dir(data.path().join("sub")).unwrap();
        fs::write(data.path().join("sub").join("Report.TXT"), "x").unwrap();
        fs::write(data.path().join("photo.png"), "x").unwrap();
        let view = view_of(data.path());
        let lookup = PathLookup::build(view.snapshot().base());
        assert_eq!(lookup.ids.len(), 1);
        let path = data.path().join("sub").join("Report.TXT");
        let found = lookup.candidates(key_hash(&path_key(&path)));
        assert_eq!(found.len(), 1);
        assert_eq!(view.path(found[0]), path);
        assert!(lookup.is_of(view.snapshot().base()));
        let png = data.path().join("photo.png");
        assert!(lookup.candidates(key_hash(&path_key(&png))).is_empty());
    }

    #[test]
    fn files_deleted_while_closed_are_pruned_at_start() {
        let data = tmp();
        fs::write(data.path().join("a.txt"), "ميزانية").unwrap();
        fs::write(data.path().join("b.txt"), "ميزانية").unwrap();
        let app = tmp();
        let service = ContentService::new(test_options());
        service.configure(app.path());
        service.base_ready(&view_of(data.path()));
        assert!(service.wait_idle(LONG));
        assert_eq!(service.status().indexed, 2);
        service.shutdown();
        drop(service);

        // "Closed": b.txt goes away, then the next start sees a name index without it.
        fs::remove_file(data.path().join("b.txt")).unwrap();
        let again = ContentService::new(test_options());
        again.configure(app.path());
        again.base_ready(&view_of(data.path()));
        assert!(again.wait_idle(LONG));
        assert_eq!(again.status().indexed, 1);
        again.shutdown();
    }

    #[test]
    fn snippets_convert_to_utf16_ranges() {
        let fragment = "😀 الميزانية".to_owned();
        let start = "😀 ".len();
        let s = ui_snippet(Snippet {
            fragment: fragment.clone(),
            offset: 10,
            highlights: vec![0..start - 1, start..fragment.len()],
        });
        let units: Vec<u16> = s.text.encode_utf16().collect();
        assert_eq!(s.highlights[0], (0, 2), "the emoji is a surrogate pair");
        let (a, b) = s.highlights[1];
        assert_eq!(String::from_utf16(&units[a..b]).unwrap(), "الميزانية");
        assert_eq!(a, 3);
    }
}
