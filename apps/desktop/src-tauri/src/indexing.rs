//! Background indexing: owns the current [`FileIndex`] and the [`IndexStatus`], and runs walks
//! on a dedicated thread (never the UI thread).
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
//! **Generations.** Every index that becomes current gets a new generation (see
//! [`LiveIndex`]); result ids carry it so ids from an older index are rejected.

use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::Instant;

use athrix_fs_index::{EntryId, FileIndex, WalkOptions, walk};

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

/// An index that became current (from a walk or a snapshot), tagged with its generation.
///
/// Every index that becomes current gets a new generation, so result ids from an older index
/// can be told apart and rejected instead of resolving to another file. Derefs to the
/// [`FileIndex`].
#[derive(Debug)]
pub struct LiveIndex {
    generation: u32,
    index: FileIndex,
    roots: Vec<IndexRoot>,
}

/// One walked root of an index and the block of entry ids it covers.
///
/// The walker (and a snapshot of it) stores each root followed by its whole subtree before
/// the next root, so a root's entries are the ids `start..end`, from the root entry up to the
/// next root entry. Search scopes rely on this to skip the roots outside the scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRoot {
    /// Id of the root entry (the first of its block).
    pub start: EntryId,
    /// One past the last id of the block.
    pub end: EntryId,
    /// [`crate::roots::path_key`] of the root's path.
    pub key: String,
}

impl LiveIndex {
    /// Wraps `index` as generation `generation` (masked to [`GENERATION_BITS`]).
    pub fn new(generation: u32, index: FileIndex) -> Self {
        let roots = index_roots(&index);
        Self {
            generation: generation & GENERATION_MASK,
            index,
            roots,
        }
    }

    /// The generation, `<= GENERATION_MASK`.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The roots of the index and their id blocks, in id order.
    pub fn roots(&self) -> &[IndexRoot] {
        &self.roots
    }
}

/// Finds the root entries (no parent) of `index` and the id block of each.
fn index_roots(index: &FileIndex) -> Vec<IndexRoot> {
    let len = EntryId::try_from(index.len()).unwrap_or(EntryId::MAX);
    let mut roots: Vec<IndexRoot> = Vec::new();
    for id in 0..len {
        if index.parent(id).is_none() {
            if let Some(last) = roots.last_mut() {
                last.end = id;
            }
            roots.push(IndexRoot {
                start: id,
                end: len,
                key: path_key(Path::new(index.name(id))),
            });
        }
    }
    roots
}

impl Deref for LiveIndex {
    type Target = FileIndex;

    fn deref(&self) -> &FileIndex {
        &self.index
    }
}

#[derive(Debug, Default)]
struct Progress {
    status: IndexStatus,
    /// Roots requested while the indexing thread was busy.
    pending: Option<Vec<PathBuf>>,
    /// The indexing thread is alive (loading a snapshot, walking or saving). Requests made
    /// meanwhile are queued in `pending`.
    running: bool,
}

/// The index and its status, shared between commands and the indexing thread.
#[derive(Debug, Default)]
pub struct Indexer {
    index: RwLock<Option<Arc<LiveIndex>>>,
    progress: Mutex<Progress>,
    /// Last generation handed out (before masking).
    generation: AtomicU32,
    /// Where snapshots are loaded from and saved to; unset means no snapshots.
    snapshots: OnceLock<SnapshotStore>,
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

