//! The SQLite cache of image hashes (`<app_data>/hashes/phash.sqlite3`).
//!
//! One row per file, keyed by a caller-chosen path key (the app uses its case-folded path key),
//! valid only while the file's size and modification time are unchanged and the hash algorithm
//! is the same ([`HASH_VERSION`]). Files that failed to hash are remembered too (`hash` NULL),
//! so a corrupt picture is not decoded again on every search. A file SQLite cannot open is
//! renamed to `<name>.corrupt-<unix ms>` and a fresh cache is started: it is only a cache.
//!
//! Rows of files that were deleted or changed size, and rows of an older hash version, are
//! removed by [`HashCache::prune`], a little at a time (see [`PruneLimits`]), resuming where
//! the previous call stopped.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::ImageHash;

/// File name of the cache inside its folder.
pub const CACHE_FILE: &str = "phash.sqlite3";

/// Version of the hashing pipeline; rows of another version are ignored (and overwritten).
/// 2: EXIF orientation applied before hashing (a photo turned by its EXIF tag matches its
/// upright copy).
const HASH_VERSION: i64 = 2;

/// Schema version kept in SQLite's `user_version`.
const SCHEMA_VERSION: i64 = 1;

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
    /// Rows looked at (rows of an older hash version are not counted).
    pub checked: usize,
    /// Rows removed, older hash versions included.
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

/// Why the cache failed.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// SQLite reported an error.
    #[error("hash cache database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Creating the folder or renaming a corrupt file failed.
    #[error("hash cache file error: {0}")]
    Io(#[from] std::io::Error),
}

/// What the cache knows about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedHash {
    /// Its hash.
    Hash(ImageHash),
    /// Hashing it failed (unreadable, corrupt, unsupported, too large).
    Failed,
}

