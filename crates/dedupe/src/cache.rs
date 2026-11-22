//! The hash cache: a small SQLite file remembering the hashes of files already read, so a
//! second `dupe:` search (or "continue the scan") does not read them again.
//!
//! One row per file, keyed by the caller's path key, valid only for the size and modification
//! time it was computed with: a row whose size or time differs is ignored by [`HashCache::get`]
//! and replaced by the next [`HashCache::put_many`].
//!
//! Rows of files that were deleted or changed size are removed by [`HashCache::prune`], a
//! little at a time (see [`PruneLimits`]), resuming where the previous call stopped.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, params};

use crate::Digest;

/// Schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// A cache error (opening or creating the file).
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// SQLite failed.
    #[error("hash cache: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The folder could not be created.
    #[error("hash cache folder: {0}")]
    Io(#[from] std::io::Error),
}

/// Cached hashes of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedHashes {
    /// Hash of the first and last 64 KiB (the whole file when it is small).
    pub partial: Digest,
    /// Hash of the whole file, once computed.
    pub full: Option<Digest>,
}

/// One row to store (see [`HashCache::put_many`]).
#[derive(Debug, Clone)]
pub struct CacheRow {
    /// The caller's path key.
    pub key: String,
    /// File size in bytes when hashed.
    pub size: u64,
    /// Modification time when hashed (nanoseconds since the Unix epoch).
    pub mtime: i64,
    /// The hashes.
    pub hashes: CachedHashes,
}

/// Bounds of one [`HashCache::prune`] call; the next call resumes where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneLimits {
    /// Most rows checked.
    pub max_rows: usize,
    /// No new batch of rows is started after this long.
    pub max_time: Duration,
}

impl Default for PruneLimits {
    /// 20 000 rows or half a second, whichever comes first.
    fn default() -> Self {
        PruneLimits {
            max_rows: 20_000,
            max_time: Duration::from_millis(500),
        }
    }
}

/// What one [`HashCache::prune`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Rows looked at.
    pub checked: usize,
    /// Rows removed.
    pub removed: usize,
    /// The pass reached the last row; the next call starts again from the first.
    pub wrapped: bool,
    /// The file was compacted (`VACUUM`) because much of it had become free.
    pub vacuumed: bool,
}

/// Rows read per prune batch.
const PRUNE_BATCH: usize = 256;

/// `meta` row holding the key the next prune starts after.
const PRUNE_CURSOR: &str = "prune_cursor";

/// The SQLite hash cache. Cheap to share behind an `Arc`; calls are serialized internally.
#[derive(Debug)]
pub struct HashCache {
    conn: Mutex<Connection>,
}

