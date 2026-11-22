//! The job queue: a binary heap ordered by urgency, deduplicated by path.
//!
//! Every path is queued at most once. Queuing a path again replaces its job (the newest
//! metadata wins) and keeps the more urgent of the two priorities. Replaced heap entries are
//! left in place and skipped when popped (lazy deletion); the heap is rebuilt when stale
//! entries outnumber live ones.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::candidate::{Candidate, starts_with_ci};

/// One unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Job {
    /// Extract this file version (from a scan, or an upsert with known metadata).
    Index(Candidate),
    /// Something changed at this path: re-read it from disk (file, folder, or gone).
    Check(PathBuf),
    /// The path is gone: remove it (and anything known below it) from the index.
    Delete(PathBuf),
}

/// Where a job came from. Watcher changes jump ahead of the initial backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    Change = 0,
    Backlog = 1,
}

/// Sort key: smaller is more urgent. Fields compare in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Priority {
    source: Source,
    /// 0 inside a priority folder (Desktop, Documents, Downloads), 1 elsewhere.
    folder: u8,
    /// 0 = modified in the last day, 1 = week, 2 = month, 3 = year, 4 = older.
    recency: u8,
    /// 0 = under 256 KiB, 1 = under 4 MiB, 2 = under 32 MiB, 3 = larger.
    size: u8,
    /// Newer first within a bucket.
    newer: Reverse<i64>,
}

/// Computes [`Priority`] values.
#[derive(Debug, Clone)]
pub(crate) struct Prioritizer {
    folders: Vec<PathBuf>,
}

impl Prioritizer {
    pub(crate) fn new(folders: Vec<PathBuf>) -> Self {
        Prioritizer { folders }
    }

    fn folder_tier(&self, path: &Path) -> u8 {
        u8::from(!self.folders.iter().any(|f| starts_with_ci(path, f)))
    }

    /// Priority of `job`, with `now` in Unix seconds.
    pub(crate) fn priority(&self, job: &Job, source: Source, now: i64) -> Priority {
        match job {
            Job::Index(c) => {
                const DAY: i64 = 86_400;
                let age = now.saturating_sub(c.mtime);
                let recency = match age {
                    a if a < DAY => 0,
                    a if a < 7 * DAY => 1,
                    a if a < 30 * DAY => 2,
                    a if a < 365 * DAY => 3,
                    _ => 4,
                };
                let size = match c.size {
                    s if s < 256 << 10 => 0,
                    s if s < 4 << 20 => 1,
                    s if s < 32 << 20 => 2,
                    _ => 3,
                };
                Priority {
                    source,
                    folder: self.folder_tier(&c.path),
                    recency,
                    size,
                    newer: Reverse(c.mtime),
                }
            }
            Job::Check(p) | Job::Delete(p) => Priority {
                source,
                folder: self.folder_tier(p),
                recency: 0,
                size: 0,
                newer: Reverse(i64::MAX),
            },
        }
    }
}

#[derive(Debug)]
struct Item {
    prio: Priority,
    seq: u64,
    key: Arc<str>,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Item {
    /// Reversed: `BinaryHeap` is a max-heap and we want the smallest key first.
    fn cmp(&self, other: &Self) -> Ordering {
        (other.prio, other.seq).cmp(&(self.prio, self.seq))
    }
}

#[derive(Debug)]
struct Entry {
    seq: u64,
    prio: Priority,
    job: Job,
}

/// Deduplicating priority queue of jobs keyed by path string.
#[derive(Debug, Default)]
pub(crate) struct JobQueue {
    heap: BinaryHeap<Item>,
    live: HashMap<Arc<str>, Entry>,
    next_seq: u64,
}

impl JobQueue {
    /// Jobs waiting.
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }

    /// Queues `job` under `key`. Returns true if the key was not queued yet (false: the
    /// queued job was replaced).
    pub(crate) fn push(&mut self, key: &str, job: Job, prio: Priority) -> bool {
        let seq = self.next_seq;
        self.next_seq += 1;
        let (key, prio, new) = match self.live.get_key_value(key) {
            Some((k, old)) => (Arc::clone(k), prio.min(old.prio), false),
            None => (Arc::<str>::from(key), prio, true),
        };
        self.live.insert(Arc::clone(&key), Entry { seq, prio, job });
        self.heap.push(Item { prio, seq, key });
        if self.heap.len() > 2 * self.live.len() + 1024 {
            self.rebuild();
        }
        new
    }

    /// Pops the most urgent job whose key `busy` does not reject. Rejected jobs stay queued.
    pub(crate) fn pop_where(&mut self, busy: impl Fn(&str) -> bool) -> Option<(String, Job)> {
        let mut held = Vec::new();
        let mut found = None;
        while let Some(item) = self.heap.pop() {
            let current = self.live.get(&item.key).is_some_and(|e| e.seq == item.seq);
            if !current {
                continue; // replaced or already taken
            }
            if busy(&item.key) {
                held.push(item);
                continue;
            }
            let entry = self.live.remove(&item.key).expect("checked above");
            found = Some((item.key.to_string(), entry.job));
            break;
        }
        self.heap.extend(held);
        found
    }

