//! Where the name-index snapshot lives on disk and which roots it covers (task 1.4).
//!
//! The store is a folder (in the app, `<app_data>/index`) holding two files:
//! - `names.snap`: the live entries of an [`IndexSnapshot`], written by
//!   [`IndexSnapshot::save`] in the [`FileIndex::save`] format (atomic, versioned, CRC-checked);
//! - `names.roots.json`: a sidecar `{"version":1,"roots":[...]}` listing the walked roots.
//!
//! A snapshot is only used when the sidecar's roots equal the requested roots exactly (same
//! order, same spelling). Saving deletes the sidecar first and rewrites it last, so a crash
//! in between leaves a snapshot without a sidecar, which is ignored. Nothing here returns an
//! error to the caller: every problem is logged and the caller just walks.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use athrix_fs_index::{FileIndex, IndexSnapshot};
use serde::{Deserialize, Serialize};

/// File name of the index snapshot inside the store folder.
pub const SNAPSHOT_FILE: &str = "names.snap";
/// File name of the roots sidecar inside the store folder.
pub const ROOTS_FILE: &str = "names.roots.json";

/// Version of the sidecar layout.
const SIDECAR_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    roots: Vec<String>,
}

/// A folder holding one index snapshot and the roots it covers.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    /// A store in `dir` (created on the first save).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Path of the snapshot file.
    pub fn snapshot_path(&self) -> PathBuf {
        self.dir.join(SNAPSHOT_FILE)
    }

    /// Path of the roots sidecar.
    pub fn roots_path(&self) -> PathBuf {
        self.dir.join(ROOTS_FILE)
    }

    /// Loads the snapshot if it exists, covers exactly `roots` and is valid. Missing, foreign,
    /// corrupt, old-version or unreadable snapshots give `None` (logged).
    pub fn load(&self, roots: &[PathBuf]) -> Option<FileIndex> {
        let sidecar = match fs::read(self.roots_path()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                tracing::info!(dir = %self.dir.display(), "no index snapshot");
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the index snapshot roots; ignoring it");
                return None;
            }
        };
        match serde_json::from_slice::<Sidecar>(&sidecar) {
            Ok(s) if s.version == SIDECAR_VERSION && s.roots == root_strings(roots) => {}
            Ok(s) if s.version != SIDECAR_VERSION => {
                tracing::info!(
                    version = s.version,
                    "index snapshot roots file has another version; ignoring the snapshot"
                );
                return None;
            }
            Ok(_) => {
                tracing::info!("index snapshot covers other roots; ignoring it");
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "index snapshot roots file is corrupt; ignoring it");
                return None;
            }
        }
        match FileIndex::load(&self.snapshot_path()) {
            Ok(index) => Some(index),
            Err(e) => {
                tracing::warn!(error = %e, "cannot load the index snapshot; ignoring it");
                None
            }
        }
    }

    /// Saves the live entries of `snapshot` (base and changes since) as the snapshot of
    /// `roots`. Errors are returned for logging only.
    pub fn save(&self, snapshot: &IndexSnapshot, roots: &[PathBuf]) -> Result<(), String> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let roots_path = self.roots_path();
        match fs::remove_file(&roots_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", roots_path.display())),
        }
        snapshot
            .save(&self.snapshot_path())
            .map_err(|e| e.to_string())?;
        let sidecar = Sidecar {
            version: SIDECAR_VERSION,
            roots: root_strings(roots),
        };
        let json = serde_json::to_vec(&sidecar).map_err(|e| e.to_string())?;
        write_atomic(&roots_path, &json)
    }
}

fn root_strings(roots: &[PathBuf]) -> Vec<String> {
    roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = fs::write(&tmp, bytes)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(|e| format!("cannot write {}: {e}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::IndexView;
    use athrix_fs_index::{EntryMeta, IndexBuilder};

    fn sample() -> IndexView {
        IndexView::from_index(0, sample_index())
    }

    fn sample_index() -> FileIndex {
        let mut b = IndexBuilder::new();
        let root = b.push(None, "/data", EntryMeta::default());
        b.push(Some(root), "ملف.txt", EntryMeta::default());
        b.finish()
    }

    #[test]
    fn round_trips_for_the_same_roots_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().join("index"));
        let roots = vec![PathBuf::from("/data")];
        assert!(store.load(&roots).is_none(), "nothing saved yet");
        store.save(&sample(), &roots).unwrap();
        assert_eq!(store.load(&roots).unwrap().len(), 2);
        assert!(store.load(&[PathBuf::from("/other")]).is_none());
        assert!(store.load(&[]).is_none());
    }

    #[test]
    fn corrupt_or_unpaired_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let roots = vec![PathBuf::from("/data")];
        store.save(&sample(), &roots).unwrap();
        fs::write(store.snapshot_path(), b"garbage").unwrap();
        assert!(store.load(&roots).is_none());

        store.save(&sample(), &roots).unwrap();
        fs::write(store.roots_path(), b"{not json").unwrap();
        assert!(store.load(&roots).is_none());

        fs::remove_file(store.roots_path()).unwrap();
        assert!(
            store.load(&roots).is_none(),
            "a snapshot without sidecar is ignored"
        );
    }
}
