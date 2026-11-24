//! Parallel directory walk with bounded memory that fills an [`IndexBuilder`].
//!
//! This is the cross-platform path; on Windows, [`crate::ntfs`] fills the same builder from an
//! NTFS volume enumeration with the same exclusion rules.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::UNIX_EPOCH;

use athrix_core::{Error, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::index::{EntryId, EntryMeta, FileIndex, IndexBuilder};

/// What to walk and what to skip.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    roots: Vec<PathBuf>,
    exclude_globs: Vec<String>,
    exclude_prefixes: Vec<PathBuf>,
    follow_links: bool,
}

impl WalkOptions {
    /// Walks the given roots, in order.
    pub fn new<I, P>(roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        WalkOptions {
            roots: roots.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    /// Excludes entries matching a glob (excluded folders are not descended into).
    ///
    /// A pattern without `/` or `\` matches the entry's name (`node_modules`, `*.tmp`); a pattern
    /// with one matches the full path (`**/target/debug`). Matching is case-insensitive on
    /// Windows.
    pub fn exclude_glob(mut self, pattern: impl Into<String>) -> Self {
        self.exclude_globs.push(pattern.into());
        self
    }

    /// Excludes a path and everything below it.
    pub fn exclude_prefix(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclude_prefixes.push(path.into());
        self
    }

    /// Follows symbolic links and junctions (off by default; a folder reached twice is listed once).
    pub fn follow_links(mut self, follow: bool) -> Self {
        self.follow_links = follow;
        self
    }

    /// The roots, in order.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether symbolic links and junctions are followed.
    pub fn follows_links(&self) -> bool {
        self.follow_links
    }
}

/// Compiled exclusion rules, shared with the walker threads (and the live watcher).
#[derive(Debug)]
pub(crate) struct Exclusions {
    names: GlobSet,
    paths: GlobSet,
    /// Normalized (see [`prefix_key`]) path prefixes.
    prefixes: Vec<String>,
}

impl Exclusions {
    pub(crate) fn new(opts: &WalkOptions) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in &opts.exclude_globs {
            let glob = GlobBuilder::new(pattern)
                .case_insensitive(cfg!(windows))
                .build()
                .map_err(|e| Error::Config {
                    key: "exclude".into(),
                    message: e.to_string(),
                })?;
            if pattern.contains(['/', '\\']) {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        let build = |b: GlobSetBuilder| {
            b.build().map_err(|e| Error::Config {
                key: "exclude".into(),
                message: e.to_string(),
            })
        };
        Ok(Exclusions {
            names: build(names)?,
            paths: build(paths)?,
            prefixes: opts
                .exclude_prefixes
                .iter()
                .map(|p| prefix_key(p))
                .collect(),
        })
    }

    /// True if some rule looks at the full path (path globs or prefixes), not only the name.
    pub(crate) fn needs_paths(&self) -> bool {
        !self.paths.is_empty() || !self.prefixes.is_empty()
    }

    pub(crate) fn excludes(&self, path: &Path, name: &std::ffi::OsStr) -> bool {
        if !self.names.is_empty() && self.names.is_match(name) {
            return true;
        }
        if !self.paths.is_empty() && self.paths.is_match(path) {
            return true;
        }
        if !self.prefixes.is_empty() {
            let key = prefix_key(path);
            return self.prefixes.iter().any(|p| {
                key.starts_with(p.as_str())
                    && (key.len() == p.len() || key.as_bytes()[p.len()] == b'/')
            });
        }
        false
    }
}

/// Path as a comparable string: `/` separators, no trailing separator, lowercase on Windows.
fn prefix_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

/// Walks every root in `opts` in parallel and builds the name index.
///
/// Entries that cannot be read (permission denied, vanished mid-walk, broken metadata) are
/// skipped or kept without metadata; they never fail the walk. Errors that whole-drive walks
/// hit by the thousand in system folders (access denied, files in use, `os error 1920` on app
/// execution aliases...; see [`WalkStats::expected_errors`]) are logged at `debug` only; other
/// errors at `warn`. One `info` line at the end gives the counts (also returned by
/// [`walk_with_stats`]). A root that cannot be read is skipped the same way. Symlinks and
/// junctions are indexed as entries but not followed unless [`WalkOptions::follow_links`] is
/// set.
///
/// # Errors
/// [`Error::Config`] if an exclusion glob is invalid.
pub fn walk(opts: &WalkOptions) -> Result<FileIndex> {
    walk_with_stats(opts).map(|(index, _)| index)
}

/// [`walk`], also returning what could not be read, for diagnostics.
///
/// # Errors
/// [`Error::Config`] if an exclusion glob is invalid.
pub fn walk_with_stats(opts: &WalkOptions) -> Result<(FileIndex, WalkStats)> {
    let exclusions = Arc::new(Exclusions::new(opts)?);
    let mut builder = IndexBuilder::new();
    let counters = Arc::new(Counters::default());
    for root in &opts.roots {
        walk_counted(
            root,
            opts.follow_links,
            &exclusions,
            &mut builder,
            &counters,
        );
    }
    let index = builder.finish();
    let stats = counters.stats();
    tracing::info!(
        entries = index.len(),
        unlisted_folders = stats.unlisted_folders,
        access_denied = stats.access_denied,
        unreadable_metadata = stats.unreadable_metadata,
        expected_errors = stats.expected_errors,
        "walk finished; {} folder(s) could not be listed ({} access denied), the metadata of {} \
         entries could not be read; {} of these errors are usual in system folders",
        stats.unlisted_folders,
        stats.access_denied,
        stats.unreadable_metadata,
        stats.expected_errors,
    );
    Ok((index, stats))
}

/// What a walk could not read ([`walk_with_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Folders whose children could not be listed (they are indexed, empty).
    pub unlisted_folders: usize,
    /// Of those, the ones refused with "access denied".
    pub access_denied: usize,
    /// Entries kept without size and date because their metadata could not be read.
    pub unreadable_metadata: usize,
    /// How many of the errors (unlisted folders, unreadable metadata, skipped entries) are of
    /// the kind whole-drive walks hit in system folders and log at `debug` only: access
    /// denied, sharing / lock violations (`pagefile.sys`), `os error 1920` (app execution
    /// aliases under `WindowsApps`), untrusted mount points and unreadable reparse points.
    pub expected_errors: usize,
}

/// Error counts of one walk, shared with the walker threads.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    unlisted_folders: AtomicUsize,
    access_denied: AtomicUsize,
    unreadable_metadata: AtomicUsize,
    expected_errors: AtomicUsize,
}

impl Counters {
    fn add(counter: &AtomicUsize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> WalkStats {
        WalkStats {
            unlisted_folders: self.unlisted_folders.load(Ordering::Relaxed),
            access_denied: self.access_denied.load(Ordering::Relaxed),
            unreadable_metadata: self.unreadable_metadata.load(Ordering::Relaxed),
            expected_errors: self.expected_errors.load(Ordering::Relaxed),
        }
    }
}

/// The roots and exclusion rules of a [`WalkOptions`], compiled once and shared by the live
/// watcher ([`crate::watch`]) and the live index ([`crate::live`]) so that a path changed after
/// the walk is judged exactly like the walk judged it.
#[derive(Debug)]
pub(crate) struct Scope {
    /// Each root with its [`path_key`].
    pub(crate) roots: Vec<(PathBuf, String)>,
    exclusions: Arc<Exclusions>,
    pub(crate) follow_links: bool,
}

impl Scope {
    pub(crate) fn new(opts: &WalkOptions) -> Result<Self> {
        Ok(Scope {
            roots: opts
                .roots
                .iter()
                .map(|r| (r.clone(), path_key(r)))
                .collect(),
            exclusions: Arc::new(Exclusions::new(opts)?),
            follow_links: opts.follow_links,
        })
    }

