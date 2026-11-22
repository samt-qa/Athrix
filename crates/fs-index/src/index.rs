//! The in-memory name index: compact per-entry columns plus two string arenas.

use std::path::PathBuf;

/// Identifier of an entry in a [`FileIndex`]. Ids are dense (`0..len`) and follow walk order.
pub type EntryId = u32;

/// Marks a root entry (no parent).
pub(crate) const NO_PARENT: u32 = u32::MAX;

pub(crate) const FLAG_DIR: u8 = 1;
pub(crate) const FLAG_SYMLINK: u8 = 1 << 1;

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

    fn heap_bytes(&self) -> usize {
        self.text.capacity() + self.ends.capacity() * size_of::<u32>()
    }

    fn shrink(&mut self) {
        self.text.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
}

/// An in-memory index of file and folder names.
///
/// Entries are stored column-wise (parent id, flags, size, mtime) and names live in two arenas:
/// the original names (for display and path reconstruction) and the normalized names
/// (see [`athrix_text_ar::normalize_name_into`]) that search scans, so a query never
/// re-normalizes the index. Full paths are rebuilt from parent links on demand.
///
/// Build one with [`IndexBuilder`] or [`crate::walk`].
#[derive(Debug, Default, Clone)]
pub struct FileIndex {
    pub(crate) names: Arena,
    pub(crate) normalized: Arena,
    pub(crate) parents: Vec<u32>,
    pub(crate) flags: Vec<u8>,
    pub(crate) sizes: Vec<u64>,
    pub(crate) mtimes: Vec<i64>,
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

    /// The original name of `id`. For a root this is the full root path as given to the walker.
    ///
    /// # Panics
    /// If `id` is out of range (as for all per-entry accessors).
    pub fn name(&self, id: EntryId) -> &str {
        self.names.get(id as usize)
    }

    /// The normalized name of `id`, as matched by search.
    pub fn normalized_name(&self, id: EntryId) -> &str {
        self.normalized.get(id as usize)
    }

    /// The parent of `id`, or `None` for a root.
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        match self.parents[id as usize] {
            NO_PARENT => None,
            p => Some(p),
        }
    }

    /// Metadata of `id`.
    pub fn meta(&self, id: EntryId) -> EntryMeta {
        let i = id as usize;
        let flags = self.flags[i];
        let mtime = self.mtimes[i];
        EntryMeta {
            is_dir: flags & FLAG_DIR != 0,
            is_symlink: flags & FLAG_SYMLINK != 0,
            size: self.sizes[i],
            modified: (mtime != i64::MIN).then_some(mtime),
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
        self.names.heap_bytes()
            + self.normalized.heap_bytes()
            + self.parents.capacity() * size_of::<u32>()
            + self.flags.capacity()
            + self.sizes.capacity() * size_of::<u64>()
            + self.mtimes.capacity() * size_of::<i64>()
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
        let name_bytes: usize = parts.iter().map(|p| p.names.text.len()).sum();
        let mut b = IndexBuilder::with_capacity(entries, name_bytes / entries.max(1) + 1);
        for (n, part) in parts.iter().enumerate() {
            let mut remap = vec![NO_PARENT; part.len()];
            for id in 0..part.len() as EntryId {
                let parent = match part.parent(id) {
                    None if !keep_root(n, part.name(id)) => continue,
                    None => None,
                    Some(p) => match remap[p as usize] {
                        NO_PARENT => continue,
                        p => Some(p),
                    },
                };
                remap[id as usize] = b.push_normalized(
                    parent,
                    part.name(id),
                    part.normalized_name(id),
                    part.meta(id),
                );
            }
        }
        b.finish()
    }
}

/// Builds a [`FileIndex`] one entry at a time. Used by the walker; also handy for tests,
/// benchmarks and future sources such as the NTFS MFT reader.
#[derive(Debug, Default)]
pub struct IndexBuilder {
    index: FileIndex,
    buf: String,
}

impl IndexBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a builder with room for `entries` entries averaging `name_bytes` bytes each.
    pub fn with_capacity(entries: usize, name_bytes: usize) -> Self {
        let mut b = Self::default();
        let ix = &mut b.index;
        ix.names.text.reserve(entries * name_bytes);
        ix.names.ends.reserve(entries);
        ix.normalized.text.reserve(entries * name_bytes);
        ix.normalized.ends.reserve(entries);
        ix.parents.reserve(entries);
        ix.flags.reserve(entries);
        ix.sizes.reserve(entries);
        ix.mtimes.reserve(entries);
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
        let mut buf = std::mem::take(&mut self.buf);
        athrix_text_ar::normalize_name_into(name, &mut buf);
        let id = self.push_normalized(parent, name, &buf, meta);
        self.buf = buf;
        id
    }

    /// Like [`IndexBuilder::push`], with `normalized` already computed by
    /// [`athrix_text_ar::normalize_name_into`] (copying entries between indexes, live updates).
    pub(crate) fn push_normalized(
        &mut self,
        parent: Option<EntryId>,
        name: &str,
        normalized: &str,
        meta: EntryMeta,
    ) -> EntryId {
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
        ix.names.push(name);
        ix.normalized.push(normalized);
        ix.parents.push(parent);
        ix.flags
            .push((u8::from(meta.is_dir) * FLAG_DIR) | (u8::from(meta.is_symlink) * FLAG_SYMLINK));
        ix.sizes.push(meta.size);
        ix.mtimes.push(meta.modified.unwrap_or(i64::MIN));
        id
    }

    /// Finishes the index, releasing spare capacity.
    pub fn finish(mut self) -> FileIndex {
        let ix = &mut self.index;
        ix.names.shrink();
        ix.normalized.shrink();
        ix.parents.shrink_to_fit();
        ix.flags.shrink_to_fit();
        ix.sizes.shrink_to_fit();
        ix.mtimes.shrink_to_fit();
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
        assert_eq!(ix.meta(file).size, 42);
        assert_eq!(ix.meta(file).modified, Some(1_700_000_000));
        assert_eq!(ix.meta(root).modified, None);
        let expected: PathBuf = ["/data", "Docs", "مُحَاضَرَةُ.PDF"].iter().collect();
        assert_eq!(ix.path(file), expected);
        assert!(ix.heap_bytes() > 0);
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
        assert_eq!(merged.parent(4), Some(3));
        assert_eq!(merged.parent(3), None);
    }
}
