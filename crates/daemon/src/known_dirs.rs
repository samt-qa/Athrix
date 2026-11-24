//! The folders the scheduler has already seen, so a change inside one is not taken for a new
//! folder.
//!
//! The watcher reports a folder whenever something in it changes. A folder that *appeared*
//! (created, renamed or copied in) has to be listed, because its files are not reported one
//! by one; an old folder must not be, or every change in a big tree lists the whole tree
//! again (a change in a drive's root would list the drive). The content index alone cannot
//! tell them apart: a folder without documents is either new, or old and not extracted yet,
//! or old without any text file.
//!
//! So the scheduler remembers the folders of the last snapshot (8 bytes each: a hash of the
//! path, compared without case), plus the ones it listed since, minus the ones that went
//! away.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::Path;

/// A hash of the folder `dir` that ignores case and a trailing separator.
pub(crate) fn dir_hash(dir: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    for c in dir.components() {
        for ch in c.as_os_str().to_string_lossy().chars() {
            for low in ch.to_lowercase() {
                h.write_u32(u32::from(low));
            }
        }
        h.write_u8(0xFF);
    }
    h.finish()
}

/// See the [module docs](self).
#[derive(Debug, Default)]
pub(crate) struct KnownDirs {
    /// The folders of the last snapshot, sorted.
    base: Vec<u64>,
    /// Folders listed since.
    added: HashSet<u64>,
    /// Folders of `base` that went away since.
    removed: HashSet<u64>,
}

impl KnownDirs {
    /// The folders of a snapshot, from their [`dir_hash`] in any order.
    pub(crate) fn new(mut hashes: Vec<u64>) -> Self {
        hashes.sort_unstable();
        hashes.dedup();
        hashes.shrink_to_fit();
        KnownDirs {
            base: hashes,
            added: HashSet::new(),
            removed: HashSet::new(),
        }
    }

    fn in_base(&self, h: u64) -> bool {
        self.base.binary_search(&h).is_ok()
    }

    /// Whether `dir` was in the last snapshot or listed since, and did not go away.
    pub(crate) fn contains(&self, dir: &Path) -> bool {
        let h = dir_hash(dir);
        self.added.contains(&h) || (self.in_base(h) && !self.removed.contains(&h))
    }

    /// `dir` was listed (or is known to be an old folder).
    pub(crate) fn insert(&mut self, dir: &Path) {
        let h = dir_hash(dir);
        if self.in_base(h) {
            self.removed.remove(&h);
        } else {
            self.added.insert(h);
        }
    }

    /// `path` went away (or what is there now is not the folder that was seen).
    pub(crate) fn remove(&mut self, path: &Path) {
        let h = dir_hash(path);
        self.added.remove(&h);
        if self.in_base(h) {
            self.removed.insert(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn hash_ignores_case_and_a_trailing_separator() {
        let a = p(&["root", "Docs", "Sub"]);
        assert_eq!(dir_hash(&a), dir_hash(&p(&["ROOT", "docs", "sub"])));
        let with_sep = format!("{}{}", a.display(), std::path::MAIN_SEPARATOR);
        assert_eq!(dir_hash(&a), dir_hash(Path::new(&with_sep)));
        assert_ne!(dir_hash(&a), dir_hash(&p(&["root", "DocsSub"])));
        assert_ne!(dir_hash(&a), dir_hash(&p(&["root", "Docs"])));
    }

    #[test]
    fn snapshot_folders_listed_folders_and_removals() {
        let (old, new, other) = (p(&["r", "old"]), p(&["r", "new"]), p(&["r", "other"]));
        let mut k = KnownDirs::new(vec![dir_hash(&old), dir_hash(&old)]);
        assert!(k.contains(&old) && !k.contains(&new));

        k.insert(&new);
        k.insert(&old);
        assert!(k.contains(&old) && k.contains(&new) && !k.contains(&other));

        // Gone, then seen again.
        k.remove(&old);
        k.remove(&new);
        k.remove(&other);
        assert!(!k.contains(&old) && !k.contains(&new) && !k.contains(&other));
        k.insert(&old);
        k.insert(&new);
        assert!(k.contains(&old) && k.contains(&new));
        assert!(k.removed.is_empty() && k.added.len() == 1);
    }
}
