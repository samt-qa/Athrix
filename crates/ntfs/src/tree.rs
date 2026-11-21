//! Rebuilds the directory tree of a volume from flat `(file ref, parent ref, name)` records.
//!
//! [`TreeBuilder`] collects records in whatever order the file system returns them (MFT order),
//! then [`TreeBuilder::finish`] resolves parent references and stores the entries breadth-first,
//! so that **every parent precedes its children** (index 0 is the volume root). Entries whose
//! parent chain does not reach the root (NTFS metadata under `$Extend`, files deleted during
//! the scan, stale sequence numbers) are dropped and counted in
//! [`VolumeTree::orphan_count`].

use std::collections::HashMap;
use std::path::PathBuf;

use crate::record::UsnRecord;
use crate::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, RECORD_NUMBER_MASK, ROOT_RECORD_NUMBER,
};

/// Parent index of the root entry.
pub const NO_PARENT: u32 = u32::MAX;
/// Stored modification time meaning "unknown".
const NO_MTIME: i64 = i64::MIN;

/// Collects USN records of one volume.
#[derive(Debug, Default)]
pub struct TreeBuilder {
    refs: Vec<u64>,
    parent_refs: Vec<u64>,
    attributes: Vec<u32>,
    names: String,
    ends: Vec<u32>,
    root_ref: Option<u64>,
}

impl TreeBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder with room for `entries` records of ~`name_bytes` UTF-8 bytes each.
    pub fn with_capacity(entries: usize, name_bytes: usize) -> Self {
        Self {
            refs: Vec::with_capacity(entries),
            parent_refs: Vec::with_capacity(entries),
            attributes: Vec::with_capacity(entries),
            names: String::with_capacity(entries * name_bytes),
            ends: Vec::with_capacity(entries),
            root_ref: None,
        }
    }

    /// Number of records pushed (including the root record if it was seen).
    pub fn len(&self) -> usize {
        self.refs.len() + usize::from(self.root_ref.is_some())
    }

    /// True if nothing was pushed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Adds a decoded USN record.
    pub fn push_record(&mut self, rec: &UsnRecord<'_>) {
        let units = rec
            .name_utf16le
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]));
        self.push_with(rec.file_ref, rec.parent_ref, rec.attributes, |s| {
            s.extend(char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)));
        });
    }

    /// Adds a record with an already decoded name.
    pub fn push(&mut self, file_ref: u64, parent_ref: u64, attributes: u32, name: &str) {
        self.push_with(file_ref, parent_ref, attributes, |s| s.push_str(name));
    }

    fn push_with(
        &mut self,
        file_ref: u64,
        parent_ref: u64,
        attributes: u32,
        write_name: impl FnOnce(&mut String),
    ) {
        if file_ref & RECORD_NUMBER_MASK == ROOT_RECORD_NUMBER {
            self.root_ref = Some(file_ref);
            return;
        }
        let Ok(_) = u32::try_from(self.refs.len() + 1) else {
            return; // > 4 billion records: impossible on NTFS (2^32 MFT records max)
        };
        let before = self.names.len();
        write_name(&mut self.names);
        let Ok(end) = u32::try_from(self.names.len()) else {
            self.names.truncate(before); // > 4 GiB of names: drop the entry
            return;
        };
        self.ends.push(end);
        self.refs.push(file_ref);
        self.parent_refs.push(parent_ref);
        self.attributes.push(attributes);
    }

    fn raw_name(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.names[start..self.ends[i] as usize]
    }

    /// Resolves parents and orders entries breadth-first under a root named `root_name`
    /// (typically `C:\`).
    pub fn finish(self, root_name: &str) -> VolumeTree {
        let n = self.refs.len();
        let root = n; // virtual raw index of the root
        let lookup = RefLookup::build(&self.refs);

        // Parent raw index per entry, `None` for orphans.
        let parent_of: Vec<Option<usize>> = (0..n)
            .map(|i| {
                let p = self.parent_refs[i];
                if p & RECORD_NUMBER_MASK == ROOT_RECORD_NUMBER {
                    return Some(root);
                }
                lookup.get(p).map(|j| j as usize).filter(|&j| j != i)
            })
            .collect();

        // Children in CSR form over n + 1 nodes (raw entries plus the root at index n):
        // count into offsets[p + 2], prefix-sum, then fill using offsets[p + 1] as cursor.
        let mut offsets = vec![0u32; n + 3];
        for p in parent_of.iter().flatten() {
            offsets[p + 2] += 1;
        }
        for k in 2..offsets.len() {
            offsets[k] += offsets[k - 1];
        }
        let mut children = vec![0u32; offsets[n + 2] as usize];
        for (i, p) in parent_of.iter().enumerate() {
            if let Some(p) = *p {
                let slot = &mut offsets[p + 1];
                children[*slot as usize] = i as u32;
                *slot += 1;
            }
        }
        // offsets[p]..offsets[p + 1] are now p's children.

        // Breadth-first order; `order[k]` is the raw index of new id k.
        let mut order: Vec<usize> = Vec::with_capacity(n + 1);
        let mut new_id = vec![NO_PARENT; n + 1];
        order.push(root);
        new_id[root] = 0;
        let mut head = 0;
        while head < order.len() {
            let node = order[head];
            head += 1;
            for &c in &children[offsets[node] as usize..offsets[node + 1] as usize] {
                let c = c as usize;
                if new_id[c] == NO_PARENT {
                    new_id[c] = order.len() as u32;
                    order.push(c);
                }
            }
        }
        drop(children);
        drop(offsets);

        let len = order.len();
        let mut tree = VolumeTree {
            names: String::with_capacity(self.names.len() + root_name.len()),
            ends: Vec::with_capacity(len),
            parents: Vec::with_capacity(len),
            file_refs: Vec::with_capacity(len),
            attributes: Vec::with_capacity(len),
            sizes: vec![0; len],
            mtimes: vec![NO_MTIME; len],
            orphans: n + 1 - len,
            has_metadata: false,
        };
        for &raw in &order {
            if raw == root {
                tree.push_entry(
                    root_name,
                    NO_PARENT,
                    self.root_ref
                        .unwrap_or(ROOT_RECORD_NUMBER | (ROOT_RECORD_NUMBER << 48)),
                    FILE_ATTRIBUTE_DIRECTORY,
                );
            } else {
                let parent = parent_of[raw].map_or(NO_PARENT, |p| new_id[p]);
                tree.push_entry(
                    self.raw_name(raw),
                    parent,
                    self.refs[raw],
                    self.attributes[raw],
                );
            }
        }
        tree
    }
}

