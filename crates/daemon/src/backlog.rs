//! The initial backlog: the files of a name-index snapshot that still have to be extracted.
//!
//! A first scan of a big disk finds a million such files. The [`JobQueue`](crate::queue::JobQueue)
//! would hold each as a heap item, a map entry and a path string (about 450 bytes); here a
//! file is a 24-byte record, its name, and a folder string shared with the other files of
//! that folder (about 60 bytes in all). The price: the backlog is built in one go and only
//! drained after that, in order of urgency. Changes reported later go through the job queue,
//! which is always served first.

use std::path::MAIN_SEPARATOR;

/// One waiting file.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Urgency: smaller goes first (see [`Prioritizer::backlog_rank`](crate::queue::Prioritizer::backlog_rank)).
    rank: u64,
    /// Index into [`Backlog::dir_starts`].
    dir: u32,
    /// Start of the name in [`Backlog::names`].
    name: u32,
    name_len: u16,
}

/// Files waiting for their first extraction, most urgent first. See the [module docs](self).
#[derive(Debug, Default)]
pub(crate) struct Backlog {
    /// Sorted so that the most urgent entry is the last one.
    entries: Vec<Entry>,
    /// Every folder (with its trailing separator), one after another.
    dirs: String,
    /// Where each folder starts in `dirs`; it ends where the next one starts.
    dir_starts: Vec<u32>,
    /// Every file name, one after another.
    names: String,
}

impl Backlog {
    /// Files waiting.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn dir(&self, i: u32) -> &str {
        let i = i as usize;
        let start = self.dir_starts[i] as usize;
        let end = self
            .dir_starts
            .get(i + 1)
            .map_or(self.dirs.len(), |&e| e as usize);
        &self.dirs[start..end]
    }

    fn write_key(&self, e: &Entry, out: &mut String) {
        out.clear();
        out.push_str(self.dir(e.dir));
        let start = e.name as usize;
        out.push_str(&self.names[start..start + usize::from(e.name_len)]);
    }

    /// Takes the most urgent file: its path key.
    pub(crate) fn pop(&mut self) -> Option<String> {
        let e = self.entries.pop()?;
        let mut key = String::new();
        self.write_key(&e, &mut key);
        if self.entries.is_empty() {
            *self = Backlog::default(); // frees the strings
        } else if self.entries.len() < self.entries.capacity() / 4 {
            self.entries.shrink_to_fit();
        }
        Some(key)
    }

    /// Drops the files whose key `drop` accepts. Returns how many were dropped. Reads every
    /// waiting file: meant for rare events (a drive unplugged, a folder no longer indexed).
    pub(crate) fn remove_where(&mut self, drop: impl Fn(&str) -> bool) -> usize {
        let before = self.entries.len();
        let mut entries = std::mem::take(&mut self.entries);
        let mut key = String::new();
        entries.retain(|e| {
            self.write_key(e, &mut key);
            !drop(&key)
        });
        if entries.is_empty() {
            *self = Backlog::default();
        } else {
            self.entries = entries;
        }
        before - self.len()
    }
}

/// Collects the files of a [`Backlog`].
#[derive(Debug, Default)]
pub(crate) struct BacklogBuilder {
    inner: Backlog,
}

impl BacklogBuilder {
    /// Adds the file `key` (a full path) with urgency `rank`. Returns false if it cannot be
    /// stored here (the strings would pass 4 GiB): the caller queues it another way.
    ///
    /// Files of one folder added one after another share its string, so add them in the name
    /// index's order.
    pub(crate) fn push(&mut self, key: &str, rank: u64) -> bool {
        let b = &mut self.inner;
        let split = key.rfind(MAIN_SEPARATOR).map_or(0, |i| i + 1);
        let (dir, name) = key.split_at(split);
        let (Ok(name_start), Ok(name_len), Ok(dir_start)) = (
            u32::try_from(b.names.len()),
            u16::try_from(name.len()),
            u32::try_from(b.dirs.len()),
        ) else {
            return false;
        };
        if u32::try_from(b.dir_starts.len()).is_err() {
            return false;
        }
        let same_dir = !b.dir_starts.is_empty() && b.dir(b.dir_starts.len() as u32 - 1) == dir;
        if !same_dir {
            b.dir_starts.push(dir_start);
            b.dirs.push_str(dir);
        }
        b.names.push_str(name);
        b.entries.push(Entry {
            rank,
            dir: b.dir_starts.len() as u32 - 1,
            name: name_start,
            name_len,
        });
        true
    }

