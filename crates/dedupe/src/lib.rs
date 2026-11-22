//! Duplicate file detection for Athrix (`dupe:` / `مكرر:`).
//!
//! [`find_duplicates`] takes candidate files (path, cache key and the size the name index
//! knows) and returns the groups of byte-identical files among them:
//!
//! 1. **Size**: only files sharing their exact size with another candidate can be duplicates;
//!    zero-byte files are left out (they would all be "identical"). Size groups are processed
//!    biggest wasted space first (size × (count − 1)).
//! 2. **Partial hash**: BLAKE3 of the size and the first and last [`PARTIAL_CHUNK`] bytes
//!    (for a file of at most two chunks, of the whole file: that is then its full hash too).
//! 3. **Full hash**: BLAKE3 of the whole file, only for files whose partial hash collides
//!    with another file of the same size.
//!
//! Each file is checked with a fresh `stat`: a file whose size no longer matches the index,
//! that is gone, is not a regular file or cannot be opened or read (locked, no permission) is
//! skipped and counted in [`Report::skipped`]. Files are read in parallel on a small rayon
//! pool, opened read-only (never written).
//!
//! **Budget**: [`Options::max_bytes`] and [`Options::max_time`] bound one call: no file is
//! started once either runs out (a file already being read is finished, since a hash cannot
//! be resumed, so one call may overrun by the time of its biggest read). The groups confirmed
//! so far are returned with [`Report::incomplete`] set; the hashes computed meanwhile are in
//! the [`HashCache`], so calling again continues where this call stopped. Setting the cancel flag stops the call with [`Cancelled`] (what was hashed
//! so far is still cached).

mod cache;

pub use cache::{CacheError, CacheRow, CachedHashes, HashCache};

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rayon::prelude::*;

/// A BLAKE3 digest.
pub type Digest = [u8; 32];

/// Bytes hashed at each end of a file for the partial hash (64 KiB).
pub const PARTIAL_CHUNK: u64 = 64 * 1024;

/// Read buffer for full hashes.
const READ_BUF: usize = 1024 * 1024;

/// Candidate files processed per wave (stat + cache lookups + hashing, then cache writes).
const WAVE_FILES: usize = 512;

/// A file that may have duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Where to read it.
    pub path: PathBuf,
    /// Its key in the hash cache (for example a normalized, case-folded path).
    pub key: String,
    /// Its size as the index knows it.
    pub size: u64,
}

/// Limits of one [`find_duplicates`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Stop reading after about this many bytes (cache hits are free).
    pub max_bytes: u64,
    /// Stop after about this long.
    pub max_time: Duration,
    /// Files read at once (0 = the number of CPUs, at most 8).
    pub threads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_bytes: 8 * 1024 * 1024 * 1024,
            max_time: Duration::from_secs(8),
            threads: 0,
        }
    }
}

/// A set of byte-identical files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Their size in bytes.
    pub size: u64,
    /// Their BLAKE3 hash.
    pub hash: Digest,
    /// Indices into the candidates, ascending (at least two).
    pub members: Vec<usize>,
}

impl Group {
    /// Space the extra copies take: `size × (count − 1)`.
    pub fn wasted(&self) -> u64 {
        self.size
            .saturating_mul(self.members.len().saturating_sub(1) as u64)
    }
}

/// What [`find_duplicates`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The groups, biggest wasted space first (then bigger files, then candidate order).
    pub groups: Vec<Group>,
    /// The budget ran out before every candidate was checked: more groups (or more members
    /// of a listed group) may exist. Calling again continues thanks to the cache.
    pub incomplete: bool,
    /// Candidates that could not be checked: gone, changed size, locked or unreadable.
    pub skipped: usize,
    /// Bytes read from disk by this call.
    pub bytes_read: u64,
    /// Candidates whose hashes came from the cache.
    pub cache_hits: usize,
}

impl Report {
    /// Total wasted space of all groups.
    pub fn wasted(&self) -> u64 {
        self.groups
            .iter()
            .fold(0u64, |acc, g| acc.saturating_add(g.wasted()))
    }
}

/// The search was cancelled through its flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("duplicate search cancelled")]
pub struct Cancelled;