impl HashCache {
    /// Opens (or creates) the cache at `path`, creating its folder. A file that cannot be
    /// used (corrupt, or from another schema) is deleted and started again: it is only a cache.
    pub fn open(path: &Path) -> Result<HashCache, CacheError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match Self::open_at(path) {
            Ok(cache) => Ok(cache),
            Err(_) => {
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = path.as_os_str().to_owned();
                    p.push(suffix);
                    let _ = std::fs::remove_file(p);
                }
                Self::open_at(path)
            }
        }
    }

    /// An in-memory cache (tests, or when no app data folder exists).
    pub fn in_memory() -> Result<HashCache, CacheError> {
        let conn = Connection::open_in_memory()?;
        init(&conn)?;
        Ok(HashCache {
            conn: Mutex::new(conn),
        })
    }

    fn open_at(path: &Path) -> Result<HashCache, CacheError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(CacheError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        init(&conn)?;
        Ok(HashCache {
            conn: Mutex::new(conn),
        })
    }

    /// The hashes cached for `key`, if they were computed for this `size` and `mtime`.
    /// Errors read as a miss.
    pub fn get(&self, key: &str, size: u64, mtime: i64) -> Option<CachedHashes> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        lookup(&conn, key, size, mtime).ok().flatten()
    }

    /// Looks up many keys under one lock; `None` for a miss or a stale row.
    pub fn get_many<'a>(
        &self,
        keys: impl IntoIterator<Item = (&'a str, u64, i64)>,
    ) -> Vec<Option<CachedHashes>> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        keys.into_iter()
            .map(|(k, s, m)| lookup(&conn, k, s, m).ok().flatten())
            .collect()
    }

    /// Stores `rows` in one transaction, replacing older rows of the same keys.
    pub fn put_many(&self, rows: &[CacheRow]) -> Result<(), CacheError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO hashes (path, size, mtime, partial, full)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.key,
                    size_to_sql(r.size),
                    r.mtime,
                    &r.hashes.partial[..],
                    r.hashes.full.as_ref().map(|f| &f[..]),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Number of rows (stale ones included).
    pub fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        conn.query_row("SELECT COUNT(*) FROM hashes", [], |r| r.get::<_, i64>(0))
            .map_or(0, |n| usize::try_from(n).unwrap_or(0))
    }

    /// True when the cache has no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes rows whose file is gone or has another size than when it was hashed, checking
    /// at most `limits` rows from where the previous call stopped (keys are taken as paths,
    /// as the app's case-folded path keys are). A file whose drive or share is unreachable
    /// (an unplugged disk) keeps its row, and so does one that cannot be checked for another
    /// reason (no permission). Compacts the file when much of it became free.
    pub fn prune(&self, limits: &PruneLimits) -> Result<PruneReport, CacheError> {
        self.prune_with(limits, disk::file_matches)
    }

    /// [`HashCache::prune`] with the caller's test: `keep(key, size)` says whether the row of
    /// `key`, hashed at `size` bytes, is still worth keeping.
    pub fn prune_with(
        &self,
        limits: &PruneLimits,
        mut keep: impl FnMut(&str, u64) -> bool,
    ) -> Result<PruneReport, CacheError> {
        let started = Instant::now();
        let mut report = PruneReport::default();
        let mut cursor = read_cursor(&self.lock())?;
        while report.checked < limits.max_rows && started.elapsed() < limits.max_time {
            let want = PRUNE_BATCH.min(limits.max_rows - report.checked);
            // The lock is held for the query and the deletions, never while files are checked.
            let rows: Vec<(String, i64)> = {
                let conn = self.lock();
                let mut stmt = conn.prepare_cached(
                    "SELECT path, size FROM hashes WHERE path > ?1 ORDER BY path LIMIT ?2",
                )?;
                let limit = i64::try_from(want).unwrap_or(i64::MAX);
                stmt.query_map(params![cursor, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            let gone: Vec<&str> = rows
                .iter()
                .filter(|(key, size)| !keep(key, u64::try_from(*size).unwrap_or(0)))
                .map(|(key, _)| key.as_str())
                .collect();
            report.checked += rows.len();
            if !gone.is_empty() {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare_cached("DELETE FROM hashes WHERE path = ?1")?;
                    for key in &gone {
                        report.removed += stmt.execute([key])?;
                    }
                }
                tx.commit()?;
            }
            match rows.last() {
                Some((last, _)) if rows.len() == want => cursor.clone_from(last),
                _ => {
                    report.wrapped = true;
                    cursor.clear();
                    break;
                }
            }
        }
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO meta (name, value) VALUES (?1, ?2)",
            params![PRUNE_CURSOR, cursor],
        )?;
        if report.removed > 0 {
            report.vacuumed = disk::vacuum_if_sparse(&conn)?;
        }
        Ok(report)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where the previous prune stopped (`""`: at the start).
fn read_cursor(conn: &Connection) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE name = ?1",
        [PRUNE_CURSOR],
        |r| r.get(0),
    )
    .optional()
    .map(Option::unwrap_or_default)
}

/// Disk checks and compaction (the image hash cache in `athrix-imagehash` has the same).
mod disk {
    use std::io::ErrorKind;
    use std::path::{Component, Path, PathBuf};

    use rusqlite::Connection;

    /// Compact the file when at least a quarter of it is free pages and it has at least this
    /// many pages (1 MiB with 4 KiB pages).
    const VACUUM_MIN_PAGES: i64 = 256;