    /// The finished backlog. Files of equal rank keep the order they were added in.
    pub(crate) fn finish(self) -> Backlog {
        let mut b = self.inner;
        // Stable, then reversed: the most urgent entry ends up last (popped first), and among
        // equal ranks the one added first.
        b.entries.sort_by_key(|e| e.rank);
        b.entries.reverse();
        b.entries.shrink_to_fit();
        b.dirs.shrink_to_fit();
        b.dir_starts.shrink_to_fit();
        b.names.shrink_to_fit();
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(parts: &[&str]) -> String {
        parts.join(std::path::MAIN_SEPARATOR_STR)
    }

    fn drain(b: &mut Backlog) -> Vec<String> {
        std::iter::from_fn(|| b.pop()).collect()
    }

    #[test]
    fn pops_by_rank_then_in_the_order_added() {
        let mut b = BacklogBuilder::default();
        let files = [
            (key(&["C:", "docs", "late.txt"]), 9),
            (key(&["C:", "docs", "first.txt"]), 1),
            (key(&["C:", "docs", "tie-a.txt"]), 5),
            (key(&["C:", "other", "tie-b.txt"]), 5),
            (key(&["C:", "docs", "ملف.txt"]), 5),
            ("no-folder.txt".to_owned(), 3),
        ];
        for (k, rank) in &files {
            assert!(b.push(k, *rank));
        }
        let mut b = b.finish();
        assert_eq!(b.len(), 6);
        let order: Vec<String> = [1, 5, 2, 3, 4, 0]
            .iter()
            .map(|&i: &usize| files[i].0.clone())
            .collect();
        assert_eq!(drain(&mut b), order);
        assert_eq!(b.len(), 0);
        assert!(b.dirs.is_empty() && b.names.is_empty());
    }

    #[test]
    fn files_of_one_folder_share_its_string() {
        let mut b = BacklogBuilder::default();
        for i in 0..100 {
            assert!(b.push(&key(&["C:", "a long folder name", &format!("{i}.txt")]), i));
        }
        assert!(b.push(&key(&["C:", "b", "x.txt"]), 0));
        let b = b.finish();
        assert_eq!(b.dir_starts.len(), 2);
        assert_eq!(
            b.dirs,
            format!(
                "{}{}",
                key(&["C:", "a long folder name", ""]),
                key(&["C:", "b", ""])
            )
        );
    }

    #[test]
    fn remove_where_drops_matching_files_only() {
        let mut b = BacklogBuilder::default();
        for (i, k) in [
            key(&["E:", "usb", "a.txt"]),
            key(&["C:", "docs", "b.txt"]),
            key(&["E:", "usb", "sub", "c.txt"]),
        ]
        .iter()
        .enumerate()
        {
            assert!(b.push(k, i as u64));
        }
        let mut b = b.finish();
        assert_eq!(b.remove_where(|k| k.starts_with("E:")), 2);
        assert_eq!(drain(&mut b), [key(&["C:", "docs", "b.txt"])]);

        let mut all = BacklogBuilder::default();
        assert!(all.push(&key(&["E:", "x.txt"]), 0));
        let mut all = all.finish();
        assert_eq!(all.remove_where(|_| true), 1);
        assert_eq!(all.len(), 0);
        assert!(all.pop().is_none());
    }
}
