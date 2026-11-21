//! The error type shared by all Athrix crates.

use std::path::PathBuf;

/// Result alias using [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors shared across Athrix crates. Crate-specific errors convert into this type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O operation on a path failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The user's query could not be parsed.
    #[error("invalid query at position {position}: {message}")]
    InvalidQuery {
        /// Byte offset in the query where the problem starts.
        position: usize,
        /// What went wrong, in plain words.
        message: String,
    },
    /// A setting has an invalid value.
    #[error("invalid configuration `{key}`: {message}")]
    Config {
        /// The setting name.
        key: String,
        /// What went wrong.
        message: String,
    },
    /// The operation was cancelled, for example a search replaced by a newer keystroke.
    #[error("operation cancelled")]
    Cancelled,
}

impl Error {
    /// Builds an [`Error::Io`] for `path`.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_names_the_path() {
        let e = Error::io("D:/docs/a.pdf", std::io::Error::other("denied"));
        assert_eq!(e.to_string(), "I/O error on D:/docs/a.pdf: denied");
    }

    #[test]
    fn query_error_names_the_position() {
        let e = Error::InvalidQuery {
            position: 4,
            message: "unknown filter `szie:`".into(),
        };
        assert_eq!(
            e.to_string(),
            "invalid query at position 4: unknown filter `szie:`"
        );
    }
}