    /// `VACUUM` when at least a quarter of a not-tiny file is free pages. Returns whether it
    /// ran.
    pub(super) fn vacuum_if_sparse(conn: &Connection) -> rusqlite::Result<bool> {
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        if pages < VACUUM_MIN_PAGES || free * 4 < pages {
            return Ok(false);
        }
        conn.execute_batch("VACUUM;")?;
        Ok(true)
    }

    /// The default prune test: the file `key` still exists with `size` bytes. Kept too when
    /// it cannot be told: its drive or share is unreachable, or the check fails otherwise.
    pub(super) fn file_matches(key: &str, size: u64) -> bool {
        match std::fs::metadata(key) {
            Ok(meta) => meta.is_file() && meta.len() == size,
            Err(e) if e.kind() == ErrorKind::NotFound => root_missing(Path::new(key)),
            Err(_) => true,
        }
    }

    /// Whether the root (drive, share or `/`) of the absolute `path` is missing.
    fn root_missing(path: &Path) -> bool {
        let root: PathBuf = path
            .components()
            .take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
            .collect();
        !root.as_os_str().is_empty() && std::fs::metadata(&root).is_err()
    }
}

fn init(conn: &Connection) -> Result<(), CacheError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(CacheError::Sqlite(rusqlite::Error::InvalidQuery));
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hashes (
             path TEXT PRIMARY KEY,
             size INTEGER NOT NULL,
             mtime INTEGER NOT NULL,
             partial BLOB NOT NULL,
             full BLOB
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS meta (
             name TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL
         ) WITHOUT ROWID;
         PRAGMA user_version = 1;",
    )?;
    Ok(())
}

