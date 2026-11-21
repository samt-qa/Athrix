//! [`WorkerExtractor`]: PDFs through the isolated `athrix-extract-worker` process, every
//! other format in-process.

use std::path::Path;
use std::sync::Arc;

use athrix_extract::{ExtractError, Kind, Limits, WorkerPool};

use crate::extractor::{ExtractFailure, Extractor};
use crate::progress::FailureKind;

/// Extracts through a shared [`WorkerPool`] with
/// [`WorkerPool::extract_isolated`]: PDFs (or every format, with
/// [`WorkerConfig::isolate_all`](athrix_extract::WorkerConfig::isolate_all)) run in a worker
/// process, where a crash, stack overflow, runaway allocation or endless loop of the parser
/// costs one worker instead of the application; everything else runs in the calling thread
/// like [`InProcessExtractor`](crate::InProcessExtractor).
///
/// Share one pool between this extractor and other users (the preview pane) and set its
/// [`max_workers`](athrix_extract::WorkerConfig::max_workers) to at least
/// [`SchedulerConfig::workers`](crate::SchedulerConfig::workers): each extraction checks a
/// worker out exclusively.
///
/// # Failures
///
/// Worker outcomes map to [`FailureKind`]s by [`worker_failure`]:
///
/// | [`ExtractError`] | [`FailureKind`] | scheduler behaviour |
/// |---|---|---|
/// | `Timeout` | `Other` | remembered: not retried until the file changes |
/// | `WorkerCrashed` | `Other` | remembered: not retried until the file changes |
/// | `WorkerUnavailable` | `Io` | not remembered: retried on the next scan, counts toward back-off |
/// | anything else | as [`ExtractFailure::from`] | |
///
/// A timeout or crash is the document's doing (the worker is replaced for the next file), so
/// the same version is not tried again. A missing or failing-to-start worker says nothing
/// about the document, so it must not mark the file as failed.
#[derive(Debug, Clone)]
pub struct WorkerExtractor {
    pool: Arc<WorkerPool>,
    /// Bounds on one document (sent to the worker with each request).
    pub limits: Limits,
}

impl WorkerExtractor {
    /// An extractor over `pool` with [`Limits::default`].
    pub fn new(pool: Arc<WorkerPool>) -> Self {
        Self::with_limits(pool, Limits::default())
    }

    /// An extractor over `pool` bounded by `limits`.
    pub fn with_limits(pool: Arc<WorkerPool>, limits: Limits) -> Self {
        WorkerExtractor { pool, limits }
    }

    /// The shared pool.
    pub fn pool(&self) -> &Arc<WorkerPool> {
        &self.pool
    }
}

impl Extractor for WorkerExtractor {
    /// PDF (handled by the worker, whether or not this process was built with the `pdf`
    /// feature) and every format the in-process extractor supports.
    fn supports(&self, kind: Kind) -> bool {
        kind == Kind::Pdf || kind.is_supported()
    }

    fn extract(&self, path: &Path, _kind: Kind) -> Result<String, ExtractFailure> {
        self.pool
            .extract_isolated(path, &self.limits)
            .map(|x| x.text)
            .map_err(worker_failure)
    }
}

/// The [`ExtractFailure`] for an error of [`WorkerPool`]: see [`WorkerExtractor`] for the
/// mapping of the worker-specific errors; other errors map as [`ExtractFailure::from`].
pub fn worker_failure(e: ExtractError) -> ExtractFailure {
    match e {
        ExtractError::WorkerUnavailable(_) => ExtractFailure::new(FailureKind::Io, e.to_string()),
        ExtractError::Timeout(_) | ExtractError::WorkerCrashed(_) => {
            ExtractFailure::new(FailureKind::Other, e.to_string())
        }
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use athrix_extract::WorkerConfig;

    use super::*;

    /// A pool whose worker can never start: stands in for "no worker shipped".
    fn missing_worker_pool() -> Arc<WorkerPool> {
        let mut config = WorkerConfig::default();
        config.program = Some(PathBuf::from("no-such-dir/athrix-extract-worker-missing"));
        config.backoff_initial = Duration::from_secs(60);
        Arc::new(WorkerPool::new(config))
    }

    #[test]
    fn maps_worker_errors_to_failure_kinds() {
        let kind = |e| worker_failure(e).kind;
        assert_eq!(
            kind(ExtractError::Timeout(Duration::from_secs(1))),
            FailureKind::Other
        );
        assert_eq!(
            kind(ExtractError::WorkerCrashed("stack overflow".into())),
            FailureKind::Other
        );
        assert_eq!(
            kind(ExtractError::WorkerUnavailable("missing".into())),
            FailureKind::Io
        );
        assert_eq!(kind(ExtractError::Encrypted), FailureKind::Encrypted);
        assert_eq!(
            kind(ExtractError::Corrupt("x".into())),
            FailureKind::Corrupt
        );
        assert_eq!(
            kind(ExtractError::Unsupported("x".into())),
            FailureKind::Unsupported
        );
        let f = worker_failure(ExtractError::WorkerCrashed("exit code 3".into()));
        assert!(f.message.contains("exit code 3"), "{f}");
    }

    #[test]
    fn supports_pdf_and_in_process_formats() {
        let x = WorkerExtractor::new(missing_worker_pool());
        assert!(x.supports(Kind::Pdf));
        assert!(x.supports(Kind::Text));
        assert!(x.supports(Kind::Docx));
    }

    #[test]
    fn missing_worker_is_retryable_and_other_formats_still_work() {
        let dir = tempfile::tempdir().unwrap();
        let x = WorkerExtractor::new(missing_worker_pool());

        let pdf = dir.path().join("report.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n%%EOF\n").unwrap();
        let first = x.extract(&pdf, Kind::Pdf).unwrap_err();
        assert_eq!(first.kind, FailureKind::Io, "{first}");
        // During the restart back-off: fails fast, still retryable.
        let second = x.extract(&pdf, Kind::Pdf).unwrap_err();
        assert_eq!(second.kind, FailureKind::Io, "{second}");

        // Not a PDF: in-process, no worker needed.
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "مرحبا").unwrap();
        assert_eq!(x.extract(&txt, Kind::Text).unwrap(), "مرحبا");
        assert_eq!(x.pool().stats().startup_failures, 1);
    }
}