/// Maps 64-bit file references to entry indexes (dense table over MFT record numbers when
/// compact enough, hash map otherwise). Lookups verify the sequence number.
#[derive(Debug, Clone)]
pub struct RefLookup {
    map: RefMap,
    refs_by_index: Vec<u64>,
}

#[derive(Debug, Clone)]
enum RefMap {
    Dense(Vec<u32>),
    Sparse(HashMap<u64, u32>),
}

impl RefLookup {
    fn build(refs: &[u64]) -> Self {
        let max = refs
            .iter()
            .map(|r| r & RECORD_NUMBER_MASK)
            .max()
            .unwrap_or(0);
        let dense_limit = (refs.len() as u64)
            .saturating_mul(4)
            .saturating_add(1 << 20);
        let map = if max < dense_limit {
            let mut v = vec![NO_PARENT; max as usize + 1];
            for (i, r) in refs.iter().enumerate() {
                v[(r & RECORD_NUMBER_MASK) as usize] = i as u32;
            }
            RefMap::Dense(v)
        } else {
            RefMap::Sparse(
                refs.iter()
                    .enumerate()
                    .map(|(i, r)| (r & RECORD_NUMBER_MASK, i as u32))
                    .collect(),
            )
        };
        Self {
            map,
            refs_by_index: refs.to_vec(),
        }
    }

    /// Index of the entry for MFT record `record` (any sequence number).
    pub fn get_record(&self, record: u64) -> Option<u32> {
        match &self.map {
            RefMap::Dense(v) => usize::try_from(record)
                .ok()
                .and_then(|r| v.get(r))
                .copied()
                .filter(|&i| i != NO_PARENT),
            RefMap::Sparse(m) => m.get(&record).copied(),
        }
    }

    /// Index of the entry with exactly this file reference (record number and sequence).
    pub fn get(&self, file_ref: u64) -> Option<u32> {
        self.get_record(file_ref & RECORD_NUMBER_MASK)
            .filter(|&i| self.refs_by_index[i as usize] == file_ref)
    }
}

/// The entries of one volume, parents before children; index 0 is the root.
#[derive(Debug, Clone, Default)]
pub struct VolumeTree {
    names: String,
    ends: Vec<u32>,
    parents: Vec<u32>,
    file_refs: Vec<u64>,
    attributes: Vec<u32>,
    sizes: Vec<u64>,
    mtimes: Vec<i64>,
    orphans: usize,
    has_metadata: bool,
}

