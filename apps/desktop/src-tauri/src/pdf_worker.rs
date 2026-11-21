//! The isolated PDF extraction worker (`athrix-extract-worker`), shared by the preview pane
//! and the content-indexing scheduler.
//!
//! PDFs are never parsed in the app process: `athrix-extract` is built here without its `pdf`
//! feature, and PDFs go through one process-wide [`WorkerPool`] (crash, stack overflow, memory
//! cap and timeout cost a worker process, not the app). Other formats stay in-process.
//!
//! # Where the worker comes from
//!
//! `apps/desktop/scripts/build-worker.mjs` (`npm run worker`, also run by `npm run tauri
//! dev|build` and the `before*Command` hooks) builds it with `--features pdf` in its own cargo
//! invocation and stages it as `src-tauri/binaries/athrix-extract-worker-<triple>[.exe]`,
//! listed under `bundle.externalBin`. tauri-build copies it next to the app executable on
//! every build (`target/<profile>/`, so dev and tests find it), and the installer puts it next
//! to `Athrix.exe`. The pool uses, in order:
//!
//! 1. the path in the [`WORKER_ENV`] environment variable, if set;
//! 2. `athrix-extract-worker[.exe]` beside the current executable
//!    ([`athrix_extract::worker::default_program`]).
//!
//! A missing worker is not fatal: PDF previews say so (`unsupported/pdf`) and the content
//! scheduler retries PDFs on its next scan.
//!
//! # Why process-wide
//!
//! The preview builder runs on the blocking pool without an `AppHandle`, so the pool lives in
//! a `OnceLock` rather than in Tauri's managed state; [`worker_pool`] takes the handle only to
//! fit the other state accessors. Workers start on first use; idle ones exit when the app does
//! (they stop at end of input), or earlier with [`shutdown_idle`].

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use athrix_daemon::{Extractor, SchedulerConfig, WorkerExtractor};
use athrix_extract::{WorkerConfig, WorkerPool};
use tauri::{AppHandle, Runtime};

/// Environment variable naming the worker executable (overrides the one beside the app).
pub const WORKER_ENV: &str = "ATHRIX_EXTRACT_WORKER";

/// Longest time one document may take in the worker.
pub const WORKER_TIMEOUT: Duration = Duration::from_secs(20);

static POOL: OnceLock<Arc<WorkerPool>> = OnceLock::new();

/// The process-wide worker pool, created on first use.
pub fn shared_pool() -> Arc<WorkerPool> {
    Arc::clone(POOL.get_or_init(|| Arc::new(WorkerPool::new(config()))))
}

/// The worker pool of the app (the same as [`shared_pool`]).
pub fn worker_pool<R: Runtime>(_app: &AppHandle<R>) -> Arc<WorkerPool> {
    shared_pool()
}

/// The extractor for the content scheduler
/// ([`SchedulerBuilder::extractor`](athrix_daemon::SchedulerBuilder::extractor)): PDFs through
/// the shared worker pool, other formats in-process. A missing worker reports PDFs as `io`
/// failures, which the scheduler retries on the next scan instead of remembering.
pub fn content_extractor() -> Arc<dyn Extractor> {
    Arc::new(WorkerExtractor::new(shared_pool()))
}

/// Stops the idle workers (for example on exit). New requests start new ones.
pub fn shutdown_idle() {
    if let Some(pool) = POOL.get() {
        pool.shutdown_idle();
    }
}

fn config() -> WorkerConfig {
    let mut config = WorkerConfig::default();
    config.program = std::env::var_os(WORKER_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    config.timeout = WORKER_TIMEOUT;
    // One per scheduler extraction thread, plus one for the preview pane.
    config.max_workers = SchedulerConfig::default().workers + 1;
    let program = match &config.program {
        Some(p) => Ok(p.clone()),
        None => athrix_extract::worker::default_program(),
    };
    match program {
        Ok(p) if p.is_file() => {
            tracing::info!(worker = %p.display(), "PDF extraction worker found");
        }
        Ok(p) => tracing::warn!(
            worker = %p.display(),
            "PDF extraction worker missing: PDFs will not be previewed or indexed \
             (run `npm run worker` in apps/desktop, or set {WORKER_ENV})"
        ),
        Err(e) => tracing::warn!(error = %e, "cannot locate the PDF extraction worker"),
    }
    config
}