    /// Drops the queued jobs whose key `drop` accepts. Returns how many were dropped.
    pub(crate) fn remove_where(&mut self, drop: impl Fn(&str) -> bool) -> usize {
        let before = self.live.len();
        self.live.retain(|k, _| !drop(k));
        let removed = before - self.live.len();
        if removed > 0 {
            // Their heap items are skipped when popped (no live entry); rebuild if mostly stale.
            if self.heap.len() > 2 * self.live.len() + 1024 {
                self.rebuild();
            }
        }
        removed
    }

    fn rebuild(&mut self) {
        self.heap = self
            .live
            .iter()
            .map(|(k, e)| Item {
                prio: e.prio,
                seq: e.seq,
                key: Arc::clone(k),
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_extract::Kind;

    const NOW: i64 = 1_800_000_000;

    fn cand(path: &str, size: u64, age_days: i64) -> Job {
        Job::Index(Candidate {
            path: PathBuf::from(path),
            size,
            mtime: NOW - age_days * 86_400,
            kind: Kind::Text,
        })
    }

    fn drain(q: &mut JobQueue) -> Vec<String> {
        std::iter::from_fn(|| q.pop_where(|_| false).map(|(k, _)| k)).collect()
    }

    #[test]
    fn order_folder_then_recency_then_size_and_changes_first() {
        let p = Prioritizer::new(vec![PathBuf::from("/home/u/Documents")]);
        let mut q = JobQueue::default();
        let mut add = |key: &str, job: Job, src| {
            let prio = p.priority(&job, src, NOW);
            q.push(key, job, prio);
        };
        add(
            "/other/old-small",
            cand("/other/old-small", 10, 400),
            Source::Backlog,
        );
        add(
            "/other/new-big",
            cand("/other/new-big", 100 << 20, 0),
            Source::Backlog,
        );
        add(
            "/other/new-small",
            cand("/other/new-small", 10, 0),
            Source::Backlog,
        );
        add(
            "/home/u/Documents/old",
            cand("/home/u/Documents/old", 10, 1000),
            Source::Backlog,
        );
        add(
            "/x/changed",
            Job::Check(PathBuf::from("/x/changed")),
            Source::Change,
        );
        assert_eq!(
            drain(&mut q),
            [
                "/x/changed",
                "/home/u/Documents/old",
                "/other/new-small",
                "/other/new-big",
                "/other/old-small"
            ]
        );
    }

    #[test]
    fn dedupe_latest_job_wins_with_the_more_urgent_priority() {
        let p = Prioritizer::new(Vec::new());
        let mut q = JobQueue::default();
        let a1 = cand("/a", 10, 500);
        let pa = p.priority(&a1, Source::Change, NOW);
        assert!(q.push("/a", a1, pa));
        let b = cand("/b", 10, 0);
        let pb = p.priority(&b, Source::Backlog, NOW);
        q.push("/b", b, pb);
        let a2 = cand("/a", 20, 600);
        let pa2 = p.priority(&a2, Source::Backlog, NOW);
        assert!(!q.push("/a", a2.clone(), pa2));
        assert_eq!(q.len(), 2);
        let (k, job) = q.pop_where(|_| false).unwrap();
        assert_eq!(k, "/a"); // kept the Change priority
        assert_eq!(job, a2); // but the newest metadata
        assert_eq!(drain(&mut q), ["/b"]);
    }

    #[test]
    fn busy_keys_are_skipped_but_kept() {
        let p = Prioritizer::new(Vec::new());
        let mut q = JobQueue::default();
        for k in ["/a", "/b"] {
            let j = Job::Delete(PathBuf::from(k));
            let pr = p.priority(&j, Source::Change, NOW);
            q.push(k, j, pr);
        }
        let (k, _) = q.pop_where(|k| k == "/a").unwrap();
        assert_eq!(k, "/b");
        assert!(q.pop_where(|k| k == "/a").is_none());
        assert_eq!(drain(&mut q), ["/a"]);
    }

    #[test]
    fn removed_jobs_are_never_popped() {
        let p = Prioritizer::new(Vec::new());
        let mut q = JobQueue::default();
        for k in ["/usb/a", "/usb/b", "/disk/c"] {
            let j = cand(k, 1, 1);
            let pr = p.priority(&j, Source::Backlog, NOW);
            q.push(k, j, pr);
        }
        assert_eq!(q.remove_where(|k| k.starts_with("/usb/")), 2);
        assert_eq!(q.len(), 1);
        assert_eq!(drain(&mut q), ["/disk/c"]);
    }

    #[test]
    fn many_replacements_stay_bounded() {
        let p = Prioritizer::new(Vec::new());
        let mut q = JobQueue::default();
        for i in 0..10_000 {
            let j = cand("/same", i, 1);
            let pr = p.priority(&j, Source::Backlog, NOW);
            q.push("/same", j, pr);
        }
        assert!(q.heap.len() <= 1026);
        assert_eq!(q.len(), 1);
    }
}
