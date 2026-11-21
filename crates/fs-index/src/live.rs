//! Live updates for the name index (task 1.5): an immutable base plus a small delta overlay.
//!
//! # Design
//!
//! A full [`FileIndex`] for millions of entries is a set of large, densely packed columns.
//! Rebuilding or even copying it for every burst of file-system changes would cost tens to
//! hundreds of milliseconds and double the memory while the copy exists. So the live layer keeps
//! the walked index as an immutable **base** and puts every change in an overlay:
//!
//! - **Tombstones**: a bitset over base ids. Removing a path marks the base entry and, for a
//!   folder, its whole subtree. A base file whose size or time changed is tombstoned too and
//!   re-added in the delta with the new metadata.
//! - **Delta**: added and updated entries, kept as a sorted map from [path key](path_key) to
//!   entry. After each batch the map is turned into a small ordinary [`FileIndex`] in which
//!   every entry whose parent lives in the base gets a chain of hidden **anchor** entries: copies
//!   of its base ancestors (names, normalized names and metadata), from the root down. The delta
//!   index is therefore self-contained, so *all* query features (path, `parent:`, `infolder:`,
//!   wildcards on folders...) evaluate exactly as in the base, with the same compiled query and
//!   the same scan code. Anchors are masked out of results.
//! - **Snapshots**: base, tombstones and delta are bundled into an immutable [`IndexSnapshot`]
//!   behind an `Arc`. Applying a batch builds a new snapshot and swaps it in; searches that are
//!   running keep the snapshot they started with, so readers never wait for writers and
//!   writers never wait for readers (only the pointer swap takes a lock, for nanoseconds).
//! - **Compaction**: when the delta or the tombstones grow past a threshold (see
//!   [`LiveIndex::set_compaction_thresholds`]), the snapshot is merged into a fresh base on the
//!   applying thread ([`IndexSnapshot::to_file_index`]), still without blocking searches.
//!
//! A search runs over the delta first and then over the base, skipping tombstoned ids with one
//! bit test per *hit* (not per entry), so search cost with a small delta is the cost of the base
//! search plus a scan of a few thousand entries.
//!
//! To map a changed path to a base id, the live index builds, once per base, a child table
//! (CSR adjacency sorted by a case-folded name hash; 12 bytes per entry), so a lookup costs one
//! binary search per path component.
//!
//! # Entry ids
//!
//! Ids are only meaningful together with the [`IndexSnapshot`] they came from:
//!
//! - `0..base.len()` are base ids. They are the ids of [`IndexSnapshot::base`] and stay valid
//!   (same entry, same path) for as long as the base is not replaced: between compactions,
//!   applying batches never renumbers them; a removed entry is simply no longer returned.
//! - `base.len()..` are delta ids. The delta is rebuilt for every batch, so a delta id can
//!   point at another entry (or nothing) in the next snapshot.
//! - Compaction and [`LiveIndex::replace_base`] renumber everything.
//!
//! Every new snapshot gets a new [`IndexSnapshot::generation`]. The app should keep the
//! snapshot (or its generation) next to the ids it shows and resolve ids with the same
//! snapshot, never with a newer one.
//!
//! # What is ignored
//!
//! Metadata changes of a folder that stays a folder (its modification time changes whenever a
//! child is added) are not tracked; the folder keeps its walked metadata until the next
//! compaction-free re-walk. Case-only renames (`Foo` to `foo` on Windows) keep the old spelling
//! for folders.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use athrix_core::Result;
use rayon::prelude::*;

use crate::index::{EntryId, EntryMeta, FileIndex, IndexBuilder, NO_PARENT};
use crate::search::{Query, bit_is_set};
use crate::walk::{Scope, WalkOptions, key_has_prefix, path_key};

/// Default: compact once the delta holds this many entries (or 1/16 of the base, if larger).
const DEFAULT_MAX_DELTA: usize = 32 * 1024;
/// Default: compact once this many base entries are removed (or 1/4 of the base, if larger).
const DEFAULT_MAX_REMOVED: usize = 256 * 1024;
/// Guard against pathological parent chains when resolving unknown ancestors.
const MAX_RESOLVE_DEPTH: usize = 256;

/// A change to apply without touching the file system (see [`LiveIndex::apply_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The path exists with this metadata (created, renamed to, or modified). A folder's
    /// contents are **not** read; add them with further upserts. Its parent must already be in
    /// the index (apply parents first) unless the path is one of the roots.
    Upsert {
        /// Full path.
        path: PathBuf,
        /// Current metadata.
        meta: EntryMeta,
    },
    /// The path no longer exists (deleted or renamed away). For a folder the whole subtree
    /// goes.
    Remove {
        /// Full path.
        path: PathBuf,
    },
}

/// What a call to [`LiveIndex::apply_paths`] or [`LiveIndex::apply_changes`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    /// Entries added to the index or updated (subtree entries of a new folder included).
    pub upserted: usize,
    /// Entries removed (each entry of a removed subtree counts).
    pub removed: usize,
    /// Paths that needed no change (already up to date, or already gone).
    pub unchanged: usize,
    /// Paths ignored: outside every root, excluded, unreadable, or whose parent is unknown.
    pub skipped: usize,
    /// The overlay was merged into a new base (all ids were renumbered).
    pub compacted: bool,
}

impl ApplyStats {
    /// True if the index content changed (a new snapshot was published).
    pub fn changed(&self) -> bool {
        self.upserted > 0 || self.removed > 0 || self.compacted
    }
}

/// An immutable view of a [`LiveIndex`] at one point in time: the base index, the removed
/// base entries and the delta. Cheap to clone via `Arc`; searching it never blocks updates.
///
/// Its accessors mirror [`FileIndex`]'s; ids are described in the [module docs](self).
#[derive(Debug)]
pub struct IndexSnapshot {
    generation: u64,
    base: Arc<FileIndex>,
    dead: Arc<Vec<u64>>,
    dead_count: usize,
    delta: Arc<Delta>,
}

