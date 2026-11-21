//! The content-indexing scheduler: queue, worker pool, single committing writer, control
//! thread (probe + progress).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use athrix_content_index::{ContentIndex, FileMeta};
use athrix_extract::Kind;
use athrix_fs_index::{Change, IndexSnapshot, WatchBatch};

use crate::candidate::{Candidate, CandidateFilter, default_priority_folders, path_key};
use crate::extractor::{ExtractFailure, Extractor, InProcessExtractor};
use crate::probe::{DefaultProbe, SystemProbe};
use crate::progress::{FailureCounts, FailureKind, PauseReasons, Progress, RunState};
use crate::queue::{Job, JobQueue, Prioritizer, Source};

/// Called with a fresh [`Progress`] whenever it changes (at most every
/// [`SchedulerConfig::progress_interval`]) and once more after shutdown. Runs on the
/// scheduler's control thread: keep it short (forward to a channel or a UI event).
pub type ProgressCallback = Arc<dyn Fn(&Progress) + Send + Sync>;

/// When to step aside for the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Politeness {
    /// Pause while on battery (default true).
    pub pause_on_battery: bool,
    /// Pause while other programs keep the CPU busy (default true).
    pub pause_when_busy: bool,
    /// CPU load of other programs (0..1) that counts as busy (default 0.70).
    pub busy_above: f32,
    /// Load under which a busy pause ends (default 0.45). Lower than `busy_above` so the
    /// state does not flap.
    pub resume_below: f32,
    /// Consecutive busy (or calm) samples needed to pause (or resume) (default 3).
    pub busy_samples: u32,
    /// How often the [`SystemProbe`] is sampled (default 2 s).
    pub probe_interval: Duration,
    /// After this many I/O or index errors in a row, workers back off (default 8).
    pub backoff_after: u32,
    /// Longest back-off sleep between jobs (default 30 s). The sleep starts at 100 ms and
    /// doubles with each further error; one success resets it.
    pub max_backoff: Duration,
}

impl Default for Politeness {
    fn default() -> Self {
        Politeness {
            pause_on_battery: true,
            pause_when_busy: true,
            busy_above: 0.70,
            resume_below: 0.45,
            busy_samples: 3,
            probe_interval: Duration::from_secs(2),
            backoff_after: 8,
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Settings of a [`ContentScheduler`].
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerConfig {
    /// Extraction threads (default `max(1, cores / 4)`).
    pub workers: usize,
    /// Commit after this many index changes (default 1,000).
    pub commit_every: u64,
    /// ... or this long after the first uncommitted change (default 10 s). The writer also
    /// commits when the queue runs dry, when paused, and on shutdown.
    pub commit_interval: Duration,
    /// Which files are candidates at all.
    pub filter: CandidateFilter,
    /// Files below these folders go first (default: Desktop, Documents, Downloads in the
    /// home folder).
    pub priority_folders: Vec<PathBuf>,
    /// Auto-pause and back-off.
    pub politeness: Politeness,
    /// How often progress is reported (default 500 ms).
    pub progress_interval: Duration,
    /// Most failed files remembered (default 20,000). Beyond it, new failures are counted
    /// but not remembered, so they are retried on the next scan.
    pub max_remembered_failures: usize,
    /// Run workers and the writer at background priority (default true). On Windows this is
    /// `THREAD_MODE_BACKGROUND_BEGIN`, which also lowers I/O and memory priority.
    pub low_priority: bool,
    /// Start paused, as if [`ContentScheduler::pause`] had been called (default false).
    pub start_paused: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        SchedulerConfig {
            workers: (cores / 4).max(1),
            commit_every: 1_000,
            commit_interval: Duration::from_secs(10),
            filter: CandidateFilter::default(),
            priority_folders: default_priority_folders(),
            politeness: Politeness::default(),
            progress_interval: Duration::from_millis(500),
            max_remembered_failures: 20_000,
            low_priority: true,
            start_paused: false,
        }
    }
}

/// A file that failed, remembered so that the same version is not retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedFile {
    /// The path, as stored in the content index.
    pub path: String,
    /// The version that failed.
    pub meta: FileMeta,
    /// Coarse reason.
    pub kind: FailureKind,
    /// Detail (truncated to 200 bytes).
    pub message: String,
}

const MAX_MESSAGE: usize = 200;

#[derive(Debug, Default)]
struct Counters {
    indexed: u64,
    up_to_date: u64,
    known_failures: u64,
    deleted: u64,
    failed: FailureCounts,
    bytes: u64,
    commits: u64,
}

struct State {
    queue: JobQueue,
    /// Keys taken by a worker and not yet handed to the writer. A key in here is not popped
    /// again, so two versions of one file never race to the writer.
    in_flight: HashSet<String>,
    /// Jobs queued + being extracted + waiting in or being applied by the writer.
    outstanding: u64,
    /// Index changes applied but not committed.
    uncommitted: u64,
    pause: PauseReasons,
    stopping: bool,
    /// Paths of every candidate seen this session: lets a deleted or renamed folder remove
    /// the documents below it (the content index has no prefix delete).
    known: BTreeSet<String>,
    failures: HashMap<String, (FileMeta, FailureKind, String)>,
    counters: Counters,
    error_streak: u32,
    docs_per_sec: f64,
}

impl State {
    fn is_idle(&self) -> bool {
        self.outstanding == 0 && self.uncommitted == 0
    }

