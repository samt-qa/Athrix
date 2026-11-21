//! The extraction step, behind a trait so it can run in-process or in a worker process.

use std::fmt;
use std::path::Path;

use athrix_extract::{ExtractError, Kind, Limits};

use crate::progress::FailureKind;

/// Why one file could not be extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractFailure {
    /// Coarse reason, used for counting and for the "do not retry" memory.
    pub kind: FailureKind,
    /// Human-readable detail for logs and a diagnostics page.
    pub message: String,
}

impl ExtractFailure {
    /// A failure of `kind` with `message`.
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        ExtractFailure {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ExtractFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ExtractFailure {}

impl From<ExtractError> for ExtractFailure {
    fn from(e: ExtractError) -> Self {
        let kind = match &e {
            ExtractError::Unsupported(_) => FailureKind::Unsupported,
            ExtractError::TooLarge(_) => FailureKind::TooLarge,
            ExtractError::Corrupt(_) => FailureKind::Corrupt,
            ExtractError::Encrypted => FailureKind::Encrypted,
            ExtractError::Io(_) => FailureKind::Io,
            _ => FailureKind::Other,
        };
        ExtractFailure::new(kind, e.to_string())
    }
}

/// Turns a file into plain text for the content index.
///
/// Called concurrently from every worker thread, so implementations must be `Send + Sync`.
/// The scheduler calls [`supports`](Self::supports) to decide which files are candidates at
/// all, and [`extract`](Self::extract) once per file version.
///
/// The default is [`InProcessExtractor`]. An out-of-process extractor (a worker process that
/// isolates crashes and hangs of risky parsers such as PDF) implements this trait too: it
/// can claim [`Kind::Pdf`] in `supports`, forward each call to its child process, enforce a
/// timeout and report a dead or killed worker as [`FailureKind::Other`] or
/// [`FailureKind::Corrupt`] (which the scheduler then does not retry until the file changes).
pub trait Extractor: Send + Sync {
    /// Whether files of `kind` can be extracted at all.
    fn supports(&self, kind: Kind) -> bool;

    /// Extracts the text of `path`, whose format the scheduler guessed as `kind`.
    fn extract(&self, path: &Path, kind: Kind) -> Result<String, ExtractFailure>;
}

/// Extracts in the calling thread with [`athrix_extract::extract`], bounded by [`Limits`].
///
/// Every parser in `athrix-extract` is bounded and runs under `catch_unwind`, so a hostile
/// file costs at most the limits. PDF is only supported when `athrix-extract` was built with
/// its `pdf` feature ([`Kind::is_supported`]).
#[derive(Debug, Clone, Default)]
pub struct InProcessExtractor {
    /// Bounds on one document.
    pub limits: Limits,
}

impl Extractor for InProcessExtractor {
    fn supports(&self, kind: Kind) -> bool {
        kind.is_supported()
    }

    fn extract(&self, path: &Path, _kind: Kind) -> Result<String, ExtractFailure> {
        Ok(athrix_extract::extract(path, &self.limits)?.text)
    }
}