    /// The index of the (innermost) root that contains `path`, or `None` if it is outside every
    /// root. A root contains itself.
    pub(crate) fn root_of(&self, key: &str) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, (_, root))| key_has_prefix(key, root))
            .max_by_key(|(_, (_, root))| root.len())
            .map(|(i, _)| i)
    }

    /// True if `path` (with key `key`) is inside a root and neither it nor any of its ancestors
    /// below that root is excluded. Roots themselves are never excluded, as in [`walk`].
    pub(crate) fn admits(&self, path: &Path, key: &str) -> bool {
        let Some(root) = self.root_of(key) else {
            return false;
        };
        let rest = &key[self.roots[root].1.len()..];
        let depth = rest.bytes().filter(|&b| b == b'/').count();
        path.ancestors().take(depth).all(|a| match a.file_name() {
            Some(name) => !self.exclusions.excludes(a, name),
            None => true,
        })
    }

    /// True if `key` is exactly one of the roots.
    pub(crate) fn is_root(&self, key: &str) -> bool {
        self.roots.iter().any(|(_, r)| r == key)
    }

    /// Walks the subtree at `dir` (which must be in scope) with the same rules as [`walk`]. The
    /// returned index has `dir` as its only root, named by its full path.
    pub(crate) fn walk_subtree(&self, dir: &Path) -> FileIndex {
        let mut builder = IndexBuilder::new();
        let counters = Arc::new(Counters::default());
        walk_counted(
            dir,
            self.follow_links,
            &self.exclusions,
            &mut builder,
            &counters,
        );
        let stats = counters.stats();
        if stats.unlisted_folders > 0 || stats.unreadable_metadata > 0 {
            tracing::debug!(
                dir = %dir.display(),
                unlisted_folders = stats.unlisted_folders,
                unreadable_metadata = stats.unreadable_metadata,
                "subtree walk could not read some entries"
            );
        }
        builder.finish()
    }

    /// Reads the metadata of `path` the way the walker records it.
    pub(crate) fn read_meta(&self, path: &Path) -> std::io::Result<EntryMeta> {
        let md = if self.follow_links {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }?;
        Ok(meta_from(&md))
    }
}

