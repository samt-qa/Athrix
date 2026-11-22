//! Hashing many files: cache first, then the missing ones in parallel, cancellable and within a
//! time budget.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use crate::ImageHash;
use crate::cache::{CachedHash, HashCache};
use crate::decode::{MAX_FILE_BYTES, hash_file};

/// One image file to hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Cache key of the file (the app uses its case-folded path key).
    pub key: String,
    /// Where to read it.
    pub path: PathBuf,
    /// Size in bytes, as indexed.
    pub size: u64,
    /// Modification time as indexed (any stable number, e.g. Unix seconds; `0` if unknown).
    pub modified: i64,
}

/// The outcome of [`hash_all`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HashRun {
    /// One entry per candidate, in order: its hash, or `None` when it could not be hashed or
    /// was not reached before the deadline.
    pub hashes: Vec<Option<ImageHash>>,
    /// Candidates whose outcome is known (hashed, cached, or failed).
    pub checked: usize,
    /// Candidates decoded during this call (the rest came from the cache).
    pub computed: usize,
    /// The deadline stopped the run before every candidate was checked.
    pub incomplete: bool,
}

/// Threads for hashing: all cores but one (the UI and searches keep one), at most 6 (each
/// decode can hold a large picture in memory), at least 1.
pub fn hash_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 6)
}

/// Hashes `candidates`: cached outcomes first, then the rest on a pool of `threads` threads,
/// stopping at `deadline` ([`HashRun::incomplete`]) and saving what was computed to `cache`.
/// Returns `None` if `cancel` was set (what was computed before is still saved). Cache errors
/// only cost speed: they are ignored.
pub fn hash_all(
    cache: Option<&HashCache>,
    candidates: &[Candidate],
    cancel: &AtomicBool,
    deadline: Instant,
    threads: usize,
) -> Option<HashRun> {
    let mut known: Vec<Option<CachedHash>> = match cache {
        Some(cache) => {
            let keys: Vec<(&str, u64, i64)> = candidates
                .iter()
                .map(|c| (c.key.as_str(), c.size, c.modified))
                .collect();
            cache
                .get_many(&keys)
                .unwrap_or_else(|_| vec![None; candidates.len()])
        }
        None => vec![None; candidates.len()],
    };
    // Too big to read: failed without opening it (and not worth a cache row).
    for (k, c) in known.iter_mut().zip(candidates) {
        if k.is_none() && c.size > MAX_FILE_BYTES {
            *k = Some(CachedHash::Failed);
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let missing: Vec<usize> = (0..candidates.len())
        .filter(|&i| known[i].is_none())
        .collect();
    let mut computed: Vec<(usize, Option<ImageHash>)> = Vec::new();
    if !missing.is_empty() {
        let work = || -> Vec<(usize, Option<ImageHash>)> {
            missing
                .par_iter()
                .with_max_len(1)
                .filter_map(|&i| {
                    if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        return None;
                    }
                    Some((i, hash_file(&candidates[i].path).ok()))
                })
                .collect()
        };
        computed = match rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|i| format!("athrix-imagehash-{i}"))
            .build()
        {
            Ok(pool) => pool.install(work),
            Err(_) => work(),
        };
    }
    if let Some(cache) = cache {
        let rows: Vec<(&str, u64, i64, Option<ImageHash>)> = computed
            .iter()
            .map(|&(i, h)| {
                let c = &candidates[i];
                (c.key.as_str(), c.size, c.modified, h)
            })
            .collect();
        // A failed write only means hashing again next time.
        let _ = cache.put_many(&rows);
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let computed_count = computed.len();
    for (i, h) in computed {
        known[i] = Some(h.map_or(CachedHash::Failed, CachedHash::Hash));
    }
    let checked = known.iter().filter(|k| k.is_some()).count();
    Some(HashRun {
        hashes: known
            .iter()
            .map(|k| match k {
                Some(CachedHash::Hash(h)) => Some(*h),
                _ => None,
            })
            .collect(),
        checked,
        computed: computed_count,
        incomplete: checked < candidates.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, RgbImage};
    use std::time::Duration;

    fn write_png(path: &std::path::Path, seed: u8) {
        let img = RgbImage::from_fn(64, 48, |x, y| {
            let v = (x as u8)
                .wrapping_mul(seed)
                .wrapping_add((y as u8).wrapping_mul(3));
            image::Rgb([v, v / 2, 255 - v])
        });
        DynamicImage::ImageRgb8(img)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    fn candidates(dir: &std::path::Path, n: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| {
                let path = dir.join(format!("{i}.png"));
                if i % 4 == 3 {
                    std::fs::write(&path, b"not a png").unwrap();
                } else {
                    write_png(&path, i as u8 + 1);
                }
                Candidate {
                    key: path.to_string_lossy().into_owned(),
                    size: std::fs::metadata(&path).unwrap().len(),
                    modified: 1,
                    path,
                }
            })
            .collect()
    }

    #[test]
    fn hashes_then_reuses_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let list = candidates(dir.path(), 8);
        let cache = HashCache::in_memory().unwrap();
        let cancel = AtomicBool::new(false);
        let later = Instant::now() + Duration::from_secs(60);
        let first = hash_all(Some(&cache), &list, &cancel, later, 2).unwrap();
        assert_eq!(first.computed, 8);
        assert_eq!(first.checked, 8);
        assert!(!first.incomplete);
        assert_eq!(first.hashes.iter().filter(|h| h.is_none()).count(), 2);
        let second = hash_all(Some(&cache), &list, &cancel, later, 2).unwrap();
        assert_eq!(second.computed, 0);
        assert_eq!(second.hashes, first.hashes);
    }

    #[test]
    fn a_past_deadline_leaves_the_run_incomplete_and_cancel_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let list = candidates(dir.path(), 4);
        let cancel = AtomicBool::new(false);
        let run = hash_all(None, &list, &cancel, Instant::now(), 2).unwrap();
        assert!(run.incomplete);
        assert_eq!(run.checked, 0);
        cancel.store(true, Ordering::Relaxed);
        let later = Instant::now() + Duration::from_secs(60);
        assert_eq!(hash_all(None, &list, &cancel, later, 2), None);
    }
}