    fn run_state(&self) -> RunState {
        if self.stopping {
            RunState::Stopped
        } else if self.is_idle() {
            RunState::Idle
        } else if self.pause.any() {
            RunState::Paused(self.pause)
        } else {
            RunState::Running
        }
    }

    fn progress(&self) -> Progress {
        let queued = self.queue.len() as u64;
        let c = &self.counters;
        let state = self.run_state();
        Progress {
            state,
            queued,
            in_flight: self.outstanding.saturating_sub(queued),
            indexed: c.indexed,
            up_to_date: c.up_to_date,
            known_failures: c.known_failures,
            deleted: c.deleted,
            failed: c.failed,
            bytes: c.bytes,
            docs_per_sec: if state == RunState::Running {
                self.docs_per_sec
            } else {
                0.0
            },
            commits: c.commits,
        }
    }

    /// Removes `key` and every known path below it from `known`; returns them all.
    fn take_known_subtree(&mut self, key: &str) -> Vec<String> {
        let mut out = vec![key.to_owned()];
        self.known.remove(key);
        self.failures.remove(key);
        let prefix = format!("{key}{}", std::path::MAIN_SEPARATOR);
        let below: Vec<String> = self
            .known
            .range(prefix.clone()..)
            .take_while(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in below {
            self.known.remove(&k);
            self.failures.remove(&k);
            out.push(k);
        }
        out
    }

    fn has_known_below(&self, key: &str) -> bool {
        let prefix = format!("{key}{}", std::path::MAIN_SEPARATOR);
        self.known
            .range(prefix.clone()..)
            .next()
            .is_some_and(|k| k.starts_with(&prefix))
    }
}

enum WriterMsg {
    Add {
        path: PathBuf,
        key: String,
        meta: FileMeta,
        text: String,
    },
    Delete(Vec<String>),
}

struct Shared {
    index: Arc<ContentIndex>,
    extractor: Arc<dyn Extractor>,
    config: SchedulerConfig,
    prioritizer: Prioritizer,
    st: Mutex<State>,
    /// Workers wait here for jobs, resume and stop.
    work_cv: Condvar,
    /// `wait_idle` waits here.
    idle_cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every critical section leaves the state consistent, so a panic elsewhere does not
        // make it unusable.
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push_jobs(&self, jobs: Vec<(String, Job)>, source: Source) -> usize {
        if jobs.is_empty() {
            return 0;
        }
        let now = unix_now();
        let prios: Vec<_> = jobs
            .iter()
            .map(|(_, j)| self.prioritizer.priority(j, source, now))
            .collect();
        let mut st = self.lock();
        let n = jobs.len();
        for ((key, job), prio) in jobs.into_iter().zip(prios) {
            if matches!(job, Job::Index(_)) {
                st.known.insert(key.clone());
            }
            if st.queue.push(&key, job, prio) {
                st.outstanding += 1;
            }
        }
        drop(st);
        self.work_cv.notify_all();
        n
    }

