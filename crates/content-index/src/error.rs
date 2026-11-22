//! Errors of the content index.

use std::path::{Path, PathBuf};
use tantivy::TantivyError;

/// Result alias using [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors of the content index.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The index files are damaged or unreadable by this version. Rebuild it with
    /// [`ContentIndex::recreate`](crate::ContentIndex::recreate) and re-index the files.
    /// [`ContentIndex::open_or_create`](crate::ContentIndex::open_or_create) does that itself.
    #[error("content index at {path} is corrupt: {message}")]
    Corrupt {
        /// The index directory.
        path: PathBuf,
        /// What tantivy reported.
        message: String,
    },
    /// Another writer (usually another Athrix process) holds the index lock.
    #[error("content index at {0} is locked by another writer")]
    Locked(PathBuf),
    /// The directory has files but is not an Athrix content index, so it is never wiped.
    #[error("{0} is not empty and is not an Athrix content index")]
    NotAnIndex(PathBuf),
    /// An I/O operation on a path failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A write on an index opened with
    /// [`ContentIndex::open_read_only`](crate::ContentIndex::open_read_only).
    #[error("content index at {0} is open read-only")]
    ReadOnly(PathBuf),
    /// Any other tantivy error.
    #[error("content index error: {0}")]
    Index(#[source] TantivyError),
}

impl Error {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Error::Io {
            path: path.to_owned(),
            source,
        }
    }

    /// Classifies a tantivy error: damaged or incompatible data becomes [`Error::Corrupt`],
    /// a busy lock [`Error::Locked`].
    pub(crate) fn from_tantivy(dir: &Path, e: TantivyError) -> Self {
        match e {
            TantivyError::LockFailure(..) => Error::Locked(dir.to_owned()),
            TantivyError::DataCorruption(_)
            | TantivyError::IncompatibleIndex(_)
            | TantivyError::OpenReadError(_)
            | TantivyError::DeserializeError(_)
            | TantivyError::SchemaError(_)
            | TantivyError::FieldNotFound(_) => Error::Corrupt {
                path: dir.to_owned(),
                message: e.to_string(),
            },
            other => Error::Index(other),
        }
    }

    /// True for [`Error::Corrupt`].
    pub fn is_corrupt(&self) -> bool {
        matches!(self, Error::Corrupt { .. })
    }
}
