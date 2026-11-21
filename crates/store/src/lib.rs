//! SQLite store of the user's search library: history, saved searches and favorites.
//!
//! One file (`library.sqlite3` in the app data folder), opened once with [`Store::open`] and
//! shared by the app's commands (the connection sits behind a mutex). The schema version is
//! SQLite's `user_version`; [`Store::open`] migrates older files. A file SQLite cannot read is
//! renamed to `<name>.corrupt-<unix ms>` and a fresh one is created, so a damaged library never
//! stops the app from starting (see [`Store::recovered_from`]).
//!
//! - History is deduplicated by (query, scope): adding again bumps `searchedAt` and `count`. At
//!   most [`HISTORY_CAP`] rows are kept (the oldest go first). Empty queries are ignored.
//! - Saved searches have a trimmed, non-empty name of at most [`MAX_NAME_CHARS`] characters.
//! - Favorites are keyed by full path, compared case-insensitively on Windows ([`path_key`]).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use serde::Serialize;

/// Most history rows kept; adding one more drops the oldest.
pub const HISTORY_CAP: usize = 500;

/// Most characters in a saved search's name.
pub const MAX_NAME_CHARS: usize = 120;

/// Schema version written to `user_version` by the latest migration.
const SCHEMA_VERSION: i64 = 1;

/// Why a store operation failed. `Display` is the message shown to the user (English).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite reported an error.
    #[error("library database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A file operation (creating the folder, renaming a corrupt file) failed.
    #[error("library file error: {0}")]
    Io(#[from] std::io::Error),
    /// The file was written by a newer version of the app.
    #[error("the library was created by a newer version of Athrix (schema {0})")]
    NewerSchema(i64),
    /// A saved search's name is empty after trimming.
    #[error("the name is empty")]
    EmptyName,
    /// A saved search's name is longer than [`MAX_NAME_CHARS`].
    #[error("the name is longer than {MAX_NAME_CHARS} characters")]
    NameTooLong,
    /// A saved search's query is empty after trimming.
    #[error("the query is empty")]
    EmptyQuery,
    /// No saved search has this id.
    #[error("saved search not found")]
    SavedNotFound,
}

/// A search in the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Row id (for `history_remove`).
    pub id: i64,
    /// The query text (trimmed).
    pub query: String,
    /// The search scope, `None` for the whole index.
    pub scope: Option<String>,
    /// Last time it was searched, unix ms.
    pub searched_at: i64,
    /// How many times it was searched.
    pub count: i64,
}

/// A named, saved search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSearch {
    /// Row id.
    pub id: i64,
    /// The user's name for it (trimmed).
    pub name: String,
    /// The query text (trimmed).
    pub query: String,
    /// The search scope, `None` for the whole index.
    pub scope: Option<String>,
    /// When it was saved, unix ms.
    pub created_at: i64,
}

/// A favorite file or folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Favorite {
    /// Full path, as it was when added.
    pub path: String,
    /// Last component of the path (the whole path for a drive root).
    pub name: String,
    /// The entry is a folder.
    pub is_dir: bool,
    /// When it was added, unix ms.
    pub added_at: i64,
    /// The path exists now (checked when listing).
    pub exists: bool,
}

/// The library store. Thread-safe: every call locks the one connection.
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
    recovered_from: Option<PathBuf>,
}