/// SQLite integers are signed: sizes above `i64::MAX` (never seen) are clamped.
fn size_to_sql(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

fn lookup(
    conn: &Connection,
    key: &str,
    size: u64,
    mtime: i64,
) -> rusqlite::Result<Option<CachedHashes>> {
    let mut stmt =
        conn.prepare_cached("SELECT size, mtime, partial, full FROM hashes WHERE path = ?1")?;
    let row = stmt
        .query_row([key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })
        .optional()?;
    let Some((s, m, partial, full)) = row else {
        return Ok(None);
    };
    if s != size_to_sql(size) || m != mtime {
        return Ok(None);
    }
    let Ok(partial) = Digest::try_from(partial.as_slice()) else {
        return Ok(None);
    };
    let full = full.and_then(|f| Digest::try_from(f.as_slice()).ok());
    Ok(Some(CachedHashes { partial, full }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, size: u64, mtime: i64, b: u8, full: bool) -> CacheRow {
        CacheRow {
            key: key.to_owned(),
            size,
            mtime,
            hashes: CachedHashes {
                partial: [b; 32],
                full: full.then_some([b.wrapping_add(1); 32]),
            },
        }
    }

    #[test]
    fn stale_rows_are_ignored_and_replaced() {
        let c = HashCache::in_memory().unwrap();
        c.put_many(&[row("a", 10, 5, 1, false)]).unwrap();
        assert_eq!(c.get("a", 10, 5).unwrap().partial, [1; 32]);
        assert!(c.get("a", 10, 5).unwrap().full.is_none());
        assert!(c.get("a", 11, 5).is_none(), "other size");
        assert!(c.get("a", 10, 6).is_none(), "other mtime");
        assert!(c.get("b", 10, 5).is_none());
        c.put_many(&[row("a", 10, 6, 7, true)]).unwrap();
        assert!(c.get("a", 10, 5).is_none());
        assert_eq!(c.get("a", 10, 6).unwrap().full, Some([8; 32]));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn prune_removes_rejected_rows_in_bounded_resumable_steps() {
        let c = HashCache::in_memory().unwrap();
        let rows: Vec<CacheRow> = (0..1000)
            .map(|i| row(&format!("k{i:04}"), i, 1, 1, false))
            .collect();
        c.put_many(&rows).unwrap();
        // Odd sizes are "gone".
        let keep = |_: &str, size: u64| size % 2 == 0;
        let limits = PruneLimits {
            max_rows: 300,
            max_time: Duration::from_secs(60),
        };
        let first = c.prune_with(&limits, keep).unwrap();
        assert_eq!(
            (first.checked, first.removed, first.wrapped),
            (300, 150, false)
        );
        assert_eq!(c.len(), 850);
        // The next calls resume after the last checked row and finish the pass.
        let mut checked = first.checked;
        let mut wrapped = false;
        while !wrapped {
            let r = c.prune_with(&limits, keep).unwrap();
            checked += r.checked;
            wrapped = r.wrapped;
        }
        assert_eq!(checked, 1000);
        assert_eq!(c.len(), 500);
        assert!(c.get("k0002", 2, 1).is_some());
        assert!(c.get("k0003", 3, 1).is_none());
        // A new pass starts from the first row again; nothing is left to remove.
        let again = c.prune_with(&PruneLimits::default(), keep).unwrap();
        assert_eq!(
            (again.checked, again.removed, again.wrapped),
            (500, 0, true)
        );
        // Zero rows allowed: nothing is looked at.
        let none = PruneLimits {
            max_rows: 0,
            ..PruneLimits::default()
        };
        assert_eq!(c.prune_with(&none, |_, _| false).unwrap().checked, 0);
        assert_eq!(c.len(), 500);
    }

    #[test]
    fn prune_checks_the_disk_and_compacts_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dupes.sqlite3");
        let c = HashCache::open(&path).unwrap();
        let here = dir.path().join("here.bin");
        let resized = dir.path().join("resized.bin");
        std::fs::write(&here, [1u8; 10]).unwrap();
        std::fs::write(&resized, [1u8; 11]).unwrap();
        let key = |p: &Path| p.to_string_lossy().into_owned();
        let mut rows = vec![
            row(&key(&here), 10, 1, 1, true),
            row(&key(&resized), 10, 1, 1, true),
            row(&key(dir.path()), 0, 1, 1, true),
        ];
        // Many rows of deleted files, enough to leave most of the file free.
        for i in 0..8000 {
            let gone = dir.path().join(format!("gone-{i}.bin"));
            rows.push(row(&key(&gone), 5, 1, 1, true));
        }
        c.put_many(&rows).unwrap();
        // Generous: a debug build on a slow disk must finish the pass in one call.
        let generous = PruneLimits {
            max_rows: 100_000,
            max_time: Duration::from_secs(120),
        };
        let r = c.prune(&generous).unwrap();
        assert_eq!(r.removed, 8002, "gone, resized and a folder");
        assert!(r.wrapped && r.vacuumed, "{r:?}");
        assert_eq!(c.len(), 1);
        assert!(c.get(&key(&here), 10, 1).is_some());
        // Reopened, the cursor was saved at the start of a new pass.
        drop(c);
        let c = HashCache::open(&path).unwrap();
        assert_eq!(c.prune(&PruneLimits::default()).unwrap().checked, 1);
    }

    #[cfg(windows)]
    #[test]
    fn rows_on_an_unreachable_drive_are_kept() {
        let Some(letter) = (b'D'..=b'Z')
            .rev()
            .map(char::from)
            .find(|l| std::fs::metadata(format!("{l}:\\")).is_err())
        else {
            return;
        };
        let missing = format!("{}:/photos/a.jpg", letter.to_ascii_lowercase());
        assert!(disk::file_matches(&missing, 1));
        let c = HashCache::in_memory().unwrap();
        c.put_many(&[row(&missing, 1, 1, 1, false)]).unwrap();
        assert_eq!(c.prune(&PruneLimits::default()).unwrap().removed, 0);
        // On a reachable drive a missing file is removed.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!disk::file_matches(
            &tmp.path().join("nope").to_string_lossy(),
            1
        ));
    }

    #[test]
    fn a_file_cache_survives_reopening_and_a_corrupt_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hashes").join("dupes.sqlite3");
        {
            let c = HashCache::open(&path).unwrap();
            c.put_many(&[row("a", 1, 1, 3, true)]).unwrap();
        }
        let c = HashCache::open(&path).unwrap();
        assert_eq!(c.get("a", 1, 1).unwrap().partial, [3; 32]);
        drop(c);
        for suffix in ["-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
        std::fs::write(&path, b"not a database at all, just some bytes").unwrap();
        let c = HashCache::open(&path).unwrap();
        assert!(c.is_empty());
    }
}