/// Comparable form of a path for the live layer: `/` separators, no trailing separator, and
/// case-folded per character on Windows (the file system there is case-insensitive).
pub(crate) fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push('/'),
            c if cfg!(windows) => out.extend(c.to_lowercase()),
            c => out.push(c),
        }
    }
    while out.ends_with('/') {
        out.pop();
    }
    out
}

/// True if `key` equals `prefix` or lies below it.
pub(crate) fn key_has_prefix(key: &str, prefix: &str) -> bool {
    key.starts_with(prefix) && (key.len() == prefix.len() || key.as_bytes()[prefix.len()] == b'/')
}

fn meta_from(md: &std::fs::Metadata) -> EntryMeta {
    EntryMeta {
        is_dir: md.is_dir(),
        is_symlink: md.file_type().is_symlink(),
        size: if md.is_dir() { 0 } else { md.len() },
        modified: md.modified().ok().and_then(unix_seconds),
        // Not every file system records it (`created()` then fails): unknown.
        created: md.created().ok().and_then(unix_seconds),
    }
}

/// Seconds since the Unix epoch (`None` before 1970).
fn unix_seconds(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// Windows error codes that whole-drive walks meet by the thousand in system folders and that
/// are not worth a warning (besides "access denied", which has its own
/// [`std::io::ErrorKind`]).
const EXPECTED_WINDOWS_ERRORS: [i32; 6] = [
    32,   // ERROR_SHARING_VIOLATION: pagefile.sys, hiberfil.sys, files held open.
    33,   // ERROR_LOCK_VIOLATION.
    448,  // ERROR_UNTRUSTED_MOUNT_POINT.
    1920, // ERROR_CANT_ACCESS_FILE: app execution aliases (WindowsApps) and the like.
    4390, // ERROR_NOT_A_REPARSE_POINT.
    4392, // ERROR_INVALID_REPARSE_DATA.
];

/// True for the errors whole-drive walks hit in system folders (System Volume Information,
/// other users' profiles, `WindowsApps` aliases, files in use...): "access denied" everywhere,
/// plus [`EXPECTED_WINDOWS_ERRORS`] on Windows. They are logged at `debug` and counted.
pub(crate) fn is_expected_error(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
        || (cfg!(windows)
            && e.raw_os_error()
                .is_some_and(|code| EXPECTED_WINDOWS_ERRORS.contains(&code)))
}

/// Logs an unreadable directory entry: `debug` for the expected errors, `warn` otherwise.
fn log_entry_error(dir: &Path, e: &std::io::Error, counters: &Counters) {
    if is_expected_error(e) {
        Counters::add(&counters.expected_errors);
        tracing::debug!(dir = %dir.display(), error = %e, "skipping unreadable entry");
    } else {
        tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable entry");
    }
}

/// Folders a walk could not list (the summary [`crate::ntfs`] logs for its fallback walks).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Skipped {
    /// All folders whose children could not be read.
    pub(crate) folders: usize,
    /// Of those, the ones refused with "access denied".
    pub(crate) access_denied: usize,
}

