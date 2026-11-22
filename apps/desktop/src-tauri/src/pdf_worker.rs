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
//! 2. debug builds only: the staged sidecar itself ([`staged_worker`]), when the copy beside
//!    the executable is missing or was built **without** PDF support (see below);
//! 3. `athrix-extract-worker[.exe]` beside the current executable
//!    ([`athrix_extract::worker::default_program`]).
//!
//! # The shadowed worker in `target/debug`
//!
//! `cargo build --workspace` (or `cargo test --workspace`) also builds the worker crate's
//! binary in the shared target dir, without the `pdf` feature (nothing in the workspace turns
//! it on, on purpose), and that file lands exactly where tauri-build had copied the staged
//! one. A dev app started afterwards would then find a worker that answers "PDF extraction is
//! not compiled in". In debug builds the app therefore asks the worker beside it for its
//! capabilities (`--version` prints `pdf: true|false`) and, if it has no PDF support, uses the
//! staged `src-tauri/binaries/athrix-extract-worker-<triple>[.exe]` instead: that file is only
//! ever written by `build-worker.mjs`, always with `--features pdf`. Release builds (the
//! installed app, whose sidecar sits beside `Athrix.exe`) skip this check entirely. The other
//! options were worse: making `pdf` a default feature would turn PDF parsing on inside the
//! app process through feature unification, and `required-features = ["pdf"]` on the binary
//! would drop the worker crate's own tests of the PDF-less worker.
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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use athrix_daemon::{Extractor, SchedulerConfig, WorkerExtractor};
use athrix_extract::worker::WORKER_BIN_NAME;
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

/// The sidecar staged by `apps/desktop/scripts/build-worker.mjs`:
/// `src-tauri/binaries/athrix-extract-worker-<triple>[.exe]` of this source tree, if present.
/// Always built with PDF support. Only meaningful on the machine that built the app (the
/// path is fixed at compile time), so the pool only uses it in debug builds.
pub fn staged_worker() -> Option<PathBuf> {
    // Set for this crate by tauri-build (`cargo:rustc-env`).
    let triple = option_env!("TAURI_ENV_TARGET_TRIPLE")?;
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(format!(
            "{WORKER_BIN_NAME}-{triple}{}",
            std::env::consts::EXE_SUFFIX
        ));
    path.is_file().then_some(path)
}

/// Whether the worker at `program` was built with PDF support: its `--version` line ends with
/// `pdf: true)`. `false` when it is missing or does not answer.
fn reports_pdf(program: &Path) -> bool {
    let mut cmd = Command::new(program);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// No console window for a console program started from a GUI app.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
        .is_ok_and(|out| out.status.success() && version_has_pdf(&out.stdout))
}

/// Reads the worker's `--version` output (`athrix-extract-worker 0.0.1 (protocol 1, pdf: true)`).
fn version_has_pdf(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).contains("pdf: true")
}

/// Debug builds: the staged worker when the one beside the executable is missing or has no
/// PDF support (see the [module docs](self#the-shadowed-worker-in-targetdebug)).
fn dev_worker_override() -> Option<PathBuf> {
    let beside = athrix_extract::worker::default_program().ok();
    if beside.as_deref().is_some_and(reports_pdf) {
        return None;
    }
    let staged = staged_worker()?;
    tracing::info!(
        worker = %staged.display(),
        shadowed = ?beside,
        "the PDF worker beside the app is missing or built without PDF support          (`cargo build --workspace`?); using the staged one"
    );
    Some(staged)
}

fn config() -> WorkerConfig {
    let mut config = WorkerConfig::default();
    config.program = std::env::var_os(WORKER_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    if cfg!(debug_assertions) && config.program.is_none() {
        config.program = dev_worker_override();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_pdf_capability_from_the_version_line() {
        assert!(version_has_pdf(
            b"athrix-extract-worker 0.0.1 (protocol 1, pdf: true)
"
        ));
        assert!(!version_has_pdf(
            b"athrix-extract-worker 0.0.1 (protocol 1, pdf: false)
"
        ));
        assert!(!version_has_pdf(b""));
    }

    #[test]
    fn a_missing_worker_has_no_pdf() {
        assert!(!reports_pdf(Path::new(
            "no-such-dir/athrix-extract-worker-missing"
        )));
    }

    /// The staged worker (`npm run worker`) answers `--version` with PDF support.
    #[test]
    fn the_staged_worker_has_pdf() {
        let staged =
            staged_worker().expect("no staged worker: run `npm run worker` in apps/desktop");
        assert!(reports_pdf(&staged), "{}", staged.display());
    }
}