    /// One job fully handled.
    fn done(&self, st: &mut State) {
        st.outstanding = st.outstanding.saturating_sub(1);
        if st.is_idle() {
            self.idle_cv.notify_all();
        }
    }

    fn finish(&self, key: &str, also_done: bool) {
        let mut st = self.lock();
        st.in_flight.remove(key);
        if also_done {
            self.done(&mut st);
        }
        drop(st);
        // A job held back because its key was in flight may be ready now.
        self.work_cv.notify_all();
    }

    fn candidate(&self, path: &Path, size: u64, mtime: i64) -> Option<Candidate> {
        self.config
            .filter
            .candidate(path, size, mtime, |k| self.extractor.supports(k))
    }
}

/// Builds a [`ContentScheduler`] with a custom extractor, probe or progress callback.
pub struct SchedulerBuilder {
    index: Arc<ContentIndex>,
    config: SchedulerConfig,
    extractor: Option<Arc<dyn Extractor>>,
    probe: Option<Box<dyn SystemProbe>>,
    on_progress: Option<ProgressCallback>,
}

impl std::fmt::Debug for SchedulerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SchedulerBuilder {
    /// Replaces the configuration.
    pub fn config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    /// Uses `extractor` instead of [`InProcessExtractor`].
    pub fn extractor(mut self, extractor: Arc<dyn Extractor>) -> Self {
        self.extractor = Some(extractor);
        self
    }

    /// Uses `probe` instead of [`DefaultProbe`].
    pub fn probe(mut self, probe: impl SystemProbe + 'static) -> Self {
        self.probe = Some(Box::new(probe));
        self
    }

    /// Reports progress to `callback`.
    pub fn on_progress(mut self, callback: impl Fn(&Progress) + Send + Sync + 'static) -> Self {
        self.on_progress = Some(Arc::new(callback));
        self
    }

    /// Starts the threads. The scheduler is idle until files are queued.
    ///
    /// # Errors
    /// If a thread cannot be spawned.
    pub fn start(self) -> io::Result<ContentScheduler> {
        ContentScheduler::spawn(self)
    }
}

/// Keeps the content index in step with the files: a priority queue of files to extract, a
/// pool of extraction workers, one committing writer, auto-pause and progress reporting.
///
/// See the [crate docs](crate) for the design and the intended app wiring.
pub struct ContentScheduler {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    control: Option<JoinHandle<()>>,
    writer_tx: Option<SyncSender<WriterMsg>>,
    control_tx: Option<Sender<()>>,
}

impl std::fmt::Debug for ContentScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentScheduler")
            .field("workers", &self.workers.len())
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

impl ContentScheduler {
    /// A builder with the default configuration, extractor and probe.
    pub fn builder(index: Arc<ContentIndex>) -> SchedulerBuilder {
        SchedulerBuilder {
            index,
            config: SchedulerConfig::default(),
            extractor: None,
            probe: None,
            on_progress: None,
        }
    }

    /// Starts a scheduler with `config`, the in-process extractor and the default probe.
    ///
    /// # Errors
    /// If a thread cannot be spawned.
    pub fn start(index: Arc<ContentIndex>, config: SchedulerConfig) -> io::Result<Self> {
        Self::builder(index).config(config).start()
    }

