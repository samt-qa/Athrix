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
}