/// A borrowed view of one entry of a [`VolumeTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryRef<'a> {
    /// Index of the entry.
    pub index: u32,
    /// Index of the parent, `None` for the root.
    pub parent: Option<u32>,
    /// Name (full root path such as `C:\` for the root).
    pub name: &'a str,
    /// NTFS file reference number.
    pub file_ref: u64,
    /// `FILE_ATTRIBUTE_*` flags.
    pub attributes: u32,
    /// Logical size in bytes, if metadata was read (0 for directories).
    pub size: Option<u64>,
    /// Last modification time (seconds since the Unix epoch), if metadata was read.
    pub modified: Option<i64>,
}

impl EntryRef<'_> {
    /// True if the entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// True if the entry is a reparse point (symlink, junction, mount point, but also cloud
    /// placeholders and dedup files: the tag is not part of USN records).
    pub fn is_reparse_point(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

impl VolumeTree {
    fn push_entry(&mut self, name: &str, parent: u32, file_ref: u64, attributes: u32) {
        self.names.push_str(name);
        // Cannot overflow: the builder capped names at 4 GiB, plus one root name.
        self.ends
            .push(u32::try_from(self.names.len()).unwrap_or(u32::MAX));
        self.parents.push(parent);
        self.file_refs.push(file_ref);
        self.attributes.push(attributes);
    }

    /// Number of entries, including the root.
    pub fn len(&self) -> usize {
        self.parents.len()
    }

    /// True if there are no entries (not even a root).
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }

    /// Records dropped because their parent chain did not reach the root.
    pub fn orphan_count(&self) -> usize {
        self.orphans
    }

    /// True if sizes and modification times were filled from the MFT.
    pub fn has_metadata(&self) -> bool {
        self.has_metadata
    }

    /// Total bytes of all names (UTF-8).
    pub fn name_bytes(&self) -> usize {
        self.names.len()
    }

    /// Name of entry `i`.
    ///
    /// # Panics
    /// If `i` is out of range (as all per-entry accessors).
    pub fn name(&self, i: u32) -> &str {
        let i = i as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.names[start..self.ends[i] as usize]
    }

    /// Parent of entry `i`; always `< i`, `None` for the root.
    pub fn parent(&self, i: u32) -> Option<u32> {
        Some(self.parents[i as usize]).filter(|&p| p != NO_PARENT)
    }

    /// The borrowed view of entry `i`.
    pub fn get(&self, i: u32) -> EntryRef<'_> {
        let k = i as usize;
        EntryRef {
            index: i,
            parent: self.parent(i),
            name: self.name(i),
            file_ref: self.file_refs[k],
            attributes: self.attributes[k],
            size: self.has_metadata.then_some(self.sizes[k]),
            modified: Some(self.mtimes[k]).filter(|&m| m != NO_MTIME),
        }
    }

    /// All entries in index order (parents before children).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = EntryRef<'_>> + '_ {
        (0..self.len() as u32).map(|i| self.get(i))
    }

    /// Full path of entry `i`, rebuilt from parent links.
    pub fn path(&self, i: u32) -> PathBuf {
        let mut chain = vec![i];
        let mut cur = i;
        while let Some(p) = self.parent(cur) {
            chain.push(p);
            cur = p;
        }
        chain.iter().rev().map(|&e| self.name(e)).collect()
    }

    /// File-reference lookup over this tree's entries (for metadata merges and journal updates).
    pub fn ref_lookup(&self) -> RefLookup {
        RefLookup::build(&self.file_refs)
    }

    /// Sets the size and/or modification time of entry `i` (used by the MFT reader); `None`
    /// leaves a field unchanged.
    pub(crate) fn set_metadata(&mut self, i: u32, size: Option<u64>, mtime: Option<i64>) {
        let k = i as usize;
        if let (Some(s), Some(slot)) = (size, self.sizes.get_mut(k)) {
            *slot = s;
        }
        if let (Some(m), Some(slot)) = (mtime, self.mtimes.get_mut(k)) {
            *slot = m;
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))] // only the Windows MFT reader sets it
    pub(crate) fn mark_metadata(&mut self) {
        self.has_metadata = true;
    }

    /// Approximate heap bytes used.
    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.ends.capacity() * 4
            + self.parents.capacity() * 4
            + self.file_refs.capacity() * 8
            + self.attributes.capacity() * 4
            + self.sizes.capacity() * 8
            + self.mtimes.capacity() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    fn r(rec: u64, seq: u64) -> u64 {
        (seq << 48) | rec
    }
    const ROOT: u64 = (5 << 48) | 5;

    #[test]
    fn rebuilds_paths_from_mft_order() {
        let mut b = TreeBuilder::new();
        // Children before parents, as MFT order allows.
        b.push(r(40, 1), r(30, 2), 0x20, "notes.txt");
        b.push(r(30, 2), r(20, 1), DIR, "docs");
        b.push(r(20, 1), ROOT, DIR, "Users");
        b.push(ROOT, ROOT, DIR, ".");
        b.push(r(21, 1), ROOT, 0x20, "pagefile.sys");
        let t = b.finish("C:\\");
        assert_eq!(t.len(), 5);
        assert_eq!(t.orphan_count(), 0);
        assert_eq!(t.name(0), "C:\\");
        assert_eq!(t.get(0).file_ref, ROOT);
        for i in 1..t.len() as u32 {
            assert!(t.parent(i).unwrap() < i, "parents precede children");
        }
        let paths: Vec<_> = (0..t.len() as u32).map(|i| t.path(i)).collect();
        assert!(paths.contains(&PathBuf::from(r"C:\Users\docs\notes.txt")));
        assert!(paths.contains(&PathBuf::from(r"C:\pagefile.sys")));
        let docs = t.iter().find(|e| e.name == "docs").unwrap();
        assert!(docs.is_dir());
        assert_eq!(docs.size, None);
        assert_eq!(docs.modified, None);
    }

    #[test]
    fn drops_orphans_cycles_and_stale_parents() {
        let mut b = TreeBuilder::new();
        b.push(r(20, 1), ROOT, DIR, "ok");
        b.push(r(21, 1), r(99, 1), 0, "missing-parent");
        b.push(r(22, 1), r(20, 7), 0, "stale-seq"); // parent record 20 has seq 1, not 7
        b.push(r(23, 1), r(24, 1), DIR, "cycle-a");
        b.push(r(24, 1), r(23, 1), DIR, "cycle-b");
        b.push(r(25, 1), r(25, 1), DIR, "self-parent");
        let t = b.finish("D:\\");
        assert_eq!(t.len(), 2);
        assert_eq!(t.orphan_count(), 5);
        assert_eq!(t.path(1), PathBuf::from(r"D:\ok"));
    }

    #[test]
    fn lookup_checks_sequence() {
        let mut b = TreeBuilder::new();
        b.push(r(20, 3), ROOT, DIR, "a");
        let t = b.finish("C:\\");
        let l = t.ref_lookup();
        assert_eq!(l.get(r(20, 3)), Some(1));
        assert_eq!(l.get(r(20, 4)), None);
        assert_eq!(l.get_record(20), Some(1));
        assert_eq!(l.get(ROOT), Some(0));
    }

    #[test]
    fn sparse_lookup_for_huge_record_numbers() {
        let mut b = TreeBuilder::new();
        b.push(r(1 << 40, 1), ROOT, DIR, "far");
        b.push(r(7, 1), r(1 << 40, 1), 0, "child");
        let t = b.finish("C:\\");
        assert_eq!(t.len(), 3);
        assert_eq!(t.path(2), PathBuf::from(r"C:\far\child"));
        assert!(matches!(t.ref_lookup().map, RefMap::Sparse(_)));
    }

    #[test]
    fn empty_volume_has_root_only() {
        let t = TreeBuilder::new().finish("E:\\");
        assert_eq!(t.len(), 1);
        assert_eq!(t.parent(0), None);
    }

    proptest! {
        /// Random forests (with random junk parents) always produce a parent-first order
        /// containing exactly the entries reachable from the root.
        #[test]
        fn random_forest(parents in proptest::collection::vec(0usize..64, 1..64)) {
            let n = parents.len();
            let mut b = TreeBuilder::new();
            for (i, &p) in parents.iter().enumerate() {
                // p == 0 → child of root; otherwise node p - 1, which may come later, not
                // exist (dangling), be itself, or form a cycle.
                let parent_ref = if p == 0 { ROOT } else { r(100 + p as u64 - 1, 1) };
                b.push(r(100 + i as u64, 1), parent_ref, DIR, &format!("n{i}"));
            }
            let mut reachable = vec![false; n];
            for _ in 0..n {
                for (i, &p) in parents.iter().enumerate() {
                    reachable[i] = p == 0 || (p - 1 < n && p - 1 != i && reachable[p - 1]);
                }
            }
            let t = b.finish("C:\\");
            let expected = reachable.iter().filter(|&&x| x).count();
            prop_assert_eq!(t.len(), expected + 1);
            prop_assert_eq!(t.orphan_count(), parents.len() - expected);
            for i in 1..t.len() as u32 {
                prop_assert!(t.parent(i).unwrap() < i);
            }
        }
    }
}