    /// The index searches should use, if a walk finished or a snapshot was loaded.
    pub fn index(&self) -> Option<Arc<LiveIndex>> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
    fn run(&self, mut roots: Vec<PathBuf>, listener: &StatusListener, use_snapshot: bool) {
        if use_snapshot {
            self.load_snapshot(&mut roots, listener);
        }
        loop {
            let started = Instant::now();
            let opts = walk_options(&roots);
            let result = catch_unwind(AssertUnwindSafe(|| walk(&opts)));
            let elapsed_ms = millis_since(started);

            let (finished, walked) = {
                let mut p = self.lock();
                let walked = match result {
                    Ok(Ok(index)) => {
                        let entries = index.len() as u64;
                        let live = self.install(index);
                        tracing::info!(
                            entries,
                            elapsed_ms,
                            generation = live.generation(),
                            "indexing finished"
                        );
                        p.status.state = IndexState::Ready;
                        p.status.entries = entries;
                        p.status.elapsed_ms = Some(elapsed_ms);
                        p.status.error = None;
                        Some(live)
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "indexing failed");
                        p.status.state = IndexState::Error;
                        p.status.error = Some(e.to_string());
                        None
                    }
                    Err(_) => {
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
            if let Some(live) = walked {
                self.save_snapshot(&live, &roots);
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
    fn load_snapshot(&self, roots: &mut Vec<PathBuf>, listener: &StatusListener) {
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
        let entries = index.len() as u64;
        let live = self.install(index);
        tracing::info!(
            entries,
            elapsed_ms = millis_since(started),
            generation = live.generation(),
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

    /// Saves `index` as the snapshot of `roots`, if a store is set. Failures are only logged.
    fn save_snapshot(&self, index: &FileIndex, roots: &[PathBuf]) {
        let Some(store) = self.snapshots.get() else {
            return;
        };
        let started = Instant::now();
        match catch_unwind(AssertUnwindSafe(|| store.save(index, roots))) {
            Ok(Ok(())) => tracing::info!(
                elapsed_ms = millis_since(started),
                path = %store.snapshot_path().display(),
                "index snapshot saved"
            ),
            Ok(Err(e)) => tracing::warn!(error = %e, "cannot save the index snapshot"),
            Err(_) => tracing::error!("saving the index snapshot panicked"),
        }
    }

    /// Makes `index` current under a new generation and returns it.
    fn install(&self, index: FileIndex) -> Arc<LiveIndex> {
        let generation = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let live = Arc::new(LiveIndex::new(generation, index));
        *self.index.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&live));
        live
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

    fn with_store(dir: &tempfile::TempDir) -> Indexer {
        let indexer = Indexer::new();
        indexer.set_snapshot_store(SnapshotStore::new(dir.path().join("index")));
        indexer
    }

    /// Runs the indexing thread's work on this thread, as `launch` would, and returns the
    /// events emitted after the initial `indexing` one.
    fn run_here(indexer: &Indexer, roots: &[PathBuf], use_snapshot: bool) -> Vec<IndexStatus> {
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
        let live = LiveIndex::new(1, walk(&walk_options(&roots)).unwrap());
        let blocks = live.roots();
        assert_eq!(blocks.len(), 2);
        assert_eq!((blocks[0].start, blocks[0].end), (0, 5));
        assert_eq!((blocks[1].start, blocks[1].end), (5, 10));
        assert_eq!(blocks[1].key, path_key(b.path()));
        for block in blocks {
            for id in block.start..block.end {
                let mut top = id;
                while let Some(parent) = live.parent(top) {
                    top = parent;
                }
                assert_eq!(top, block.start, "entry {id} belongs to its block's root");
            }
        }
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
        let store = SnapshotStore::new(snaps.path().join("index"));
        assert!(store.snapshot_path().is_file());
        assert_eq!(store.load(&roots).unwrap().len(), 5);

        // The disk changes; the next start-up shows the snapshot, then the fresh walk.
        fs::write(data.path().join("docs/جديد.txt"), b"x").unwrap();
        let second = Arc::new(with_store(&snaps));
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
        let live = second.index().unwrap();
        assert_eq!(live.len(), 6);
        assert_eq!(live.generation(), 2, "snapshot = generation 1, walk = 2");
        // The walk's result is saved for the next start-up (wait for the thread to finish).
        for _ in 0..300 {
            if !second.lock().running {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!second.lock().running);
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
        let indexer = Indexer::new();
        let events = run_here(&indexer, &[data.path().to_path_buf()], true);
        assert_eq!(states(&events), [IndexState::Ready]);
    }

    #[test]
    fn generations_increase_and_wrap_within_21_bits() {
        let indexer = Indexer::new();
        assert_eq!(indexer.install(FileIndex::default()).generation(), 1);
        assert_eq!(indexer.install(FileIndex::default()).generation(), 2);
        indexer
            .generation
            .store(GENERATION_MASK - 1, Ordering::Relaxed);
        assert_eq!(
            indexer.install(FileIndex::default()).generation(),
            GENERATION_MASK
        );
        assert_eq!(indexer.install(FileIndex::default()).generation(), 0);
        assert_eq!(indexer.index().unwrap().generation(), 0);
        indexer.generation.store(u32::MAX, Ordering::Relaxed);
        assert_eq!(indexer.install(FileIndex::default()).generation(), 0);
    }
}
