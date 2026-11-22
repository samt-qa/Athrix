//! The in-memory name index: compact per-entry columns plus a table of distinct names.
//!
//! # Layout
//!
//! Most file names repeat (`index.js`, `cover.jpg`, `META-INF`...): on a real 7.35M-entry
//! index only a third of the names are distinct. So names are **interned**: every distinct
//! original name is stored once in a [`Names`] table together with its normalized form, and
//! each entry holds the 4-byte id of its name. Search evaluates name filters once per distinct
//! name (see `search.rs`), and the per-entry columns are packed:
//!
//! | column | bytes/entry | note |
//! |---|---|---|
//! | name id | 4 | into [`Names`] |
//! | parent | 4 | `NO_PARENT` for a root |
//! | flags | 1 | directory, symlink |
//! | size | 4 | `u32`; larger sizes in a sparse overflow list |
//! | modified, created | 4 + 4 | Unix seconds as `u32` (1970..2106); others in an overflow list |
//!
//! That is 21 bytes per entry plus the distinct names (original + normalized text and two
//! 4-byte offsets each), against 78 bytes per entry for the former layout of two full name
//! arenas, `u64` sizes and `i64` times.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;

/// Identifier of an entry in a [`FileIndex`]. Ids are dense (`0..len`) and follow walk order.
pub type EntryId = u32;

/// Marks a root entry (no parent).
pub(crate) const NO_PARENT: u32 = u32::MAX;

pub(crate) const FLAG_DIR: u8 = 1;
pub(crate) const FLAG_SYMLINK: u8 = 1 << 1;

/// Stored time column value: unknown.
pub(crate) const TIME_NONE: u32 = u32::MAX;
/// Stored time column value: outside `0..TIME_BIG`, kept in the overflow list.
pub(crate) const TIME_BIG: u32 = u32::MAX - 1;
/// Stored size column value: kept in the overflow list.
pub(crate) const SIZE_BIG: u32 = u32::MAX;

/// Metadata stored for every entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryMeta {
    /// The entry is a directory.
    pub is_dir: bool,
    /// The entry is a symbolic link or junction (never followed by default).
    pub is_symlink: bool,
    /// Size in bytes (0 for directories and when unknown).
    pub size: u64,
    /// Last modification time in seconds since the Unix epoch, if known.
    pub modified: Option<i64>,
    /// Creation time in seconds since the Unix epoch, if known.
    pub created: Option<i64>,
}

/// Append-only string arena: all strings are concatenated, `ends[i]` is the end offset of string `i`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Arena {
    pub(crate) text: String,
    pub(crate) ends: Vec<u32>,
}

impl Arena {
    fn push(&mut self, s: &str) {
        self.text.push_str(s);
        let end = u32::try_from(self.text.len()).expect("name arena exceeds 4 GiB");
        self.ends.push(end);
    }

    #[inline]
    pub(crate) fn get(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.text[start..self.ends[i] as usize]
    }

    pub(crate) fn len(&self) -> usize {
        self.ends.len()
    }

    fn heap_bytes(&self) -> usize {
        self.text.capacity() + self.ends.capacity() * size_of::<u32>()
    }

    fn shrink(&mut self) {
        self.text.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}

/// The distinct names of an index: original `orig[i]` normalizes to `norm[i]`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Names {
    pub(crate) orig: Arena,
    pub(crate) norm: Arena,
}

impl Names {
    pub(crate) fn len(&self) -> usize {
        self.orig.len()
    }
}

/// Values that do not fit their packed column, sorted by entry id (entries are pushed in id
/// order). Rare: sizes of 4 GiB or more, times before 1970 or after 2106.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Overflow {
    pub(crate) sizes: Vec<(EntryId, u64)>,
    pub(crate) mtimes: Vec<(EntryId, i64)>,
    pub(crate) ctimes: Vec<(EntryId, i64)>,
}

fn overflow_get<T: Copy + Default>(list: &[(EntryId, T)], id: EntryId) -> T {
    list.binary_search_by_key(&id, |&(i, _)| i)
        .map_or_else(|_| T::default(), |at| list[at].1)
}

