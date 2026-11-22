//! The SQLite cache of image hashes (`<app_data>/hashes/phash.sqlite3`).
//!
//! One row per file, keyed by a caller-chosen path key (the app uses its case-folded path key),
//! valid only while the file's size and modification time are unchanged and the hash algorithm
//! is the same ([`HASH_VERSION`]). Files that failed to hash are remembered too (`hash` NULL),
//! so a corrupt picture is not decoded again on every search. A file SQLite cannot open is
//! renamed to `<name>.corrupt-<unix ms>` and a fresh cache is started: it is only a cache.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::ImageHash;

/// File name of the cache inside its folder.
pub const CACHE_FILE: &str = "phash.sqlite3";

/// Version of the hashing pipeline; rows of another version are ignored (and overwritten).
const HASH_VERSION: i64 = 1;

/// Schema version kept in SQLite's `user_version`.
const SCHEMA_VERSION: i64 = 1;

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