/// The overlay as a self-contained index (see the module docs).
#[derive(Debug, Default)]
struct Delta {
    index: FileIndex,
    /// Bits set for anchors (copies of base ancestors), which are never results.
    hidden: Vec<u64>,
    /// Per delta entry: the base id an anchor copies, or `NO_PARENT` for a real entry.
    origin: Vec<u32>,
    anchors: usize,
}

enum Loc {
    Base(EntryId),
    Delta(EntryId),
}

impl IndexSnapshot {
    fn new(generation: u64, base: Arc<FileIndex>) -> Self {
        IndexSnapshot {
            generation,
            base,
            dead: Arc::new(Vec::new()),
            dead_count: 0,
            delta: Arc::new(Delta::default()),
        }
    }

    /// Counter that changes every time the live index publishes a new snapshot.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The base index. Base ids (`< base().len()`) are its ids; some may be removed in this
    /// snapshot (see [`IndexSnapshot::contains`]).
    pub fn base(&self) -> &Arc<FileIndex> {
        &self.base
    }

    /// True if this snapshot differs from its base (entries were removed, added or updated).
    pub fn has_changes(&self) -> bool {
        self.dead_count > 0 || self.delta_entries() > 0
    }

    /// Number of entries added or updated since the base was built.
    pub fn delta_entries(&self) -> usize {
        self.delta.index.len() - self.delta.anchors
    }

    /// Number of base entries that are removed or superseded in this snapshot.
    pub fn removed_entries(&self) -> usize {
        self.dead_count
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.base.len() - self.dead_count + self.delta_entries()
    }

    /// True if no entry is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One past the largest id of this snapshot (live or not).
    pub fn id_bound(&self) -> usize {
        self.base.len() + self.delta.index.len()
    }

    fn loc(&self, id: EntryId) -> Loc {
        let n = self.base.len();
        if (id as usize) < n {
            Loc::Base(id)
        } else {
            Loc::Delta(id - n as u32)
        }
    }

    /// True if `id` is a live entry of this snapshot (search only returns live ids).
    pub fn contains(&self, id: EntryId) -> bool {
        match self.loc(id) {
            Loc::Base(b) => !bit_is_set(&self.dead, b),
            Loc::Delta(d) => {
                (d as usize) < self.delta.index.len() && !bit_is_set(&self.delta.hidden, d)
            }
        }
    }

    /// Runs `query` and returns up to `limit` live ids: entries added or updated since the
    /// base first (in path order), then base entries in index order. Same cancellation and
    /// errors as [`FileIndex::search`].
    pub fn search(&self, query: &Query, limit: usize, cancel: &AtomicBool) -> Result<Vec<EntryId>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if self.delta_entries() > 0 {
            let offset = self.base.len() as EntryId;
            let hits = self.delta.index.search_masked(
                query,
                limit,
                cancel,
                Some(self.delta.hidden.as_slice()),
            )?;
            out.extend(hits.into_iter().map(|d| d + offset));
        }
        if out.len() < limit {
            let mask = (self.dead_count > 0).then_some(self.dead.as_slice());
            out.extend(
                self.base
                    .search_masked(query, limit - out.len(), cancel, mask)?,
            );
        }
        Ok(out)
    }

    /// True if entry `id` matches `query` (ignores whether it is live).
    ///
    /// # Panics
    /// If `id` is out of range (as for all per-entry accessors).
    pub fn is_match(&self, query: &Query, id: EntryId) -> bool {
        match self.loc(id) {
            Loc::Base(b) => self.base.is_match(query, b),
            Loc::Delta(d) => self.delta.index.is_match(query, d),
        }
    }

    /// The original name of `id` (for a root, its full path).
    pub fn name(&self, id: EntryId) -> &str {
        match self.loc(id) {
            Loc::Base(b) => self.base.name(b),
            Loc::Delta(d) => self.delta.index.name(d),
        }
    }

    /// The normalized name of `id`, as matched by search.
    pub fn normalized_name(&self, id: EntryId) -> &str {
        match self.loc(id) {
            Loc::Base(b) => self.base.normalized_name(b),
            Loc::Delta(d) => self.delta.index.normalized_name(d),
        }
    }

    /// Metadata of `id`.
    pub fn meta(&self, id: EntryId) -> EntryMeta {
        match self.loc(id) {
            Loc::Base(b) => self.base.meta(b),
            Loc::Delta(d) => self.delta.index.meta(d),
        }
    }

    /// The parent of `id`, or `None` for a root. A base folder is reported by its base id
    /// even when `id` is in the delta.
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        match self.loc(id) {
            Loc::Base(b) => self.base.parent(b),
            Loc::Delta(d) => {
                self.delta
                    .index
                    .parent(d)
                    .map(|p| match self.delta.origin[p as usize] {
                        NO_PARENT => p + self.base.len() as EntryId,
                        base => base,
                    })
            }
        }
    }

    /// The full path of `id`.
    pub fn path(&self, id: EntryId) -> PathBuf {
        match self.loc(id) {
            Loc::Base(b) => self.base.path(b),
            Loc::Delta(d) => self.delta.index.path(d),
        }
    }

    /// Merges the overlay into a plain [`FileIndex`] holding exactly the live entries (base
    /// entries first, in base order, then the delta). Costs about one pass over the base
    /// (tens of milliseconds per million entries); without changes it is a plain clone.
    pub fn to_file_index(&self) -> FileIndex {
        if !self.has_changes() {
            return (*self.base).clone();
        }
        let base = &*self.base;
        let n = base.len();
        let avg = base.names.text.len() / n.max(1) + 1;
        let mut b = IndexBuilder::with_capacity(self.len(), avg);
        let mut remap = vec![NO_PARENT; n];
        for id in 0..n as EntryId {
            if bit_is_set(&self.dead, id) {
                continue;
            }
            let parent = match base.parent(id) {
                None => None,
                Some(p) => match remap[p as usize] {
                    // A live child of a removed folder cannot exist; skip defensively.
                    NO_PARENT => continue,
                    p => Some(p),
                },
            };
            remap[id as usize] = b.push_normalized(
                parent,
                base.name(id),
                base.normalized_name(id),
                base.meta(id),
            );
        }
        let d = &self.delta.index;
        let mut dremap = vec![NO_PARENT; d.len()];
        for id in 0..d.len() as EntryId {
            let origin = self.delta.origin[id as usize];
            if origin != NO_PARENT {
                dremap[id as usize] = remap[origin as usize];
                continue;
            }
            let parent = match d.parent(id) {
                None => None,
                Some(p) => match dremap[p as usize] {
                    NO_PARENT => continue,
                    p => Some(p),
                },
            };
            dremap[id as usize] =
                b.push_normalized(parent, d.name(id), d.normalized_name(id), d.meta(id));
        }
        b.finish()
    }

    /// Saves the live entries as a snapshot file (see [`FileIndex::save`]); compacts first if
    /// there are changes.
    pub fn save(&self, path: &Path) -> Result<()> {
        if self.has_changes() {
            self.to_file_index().save(path)
        } else {
            self.base.save(path)
        }
    }
}