/// Packs a time into its column value, recording it in `big` if it does not fit.
pub(crate) fn pack_time(t: Option<i64>, id: EntryId, big: &mut Vec<(EntryId, i64)>) -> u32 {
    match t {
        None => TIME_NONE,
        Some(t) => match u32::try_from(t) {
            Ok(v) if v < TIME_BIG => v,
            _ => {
                big.push((id, t));
                TIME_BIG
            }
        },
    }
}

/// Packs a size into its column value, recording it in `big` if it does not fit.
pub(crate) fn pack_size(s: u64, id: EntryId, big: &mut Vec<(EntryId, u64)>) -> u32 {
    match u32::try_from(s) {
        Ok(v) if v < SIZE_BIG => v,
        _ => {
            big.push((id, s));
            SIZE_BIG
        }
    }
}

fn unpack_time(v: u32, id: EntryId, big: &[(EntryId, i64)]) -> Option<i64> {
    match v {
        TIME_NONE => None,
        TIME_BIG => Some(overflow_get(big, id)),
        v => Some(i64::from(v)),
    }
}

/// Per-query scratch tables over the distinct names (see `search.rs`), kept for reuse so a
/// search does not allocate and clear one every keystroke. Each table is tagged with an epoch
/// that changes per use, so stale cells read as "unknown" without clearing.
#[derive(Default)]
pub(crate) struct MemoPool(pub(crate) Mutex<Vec<NameMemo>>);

/// One memo table: a cell per distinct name, `epoch << 2 | state`.
pub(crate) struct NameMemo {
    pub(crate) cells: Box<[AtomicU8]>,
    pub(crate) epoch: u8,
}

impl Clone for MemoPool {
    fn clone(&self) -> Self {
        MemoPool::default()
    }
}

impl std::fmt::Debug for MemoPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoPool")
    }
}

/// An in-memory index of file and folder names.
///
/// Entries are stored column-wise (name id, parent id, flags, size, times) and the distinct
/// names live in one table holding each original name (for display and path reconstruction)
/// and its normalized form (see [`athrix_text_ar::normalize_name_into`]) that search matches,
/// so a query never re-normalizes the index. Full paths are rebuilt from parent links on
/// demand. See the `index` module docs (in the source) for the byte budget.
///
/// Build one with [`IndexBuilder`] or [`crate::walk`].
#[derive(Debug, Default, Clone)]
pub struct FileIndex {
    pub(crate) names: Names,
    pub(crate) name_ids: Vec<u32>,
    pub(crate) parents: Vec<u32>,
    pub(crate) flags: Vec<u8>,
    pub(crate) sizes: Vec<u32>,
    pub(crate) mtimes: Vec<u32>,
    pub(crate) ctimes: Vec<u32>,
    pub(crate) overflow: Overflow,
    pub(crate) memos: MemoPool,
}

impl FileIndex {
    /// Number of entries.
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    /// True if the index has no entries.
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Number of distinct names (entries share the storage of equal names).
    pub fn distinct_names(&self) -> usize {
        self.names.len()
    }

    /// The original name of `id`. For a root this is the full root path as given to the walker.
    ///
    /// # Panics
    /// If `id` is out of range (as for all per-entry accessors).
    #[inline]
    pub fn name(&self, id: EntryId) -> &str {
        self.names.orig.get(self.name_ids[id as usize] as usize)
    }

    /// The normalized name of `id`, as matched by search.
    #[inline]
    pub fn normalized_name(&self, id: EntryId) -> &str {
        self.names.norm.get(self.name_ids[id as usize] as usize)
    }

