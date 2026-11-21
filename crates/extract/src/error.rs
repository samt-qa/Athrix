//! Typed extraction errors.

/// Why a document could not be extracted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExtractError {
    /// The format is unknown, not compiled in, or the file is binary.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A size limit from [`crate::Limits`] was exceeded (input size, zip
    /// entry count, decompressed size, MIME part count).
    #[error("too large: {0}")]
    TooLarge(String),
    /// The document is malformed.
    #[error("corrupt document: {0}")]
    Corrupt(String),
    /// The document is password-protected or DRM-encrypted.
    #[error("document is encrypted")]
    Encrypted,
    /// Reading the file failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Out-of-process extraction ([`crate::WorkerPool`]) did not finish in
    /// time. The worker was killed; the next request gets a fresh one.
    #[error("extraction timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// The extraction worker process died while handling this document
    /// (crash, stack overflow, memory cap) or broke the protocol. The
    /// message describes the exit status and the worker's last stderr line.
    /// The next request gets a fresh worker.
    #[error("extraction worker crashed: {0}")]
    WorkerCrashed(String),
    /// The extraction worker could not be started (missing executable,
    /// failed handshake), or is in its restart backoff after such a failure.
    #[error("extraction worker unavailable: {0}")]
    WorkerUnavailable(String),
}