    fn spawn(b: SchedulerBuilder) -> io::Result<Self> {
        let workers = b.config.workers.max(1);
        let state = State {
            queue: JobQueue::default(),
            in_flight: HashSet::new(),
            outstanding: 0,
            uncommitted: 0,
            pause: PauseReasons {
                user: b.config.start_paused,
                ..PauseReasons::default()
            },
            stopping: false,
            known: BTreeSet::new(),
            failures: HashMap::new(),
            counters: Counters::default(),
            error_streak: 0,
            docs_per_sec: 0.0,
        };
        let shared = Arc::new(Shared {
            index: b.index,
            extractor: b
                .extractor
                .unwrap_or_else(|| Arc::new(InProcessExtractor::default())),
            prioritizer: Prioritizer::new(b.config.priority_folders.clone()),
            config: b.config,
            st: Mutex::new(state),
            work_cv: Condvar::new(),
            idle_cv: Condvar::new(),
        });
        let (writer_tx, writer_rx) = mpsc::sync_channel(workers * 4);
        let (control_tx, control_rx) = mpsc::channel();
        let mut me = ContentScheduler {
            shared: Arc::clone(&shared),
            workers: Vec::new(),
            writer: None,
            control: None,
            writer_tx: Some(writer_tx.clone()),
            control_tx: Some(control_tx),
        };
        // On a spawn error `me` is dropped, which shuts down whatever already started.
        for i in 0..workers {
            let sh = Arc::clone(&shared);
            let tx = writer_tx.clone();
            me.workers.push(
                std::thread::Builder::new()
                    .name(format!("athrix-content-{i}"))
                    .spawn(move || worker_loop(&sh, &tx))?,
            );
        }
        drop(writer_tx);
        let sh = Arc::clone(&shared);
        me.writer = Some(
            std::thread::Builder::new()
                .name("athrix-content-writer".into())
                .spawn(move || writer_loop(&sh, &writer_rx))?,
        );
        let sh = Arc::clone(&shared);
        let probe = b.probe.unwrap_or_else(|| Box::new(DefaultProbe::new()));
        let on_progress = b.on_progress;
        me.control = Some(
            std::thread::Builder::new()
                .name("athrix-content-control".into())
                .spawn(move || control_loop(&sh, probe, on_progress.as_deref(), &control_rx))?,
        );
        Ok(me)
    }

    /// Queues every candidate file of a name-index snapshot (the initial backlog). Files the
    /// content index already holds in this version are skipped cheaply by the workers.
    /// Returns the number of candidates queued.
    ///
    /// Call it after startup, after a [`WatchBatch::RescanNeeded`] re-walk, and whenever
    /// [`ContentIndex::rebuilt`] is `Some` (then every file is re-extracted).
    pub fn enqueue_snapshot(&self, snapshot: &IndexSnapshot) -> usize {
        let max = self.shared.config.filter.max_size;
        let mut total = 0;
        let mut batch = Vec::with_capacity(4096);
        for id in 0..snapshot.id_bound() {
            let Ok(id) = u32::try_from(id) else { break };
            if !snapshot.contains(id) {
                continue;
            }
            let m = snapshot.meta(id);
            if m.is_dir || m.is_symlink || m.size > max {
                continue;
            }
            // Cheap reject by name before building the full path.
            if Kind::from_path(Path::new(snapshot.name(id))).is_none() {
                continue;
            }
            let path = snapshot.path(id);
            if let Some(c) = self
                .shared
                .candidate(&path, m.size, m.modified.unwrap_or(0))
            {
                batch.push((c.key(), Job::Index(c)));
            }
            if batch.len() == 4096 {
                total += self
                    .shared
                    .push_jobs(std::mem::take(&mut batch), Source::Backlog);
            }
        }
        total + self.shared.push_jobs(batch, Source::Backlog)
    }

    /// Queues files with known metadata (`path`, size, mtime in Unix seconds) as backlog,
    /// after filtering. Returns the number queued. Useful for sources other than a snapshot.
    pub fn enqueue_files<I>(&self, files: I) -> usize
    where
        I: IntoIterator<Item = (PathBuf, u64, i64)>,
    {
        let jobs = files
            .into_iter()
            .filter_map(|(p, size, mtime)| self.shared.candidate(&p, size, mtime))
            .map(|c| (c.key(), Job::Index(c)))
            .collect();
        self.shared.push_jobs(jobs, Source::Backlog)
    }