    /// The parent of `id`, or `None` for a root.
    #[inline]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        match self.parents[id as usize] {
            NO_PARENT => None,
            p => Some(p),
        }
    }

    /// True if `id` is a directory (cheaper than [`FileIndex::meta`]).
    #[inline]
    pub fn is_dir(&self, id: EntryId) -> bool {
        self.flags[id as usize] & FLAG_DIR != 0
    }

    /// Size of `id` in bytes (cheaper than [`FileIndex::meta`]).
    #[inline]
    pub fn size(&self, id: EntryId) -> u64 {
        match self.sizes[id as usize] {
            SIZE_BIG => overflow_get(&self.overflow.sizes, id),
            v => u64::from(v),
        }
    }

    /// Modification time of `id` (cheaper than [`FileIndex::meta`]).
    #[inline]
    pub fn modified(&self, id: EntryId) -> Option<i64> {
        unpack_time(self.mtimes[id as usize], id, &self.overflow.mtimes)
    }

    /// Creation time of `id` (cheaper than [`FileIndex::meta`]).
    #[inline]
    pub fn created(&self, id: EntryId) -> Option<i64> {
        unpack_time(self.ctimes[id as usize], id, &self.overflow.ctimes)
    }

    /// Metadata of `id`.
    pub fn meta(&self, id: EntryId) -> EntryMeta {
        let flags = self.flags[id as usize];
        EntryMeta {
            is_dir: flags & FLAG_DIR != 0,
            is_symlink: flags & FLAG_SYMLINK != 0,
            size: self.size(id),
            modified: self.modified(id),
            created: self.created(id),
        }
    }

    /// Rebuilds the full path of `id` by following parent links up to its root.
    pub fn path(&self, id: EntryId) -> PathBuf {
        let mut chain = vec![id];
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            chain.push(p);
            cur = p;
        }
        let mut path = PathBuf::new();
        for &e in chain.iter().rev() {
            path.push(self.name(e));
        }
        path
    }

    /// Approximate heap memory used by the index, in bytes.
    pub fn heap_bytes(&self) -> usize {
        let o = &self.overflow;
        self.names.orig.heap_bytes()
            + self.names.norm.heap_bytes()
            + (self.name_ids.capacity()
                + self.parents.capacity()
                + self.sizes.capacity()
                + self.mtimes.capacity()
                + self.ctimes.capacity())
                * size_of::<u32>()
            + self.flags.capacity()
            + o.sizes.capacity() * size_of::<(EntryId, u64)>()
            + (o.mtimes.capacity() + o.ctimes.capacity()) * size_of::<(EntryId, i64)>()
    }

    /// Builds one index from `parts`, in order: every entry of each part, except the roots
    /// (entries without a parent, named by their full path) that `keep_root(part, name)` rejects
    /// (`part` = position in `parts`), with their whole subtrees. Ids are renumbered; names
    /// keep their normalized forms (no re-normalizing), so this costs one copy of the entries.
    ///
    /// Used to add or drop whole roots (a drive plugged in or removed) without walking the
    /// other roots again. Each part must hold parents before children, as every index does.
    pub fn concat(
        parts: &[&FileIndex],
        mut keep_root: impl FnMut(usize, &str) -> bool,
    ) -> FileIndex {
        let entries: usize = parts.iter().map(|p| p.len()).sum();
        let mut b = IndexBuilder::with_capacity(entries, 0);
        for (n, part) in parts.iter().enumerate() {
            b.push_part(part, |name| keep_root(n, name));
        }
        b.finish()
    }
}

/// Distinct-name ids of a source index already interned in a builder (see
/// [`IndexBuilder::copy_entry`]), so copying an entry whose name was seen costs no hashing.
#[derive(Debug, Default)]
pub(crate) struct NameRemap(Vec<u32>);

/// Builds a [`FileIndex`] one entry at a time. Used by the walker; also handy for tests,
/// benchmarks and future sources such as the NTFS MFT reader.
#[derive(Debug, Default)]
pub struct IndexBuilder {
    index: FileIndex,
    buf: String,
    /// Open-addressing hash table of distinct-name ids (`u32::MAX` = empty slot), keyed by the
    /// original name. Its length is a power of two at least twice the number of names.
    table: Vec<u32>,
}

/// Hash of a name for the intern table (FNV-1a over 8-byte words, then a final mix).
fn name_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ s.len() as u64;
    let mut chunks = s.as_bytes().chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
    }
    for &b in chunks.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 32;
    h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^ (h >> 29)
}

const EMPTY_SLOT: u32 = u32::MAX;