/// [`walk_counted`] for callers that only keep the [`Skipped`] summary.
pub(crate) fn walk_root(
    root: &Path,
    follow_links: bool,
    exclusions: &Arc<Exclusions>,
    builder: &mut IndexBuilder,
    skipped: &mut Skipped,
) {
    let counters = Arc::new(Counters::default());
    walk_counted(root, follow_links, exclusions, builder, &counters);
    let stats = counters.stats();
    skipped.folders += stats.unlisted_folders;
    skipped.access_denied += stats.access_denied;
}

/// Most threads listing folders at once. Listing is I/O bound and each thread adds buffers,
/// so more than this does not pay off.
const MAX_WALK_THREADS: usize = 8;

/// Listed folders waiting for the builder. Bounds the walk's memory: workers block once this
/// many listings are pending (a jwalk walk buffered without limit, gigabytes on big drives).
const PENDING_LISTINGS: usize = 64;

/// A folder to list: its path and its id in the builder.
struct DirTask {
    path: PathBuf,
    id: EntryId,
    /// When links are followed: the real paths of this folder's ancestors (a link back up
    /// to one of them would loop).
    chain: Option<Arc<Chain>>,
}

/// One real folder path in a chain of ancestors.
struct Chain {
    real: PathBuf,
    up: Option<Arc<Chain>>,
}

impl Chain {
    fn contains(mut node: Option<&Arc<Chain>>, real: &Path) -> bool {
        while let Some(n) = node {
            if n.real == real {
                return true;
            }
            node = n.up.as_ref();
        }
        false
    }
}

/// A listed folder: its kept children, sorted by name.
struct Listing {
    task: DirTask,
    children: Vec<Child>,
    /// The chain for its subfolders (links followed only).
    chain: Option<Arc<Chain>>,
}

struct Child {
    name: std::ffi::OsString,
    meta: EntryMeta,
    /// Listed in turn (a folder, not a link unless links are followed).
    descend: bool,
}

/// Folders waiting for a worker. `done` is set by the builder once every folder is listed.
#[derive(Default)]
struct TaskQueue {
    tasks: Vec<DirTask>,
    done: bool,
}