impl Store {
    /// Opens (or creates) the store at `path`, creating its folder and migrating the schema.
    /// A corrupt file is renamed to `<name>.corrupt-<unix ms>` and replaced by an empty one.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        match open_connection(path) {
            Ok(conn) => Ok(Store {
                conn: Mutex::new(conn),
                recovered_from: None,
            }),
            Err(e) if is_corruption(&e) => {
                let moved = set_aside(path)?;
                let conn = open_connection(path)?;
                Ok(Store {
                    conn: Mutex::new(conn),
                    recovered_from: Some(moved),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Where a corrupt file found by [`Store::open`] was moved, if one was.
    pub fn recovered_from(&self) -> Option<&Path> {
        self.recovered_from.as_deref()
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- History ----

    /// The most recent `limit` searches (at most [`HISTORY_CAP`]), newest first.
    pub fn history_list(&self, limit: usize) -> Result<Vec<HistoryEntry>, StoreError> {
        let limit = limit.min(HISTORY_CAP) as i64;
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, query, scope, searched_at, count FROM history
             ORDER BY searched_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            let scope: String = r.get(2)?;
            Ok(HistoryEntry {
                id: r.get(0)?,
                query: r.get(1)?,
                scope: (!scope.is_empty()).then_some(scope),
                searched_at: r.get(3)?,
                count: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a search. An empty (or whitespace) query is ignored; a (query, scope) already
    /// in the history is bumped (`searchedAt` = now, `count` + 1) instead of added again.
    pub fn history_add(&self, query: &str, scope: Option<&str>) -> Result<(), StoreError> {
        self.history_add_at(query, scope, now_ms())
    }

    fn history_add_at(&self, query: &str, scope: Option<&str>, now: i64) -> Result<(), StoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(());
        }
        // An empty scope is stored as '' so the UNIQUE constraint dedupes "no scope" too.
        let scope = scope.map(str::trim).unwrap_or_default();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO history (query, scope, searched_at, count) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT (query, scope)
             DO UPDATE SET searched_at = excluded.searched_at, count = count + 1",
            params![query, scope, now],
        )?;
        tx.execute(
            "DELETE FROM history WHERE id NOT IN
             (SELECT id FROM history ORDER BY searched_at DESC, id DESC LIMIT ?1)",
            [HISTORY_CAP as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes one history row (no error if it is already gone).
    pub fn history_remove(&self, id: i64) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM history WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Removes every history row.
    pub fn history_clear(&self) -> Result<(), StoreError> {
        self.conn().execute("DELETE FROM history", [])?;
        Ok(())
    }

    // ---- Saved searches ----

    /// Every saved search, newest first.
    pub fn saved_list(&self) -> Result<Vec<SavedSearch>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, query, scope, created_at FROM saved
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], saved_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Saves a search under `name` (trimmed, non-empty, at most [`MAX_NAME_CHARS`] characters).
    /// Names need not be unique.
    pub fn saved_create(
        &self,
        name: &str,
        query: &str,
        scope: Option<&str>,
    ) -> Result<SavedSearch, StoreError> {
        let name = check_name(name)?;
        let query = query.trim();
        if query.is_empty() {
            return Err(StoreError::EmptyQuery);
        }
        let scope = scope.map(str::trim).filter(|s| !s.is_empty());
        let created_at = now_ms();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO saved (name, query, scope, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![name, query, scope, created_at],
        )?;
        Ok(SavedSearch {
            id: conn.last_insert_rowid(),
            name: name.to_owned(),
            query: query.to_owned(),
            scope: scope.map(str::to_owned),
            created_at,
        })
    }

    /// Renames a saved search (same name rules as [`Store::saved_create`]).
    pub fn saved_rename(&self, id: i64, name: &str) -> Result<SavedSearch, StoreError> {
        let name = check_name(name)?;
        let conn = self.conn();
        conn.execute(
            "UPDATE saved SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        conn.query_row(
            "SELECT id, name, query, scope, created_at FROM saved WHERE id = ?1",
            [id],
            saved_from_row,
        )
        .optional()?
        .ok_or(StoreError::SavedNotFound)
    }

    /// Deletes a saved search (no error if it is already gone).
    pub fn saved_delete(&self, id: i64) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM saved WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- Favorites ----

    /// Every favorite, newest first, with `exists` checked now.
    pub fn favorite_list(&self) -> Result<Vec<Favorite>, StoreError> {
        let rows: Vec<(String, bool, i64)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT path, is_dir, added_at FROM favorites ORDER BY added_at DESC, rowid DESC",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        // File system checks happen without the lock held.
        Ok(rows
            .into_iter()
            .map(|(path, is_dir, added_at)| favorite(path, is_dir, added_at))
            .collect())
    }

    /// Adds `path` to the favorites. Adding a path already there (same [`path_key`]) updates
    /// it and moves it to the top.
    pub fn favorite_add(&self, path: &Path, is_dir: bool) -> Result<Favorite, StoreError> {
        let text = path.to_string_lossy().into_owned();
        let added_at = now_ms();
        self.conn().execute(
            "INSERT INTO favorites (key, path, is_dir, added_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (key) DO UPDATE SET
                path = excluded.path, is_dir = excluded.is_dir, added_at = excluded.added_at",
            params![path_key(&text), text, is_dir, added_at],
        )?;
        Ok(favorite(text, is_dir, added_at))
    }

    /// Removes a favorite by path (compared by [`path_key`]; no error if it is not there).
    pub fn favorite_remove(&self, path: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM favorites WHERE key = ?1", [path_key(path)])?;
        Ok(())
    }

    /// The stored path of the favorite matching `path` (by [`path_key`]), or `None` if `path`
    /// is not a favorite. Callers open / reveal the returned path, not the one they were given.
    pub fn favorite_path(&self, path: &str) -> Result<Option<PathBuf>, StoreError> {
        let stored: Option<String> = self
            .conn()
            .query_row(
                "SELECT path FROM favorites WHERE key = ?1",
                [path_key(path)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.map(PathBuf::from))
    }
}

/// The identity form of a path on this platform: on Windows `\` -> `/` and lowercase (the
/// same rule as the content index's path keys); elsewhere unchanged.
pub fn path_key(path: &str) -> String {
    if cfg!(windows) {
        path.replace('\\', "/").to_lowercase()
    } else {
        path.to_owned()
    }
}

/// Opens the connection, sets pragmas, checks integrity and migrates.
fn open_connection(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    // journal_mode returns a row; query it instead of executing.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(check),
        )
        .into());
    }
    migrate(&conn)?;
    Ok(conn)
}

/// Brings the schema to [`SCHEMA_VERSION`].
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StoreError::NewerSchema(version));
    }
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE history (
                 id INTEGER PRIMARY KEY,
                 query TEXT NOT NULL,
                 scope TEXT NOT NULL DEFAULT '',
                 searched_at INTEGER NOT NULL,
                 count INTEGER NOT NULL DEFAULT 1,
                 UNIQUE (query, scope)
             );
             CREATE INDEX history_by_time ON history (searched_at);
             CREATE TABLE saved (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 query TEXT NOT NULL,
                 scope TEXT,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE favorites (
                 key TEXT PRIMARY KEY,
                 path TEXT NOT NULL,
                 is_dir INTEGER NOT NULL,
                 added_at INTEGER NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    Ok(())
}

/// True if `e` means the file is not a readable SQLite database.
fn is_corruption(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Renames the database (and its WAL / shared-memory files) to `.corrupt-<unix ms>`.
fn set_aside(path: &Path) -> Result<PathBuf, StoreError> {
    let suffix = format!(".corrupt-{}", now_ms());
    let with_suffix = |p: &Path, extra: &str| {
        let mut s = p.as_os_str().to_owned();
        s.push(extra);
        PathBuf::from(s)
    };
    let target = with_suffix(path, &suffix);
    std::fs::rename(path, &target)?;
    for side in ["-wal", "-shm"] {
        let from = with_suffix(path, side);
        if from.exists() {
            // Best effort: a stale WAL next to a fresh database would be replayed into it.
            if std::fs::rename(&from, with_suffix(&target, side)).is_err() {
                let _ = std::fs::remove_file(&from);
            }
        }
    }
    Ok(target)
}

fn check_name(name: &str) -> Result<&str, StoreError> {
    let name = name.trim();
    if name.is_empty() {
        Err(StoreError::EmptyName)
    } else if name.chars().count() > MAX_NAME_CHARS {
        Err(StoreError::NameTooLong)
    } else {
        Ok(name)
    }
}

fn saved_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SavedSearch> {
    Ok(SavedSearch {
        id: r.get(0)?,
        name: r.get(1)?,
        query: r.get(2)?,
        scope: r.get(3)?,
        created_at: r.get(4)?,
    })
}

fn favorite(path: String, is_dir: bool, added_at: i64) -> Favorite {
    let p = Path::new(&path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    let exists = p.exists();
    Favorite {
        path,
        name,
        is_dir,
        added_at,
        exists,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("sub").join("library.sqlite3")).unwrap();
        (dir, store)
    }

    #[test]
    fn history_dedupes_bumps_and_orders_newest_first() {
        let (_dir, s) = store();
        s.history_add_at("report", None, 10).unwrap();
        s.history_add_at("report", Some("C:\\"), 20).unwrap();
        s.history_add_at("budget", None, 30).unwrap();
        s.history_add_at("  report ", None, 40).unwrap();
        s.history_add_at("   ", None, 50).unwrap();
        s.history_add_at("", Some("D:\\"), 50).unwrap();

        let list = s.history_list(50).unwrap();
        let got: Vec<_> = list
            .iter()
            .map(|e| (e.query.as_str(), e.scope.as_deref(), e.searched_at, e.count))
            .collect();
        assert_eq!(
            got,
            [
                ("report", None, 40, 2),
                ("budget", None, 30, 1),
                ("report", Some("C:\\"), 20, 1),
            ]
        );
        assert_eq!(s.history_list(1).unwrap().len(), 1);

        s.history_remove(list[1].id).unwrap();
        assert_eq!(s.history_list(50).unwrap().len(), 2);
        s.history_clear().unwrap();
        assert!(s.history_list(50).unwrap().is_empty());
    }

    #[test]
    fn history_keeps_the_newest_rows_up_to_the_cap() {
        let (_dir, s) = store();
        for i in 0..(HISTORY_CAP + 5) {
            s.history_add_at(&format!("q{i}"), None, i as i64).unwrap();
        }
        let list = s.history_list(usize::MAX).unwrap();
        assert_eq!(list.len(), HISTORY_CAP);
        assert_eq!(list[0].query, format!("q{}", HISTORY_CAP + 4));
        assert_eq!(list.last().unwrap().query, "q5");
    }

    #[test]
    fn saved_searches_validate_names_and_rename() {
        let (_dir, s) = store();
        assert!(matches!(
            s.saved_create("  ", "x", None),
            Err(StoreError::EmptyName)
        ));
        assert!(matches!(
            s.saved_create("n", " ", None),
            Err(StoreError::EmptyQuery)
        ));
        let long = "ب".repeat(MAX_NAME_CHARS + 1);
        assert!(matches!(
            s.saved_create(&long, "x", None),
            Err(StoreError::NameTooLong)
        ));
        assert!(s.saved_create(&long[..long.len() - 2], "x", None).is_ok());

        let a = s.saved_create(" تقارير ", "ext:pdf", Some("D:\\")).unwrap();
        assert_eq!(a.name, "تقارير");
        assert_eq!(a.scope.as_deref(), Some("D:\\"));
        let b = s.saved_create("b", "x", Some("")).unwrap();
        assert_eq!(b.scope, None);

        let list = s.saved_list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, b.id, "newest first");

        let renamed = s.saved_rename(a.id, "  new ").unwrap();
        assert_eq!(renamed.name, "new");
        assert_eq!(renamed.query, "ext:pdf");
        assert!(matches!(
            s.saved_rename(9999, "x"),
            Err(StoreError::SavedNotFound)
        ));
        assert!(matches!(
            s.saved_rename(a.id, ""),
            Err(StoreError::EmptyName)
        ));

        s.saved_delete(a.id).unwrap();
        assert_eq!(s.saved_list().unwrap().len(), 2);
    }

    #[test]
    fn favorites_are_keyed_by_path_and_report_existence() {
        let (dir, s) = store();
        let file = dir.path().join("Notes.txt");
        std::fs::write(&file, "x").unwrap();
        let gone = dir.path().join("gone");

        let fav = s.favorite_add(&file, false).unwrap();
        assert_eq!(fav.name, "Notes.txt");
        assert!(fav.exists && !fav.is_dir);
        s.favorite_add(&gone, true).unwrap();

        let list = s.favorite_list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "gone");
        assert!(!list[0].exists && list[0].is_dir);

        let text = file.to_string_lossy().into_owned();
        assert_eq!(s.favorite_path(&text).unwrap(), Some(file.clone()));
        assert_eq!(s.favorite_path("C:\\not\\a\\favorite").unwrap(), None);
        if cfg!(windows) {
            let other_case = text.to_uppercase().replace('\\', "/");
            assert_eq!(s.favorite_path(&other_case).unwrap(), Some(file.clone()));
            // Re-adding under another spelling updates the same row.
            s.favorite_add(Path::new(&other_case), false).unwrap();
            assert_eq!(s.favorite_list().unwrap().len(), 2);
        }

        s.favorite_remove(&text).unwrap();
        assert_eq!(s.favorite_path(&text).unwrap(), None);
        assert_eq!(s.favorite_list().unwrap().len(), 1);
    }

    #[test]
    fn data_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        {
            let s = Store::open(&path).unwrap();
            s.history_add("q", None).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert!(s.recovered_from().is_none());
        assert_eq!(s.history_list(10).unwrap()[0].query, "q");
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        std::fs::write(
            &path,
            b"this is not a sqlite database, just junk bytes".repeat(200),
        )
        .unwrap();
        let s = Store::open(&path).unwrap();
        let moved = s
            .recovered_from()
            .expect("the junk file was set aside")
            .to_owned();
        assert!(moved.exists());
        assert!(moved.to_string_lossy().contains(".corrupt-"));
        s.history_add("q", None).unwrap();
        assert_eq!(s.history_list(10).unwrap().len(), 1);
    }

    #[test]
    fn a_newer_schema_is_refused_not_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.sqlite3");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99;")
            .unwrap();
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema(99))
        ));
        assert!(path.exists());
    }

    #[test]
    fn json_uses_camel_case() {
        let e = HistoryEntry {
            id: 1,
            query: "q".into(),
            scope: None,
            searched_at: 5,
            count: 2,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["searchedAt"], 5);
        assert!(v["scope"].is_null());
        let f = favorite("C:\\x\\y.txt".into(), false, 1);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["isDir"], false);
        assert_eq!(v["addedAt"], 1);
        assert!(v.get("exists").is_some());
    }
}