/// The hash cache; share it behind an `Arc` (the connection sits behind a mutex).
#[derive(Debug)]
pub struct HashCache {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl HashCache {
    /// Opens (or creates) the cache file at `path`, creating its folder. A file SQLite cannot
    /// read is set aside and replaced by an empty cache.
    pub fn open(path: &Path) -> Result<Self, CacheError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = match open_conn(path) {
            Ok(conn) => conn,
            Err(first) => {
                if !path.exists() {
                    return Err(first);
                }
                let ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis());
                let mut aside = path.as_os_str().to_owned();
                aside.push(format!(".corrupt-{ms}"));
                std::fs::rename(path, &aside)?;
                open_conn(path)?
            }
        };
        Ok(HashCache {
            conn: Mutex::new(conn),
            path: Some(path.to_owned()),
        })
    }

    /// A cache that lives in memory only (tests, or when no app data folder exists).
    pub fn in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory()?;
        init(&conn)?;
        Ok(HashCache {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// The cache file, `None` for an in-memory cache.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Looks up `(key, size, modified)` triples; a row whose size or time differ is a miss.
    pub fn get_many(
        &self,
        files: &[(&str, u64, i64)],
    ) -> Result<Vec<Option<CachedHash>>, CacheError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stmt =
            conn.prepare_cached("SELECT size, mtime, hash, version FROM phash WHERE key = ?1")?;
        let mut out = Vec::with_capacity(files.len());
        for &(key, size, modified) in files {
            let row = stmt
                .query_row(params![key], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .optional()?;
            out.push(row.and_then(|(s, m, hash, version)| {
                (s == size_value(size) && m == modified && version == HASH_VERSION).then(|| {
                    match hash {
                        Some(h) => CachedHash::Hash(ImageHash(u64::from_ne_bytes(h.to_ne_bytes()))),
                        None => CachedHash::Failed,
                    }
                })
            }));
        }
        Ok(out)
    }

    /// Stores outcomes: `(key, size, modified, hash or None for a failure)`, in one
    /// transaction.
    pub fn put_many(&self, rows: &[(&str, u64, i64, Option<ImageHash>)]) -> Result<(), CacheError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO phash (key, size, mtime, hash, version) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for &(key, size, modified, hash) in rows {
                let hash = hash.map(|h| i64::from_ne_bytes(h.0.to_ne_bytes()));
                stmt.execute(params![key, size_value(size), modified, hash, HASH_VERSION])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Number of rows (for tests and diagnostics).
    pub fn len(&self) -> Result<u64, CacheError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM phash", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// True when the cache holds no row.
    pub fn is_empty(&self) -> Result<bool, CacheError> {
        Ok(self.len()? == 0)
    }

    /// Removes rows of an older hash version, then rows whose file is gone or has another
    /// size than when it was hashed, checking at most `limits` rows from where the previous
    /// call stopped (keys are taken as paths, as the app's case-folded path keys are). A file
    /// whose drive or share is unreachable (an unplugged disk) keeps its row, and so does one
    /// that cannot be checked for another reason (no permission). Compacts the file when much
    /// of it became free.
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
        let mut cursor = {
            let conn = self.lock();
            // Never hit again: one statement, no file checks.
            report.removed += conn.execute(
                "DELETE FROM phash WHERE version <> ?1",
                params![HASH_VERSION],
            )?;
            read_cursor(&conn)?
        };
        while report.checked < limits.max_rows && started.elapsed() < limits.max_time {
            let want = PRUNE_BATCH.min(limits.max_rows - report.checked);
            // The lock is held for the query and the deletions, never while files are checked.
            let rows: Vec<(String, i64)> = {
                let conn = self.lock();
                let mut stmt = conn.prepare_cached(
                    "SELECT key, size FROM phash WHERE key > ?1 ORDER BY key LIMIT ?2",
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
                    let mut stmt = tx.prepare_cached("DELETE FROM phash WHERE key = ?1")?;
                    for key in &gone {
                        report.removed += stmt.execute(params![key])?;
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

    fn lock(&self) -> MutexGuard<'_, Connection> {
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

/// Disk checks and compaction (the duplicate hash cache in `athrix-dedupe` has the same).
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

/// Sizes are stored as SQLite integers (i64); no file is that big, so saturate.
fn size_value(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

fn open_conn(path: &Path) -> Result<Connection, CacheError> {
    let conn = Connection::open(path)?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<(), CacheError> {
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    // WAL: readers never wait for the writer; NORMAL sync is enough for a cache.
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS phash;
             CREATE TABLE phash (
                 key TEXT PRIMARY KEY NOT NULL,
                 size INTEGER NOT NULL,
                 mtime INTEGER NOT NULL,
                 hash INTEGER,
                 version INTEGER NOT NULL
             ) WITHOUT ROWID;
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
             name TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_need_the_same_size_and_time() {
        let cache = HashCache::in_memory().unwrap();
        let h = ImageHash(0xDEAD_BEEF_0123_4567 | (1 << 63));
        cache
            .put_many(&[("c:/a.jpg", 10, 100, Some(h)), ("c:/b.jpg", 5, 7, None)])
            .unwrap();
        let got = cache
            .get_many(&[
                ("c:/a.jpg", 10, 100),
                ("c:/a.jpg", 11, 100),
                ("c:/a.jpg", 10, 101),
                ("c:/b.jpg", 5, 7),
                ("c:/zzz.jpg", 1, 1),
            ])
            .unwrap();
        assert_eq!(
            got,
            vec![
                Some(CachedHash::Hash(h)),
                None,
                None,
                Some(CachedHash::Failed),
                None
            ]
        );
        // Replacing a row.
        cache.put_many(&[("c:/a.jpg", 11, 100, None)]).unwrap();
        assert_eq!(
            cache.get_many(&[("c:/a.jpg", 11, 100)]).unwrap(),
            vec![Some(CachedHash::Failed)]
        );
        assert_eq!(cache.len().unwrap(), 2);
    }

    #[test]
    fn rows_of_an_older_hash_version_are_misses_and_pruned() {
        let cache = HashCache::in_memory().unwrap();
        cache
            .put_many(&[("new", 1, 1, Some(ImageHash(1)))])
            .unwrap();
        cache
            .lock()
            .execute(
                "INSERT INTO phash (key, size, mtime, hash, version) VALUES ('old', 1, 1, 5, 1)",
                [],
            )
            .unwrap();
        assert_eq!(cache.get_many(&[("old", 1, 1)]).unwrap(), vec![None]);
        let r = cache
            .prune_with(&PruneLimits::default(), |_, _| true)
            .unwrap();
        assert_eq!((r.checked, r.removed, r.wrapped), (1, 1, true));
        assert_eq!(cache.len().unwrap(), 1);
    }

    #[test]
    fn prune_removes_rejected_rows_in_bounded_resumable_steps() {
        let cache = HashCache::in_memory().unwrap();
        let keys: Vec<String> = (0..700).map(|i| format!("k{i:04}")).collect();
        let rows: Vec<(&str, u64, i64, Option<ImageHash>)> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i as u64, 1, None))
            .collect();
        cache.put_many(&rows).unwrap();
        let keep = |_: &str, size: u64| size % 3 != 0;
        let limits = PruneLimits {
            max_rows: 256,
            max_time: Duration::from_secs(60),
        };
        let mut passes = Vec::new();
        loop {
            let r = cache.prune_with(&limits, keep).unwrap();
            passes.push(r.checked);
            if r.wrapped {
                break;
            }
        }
        assert_eq!(passes, vec![256, 256, 188]);
        assert_eq!(cache.len().unwrap(), 700 - 234);
        assert_eq!(
            cache.get_many(&[("k0003", 3, 1), ("k0004", 4, 1)]).unwrap(),
            vec![None, Some(CachedHash::Failed)]
        );
        // A time budget already spent checks nothing.
        let spent = PruneLimits {
            max_rows: 1000,
            max_time: Duration::ZERO,
        };
        assert_eq!(cache.prune_with(&spent, |_, _| false).unwrap().checked, 0);
    }

    #[test]
    fn prune_checks_the_disk_and_compacts_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let cache = HashCache::open(&path).unwrap();
        let here = dir.path().join("here.jpg");
        let resized = dir.path().join("resized.jpg");
        std::fs::write(&here, [1u8; 10]).unwrap();
        std::fs::write(&resized, [1u8; 11]).unwrap();
        let mut keys = vec![
            here.to_string_lossy().into_owned(),
            resized.to_string_lossy().into_owned(),
        ];
        for i in 0..12_000 {
            let gone = dir.path().join(format!("deleted picture number {i}.jpg"));
            keys.push(gone.to_string_lossy().into_owned());
        }
        let rows: Vec<(&str, u64, i64, Option<ImageHash>)> = keys
            .iter()
            .map(|k| (k.as_str(), 10, 1, Some(ImageHash(7))))
            .collect();
        cache.put_many(&rows).unwrap();
        // Generous: a debug build on a slow disk must finish the pass in one call.
        let generous = PruneLimits {
            max_rows: 100_000,
            max_time: Duration::from_secs(120),
        };
        let r = cache.prune(&generous).unwrap();
        assert_eq!(r.removed, 12_001, "{r:?}");
        assert!(r.wrapped && r.vacuumed, "{r:?}");
        assert_eq!(
            cache.get_many(&[(keys[0].as_str(), 10, 1)]).unwrap(),
            vec![Some(CachedHash::Hash(ImageHash(7)))]
        );
        assert_eq!(cache.len().unwrap(), 1);
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
        let cache = HashCache::in_memory().unwrap();
        cache.put_many(&[(missing.as_str(), 1, 1, None)]).unwrap();
        assert_eq!(cache.prune(&PruneLimits::default()).unwrap().removed, 0);
        assert_eq!(cache.len().unwrap(), 1);
    }

    #[test]
    fn persists_and_recovers_from_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hashes").join(CACHE_FILE);
        {
            let cache = HashCache::open(&path).unwrap();
            cache.put_many(&[("k", 1, 2, Some(ImageHash(3)))]).unwrap();
        }
        let cache = HashCache::open(&path).unwrap();
        assert_eq!(
            cache.get_many(&[("k", 1, 2)]).unwrap(),
            vec![Some(CachedHash::Hash(ImageHash(3)))]
        );
        drop(cache);

        let bad = dir.path().join("bad.sqlite3");
        std::fs::write(&bad, vec![0x5Au8; 8192]).unwrap();
        let cache = HashCache::open(&bad).unwrap();
        assert!(cache.is_empty().unwrap());
        let aside = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("bad.sqlite3.corrupt-")
            });
        assert!(aside);
    }
}