    /// Queues paths reported changed by the watcher, ahead of the backlog. Each is re-read
    /// from disk by a worker: a file is (re-)extracted, a missing path is deleted from the
    /// index together with every known file below it, and a new folder (one with no known
    /// files below it, such as a folder renamed or copied in) is listed and its files queued.
    pub fn paths_changed<P: AsRef<Path>>(&self, paths: &[P]) {
        let jobs = paths
            .iter()
            .map(|p| {
                let p = p.as_ref().to_path_buf();
                (path_key(&p), Job::Check(p))
            })
            .collect();
        self.shared.push_jobs(jobs, Source::Change);
    }

    /// Queues changes with known metadata (for example from the NTFS USN journal), ahead of
    /// the backlog. Folder upserts are re-read from disk like [`paths_changed`](Self::paths_changed).
    pub fn apply_changes(&self, changes: &[Change]) {
        let mut jobs = Vec::with_capacity(changes.len());
        for ch in changes {
            match ch {
                Change::Upsert { path, meta } if meta.is_dir => {
                    jobs.push((path_key(path), Job::Check(path.clone())));
                }
                Change::Upsert { path, meta } => {
                    let key = path_key(path);
                    match self
                        .shared
                        .candidate(path, meta.size, meta.modified.unwrap_or(0))
                    {
                        Some(c) => jobs.push((key, Job::Index(c))),
                        // No longer a candidate (renamed to ~$x, grew too big): drop any
                        // older version.
                        None => jobs.push((key, Job::Delete(path.clone()))),
                    }
                }
                Change::Remove { path } => jobs.push((path_key(path), Job::Delete(path.clone()))),
            }
        }
        self.shared.push_jobs(jobs, Source::Change);
    }

    /// Feeds one batch of a name-index [`Watcher`](athrix_fs_index::Watcher). Returns false
    /// for [`WatchBatch::RescanNeeded`]: the caller should re-walk and call
    /// [`enqueue_snapshot`](Self::enqueue_snapshot) with the new snapshot.
    pub fn watch_batch(&self, batch: &WatchBatch) -> bool {
        match batch {
            WatchBatch::Changed(paths) => {
                self.paths_changed(paths);
                true
            }
            WatchBatch::RescanNeeded(_) => false,
        }
    }

    /// Pauses: workers finish the file in hand and take no new one; pending changes are
    /// committed.
    pub fn pause(&self) {
        self.shared.lock().pause.user = true;
    }

    /// Ends a [`pause`](Self::pause). Auto-pause (battery, busy) may still hold.
    pub fn resume(&self) {
        self.shared.lock().pause.user = false;
        self.shared.work_cv.notify_all();
    }

    /// Why the scheduler is paused, if it is.
    pub fn pause_reasons(&self) -> PauseReasons {
        self.shared.lock().pause
    }

    /// The current counters.
    pub fn progress(&self) -> Progress {
        self.shared.lock().progress()
    }