impl IndexBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder with room for `entries` entries averaging `name_bytes` bytes each.
    /// (Equal names are stored once, so the name space reserved is a third of that.)
    pub fn with_capacity(entries: usize, name_bytes: usize) -> Self {
        let mut b = Self::default();
        let distinct = entries / 3;
        let ix = &mut b.index;
        ix.names.orig.text.reserve(distinct * name_bytes);
        ix.names.orig.ends.reserve(distinct);
        ix.names.norm.text.reserve(distinct * name_bytes);
        ix.names.norm.ends.reserve(distinct);
        ix.name_ids.reserve(entries);
        ix.parents.reserve(entries);
        ix.flags.reserve(entries);
        ix.sizes.reserve(entries);
        ix.mtimes.reserve(entries);
        ix.ctimes.reserve(entries);
        b
    }

    /// Number of entries pushed so far.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// True if nothing was pushed yet.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Adds an entry and returns its id. `parent` must be an id returned earlier (or `None`
    /// for a root, whose `name` is then the full root path).
    ///
    /// # Panics
    /// If `parent` is not an existing id, or beyond `u32::MAX - 1` entries / 4 GiB of names.
    pub fn push(&mut self, parent: Option<EntryId>, name: &str, meta: EntryMeta) -> EntryId {
        let hash = name_hash(name);
        let nid = match self.find(name, hash) {
            Ok(nid) => nid,
            Err(slot) => {
                let mut buf = std::mem::take(&mut self.buf);
                athrix_text_ar::normalize_name_into(name, &mut buf);
                let nid = self.insert_name(slot, name, &buf);
                self.buf = buf;
                nid
            }
        };
        self.push_entry(parent, nid, meta)
    }

    /// Like [`IndexBuilder::push`], with `normalized` already computed by
    /// [`athrix_text_ar::normalize_name_into`] (live updates).
    pub(crate) fn push_normalized(
        &mut self,
        parent: Option<EntryId>,
        name: &str,
        normalized: &str,
        meta: EntryMeta,
    ) -> EntryId {
        let nid = self.intern(name, normalized);
        self.push_entry(parent, nid, meta)
    }

    /// Copies entry `id` of `src` (its name, normalized name and metadata) under `parent`.
    /// `names` caches the names of `src` already interned here: use one per source index.
    pub(crate) fn copy_entry(
        &mut self,
        src: &FileIndex,
        id: EntryId,
        parent: Option<EntryId>,
        names: &mut NameRemap,
    ) -> EntryId {
        let src_nid = src.name_ids[id as usize] as usize;
        if names.0.len() < src.names.len() {
            names.0.resize(src.names.len(), EMPTY_SLOT);
        }
        let nid = match names.0[src_nid] {
            EMPTY_SLOT => {
                let nid = self.intern(src.names.orig.get(src_nid), src.names.norm.get(src_nid));
                names.0[src_nid] = nid;
                nid
            }
            nid => nid,
        };
        self.push_entry(parent, nid, src.meta(id))
    }

    /// Copies every entry of `part` except the roots `keep_root(name)` rejects, with their
    /// subtrees (see [`FileIndex::concat`]).
    pub(crate) fn push_part(&mut self, part: &FileIndex, mut keep_root: impl FnMut(&str) -> bool) {
        let mut remap = vec![NO_PARENT; part.len()];
        let mut names = NameRemap::default();
        for id in 0..part.len() as EntryId {
            let parent = match part.parent(id) {
                None if !keep_root(part.name(id)) => continue,
                None => None,
                Some(p) => match remap[p as usize] {
                    NO_PARENT => continue,
                    p => Some(p),
                },
            };
            remap[id as usize] = self.copy_entry(part, id, parent, &mut names);
        }
    }

    /// The distinct-name id of `name`, added with `normalized` if new.
    fn intern(&mut self, name: &str, normalized: &str) -> u32 {
        let hash = name_hash(name);
        match self.find(name, hash) {
            Ok(nid) => nid,
            Err(slot) => self.insert_name(slot, name, normalized),
        }
    }

    /// `Ok(id)` of `name`, or `Err(empty slot)` where to insert it.
    fn find(&mut self, name: &str, hash: u64) -> Result<u32, usize> {
        if self.table.len() < (self.index.names.len() + 1) * 2 {
            self.grow();
        }
        let mask = self.table.len() - 1;
        let mut slot = hash as usize & mask;
        loop {
            match self.table[slot] {
                EMPTY_SLOT => return Err(slot),
                nid if self.index.names.orig.get(nid as usize) == name => return Ok(nid),
                _ => slot = (slot + 1) & mask,
            }
        }
    }

    fn insert_name(&mut self, slot: usize, name: &str, normalized: &str) -> u32 {
        let names = &mut self.index.names;
        let nid = u32::try_from(names.len()).expect("too many names");
        assert!(nid != EMPTY_SLOT, "too many names");
        names.orig.push(name);
        names.norm.push(normalized);
        self.table[slot] = nid;
        nid
    }

    /// Doubles the intern table (at least 1024 slots) and re-inserts every name.
    fn grow(&mut self) {
        let size = (self.table.len() * 2).max(1024);
        self.table = vec![EMPTY_SLOT; size];
        let mask = size - 1;
        for nid in 0..self.index.names.len() {
            let mut slot = name_hash(self.index.names.orig.get(nid)) as usize & mask;
            while self.table[slot] != EMPTY_SLOT {
                slot = (slot + 1) & mask;
            }
            self.table[slot] = nid as u32;
        }
    }

    fn push_entry(&mut self, parent: Option<EntryId>, nid: u32, meta: EntryMeta) -> EntryId {
        let id = u32::try_from(self.index.len()).expect("too many entries");
        assert!(id != NO_PARENT, "too many entries");
        let parent = match parent {
            Some(p) => {
                assert!(p < id, "parent {p} does not exist");
                p
            }
            None => NO_PARENT,
        };
        let ix = &mut self.index;
        ix.name_ids.push(nid);
        ix.parents.push(parent);
        ix.flags
            .push((u8::from(meta.is_dir) * FLAG_DIR) | (u8::from(meta.is_symlink) * FLAG_SYMLINK));
        let o = &mut ix.overflow;
        ix.sizes.push(pack_size(meta.size, id, &mut o.sizes));
        ix.mtimes.push(pack_time(meta.modified, id, &mut o.mtimes));
        ix.ctimes.push(pack_time(meta.created, id, &mut o.ctimes));
        id
    }

    /// Finishes the index, releasing spare capacity.
    pub fn finish(mut self) -> FileIndex {
        self.table = Vec::new();
        let ix = &mut self.index;
        ix.names.orig.shrink();
        ix.names.norm.shrink();
        ix.name_ids.shrink_to_fit();
        ix.parents.shrink_to_fit();
        ix.flags.shrink_to_fit();
        ix.sizes.shrink_to_fit();
        ix.mtimes.shrink_to_fit();
        ix.ctimes.shrink_to_fit();
        ix.overflow.sizes.shrink_to_fit();
        ix.overflow.mtimes.shrink_to_fit();
        ix.overflow.ctimes.shrink_to_fit();
        self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..Default::default()
        }
    }

    #[test]
    fn stores_names_metadata_and_rebuilds_paths() {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/data", dir());
        let docs = b.push(Some(root), "Docs", dir());
        let file = b.push(
            Some(docs),
            "مُحَاضَرَةُ.PDF",
            EntryMeta {
                size: 42,
                modified: Some(1_700_000_000),
                created: Some(1_600_000_000),
                ..Default::default()
            },
        );
        let ix = b.finish();
        assert_eq!(ix.len(), 3);
        assert_eq!(ix.name(file), "مُحَاضَرَةُ.PDF");
        assert_eq!(ix.normalized_name(file), "محاضره pdf");
        assert_eq!(ix.parent(file), Some(docs));
        assert_eq!(ix.parent(root), None);
        assert!(ix.meta(docs).is_dir);
        assert!(ix.is_dir(docs) && !ix.is_dir(file));
        assert_eq!(ix.meta(file).size, 42);
        assert_eq!(ix.meta(file).modified, Some(1_700_000_000));
        assert_eq!(ix.meta(file).created, Some(1_600_000_000));
        assert_eq!(ix.meta(root).modified, None);
        assert_eq!(ix.meta(root).created, None);
        let expected: PathBuf = ["/data", "Docs", "مُحَاضَرَةُ.PDF"].iter().collect();
        assert_eq!(ix.path(file), expected);
        assert!(ix.heap_bytes() > 0);
    }

    #[test]
    fn equal_names_are_stored_once() {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/r", dir());
        let mut ids = Vec::new();
        for i in 0..3000u32 {
            let d = b.push(Some(root), &format!("d{i}"), dir());
            ids.push(b.push(Some(d), "index.js", EntryMeta::default()));
            ids.push(b.push(Some(d), "Index.js", EntryMeta::default()));
        }
        let ix = b.finish();
        assert_eq!(ix.distinct_names(), 1 + 3000 + 2);
        for pair in ids.chunks(2) {
            assert_eq!(ix.name(pair[0]), "index.js");
            assert_eq!(ix.name(pair[1]), "Index.js");
            assert_eq!(ix.normalized_name(pair[1]), "index js");
        }
    }

    #[test]
    fn out_of_range_values_round_trip_through_the_overflow_lists() {
        let cases = [
            (0u64, Some(0i64), Some(1i64)),
            (u64::from(u32::MAX) - 1, Some(i64::from(u32::MAX) - 2), None),
            (u64::from(u32::MAX), Some(i64::from(u32::MAX) - 1), Some(-5)),
            (u64::MAX, Some(i64::MAX), Some(i64::MIN)),
            (5 << 40, None, Some(i64::from(u32::MAX))),
        ];
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/r", dir());
        let ids: Vec<EntryId> = cases
            .iter()
            .map(|&(size, modified, created)| {
                b.push(
                    Some(root),
                    "f",
                    EntryMeta {
                        size,
                        modified,
                        created,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let ix = b.finish();
        for (&id, &(size, modified, created)) in ids.iter().zip(&cases) {
            let m = ix.meta(id);
            assert_eq!((m.size, m.modified, m.created), (size, modified, created));
        }
    }

    #[test]
    fn concat_keeps_chosen_roots_with_their_subtrees() {
        let mut b = IndexBuilder::new();
        let c = b.push(None, "C:\\", dir());
        let e = b.push(None, "E:\\", dir());
        let docs = b.push(Some(c), "Docs", dir());
        b.push(Some(e), "قديم.txt", EntryMeta::default());
        b.push(Some(docs), "a.txt", EntryMeta::default());
        let first = b.finish();
        let mut b = IndexBuilder::new();
        let e2 = b.push(None, "E:\\", dir());
        b.push(
            Some(e2),
            "جديد.TXT",
            EntryMeta {
                size: 7,
                created: Some(9),
                ..Default::default()
            },
        );
        let second = b.finish();

        let merged = FileIndex::concat(&[&first, &second], |_, root| root != "E:\\");
        let paths: Vec<PathBuf> = (0..merged.len() as EntryId)
            .map(|id| merged.path(id))
            .collect();
        let p = |parts: &[&str]| parts.iter().collect::<PathBuf>();
        assert_eq!(
            paths,
            [
                p(&["C:\\"]),
                p(&["C:\\", "Docs"]),
                p(&["C:\\", "Docs", "a.txt"]),
            ],
            "the root filter applies to every part"
        );
        let merged =
            FileIndex::concat(&[&first, &second], |part, root| part == 1 || root != "E:\\");
        assert_eq!(merged.len(), 5, "old E:\\ dropped, new E:\\ added");
        assert_eq!(merged.name(4), "جديد.TXT");
        assert_eq!(merged.normalized_name(4), "جديد txt");
        assert_eq!(merged.meta(4).size, 7);
        assert_eq!(merged.meta(4).created, Some(9));
        assert_eq!(merged.parent(4), Some(3));
        assert_eq!(merged.parent(3), None);
        assert_eq!(merged.distinct_names(), 5, "E:\\ is shared by both parts");
    }
}