/// Walks one root into `builder`, counting what could not be read in `counters`.
///
/// Up to [`MAX_WALK_THREADS`] workers list folders (children with their metadata, filtered by
/// the exclusions) and hand each listing to this thread, which adds the children to the
/// builder and queues the subfolders. Parents always come before their children, and the root
/// with its whole subtree forms one block of ids. Pending folders are taken last-in first-out,
/// so the walk goes roughly depth-first and the queue stays small.
pub(crate) fn walk_counted(
    root: &Path,
    follow_links: bool,
    exclusions: &Arc<Exclusions>,
    builder: &mut IndexBuilder,
    counters: &Arc<Counters>,
) {
    let meta = match std::fs::metadata(root) {
        Ok(md) => meta_from(&md),
        Err(e) => {
            tracing::warn!(root = %root.display(), error = %e, "skipping unreadable root");
            return;
        }
    };
    let id = builder.push(None, &root.to_string_lossy(), meta);
    if !meta.is_dir {
        return;
    }
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(1, MAX_WALK_THREADS);
    let queue = Mutex::new(TaskQueue {
        tasks: vec![DirTask {
            path: root.to_path_buf(),
            id,
            chain: None,
        }],
        done: false,
    });
    let wake = Condvar::new();
    let (tx, rx) = mpsc::sync_channel::<Listing>(PENDING_LISTINGS);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let tx = tx.clone();
            let (queue, wake) = (&queue, &wake);
            s.spawn(move || {
                while let Some(task) = next_task(queue, wake) {
                    let (children, chain) = catch_unwind(AssertUnwindSafe(|| {
                        list_task(&task, follow_links, exclusions, counters)
                    }))
                    .unwrap_or_else(|_| {
                        tracing::error!(dir = %task.path.display(), "listing a folder panicked");
                        (Vec::new(), None)
                    });
                    // Every task sends exactly one listing: the builder counts them to know
                    // when the walk is over. It only fails if the builder is gone.
                    let listing = Listing {
                        task,
                        children,
                        chain,
                    };
                    if tx.send(listing).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut outstanding = 1usize;
        while outstanding > 0 {
            let Ok(listing) = rx.recv() else { break };
            outstanding -= 1;
            let mut subdirs = Vec::new();
            for child in listing.children {
                let child_id = builder.push(
                    Some(listing.task.id),
                    &child.name.to_string_lossy(),
                    child.meta,
                );
                if child.descend {
                    subdirs.push(DirTask {
                        path: listing.task.path.join(&child.name),
                        id: child_id,
                        chain: listing.chain.clone(),
                    });
                }
            }
            if !subdirs.is_empty() {
                outstanding += subdirs.len();
                // Reversed so that the first subfolder by name is taken first.
                subdirs.reverse();
                lock(&queue).tasks.extend(subdirs);
                wake.notify_all();
            }
        }
        lock(&queue).done = true;
        wake.notify_all();
        // Unblock workers waiting to send (only after an early end).
        drop(rx);
    });
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The next folder to list, waiting for one; `None` once the walk is over.
fn next_task(queue: &Mutex<TaskQueue>, wake: &Condvar) -> Option<DirTask> {
    let mut q = lock(queue);
    loop {
        if let Some(t) = q.tasks.pop() {
            return Some(t);
        }
        if q.done {
            return None;
        }
        q = wake.wait(q).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Lists a task's folder ([`list_dir`]). When links are followed, a folder that is one of its
/// own ancestors (reached through a link back up) is not listed again, and the chain for its
/// subfolders is returned.
fn list_task(
    task: &DirTask,
    follow_links: bool,
    exclusions: &Exclusions,
    counters: &Counters,
) -> (Vec<Child>, Option<Arc<Chain>>) {
    if !follow_links {
        return (list_dir(&task.path, false, exclusions, counters), None);
    }
    let Ok(real) = std::fs::canonicalize(&task.path) else {
        return (list_dir(&task.path, true, exclusions, counters), None);
    };
    if Chain::contains(task.chain.as_ref(), &real) {
        tracing::debug!(dir = %task.path.display(), "link loop, not listed again");
        return (Vec::new(), None);
    }
    let chain = Arc::new(Chain {
        real,
        up: task.chain.clone(),
    });
    (
        list_dir(&task.path, true, exclusions, counters),
        Some(chain),
    )
}

/// Lists `dir`: the children the exclusions keep, with their metadata, sorted by name. A
/// folder that cannot be listed is counted and yields no children.
fn list_dir(
    dir: &Path,
    follow_links: bool,
    exclusions: &Exclusions,
    counters: &Counters,
) -> Vec<Child> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            Counters::add(&counters.unlisted_folders);
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Counters::add(&counters.access_denied);
            }
            if is_expected_error(&e) {
                Counters::add(&counters.expected_errors);
                tracing::debug!(path = %dir.display(), error = %e, "cannot list folder");
            } else {
                tracing::warn!(path = %dir.display(), error = %e, "cannot list folder");
            }
            return Vec::new();
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log_entry_error(dir, &e, counters);
                continue;
            }
        };
        let name = entry.file_name();
        let path = entry.path();
        if exclusions.excludes(&path, &name) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            Counters::add(&counters.unreadable_metadata);
            continue;
        };
        // On Windows the listing already carries the metadata (no extra open per file); it
        // describes a link itself, like `symlink_metadata`.
        let md = if follow_links && ft.is_symlink() {
            std::fs::metadata(&path)
        } else {
            entry.metadata()
        };
        let mut meta = match md {
            Ok(md) => meta_from(&md),
            Err(e) => {
                Counters::add(&counters.unreadable_metadata);
                if is_expected_error(&e) {
                    Counters::add(&counters.expected_errors);
                    tracing::debug!(path = %path.display(), error = %e, "cannot read metadata");
                } else {
                    tracing::warn!(path = %path.display(), error = %e, "cannot read metadata");
                }
                EntryMeta::default()
            }
        };
        // Trust the listing for the kind of a non-link.
        meta.is_symlink |= ft.is_symlink();
        let descend = if ft.is_symlink() {
            // Covers junctions too: std reports them as symlinks on Windows.
            if follow_links { meta.is_dir } else { false }
        } else {
            ft.is_dir()
        };
        if !ft.is_symlink() {
            meta.is_dir = ft.is_dir();
        } else if !follow_links {
            meta.is_dir = false;
        }
        if meta.is_dir {
            meta.size = 0;
        }
        children.push(Child {
            name,
            meta,
            descend,
        });
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Query;
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in [
            "docs/عربي",
            "docs/sub/deep",
            "node_modules/pkg",
            "build/out",
            "keep",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        for f in [
            "docs/report.pdf",
            "docs/عربي/مُحَاضَرَة.docx",
            "docs/sub/deep/notes.txt",
            "docs/sub/cache.tmp",
            "node_modules/pkg/index.js",
            "build/out/app.exe",
            "keep/readme.md",
        ] {
            fs::write(p.join(f), b"hello").unwrap();
        }
        dir
    }

    fn relative_paths(ix: &FileIndex, root: &Path) -> BTreeSet<String> {
        (0..ix.len() as EntryId)
            .filter(|&id| ix.parent(id).is_some())
            .map(|id| {
                ix.path(id)
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walks_everything_and_rebuilds_paths() {
        let dir = tree();
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert_eq!(paths.len(), 16, "{paths:?}");
        assert!(paths.contains("docs/عربي/مُحَاضَرَة.docx"));
        assert!(paths.contains("node_modules/pkg/index.js"));
        // Every rebuilt path exists on disk.
        for id in 0..ix.len() as EntryId {
            assert!(ix.path(id).exists(), "{:?}", ix.path(id));
        }
        let id = ix
            .search(
                &Query::parse("محاضرة").unwrap(),
                10,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(id.len(), 1);
        let meta = ix.meta(id[0]);
        assert_eq!(meta.size, 5);
        assert!(!meta.is_dir && meta.modified.is_some());
        assert!(ix.meta(0).is_dir);
    }

    #[test]
    fn exclusions_by_name_glob_path_glob_and_prefix() {
        let dir = tree();
        let opts = WalkOptions::new([dir.path()])
            .exclude_glob("node_modules")
            .exclude_glob("*.tmp")
            .exclude_glob("**/build/out")
            .exclude_prefix(dir.path().join("docs").join("sub").join("deep"));
        let paths = relative_paths(&walk(&opts).unwrap(), dir.path());
        let expected: BTreeSet<String> = [
            "build",
            "docs",
            "docs/report.pdf",
            "docs/sub",
            "docs/عربي",
            "docs/عربي/مُحَاضَرَة.docx",
            "keep",
            "keep/readme.md",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(paths, expected);
    }

    #[test]
    fn prefix_does_not_exclude_siblings_with_same_start() {
        let dir = tree();
        fs::create_dir_all(dir.path().join("keep2")).unwrap();
        let opts = WalkOptions::new([dir.path()]).exclude_prefix(dir.path().join("keep"));
        let paths = relative_paths(&walk(&opts).unwrap(), dir.path());
        assert!(paths.contains("keep2"));
        assert!(!paths.contains("keep"));
    }

    #[test]
    fn multiple_roots_and_missing_root_is_skipped() {
        let a = tree();
        let b = tree();
        let missing = a.path().join("does-not-exist");
        let ix = walk(&WalkOptions::new([a.path(), &missing, b.path()])).unwrap();
        let roots: Vec<_> = (0..ix.len() as EntryId)
            .filter(|&id| ix.parent(id).is_none())
            .collect();
        assert_eq!(roots.len(), 2);
        assert_eq!(ix.len(), 2 * 17);
    }

    #[test]
    fn system_folder_errors_are_expected_others_are_not() {
        use std::io::{Error as IoError, ErrorKind};
        assert!(is_expected_error(&IoError::from(
            ErrorKind::PermissionDenied
        )));
        assert!(!is_expected_error(&IoError::from(ErrorKind::NotFound)));
        let cant_access = IoError::from_raw_os_error(1920);
        let sharing = IoError::from_raw_os_error(32);
        assert_eq!(is_expected_error(&cant_access), cfg!(windows));
        assert_eq!(is_expected_error(&sharing), cfg!(windows));
        assert!(!is_expected_error(&IoError::from_raw_os_error(2)));
    }

    #[test]
    fn walk_with_stats_counts_what_could_not_be_read() {
        let dir = tree();
        let missing = dir.path().join("does-not-exist");
        let (ix, stats) = walk_with_stats(&WalkOptions::new([dir.path(), &missing])).unwrap();
        assert_eq!(ix.len(), 17);
        // A missing root is skipped (with a warning), not counted as a system-folder error.
        assert_eq!(stats, WalkStats::default());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("keep");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            let (_, stats) = walk_with_stats(&WalkOptions::new([dir.path()])).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            if stats.unlisted_folders > 0 {
                // (Running as root reads it anyway.)
                assert_eq!(stats.access_denied, 1);
                assert_eq!(stats.expected_errors, 1);
            }
        }
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        let err = walk(&WalkOptions::new(["."]).exclude_glob("a[")).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_by_default() {
        let dir = tree();
        std::os::unix::fs::symlink(dir.path().join("docs"), dir.path().join("link")).unwrap();
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert!(paths.contains("link"));
        assert!(!paths.contains("link/report.pdf"));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_not_followed_by_default() {
        let dir = tree();
        let target = dir.path().join("docs");
        let link = dir.path().join("link");
        // `mklink /J` needs no admin rights; skip quietly if the shell refuses.
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert!(paths.contains("link"));
        assert!(!paths.contains("link/report.pdf"), "{paths:?}");
        let followed = walk(&WalkOptions::new([dir.path()]).follow_links(true)).unwrap();
        assert!(relative_paths(&followed, dir.path()).contains("link/report.pdf"));
    }
}