    /// The files that failed and will not be retried until they change.
    pub fn failures(&self) -> Vec<FailedFile> {
        let st = self.shared.lock();
        let mut v: Vec<FailedFile> = st
            .failures
            .iter()
            .map(|(path, (meta, kind, message))| FailedFile {
                path: path.clone(),
                meta: *meta,
                kind: *kind,
                message: message.clone(),
            })
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    /// Waits until nothing is queued or in flight and everything is committed. Returns false
    /// on timeout (for example while paused) or after shutdown started.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if st.is_idle() {
                return true;
            }
            if st.stopping {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .shared
                .idle_cv
                .wait_timeout(st, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Stops cleanly: workers finish the file in hand, the writer applies and commits what
    /// was extracted, then all threads end. Queued work is dropped (the next scan finds it
    /// again; already indexed files are skipped). Returns the final counters.
    pub fn shutdown(mut self) -> Progress {
        self.stop();
        self.progress()
    }

    fn stop(&mut self) {
        self.shared.lock().stopping = true;
        self.shared.work_cv.notify_all();
        self.shared.idle_cv.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        // Workers are gone; dropping the last sender lets the writer drain, commit and end.
        self.writer_tx = None;
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        self.control_tx = None;
        if let Some(c) = self.control.take() {
            let _ = c.join();
        }
    }
}

impl Drop for ContentScheduler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    // Same conversion as the name index's walk, so keys match.
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn lower_thread_priority() {
    use thread_priority::{ThreadPriority, set_current_thread_priority};
    #[cfg(windows)]
    let p = ThreadPriority::Os(thread_priority::WinAPIThreadPriority::BackgroundModeBegin.into());
    #[cfg(not(windows))]
    let p = ThreadPriority::Min;
    if let Err(e) = set_current_thread_priority(p) {
        tracing::debug!(error = ?e, "cannot lower thread priority");
    }
}

fn backoff_delay(streak: u32, p: &Politeness) -> Option<Duration> {
    if streak < p.backoff_after.max(1) {
        return None;
    }
    let exp = (streak - p.backoff_after.max(1)).min(16);
    Some((Duration::from_millis(100) * (1u32 << exp)).min(p.max_backoff))
}

fn worker_loop(sh: &Shared, tx: &SyncSender<WriterMsg>) {
    if sh.config.low_priority {
        lower_thread_priority();
    }
    loop {
        let (key, job) = {
            let mut st = sh.lock();
            loop {
                if st.stopping {
                    return;
                }
                if !st.pause.any() {
                    let State {
                        queue, in_flight, ..
                    } = &mut *st;
                    if let Some((key, job)) = queue.pop_where(|k| in_flight.contains(k)) {
                        in_flight.insert(key.clone());
                        break (key, job);
                    }
                }
                st = sh.work_cv.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        run_job(sh, tx, key, job);

        let delay = backoff_delay(sh.lock().error_streak, &sh.config.politeness);
        if let Some(delay) = delay {
            let st = sh.lock();
            let _ = sh
                .work_cv
                .wait_timeout_while(st, delay, |s| !s.stopping)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn run_job(sh: &Shared, tx: &SyncSender<WriterMsg>, key: String, job: Job) {
    match job {
        Job::Index(c) => index_file(sh, tx, key, c),
        Job::Delete(_) => delete_subtree(sh, tx, key),
        Job::Check(path) => match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => delete_subtree(sh, tx, key),
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "cannot stat changed path");
                sh.finish(&key, true);
            }
            Ok(md) if md.is_file() => match sh.candidate(&path, md.len(), mtime_secs(&md)) {
                Some(c) => {
                    sh.lock().known.insert(key.clone());
                    index_file(sh, tx, key, c);
                }
                None => {
                    let was_known = sh.lock().known.contains(&key);
                    if was_known {
                        delete_subtree(sh, tx, key);
                    } else {
                        sh.finish(&key, true);
                    }
                }
            },
            Ok(md) if md.is_dir() => {
                expand_dir(sh, &key, &path);
                sh.finish(&key, true);
            }
            Ok(_) => sh.finish(&key, true), // symlink or special file: never followed
        },
    }
}

/// A folder appeared (created, renamed or copied in): queue its entries. A folder with known
/// files below it existed before, and its children report their own changes.
fn expand_dir(sh: &Shared, key: &str, dir: &Path) {
    if sh.config.filter.is_excluded_dir(dir) || sh.lock().has_known_below(key) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let jobs = entries
        .flatten()
        .map(|e| {
            let p = e.path();
            (path_key(&p), Job::Check(p))
        })
        .collect();
    sh.push_jobs(jobs, Source::Change);
}

fn delete_subtree(sh: &Shared, tx: &SyncSender<WriterMsg>, key: String) {
    let keys = sh.lock().take_known_subtree(&key);
    // Sent before the key leaves `in_flight`, so it stays ordered with any later version.
    let sent = tx.send(WriterMsg::Delete(keys)).is_ok();
    sh.finish(&key, !sent);
}

fn index_file(sh: &Shared, tx: &SyncSender<WriterMsg>, key: String, c: Candidate) {
    let meta = c.meta();
    let known_failure = {
        let mut st = sh.lock();
        let hit = st.failures.get(&key).is_some_and(|(m, _, _)| *m == meta);
        if hit {
            st.counters.known_failures += 1;
        }
        hit
    };
    if known_failure {
        sh.finish(&key, true);
        return;
    }
    match sh.index.is_current(&key, meta) {
        Ok(true) => {
            sh.lock().counters.up_to_date += 1;
            sh.finish(&key, true);
            return;
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(path = %key, error = %e, "freshness check failed"),
    }
    match sh.extractor.extract(&c.path, c.kind) {
        Ok(text) => {
            let msg = WriterMsg::Add {
                path: c.path,
                key: key.clone(),
                meta,
                text,
            };
            let sent = tx.send(msg).is_ok();
            sh.finish(&key, !sent);
        }
        Err(f) => {
            record_failure(sh, &key, meta, &f);
            sh.finish(&key, true);
        }
    }
}

fn record_failure(sh: &Shared, key: &str, meta: FileMeta, f: &ExtractFailure) {
    tracing::debug!(path = %key, error = %f, "extraction failed");
    let mut st = sh.lock();
    st.counters.failed.add(f.kind);
    if f.kind == FailureKind::Io {
        // Locked or vanished files are often transient: retry them on the next scan, and
        // count toward back-off (a disconnected drive fails every file).
        st.error_streak += 1;
        return;
    }
    if f.kind == FailureKind::Index {
        st.error_streak += 1;
        return;
    }
    st.error_streak = 0;
    if st.failures.len() < sh.config.max_remembered_failures || st.failures.contains_key(key) {
        let mut message = f.message.clone();
        if message.len() > MAX_MESSAGE {
            let mut cut = MAX_MESSAGE;
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
        }
        st.failures.insert(key.to_owned(), (meta, f.kind, message));
    }
}

fn writer_loop(sh: &Shared, rx: &Receiver<WriterMsg>) {
    if sh.config.low_priority {
        lower_thread_priority();
    }
    let mut first_uncommitted: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;
    loop {
        let wait = if first_uncommitted.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };
        let disconnected = match rx.recv_timeout(wait) {
            Ok(msg) => {
                apply(sh, msg);
                first_uncommitted.get_or_insert_with(Instant::now);
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        let due = {
            let st = sh.lock();
            st.uncommitted > 0
                && (disconnected
                    || st.stopping
                    || st.uncommitted >= sh.config.commit_every
                    || st.outstanding == 0
                    || st.pause.any()
                    || first_uncommitted.is_some_and(|t| t.elapsed() >= sh.config.commit_interval))
        };
        let may_retry = retry_at.is_none_or(|t| Instant::now() >= t) || disconnected;
        if due && may_retry {
            match sh.index.commit() {
                Ok(()) => {
                    let mut st = sh.lock();
                    st.uncommitted = 0;
                    st.counters.commits += 1;
                    if st.is_idle() {
                        sh.idle_cv.notify_all();
                    }
                    first_uncommitted = None;
                    retry_at = None;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "content index commit failed");
                    let mut st = sh.lock();
                    st.counters.failed.add(FailureKind::Index);
                    st.error_streak += 1;
                    retry_at = Some(Instant::now() + sh.config.commit_interval);
                }
            }
        }
        if disconnected {
            return;
        }
    }
}

fn apply(sh: &Shared, msg: WriterMsg) {
    match msg {
        WriterMsg::Add {
            path,
            key,
            meta,
            text,
        } => {
            // A delete of this file (or its folder) may have been applied while it was being
            // extracted; adding it now would leave a stale document.
            if !path.exists() {
                let mut st = sh.lock();
                sh.done(&mut st);
                return;
            }
            let res = sh.index.add_or_update(&key, meta, &text);
            let mut st = sh.lock();
            match res {
                Ok(_) => {
                    st.counters.indexed += 1;
                    st.counters.bytes += meta.size;
                    st.uncommitted += 1;
                    st.error_streak = 0;
                    st.failures.remove(&key);
                }
                Err(e) => {
                    tracing::warn!(path = %key, error = %e, "content index add failed");
                    st.counters.failed.add(FailureKind::Index);
                    st.error_streak += 1;
                }
            }
            sh.done(&mut st);
        }
        WriterMsg::Delete(keys) => {
            for k in &keys {
                sh.index.delete(k);
            }
            let mut st = sh.lock();
            st.counters.deleted += keys.len() as u64;
            st.uncommitted += 1;
            sh.done(&mut st);
        }
    }
}

/// Hysteresis for the busy signal.
#[derive(Debug, Default)]
struct BusyTracker {
    busy_run: u32,
    calm_run: u32,
}

fn control_loop(
    sh: &Shared,
    mut probe: Box<dyn SystemProbe>,
    on_progress: Option<&(dyn Fn(&Progress) + Send + Sync)>,
    stop: &Receiver<()>,
) {
    let pol = &sh.config.politeness;
    let tick = pol.probe_interval.min(sh.config.progress_interval);
    let mut next_probe = Instant::now();
    let mut next_report = Instant::now();
    let mut busy = BusyTracker::default();
    let mut last: Option<Progress> = None;
    let mut rate_mark = (Instant::now(), 0u64);
    loop {
        let now = Instant::now();
        if now >= next_probe {
            next_probe = now + pol.probe_interval;
            let s = probe.sample();
            let on_battery = pol.pause_on_battery && s.on_battery;
            if pol.pause_when_busy && s.cpu_load >= pol.busy_above {
                busy.busy_run += 1;
                busy.calm_run = 0;
            } else if s.cpu_load < pol.resume_below || !pol.pause_when_busy {
                busy.calm_run += 1;
                busy.busy_run = 0;
            }
            let mut st = sh.lock();
            let was = st.pause.any();
            st.pause.battery = on_battery;
            let need = pol.busy_samples.max(1);
            if busy.busy_run >= need {
                st.pause.busy = true;
            } else if busy.calm_run >= need || !pol.pause_when_busy {
                st.pause.busy = false;
            }
            let is = st.pause.any();
            drop(st);
            if was && !is {
                sh.work_cv.notify_all();
            }
        }
        if now >= next_report {
            next_report = now + sh.config.progress_interval;
            let mut st = sh.lock();
            let dt = rate_mark.0.elapsed().as_secs_f64();
            if dt > 0.0 {
                let inst = (st.counters.indexed - rate_mark.1) as f64 / dt;
                st.docs_per_sec = if st.docs_per_sec == 0.0 {
                    inst
                } else {
                    0.6 * st.docs_per_sec + 0.4 * inst
                };
            }
            rate_mark = (Instant::now(), st.counters.indexed);
            let p = st.progress();
            drop(st);
            if let Some(cb) = on_progress {
                if last != Some(p) {
                    cb(&p);
                }
            }
            last = Some(p);
        }
        match stop.recv_timeout(tick) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(cb) = on_progress {
        cb(&sh.lock().progress());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_after_threshold_doubles_and_caps() {
        let p = Politeness {
            backoff_after: 3,
            max_backoff: Duration::from_secs(1),
            ..Politeness::default()
        };
        assert_eq!(backoff_delay(2, &p), None);
        assert_eq!(backoff_delay(3, &p), Some(Duration::from_millis(100)));
        assert_eq!(backoff_delay(4, &p), Some(Duration::from_millis(200)));
        assert_eq!(backoff_delay(40, &p), Some(Duration::from_secs(1)));
    }
}
