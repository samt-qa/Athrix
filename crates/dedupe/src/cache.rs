//! The hash cache: a small SQLite file remembering the hashes of files already read, so a
//! second `dupe:` search (or "continue the scan") does not read them again.
//!
//! One row per file, keyed by the caller's path key, valid only for the size and modification
//! time it was computed with: a row whose size or time differs is ignored by [`HashCache::get`]
//! and replaced by the next [`HashCache::put_many`].

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

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