/// Case-folded name for lookups: per character lower case on Windows, unchanged elsewhere.
#[cfg(windows)]
fn fold(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars().flat_map(char::to_lowercase)
}

/// Case-folded name for lookups: per character lower case on Windows, unchanged elsewhere.
#[cfg(not(windows))]
fn fold(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars()
}

/// FNV-1a over the folded characters.
fn fold_hash(name: &str) -> u64 {
    fold(name).fold(0xcbf2_9ce4_8422_2325, |h, c| {
        (h ^ u64::from(u32::from(c))).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// [`fold_hash`] truncated to 32 bits (collisions only cost a name comparison).
fn short_hash(name: &str) -> u32 {
    let h = fold_hash(name);
    (h ^ (h >> 32)) as u32
}

/// Path-to-id lookup over one base: children of every folder sorted by folded-name hash.
#[derive(Debug, Default)]
struct Lookup {
    /// Root key, root id, and number of path components of the root's name.
    roots: Vec<(String, EntryId, usize)>,
    /// `kids[start[p]..start[p + 1]]` are the children of `p`.
    start: Vec<u32>,
    kids: Vec<u32>,
    /// `hashes[i]`: folded-name hash of `kids[i]` (truncated), so binary searches scan a
    /// small contiguous array instead of hashing names scattered over the arena.
    hashes: Vec<u32>,
}

impl Lookup {
    fn build(ix: &FileIndex) -> Self {
        let n = ix.len();
        let mut roots = Vec::new();
        let mut start = vec![0u32; n + 1];
        for id in 0..n as EntryId {
            match ix.parent(id) {
                Some(p) => start[p as usize + 1] += 1,
                None => {
                    let path = Path::new(ix.name(id));
                    roots.push((path_key(path), id, path.components().count()));
                }
            }
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let all: Vec<u32> = (0..n as EntryId)
            .into_par_iter()
            .map(|id| short_hash(ix.name(id)))
            .collect();
        let mut kids: Vec<u32> = (0..n as EntryId)
            .filter(|&id| ix.parent(id).is_some())
            .collect();
        kids.par_sort_unstable_by_key(|&k| (ix.parents[k as usize], all[k as usize]));
        let hashes = kids.par_iter().map(|&k| all[k as usize]).collect();
        Lookup {
            roots,
            start,
            kids,
            hashes,
        }
    }

    fn range(&self, p: EntryId) -> std::ops::Range<usize> {
        self.start[p as usize] as usize..self.start[p as usize + 1] as usize
    }

    fn children(&self, p: EntryId) -> &[u32] {
        &self.kids[self.range(p)]
    }

    fn child(&self, ix: &FileIndex, parent: EntryId, name: &str) -> Option<EntryId> {
        let h = short_hash(name);
        let range = self.range(parent);
        let hashes = &self.hashes[range.clone()];
        let first = hashes.partition_point(|&x| x < h);
        let equal = hashes[first..].iter().take_while(|&&x| x == h).count();
        self.kids[range.start + first..range.start + first + equal]
            .iter()
            .find(|&&k| fold(ix.name(k)).eq(fold(name)))
            .copied()
    }

    /// The base id of `path` (removed or not).
    fn find(&self, ix: &FileIndex, path: &Path, key: &str) -> Option<EntryId> {
        let &(_, root, comps) = self
            .roots
            .iter()
            .filter(|(k, ..)| key_has_prefix(key, k))
            .max_by_key(|(k, ..)| k.len())?;
        let mut cur = root;
        for c in path.components().skip(comps) {
            match c {
                Component::Normal(name) => cur = self.child(ix, cur, &name.to_string_lossy())?,
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(cur)
    }
}

/// Where an added entry hangs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddedParent {
    /// It is a root.
    Root,
    /// A live base folder.
    Base(EntryId),
    /// The added folder whose key is this entry's key up to the last `/`.
    Delta,
}

#[derive(Debug, Clone)]
struct Added {
    name: String,
    normalized: String,
    meta: EntryMeta,
    parent: AddedParent,
}

/// The writer side of a [`LiveIndex`], behind its mutex.
#[derive(Debug)]
struct State {
    scope: Scope,
    base: Arc<FileIndex>,
    lookup: Lookup,
    dead: Arc<Vec<u64>>,
    dead_count: usize,
    added: BTreeMap<String, Added>,
    generation: u64,
    max_delta: usize,
    max_removed: usize,
    buf: String,
}

/// A name index that follows file-system changes: an immutable base [`FileIndex`] plus an
/// overlay of changes (see the [module docs](self) for the design and the id rules).
///
/// Typical use in the app:
///
/// ```no_run
/// use std::sync::{Arc, atomic::AtomicBool};
/// use athrix_fs_index::{LiveIndex, Query, WalkOptions, WatchBatch, WatchConfig, Watcher, walk};
///
/// let opts = WalkOptions::new(["D:/Documents"]).exclude_glob("node_modules");
/// let live = Arc::new(LiveIndex::new(walk(&opts)?, &opts)?);
///
/// let writer = Arc::clone(&live);
/// let rewalk_opts = opts.clone();
/// let _watcher = Watcher::start(&opts, WatchConfig::default(), move |batch| match batch {
///     WatchBatch::Changed(paths) => {
///         writer.apply_paths(&paths);
///     }
///     WatchBatch::RescanNeeded(_) => {
///         if let Ok(fresh) = walk(&rewalk_opts) {
///             writer.replace_base(fresh);
///         }
///     }
/// })?;
///
/// // Any thread: take a snapshot, search it, and resolve ids with the same snapshot.
/// let snap = live.snapshot();
/// for id in snap.search(&Query::parse("report")?, 100, &AtomicBool::new(false))? {
///     println!("{} {}", snap.generation(), snap.path(id).display());
/// }
/// # Ok::<(), athrix_core::Error>(())
/// ```
#[derive(Debug)]
pub struct LiveIndex {
    current: RwLock<Arc<IndexSnapshot>>,
    state: Mutex<State>,
}

impl LiveIndex {
    /// Wraps a walked (or loaded) index. `opts` must be the options it was walked with: its
    /// roots and exclusions decide which changed paths are taken in. Builds the path lookup
    /// table (one parallel pass, roughly 50-100 ms per million entries).
    ///
    /// # Errors
    /// [`athrix_core::Error::Config`] if an exclusion glob is invalid.
    pub fn new(base: impl Into<Arc<FileIndex>>, opts: &WalkOptions) -> Result<Self> {
        let base = base.into();
        let state = State {
            scope: Scope::new(opts)?,
            lookup: Lookup::build(&base),
            base: Arc::clone(&base),
            dead: Arc::new(Vec::new()),
            dead_count: 0,
            added: BTreeMap::new(),
            generation: 0,
            max_delta: DEFAULT_MAX_DELTA,
            max_removed: DEFAULT_MAX_REMOVED,
            buf: String::new(),
        };
        Ok(LiveIndex {
            current: RwLock::new(Arc::new(IndexSnapshot::new(0, base))),
            state: Mutex::new(state),
        })
    }

    /// The current snapshot. Hold on to it for as long as you use ids from its searches.
    pub fn snapshot(&self) -> Arc<IndexSnapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The generation of the current snapshot.
    pub fn generation(&self) -> u64 {
        self.snapshot().generation
    }

    /// Compact when the delta exceeds `max(max_delta_entries, base/16)` entries or more than
    /// `max(max_removed, base/4)` base entries are removed. Defaults: 32 Ki and 256 Ki.
    pub fn set_compaction_thresholds(&self, max_delta_entries: usize, max_removed: usize) {
        let mut st = self.lock();
        st.max_delta = max_delta_entries;
        st.max_removed = max_removed;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Brings the given paths up to date by looking at the file system: a path that exists is
    /// added or updated (a new folder is walked, with the walk's exclusions), a path that is
    /// gone is removed with its subtree. Unknown parent folders are added on the way. Paths
    /// outside the roots or excluded are skipped. Typically called with a
    /// [`WatchBatch::Changed`](crate::WatchBatch) batch.
    ///
    /// Searches keep running on the previous snapshot meanwhile; the new one is published at
    /// the end (only if something changed).
    pub fn apply_paths<P: AsRef<Path>>(&self, paths: &[P]) -> ApplyStats {
        let mut sorted: Vec<&Path> = paths.iter().map(AsRef::as_ref).collect();
        sorted.sort_by_key(|p| p.components().count());
        sorted.dedup();
        let mut st = self.lock();
        let mut stats = ApplyStats::default();
        for p in sorted {
            st.resolve(p, &mut stats, 0);
        }
        self.finish(st, stats)
    }

    /// Applies changes as given, without reading the file system (for sources that already
    /// know the metadata, such as a USN journal reader, and for tests). Apply parents before
    /// children.
    pub fn apply_changes(&self, changes: &[Change]) -> ApplyStats {
        let mut st = self.lock();
        let mut stats = ApplyStats::default();
        for c in changes {
            match c {
                Change::Upsert { path, meta } => {
                    let key = path_key(path);
                    if !st.scope.admits(path, &key) || !st.parent_known(path, &key) {
                        stats.skipped += 1;
                        continue;
                    }
                    st.upsert(path, &key, *meta, &mut stats);
                }
                Change::Remove { path } => {
                    let key = path_key(path);
                    if !st.scope.admits(path, &key) {
                        stats.skipped += 1;
                        continue;
                    }
                    st.remove_counted(path, &key, &mut stats);
                }
            }
        }
        self.finish(st, stats)
    }

    /// Replaces the base with a freshly walked index (after [`WatchBatch::RescanNeeded`]
    /// (crate::WatchBatch)), dropping the overlay. Renumbers all ids.
    pub fn replace_base(&self, base: impl Into<Arc<FileIndex>>) {
        let mut st = self.lock();
        st.set_base(base.into());
        self.publish(&mut st);
    }

    /// Merges the overlay into a new base now (see [`IndexSnapshot::to_file_index`]).
    /// Renumbers all ids. Does nothing without changes.
    pub fn compact(&self) {
        let mut st = self.lock();
        if st.has_changes() {
            let merged = st.snapshot(st.generation).to_file_index();
            st.set_base(Arc::new(merged));
            self.publish(&mut st);
        }
    }

    fn finish(&self, mut st: MutexGuard<'_, State>, mut stats: ApplyStats) -> ApplyStats {
        if st.needs_compaction() {
            let merged = st.snapshot(st.generation).to_file_index();
            st.set_base(Arc::new(merged));
            stats.compacted = true;
        }
        if stats.changed() {
            self.publish(&mut st);
        }
        stats
    }

    fn publish(&self, st: &mut State) {
        st.generation += 1;
        let snap = Arc::new(st.snapshot(st.generation));
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = snap;
    }
}

impl State {
    fn has_changes(&self) -> bool {
        self.dead_count > 0 || !self.added.is_empty()
    }

    fn needs_compaction(&self) -> bool {
        let n = self.base.len();
        self.added.len() > self.max_delta.max(n / 16)
            || self.dead_count > self.max_removed.max(n / 4)
    }

    fn set_base(&mut self, base: Arc<FileIndex>) {
        self.lookup = Lookup::build(&base);
        self.base = base;
        self.dead = Arc::new(Vec::new());
        self.dead_count = 0;
        self.added.clear();
    }

    /// The live base id of `path`.
    fn live_base(&self, path: &Path, key: &str) -> Option<EntryId> {
        self.lookup
            .find(&self.base, path, key)
            .filter(|&id| !bit_is_set(&self.dead, id))
    }

    /// True if `path` is a folder in the delta or a live folder in the base.
    fn known_dir(&self, path: &Path, key: &str) -> bool {
        match self.added.get(key) {
            Some(a) => a.meta.is_dir,
            None => self
                .live_base(path, key)
                .is_some_and(|id| self.base.meta(id).is_dir),
        }
    }

    /// True if `path` is a root or its parent is a known folder.
    fn parent_known(&self, path: &Path, key: &str) -> bool {
        if self.scope.is_root(key) {
            return true;
        }
        path.parent()
            .is_some_and(|parent| self.known_dir(parent, &path_key(parent)))
    }

    fn resolve(&mut self, path: &Path, stats: &mut ApplyStats, depth: usize) {
        let key = path_key(path);
        if !self.scope.admits(path, &key) {
            stats.skipped += 1;
            return;
        }
        let mut meta = match self.scope.read_meta(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.remove_counted(path, &key, stats);
                return;
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read changed path");
                stats.skipped += 1;
                return;
            }
        };
        if meta.is_symlink && !self.scope.follow_links {
            // Like the walk: links and junctions are entries, never folders to descend.
            meta.is_dir = false;
        }
        if !self.parent_known(path, &key) {
            let parent = path.parent();
            if let Some(parent) = parent.filter(|_| depth < MAX_RESOLVE_DEPTH) {
                // Adds (and walks) the parent, which normally picks up `path` too.
                self.resolve(parent, stats, depth + 1);
            }
            if !self.parent_known(path, &key) {
                stats.skipped += 1;
                return;
            }
        }
        let was_dir = self.known_dir(path, &key);
        self.upsert(path, &key, meta, stats);
        if meta.is_dir && !was_dir {
            let sub = self.scope.walk_subtree(path);
            for id in 1..sub.len() as EntryId {
                let child = sub.path(id);
                let child_key = path_key(&child);
                self.upsert(&child, &child_key, sub.meta(id), stats);
            }
        }
    }

    fn remove_counted(&mut self, path: &Path, key: &str, stats: &mut ApplyStats) {
        match self.remove(path, key) {
            0 => stats.unchanged += 1,
            n => stats.removed += n,
        }
    }

    /// Removes `path` and everything below it from the delta and the base. Returns the number
    /// of entries removed.
    fn remove(&mut self, path: &Path, key: &str) -> usize {
        let mut removed = 0;
        if self.added.remove(key).is_some() {
            removed += 1;
        }
        let below: Vec<String> = self
            .added
            .range(format!("{key}/")..format!("{key}0"))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &below {
            self.added.remove(k);
        }
        removed += below.len();
        if let Some(id) = self.live_base(path, key) {
            removed += self.kill_subtree(id);
        }
        removed
    }

    /// Tombstones one base entry. Returns 1 if it was live.
    fn kill(&mut self, id: EntryId) -> usize {
        let n = self.base.len();
        let dead = Arc::make_mut(&mut self.dead);
        if dead.len() < n.div_ceil(64) {
            dead.resize(n.div_ceil(64), 0);
        }
        let (w, b) = (id as usize / 64, 1u64 << (id % 64));
        if dead[w] & b != 0 {
            return 0;
        }
        dead[w] |= b;
        self.dead_count += 1;
        1
    }

    /// Tombstones a base entry and all its descendants. Returns how many were live.
    fn kill_subtree(&mut self, id: EntryId) -> usize {
        let mut count = 0;
        let mut stack = vec![id];
        while let Some(e) = stack.pop() {
            count += self.kill(e);
            let kids = self.lookup.children(e);
            stack.extend_from_slice(kids);
        }
        count
    }

    /// Records that `path` exists with `meta`. Its parent must be known (or it is a root).
    fn upsert(&mut self, path: &Path, key: &str, meta: EntryMeta, stats: &mut ApplyStats) {
        let is_root = self.scope.is_root(key);
        let name = if is_root {
            path.to_string_lossy()
        } else {
            match path.file_name() {
                Some(n) => n.to_string_lossy(),
                None => {
                    stats.skipped += 1;
                    return;
                }
            }
        };
        if let Some(a) = self.added.get_mut(key) {
            if a.meta.is_dir == meta.is_dir {
                if a.meta == meta && a.name == name {
                    stats.unchanged += 1;
                } else {
                    a.meta = meta;
                    if a.name != name {
                        a.name = name.into_owned();
                        athrix_text_ar::normalize_name_into(&a.name, &mut a.normalized);
                    }
                    stats.upserted += 1;
                }
                return;
            }
            // Kind changed: drop the old entry and its subtree, then add as new.
            stats.removed += self.remove(path, key);
        } else if let Some(id) = self.live_base(path, key) {
            let old = self.base.meta(id);
            if old.is_dir != meta.is_dir {
                stats.removed += self.remove(path, key);
            } else if old.is_dir || (old == meta && self.base.name(id) == name) {
                // Folder metadata is not tracked (see the module docs).
                stats.unchanged += 1;
                return;
            } else {
                // A changed base file: supersede it with a delta copy.
                self.kill(id);
            }
        }
        let parent = if is_root {
            AddedParent::Root
        } else {
            let pkey = key.rfind('/').map_or("", |i| &key[..i]);
            match self.added.get(pkey) {
                Some(p) if p.meta.is_dir => AddedParent::Delta,
                Some(_) => {
                    stats.skipped += 1;
                    return;
                }
                None => {
                    let parent_path = path.parent().unwrap_or(path);
                    match self.live_base(parent_path, pkey) {
                        Some(p) if self.base.meta(p).is_dir => AddedParent::Base(p),
                        _ => {
                            stats.skipped += 1;
                            return;
                        }
                    }
                }
            }
        };
        athrix_text_ar::normalize_name_into(&name, &mut self.buf);
        self.added.insert(
            key.to_owned(),
            Added {
                name: name.into_owned(),
                normalized: self.buf.clone(),
                meta,
                parent,
            },
        );
        stats.upserted += 1;
    }

    /// Builds the delta index from the added entries (see the module docs).
    fn build_delta(&self) -> Delta {
        let mut b = IndexBuilder::new();
        let mut origin: Vec<u32> = Vec::with_capacity(self.added.len());
        let mut anchors: HashMap<EntryId, EntryId> = HashMap::new();
        let mut dirs: HashMap<&str, EntryId> = HashMap::new();
        let mut chain = Vec::new();
        for (key, a) in &self.added {
            let parent = match a.parent {
                AddedParent::Root => None,
                AddedParent::Delta => {
                    let pkey = key.rfind('/').map_or("", |i| &key[..i]);
                    match dirs.get(pkey) {
                        Some(&p) => Some(p),
                        None => {
                            debug_assert!(false, "delta parent of {key} missing");
                            continue;
                        }
                    }
                }
                AddedParent::Base(bid) => {
                    // Copy the missing part of the base ancestor chain, root first.
                    chain.clear();
                    let mut cur = Some(bid);
                    let mut top = None;
                    while let Some(c) = cur {
                        if let Some(&d) = anchors.get(&c) {
                            top = Some(d);
                            break;
                        }
                        chain.push(c);
                        cur = self.base.parent(c);
                    }
                    for &c in chain.iter().rev() {
                        let d = b.push_normalized(
                            top,
                            self.base.name(c),
                            self.base.normalized_name(c),
                            self.base.meta(c),
                        );
                        origin.push(c);
                        anchors.insert(c, d);
                        top = Some(d);
                    }
                    top
                }
            };
            let id = b.push_normalized(parent, &a.name, &a.normalized, a.meta);
            origin.push(NO_PARENT);
            if a.meta.is_dir {
                dirs.insert(key, id);
            }
        }
        let index = b.finish();
        let mut hidden = vec![0u64; index.len().div_ceil(64)];
        let mut count = 0;
        for (i, &o) in origin.iter().enumerate() {
            if o != NO_PARENT {
                hidden[i / 64] |= 1 << (i % 64);
                count += 1;
            }
        }
        Delta {
            index,
            hidden,
            origin,
            anchors: count,
        }
    }

    fn snapshot(&self, generation: u64) -> IndexSnapshot {
        IndexSnapshot {
            generation,
            base: Arc::clone(&self.base),
            dead: Arc::clone(&self.dead),
            dead_count: self.dead_count,
            delta: Arc::new(self.build_delta()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::walk;
    use std::collections::BTreeSet;
    use std::fs;

    fn search(snap: &IndexSnapshot, text: &str) -> Vec<PathBuf> {
        snap.search(
            &Query::parse(text).unwrap(),
            usize::MAX,
            &AtomicBool::new(false),
        )
        .unwrap()
        .into_iter()
        .map(|id| snap.path(id))
        .collect()
    }

    fn names(snap: &IndexSnapshot, text: &str) -> BTreeSet<String> {
        snap.search(
            &Query::parse(text).unwrap(),
            usize::MAX,
            &AtomicBool::new(false),
        )
        .unwrap()
        .into_iter()
        .map(|id| snap.name(id).to_owned())
        .collect()
    }

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| (*s).to_owned()).collect()
    }

    fn file(size: u64) -> EntryMeta {
        EntryMeta {
            size,
            modified: Some(1_700_000_000),
            ..Default::default()
        }
    }

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    /// A synthetic base under a root that does not exist on disk.
    fn synthetic() -> (LiveIndex, PathBuf) {
        let root = PathBuf::from(if cfg!(windows) {
            r"Q:\synthetic"
        } else {
            "/synthetic"
        });
        let mut b = IndexBuilder::new();
        let r = b.push(None, &root.to_string_lossy(), dir());
        let docs = b.push(Some(r), "Docs", dir());
        let sub = b.push(Some(docs), "Sub", dir());
        b.push(Some(docs), "report.pdf", file(10));
        b.push(Some(sub), "notes.txt", file(20));
        b.push(Some(sub), "مُحَاضَرَة.docx", file(30));
        b.push(Some(r), "keep.md", file(40));
        let opts = WalkOptions::new([&root]).exclude_glob("node_modules");
        (LiveIndex::new(b.finish(), &opts).unwrap(), root)
    }

    #[test]
    fn upsert_and_remove_without_file_system() {
        let (live, root) = synthetic();
        let g0 = live.generation();
        let docs = root.join("Docs");
        let stats = live.apply_changes(&[
            Change::Upsert {
                path: docs.join("Sub").join("new_report.txt"),
                meta: file(5),
            },
            Change::Upsert {
                path: docs.join("Fresh"),
                meta: dir(),
            },
            Change::Upsert {
                path: docs.join("Fresh").join("محاضرة جديدة.pdf"),
                meta: file(6),
            },
            Change::Remove {
                path: docs.join("report.pdf"),
            },
        ]);
        assert_eq!(stats.upserted, 3);
        assert_eq!(stats.removed, 1);
        assert!(live.generation() > g0);
        let snap = live.snapshot();
        assert_eq!(names(&snap, "report"), set(&["new_report.txt"]));
        assert_eq!(
            names(&snap, "محاضرة"),
            set(&["مُحَاضَرَة.docx", "محاضرة جديدة.pdf"])
        );
        let hit = snap
            .search(&Query::parse("جديدة").unwrap(), 10, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            snap.path(hit[0]),
            docs.join("Fresh").join("محاضرة جديدة.pdf")
        );
        assert_eq!(snap.meta(hit[0]).size, 6);
        let parent = snap.parent(hit[0]).unwrap();
        assert_eq!(snap.name(parent), "Fresh");
        // The delta folder's parent is the base folder, by its base id.
        assert_eq!(snap.parent(parent), Some(1));
        assert_eq!(snap.len(), 7 - 1 + 3);
        // Path filters see the full ancestry through the anchors.
        assert_eq!(
            names(&snap, "infolder:docs pdf"),
            set(&["محاضرة جديدة.pdf"])
        );
        assert_eq!(names(&snap, "parent:sub new"), set(&["new_report.txt"]));
        assert_eq!(
            names(&snap, "path:*synthetic*fresh*"),
            set(&["Fresh", "محاضرة جديدة.pdf"])
        );
        // Anchors never show up, even for a match-everything query.
        assert_eq!(search(&snap, "").len(), snap.len());
    }

    #[test]
    fn removing_a_base_folder_removes_its_subtree_and_delta_children() {
        let (live, root) = synthetic();
        let sub = root.join("Docs").join("Sub");
        live.apply_changes(&[Change::Upsert {
            path: sub.join("added.txt"),
            meta: file(1),
        }]);
        let stats = live.apply_changes(&[Change::Remove { path: sub.clone() }]);
        assert_eq!(stats.removed, 4); // Sub, notes.txt, the Arabic file, added.txt
        let snap = live.snapshot();
        assert!(names(&snap, "notes").is_empty());
        assert!(names(&snap, "added").is_empty());
        let r = root.to_string_lossy().into_owned();
        assert_eq!(
            names(&snap, ""),
            set(&[r.as_str(), "Docs", "report.pdf", "keep.md"])
        );
        // Removing again changes nothing and publishes nothing.
        let g = live.generation();
        let again = live.apply_changes(&[Change::Remove { path: sub }]);
        assert_eq!(again.unchanged, 1);
        assert_eq!(live.generation(), g);
    }

    #[test]
    fn modified_base_file_is_superseded_and_unchanged_is_a_no_op() {
        let (live, root) = synthetic();
        let keep = root.join("keep.md");
        let same = live.apply_changes(&[Change::Upsert {
            path: keep.clone(),
            meta: file(40),
        }]);
        assert_eq!(same.unchanged, 1);
        assert!(!same.changed());
        live.apply_changes(&[Change::Upsert {
            path: keep.clone(),
            meta: file(99),
        }]);
        let snap = live.snapshot();
        let hits = snap
            .search(&Query::parse("keep").unwrap(), 10, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(snap.meta(hits[0]).size, 99);
        assert_eq!(snap.path(hits[0]), keep);
        assert_eq!(names(&snap, "size:>50"), set(&["keep.md"]));
    }

    #[test]
    fn out_of_scope_excluded_and_orphan_paths_are_skipped() {
        let (live, root) = synthetic();
        let other = if cfg!(windows) {
            r"Q:\elsewhere\x.txt"
        } else {
            "/elsewhere/x.txt"
        };
        let stats = live.apply_changes(&[
            Change::Upsert {
                path: PathBuf::from(other),
                meta: file(1),
            },
            Change::Upsert {
                path: root.join("node_modules"),
                meta: dir(),
            },
            Change::Upsert {
                path: root.join("node_modules").join("pkg.js"),
                meta: file(1),
            },
            Change::Upsert {
                path: root.join("missing").join("orphan.txt"),
                meta: file(1),
            },
        ]);
        assert_eq!(stats.skipped, 4);
        assert!(!stats.changed());
    }

    #[cfg(windows)]
    #[test]
    fn lookup_is_case_insensitive_on_windows() {
        let (live, root) = synthetic();
        let stats = live.apply_changes(&[Change::Remove {
            path: root.join("DOCS").join("REPORT.PDF"),
        }]);
        assert_eq!(stats.removed, 1);
        assert!(names(&live.snapshot(), "report").is_empty());
    }

    #[test]
    fn snapshots_are_isolated_and_ids_resolve_per_snapshot() {
        let (live, root) = synthetic();
        let before = live.snapshot();
        live.apply_changes(&[
            Change::Upsert {
                path: root.join("a.txt"),
                meta: file(1),
            },
            Change::Remove {
                path: root.join("keep.md"),
            },
        ]);
        let after = live.snapshot();
        assert_eq!(names(&before, "keep"), set(&["keep.md"]));
        assert!(names(&before, "a txt").is_empty());
        assert_eq!(names(&after, "^a txt$"), set(&["a.txt"]));
        assert!(names(&after, "keep").is_empty());
        assert!(after.generation() > before.generation());
        // Base ids are stable across batches; the removed one is no longer live.
        let keep_id = before
            .search(&Query::parse("keep").unwrap(), 1, &AtomicBool::new(false))
            .unwrap()[0];
        assert!(before.contains(keep_id) && !after.contains(keep_id));
        assert_eq!(after.path(keep_id), root.join("keep.md"));
    }

    #[test]
    fn limit_counts_delta_first_then_base() {
        let (live, root) = synthetic();
        live.apply_changes(&[Change::Upsert {
            path: root.join("zz.txt"),
            meta: file(1),
        }]);
        let snap = live.snapshot();
        let ids = snap
            .search(&Query::parse("txt").unwrap(), 1, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(snap.name(ids[0]), "zz.txt");
        let all = snap
            .search(&Query::parse("txt").unwrap(), 10, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(all.len(), 2);
        let err = snap
            .search(&Query::parse("txt").unwrap(), 10, &AtomicBool::new(true))
            .unwrap_err();
        assert!(matches!(err, athrix_core::Error::Cancelled));
    }

    #[test]
    fn compaction_preserves_content_and_renumbers() {
        let (live, root) = synthetic();
        live.apply_changes(&[
            Change::Upsert {
                path: root.join("Docs").join("Fresh"),
                meta: dir(),
            },
            Change::Upsert {
                path: root.join("Docs").join("Fresh").join("in fresh.txt"),
                meta: file(3),
            },
            Change::Remove {
                path: root.join("Docs").join("Sub"),
            },
        ]);
        let before = live.snapshot();
        let mut expected: Vec<PathBuf> = search(&before, "");
        expected.sort();
        live.compact();
        let after = live.snapshot();
        assert!(!after.has_changes());
        assert_eq!(after.base().len(), before.len());
        let mut got = search(&after, "");
        got.sort();
        assert_eq!(got, expected);
        assert_eq!(names(&after, "infolder:fresh"), set(&["in fresh.txt"]));
        // Changes keep working on the compacted base.
        live.apply_changes(&[Change::Remove {
            path: root.join("Docs").join("Fresh"),
        }]);
        assert!(names(&live.snapshot(), "fresh").is_empty());
    }

    #[test]
    fn automatic_compaction_past_the_threshold() {
        let (live, root) = synthetic();
        live.set_compaction_thresholds(4, 4);
        let changes: Vec<Change> = (0..6)
            .map(|i| Change::Upsert {
                path: root.join(format!("bulk_{i}.txt")),
                meta: file(i),
            })
            .collect();
        let stats = live.apply_changes(&changes);
        assert!(stats.compacted);
        let snap = live.snapshot();
        assert!(!snap.has_changes());
        assert_eq!(names(&snap, "bulk").len(), 6);
    }

    #[test]
    fn apply_paths_follows_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        fs::create_dir_all(p.join("docs")).unwrap();
        fs::write(p.join("docs").join("old.txt"), b"x").unwrap();
        let opts = WalkOptions::new([p]).exclude_glob("node_modules");
        let live = LiveIndex::new(walk(&opts).unwrap(), &opts).unwrap();

        // A new folder with contents, created inside another new folder: only the deepest file
        // is reported, so the unknown ancestors are resolved and walked.
        fs::create_dir_all(p.join("a").join("b").join("node_modules")).unwrap();
        fs::write(p.join("a").join("b").join("deep.txt"), b"x").unwrap();
        fs::write(p.join("a").join("sibling.md"), b"x").unwrap();
        fs::write(
            p.join("a").join("b").join("node_modules").join("x.js"),
            b"x",
        )
        .unwrap();
        let stats = live.apply_paths(&[p.join("a").join("b").join("deep.txt")]);
        assert!(stats.changed(), "{stats:?}");
        let snap = live.snapshot();
        assert_eq!(names(&snap, "deep"), set(&["deep.txt"]));
        assert_eq!(names(&snap, "sibling"), set(&["sibling.md"]));
        assert!(names(&snap, "node_modules").is_empty());
        assert!(names(&snap, "x js").is_empty());

        // Rename a folder: old path gone, new path walked.
        fs::rename(p.join("a"), p.join("renamed")).unwrap();
        live.apply_paths(&[p.join("a"), p.join("renamed")]);
        let snap = live.snapshot();
        assert_eq!(
            search(&snap, "deep"),
            [p.join("renamed").join("b").join("deep.txt")]
        );

        // Rename and delete base entries.
        fs::rename(
            p.join("docs").join("old.txt"),
            p.join("docs").join("new.txt"),
        )
        .unwrap();
        live.apply_paths(&[
            p.join("docs").join("old.txt"),
            p.join("docs").join("new.txt"),
        ]);
        let snap = live.snapshot();
        assert!(names(&snap, "old").is_empty());
        assert_eq!(names(&snap, "new txt"), set(&["new.txt"]));
        fs::remove_dir_all(p.join("docs")).unwrap();
        live.apply_paths(&[p.join("docs")]);
        let snap = live.snapshot();
        assert!(names(&snap, "docs").is_empty() && names(&snap, "new").is_empty());

        // Excluded paths are skipped without reading the disk.
        let stats = live.apply_paths(&[p.join("node_modules").join("y.js")]);
        assert_eq!(stats.skipped, 1);

        // A folder replaced by a file of the same name.
        fs::remove_dir_all(p.join("renamed")).unwrap();
        fs::write(p.join("renamed"), b"now a file").unwrap();
        live.apply_paths(&[p.join("renamed")]);
        let snap = live.snapshot();
        assert_eq!(names(&snap, "renamed"), set(&["renamed"]));
        assert!(names(&snap, "deep").is_empty());
        let id = snap
            .search(
                &Query::parse("renamed").unwrap(),
                1,
                &AtomicBool::new(false),
            )
            .unwrap()[0];
        assert!(!snap.meta(id).is_dir);
        assert_eq!(snap.meta(id).size, 10);

        // The merged result equals a fresh walk.
        let fresh = walk(&opts).unwrap();
        let mut walked: Vec<PathBuf> = (0..fresh.len() as EntryId).map(|i| fresh.path(i)).collect();
        walked.sort();
        let mut live_paths = search(&snap, "");
        live_paths.sort();
        assert_eq!(live_paths, walked);
    }

    #[test]
    fn save_round_trips_live_entries() {
        let (live, root) = synthetic();
        live.apply_changes(&[Change::Upsert {
            path: root.join("saved.txt"),
            meta: file(1),
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("names.snap");
        live.snapshot().save(&file).unwrap();
        let loaded = FileIndex::load(&file).unwrap();
        assert_eq!(loaded.len(), live.snapshot().len());
        let hits = loaded
            .search(&Query::parse("saved").unwrap(), 10, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(loaded.path(hits[0]), root.join("saved.txt"));
    }
}