/// Finds the groups of identical files among `candidates` (see the crate docs). Candidates
/// should be distinct files; a path listed twice would look like its own duplicate.
pub fn find_duplicates(
    candidates: &[Candidate],
    cache: Option<&HashCache>,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Report, Cancelled> {
    let budget = Budget {
        deadline: Instant::now() + options.max_time,
        max_bytes: options.max_bytes,
        read: AtomicU64::new(0),
        out: AtomicBool::new(false),
    };
    let threads = match options.threads {
        0 => std::thread::available_parallelism().map_or(4, |n| n.get().min(8)),
        n => n,
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("athrix-dedupe-{i}"))
        .build()
        .ok();
    let mut report = Report::default();
    let groups = size_groups(candidates);
    let mut next = 0;
    while next < groups.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
        if budget.exhausted() {
            report.incomplete = true;
            break;
        }
        let mut end = next;
        let mut files = 0;
        while end < groups.len() && files < WAVE_FILES {
            files += groups[end].len();
            end += 1;
        }
        let wave = &groups[next..end];
        next = end;
        match &pool {
            Some(pool) => {
                pool.install(|| run_wave(candidates, wave, cache, &budget, cancel, &mut report));
            }
            None => run_wave(candidates, wave, cache, &budget, cancel, &mut report),
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
    }
    report.bytes_read = budget.read.load(Ordering::Relaxed);
    report.groups.sort_by(|a, b| {
        b.wasted()
            .cmp(&a.wasted())
            .then(b.size.cmp(&a.size))
            .then(a.members[0].cmp(&b.members[0]))
    });
    Ok(report)
}

/// Indices of the candidates grouped by size: only sizes shared by two or more non-empty
/// files, biggest `size × (count − 1)` first.
fn size_groups(candidates: &[Candidate]) -> Vec<Vec<usize>> {
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        if c.size > 0 {
            by_size.entry(c.size).or_default().push(i);
        }
    }
    let mut groups: Vec<(u64, Vec<usize>)> =
        by_size.into_iter().filter(|(_, v)| v.len() >= 2).collect();
    groups.sort_by(|(sa, a), (sb, b)| {
        let wa = sa.saturating_mul(a.len() as u64 - 1);
        let wb = sb.saturating_mul(b.len() as u64 - 1);
        wb.cmp(&wa).then(sb.cmp(sa)).then(a[0].cmp(&b[0]))
    });
    groups.into_iter().map(|(_, v)| v).collect()
}

struct Budget {
    deadline: Instant,
    max_bytes: u64,
    read: AtomicU64,
    out: AtomicBool,
}

impl Budget {
    /// True once time or bytes ran out (sticky).
    fn exhausted(&self) -> bool {
        if self.out.load(Ordering::Relaxed) {
            return true;
        }
        if Instant::now() >= self.deadline || self.read.load(Ordering::Relaxed) >= self.max_bytes {
            self.out.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn charge(&self, n: usize) {
        self.read.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// What is known about one candidate.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Gone, changed, locked or unreadable.
    Skipped,
    /// Not checked: the budget ran out or the search was cancelled.
    Unknown,
    /// Hashed (or cached).
    Known {
        mtime: Option<i64>,
        partial: Digest,
        full: Option<Digest>,
    },
}

/// Why a read stopped.
enum ReadError {
    /// The file cannot be used (I/O error, changed size).
    Skip,
    /// Budget or cancel.
    Stopped,
}

/// Checks the candidates of the size groups `wave`, adding confirmed groups to `report`.
fn run_wave(
    candidates: &[Candidate],
    wave: &[Vec<usize>],
    cache: Option<&HashCache>,
    budget: &Budget,
    cancel: &AtomicBool,
    report: &mut Report,
) {
    let files: Vec<usize> = wave.iter().flatten().copied().collect();
    // 1. stat every file (its real size and modification time).
    let stats: Vec<Option<Option<i64>>> = files.par_iter().map(|&i| stat(&candidates[i])).collect();
    // 2. cache lookups for the files that still match the index.
    let cached: Vec<Option<CachedHashes>> = match cache {
        Some(cache) => {
            let keys: Vec<(&str, u64, i64)> = files
                .iter()
                .zip(&stats)
                .filter_map(|(&i, s)| match s {
                    Some(Some(mtime)) => {
                        Some((candidates[i].key.as_str(), candidates[i].size, *mtime))
                    }
                    _ => None,
                })
                .collect();
            let mut found = cache.get_many(keys).into_iter();
            stats
                .iter()
                .map(|s| match s {
                    Some(Some(_)) => found.next().flatten(),
                    _ => None,
                })
                .collect()
        }
        None => vec![None; files.len()],
    };
    // 3. partial hashes for the rest.
    let mut states: Vec<State> = files
        .par_iter()
        .zip(stats.par_iter().zip(cached.par_iter()))
        .map(|(&i, (stat, hit))| {
            let Some(mtime) = *stat else {
                return State::Skipped;
            };
            if let Some(h) = hit {
                return State::Known {
                    mtime,
                    partial: h.partial,
                    full: h.full,
                };
            }
            if cancel.load(Ordering::Relaxed) || budget.exhausted() {
                return State::Unknown;
            }
            match partial_hash(&candidates[i], budget, cancel) {
                Ok((partial, full)) => State::Known {
                    mtime,
                    partial,
                    full,
                },
                Err(ReadError::Skip) => State::Skipped,
                Err(ReadError::Stopped) => State::Unknown,
            }
        })
        .collect();
    let mut dirty: Vec<bool> = cached.iter().map(Option::is_none).collect();
    report.cache_hits += cached.iter().filter(|c| c.is_some()).count();
    // 4. full hashes where partial hashes collide within a size group.
    let pos: HashMap<usize, usize> = files.iter().enumerate().map(|(p, &i)| (i, p)).collect();
    let mut need_full: Vec<usize> = Vec::new();
    for group in wave {
        let mut by_partial: HashMap<Digest, Vec<usize>> = HashMap::new();
        for &i in group {
            if let State::Known { partial, .. } = states[pos[&i]] {
                by_partial.entry(partial).or_default().push(pos[&i]);
            }
        }
        for same in by_partial.values().filter(|v| v.len() >= 2) {
            need_full.extend(
                same.iter()
                    .copied()
                    .filter(|&p| matches!(states[p], State::Known { full: None, .. })),
            );
        }
    }
    let mut stopped = vec![false; files.len()];
    let fulls: Vec<(usize, Result<Digest, ReadError>)> = need_full
        .par_iter()
        .map(|&p| {
            if cancel.load(Ordering::Relaxed) || budget.exhausted() {
                return (p, Err(ReadError::Stopped));
            }
            (p, full_hash(&candidates[files[p]], budget, cancel))
        })
        .collect();
    for (p, result) in fulls {
        match result {
            Ok(digest) => {
                if let State::Known { full, .. } = &mut states[p] {
                    *full = Some(digest);
                    dirty[p] = true;
                }
            }
            Err(ReadError::Skip) => states[p] = State::Skipped,
            // Keeps its partial hash (cached below); the full hash is left for next time.
            Err(ReadError::Stopped) => stopped[p] = true,
        }
    }
    // 5. group by full hash.
    for group in wave {
        let mut by_full: HashMap<Digest, Vec<usize>> = HashMap::new();
        for &i in group {
            if stopped[pos[&i]] {
                report.incomplete = true;
            }
            match states[pos[&i]] {
                State::Known {
                    full: Some(full), ..
                } => by_full.entry(full).or_default().push(i),
                State::Skipped => report.skipped += 1,
                State::Unknown => report.incomplete = true,
                State::Known { full: None, .. } => {}
            }
        }
        for (hash, mut members) in by_full {
            if members.len() >= 2 {
                members.sort_unstable();
                report.groups.push(Group {
                    size: candidates[members[0]].size,
                    hash,
                    members,
                });
            }
        }
    }
    // 6. remember what was computed.
    if let Some(cache) = cache {
        let rows: Vec<CacheRow> = files
            .iter()
            .enumerate()
            .filter(|&(p, _)| dirty[p])
            .filter_map(|(p, &i)| match partials_of(&states[p]) {
                Some((Some(mtime), partial, full)) => Some(CacheRow {
                    key: candidates[i].key.clone(),
                    size: candidates[i].size,
                    mtime,
                    hashes: CachedHashes { partial, full },
                }),
                _ => None,
            })
            .collect();
        let _ = cache.put_many(&rows);
    }
}

/// `(mtime, partial, full)` of a known state.
fn partials_of(s: &State) -> Option<(Option<i64>, Digest, Option<Digest>)> {
    match *s {
        State::Known {
            mtime,
            partial,
            full,
        } => Some((mtime, partial, full)),
        _ => None,
    }
}

/// `Some(mtime)` if the file is still a regular file of the candidate's size (`mtime` is
/// `None` when the system does not report one: the hashes are then not cached).
fn stat(c: &Candidate) -> Option<Option<i64>> {
    let meta = std::fs::metadata(&c.path).ok()?;
    if !meta.is_file() || meta.len() != c.size {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    Some(mtime)
}

fn open(path: &Path) -> Result<File, ReadError> {
    File::open(path).map_err(|_| ReadError::Skip)
}

/// Reads exactly `buf.len()` bytes, charging the budget.
fn read_exact(f: &mut File, buf: &mut [u8], budget: &Budget) -> Result<(), ReadError> {
    f.read_exact(buf).map_err(|_| ReadError::Skip)?;
    budget.charge(buf.len());
    Ok(())
}

/// The partial hash, and the full hash too when the partial hash covered the whole file.
fn partial_hash(
    c: &Candidate,
    budget: &Budget,
    cancel: &AtomicBool,
) -> Result<(Digest, Option<Digest>), ReadError> {
    let mut f = open(&c.path)?;
    if c.size <= 2 * PARTIAL_CHUNK {
        // Small: one read of the whole file (`size` checked by `stat`; a file that grew since
        // is caught by reading one byte past the end).
        let mut buf = vec![0u8; usize::try_from(c.size).map_err(|_| ReadError::Skip)?];
        read_exact(&mut f, &mut buf, budget)?;
        if !at_eof(&mut f)? {
            return Err(ReadError::Skip);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadError::Stopped);
        }
        let h = *blake3::hash(&buf).as_bytes();
        return Ok((h, Some(h)));
    }
    let chunk = PARTIAL_CHUNK as usize;
    let mut buf = vec![0u8; chunk];
    let mut hasher = blake3::Hasher::new();
    hasher.update(&c.size.to_le_bytes());
    read_exact(&mut f, &mut buf, budget)?;
    hasher.update(&buf);
    f.seek(SeekFrom::Start(c.size - PARTIAL_CHUNK))
        .map_err(|_| ReadError::Skip)?;
    read_exact(&mut f, &mut buf, budget)?;
    hasher.update(&buf);
    if !at_eof(&mut f)? {
        return Err(ReadError::Skip);
    }
    Ok((*hasher.finalize().as_bytes(), None))
}

/// The BLAKE3 hash of the whole file (exactly `size` bytes), checking the cancel flag between
/// reads.
fn full_hash(c: &Candidate, budget: &Budget, cancel: &AtomicBool) -> Result<Digest, ReadError> {
    let mut f = open(&c.path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_BUF];
    let mut total: u64 = 0;
    while total < c.size {
        // Only cancelling stops a file once started: a hash cannot be resumed later, so a
        // budget check here would never let a file bigger than the budget finish.
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadError::Stopped);
        }
        let want = usize::try_from(c.size - total).map_or(buf.len(), |left| left.min(buf.len()));
        let n = match f.read(&mut buf[..want]) {
            Ok(0) => return Err(ReadError::Skip), // shrank
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadError::Skip),
        };
        budget.charge(n);
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    if !at_eof(&mut f)? {
        return Err(ReadError::Skip); // grew
    }
    Ok(*hasher.finalize().as_bytes())
}

/// True if `f` has nothing left to read.
fn at_eof(f: &mut File) -> Result<bool, ReadError> {
    let mut one = [0u8; 1];
    loop {
        match f.read(&mut one) {
            Ok(n) => return Ok(n == 0),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return Err(ReadError::Skip),
        }
    }
}

#[cfg(test)]
mod tests;
