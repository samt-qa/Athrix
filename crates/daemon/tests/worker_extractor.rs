//! [`WorkerExtractor`] against the real `athrix-extract-worker` binary, built with PDF support.
//!
//! `CARGO_BIN_EXE_*` only exists for binaries of the crate under test, so the worker is found
//! through `ATHRIX_EXTRACT_WORKER` (a path), or else built here exactly like
//! `apps/desktop/scripts/build-worker.mjs` does: a separate cargo invocation with
//! `--features pdf` into `target/worker` (so the feature does not leak into this build). That
//! compiles `pdf-extract` on first use, so the tests are ignored by default:
//!
//! ```text
//! cargo test -p athrix-daemon --test worker_extractor -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use athrix_daemon::{Extractor, FailureKind, WorkerExtractor};
use athrix_extract::{Kind, WorkerConfig, WorkerPool};

#[path = "../../extract/src/tests/pdf_fixtures.rs"]
mod pdf_fixtures;

/// The worker executable: `ATHRIX_EXTRACT_WORKER`, or built with `--features pdf`.
fn worker() -> &'static Path {
    static WORKER: OnceLock<PathBuf> = OnceLock::new();
    WORKER.get_or_init(|| {
        if let Some(p) = std::env::var_os("ATHRIX_EXTRACT_WORKER") {
            return PathBuf::from(p);
        }
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = workspace.join("target").join("worker");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(&workspace)
            .args(["build", "-p", "athrix-extract", "--bin"])
            .arg(athrix_extract::worker::WORKER_BIN_NAME)
            .args(["--features", "pdf", "--target-dir"])
            .arg(&target)
            .status()
            .expect("run cargo to build the worker");
        assert!(status.success(), "building the worker failed");
        target.join("debug").join(format!(
            "{}{}",
            athrix_extract::worker::WORKER_BIN_NAME,
            std::env::consts::EXE_SUFFIX
        ))
    })
}

fn extractor() -> WorkerExtractor {
    let mut config = WorkerConfig::default();
    config.program = Some(worker().to_path_buf());
    config.timeout = Duration::from_secs(20);
    WorkerExtractor::new(Arc::new(WorkerPool::new(config)))
}

#[test]
#[ignore = "builds athrix-extract-worker with --features pdf; run with --ignored"]
fn pdf_goes_through_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("تقرير.pdf");
    let pdf = pdf_fixtures::arabic_pdf(pdf_fixtures::FORMS, &[3, 2, 1], "Hello 2024", None);
    std::fs::write(&path, pdf).unwrap();
    let x = extractor();
    assert_eq!(x.extract(&path, Kind::Pdf).unwrap(), "سلام\nHello 2024\nسلام");
    assert_eq!(x.pool().stats().spawned, 1);
    assert_eq!(x.pool().stats().answered, 1);

    // Other formats stay in-process.
    let txt = dir.path().join("a.txt");
    std::fs::write(&txt, "نص").unwrap();
    assert_eq!(x.extract(&txt, Kind::Text).unwrap(), "نص");
    assert_eq!(x.pool().stats().answered, 1);
}

#[test]
#[ignore = "builds athrix-extract-worker with --features pdf; run with --ignored"]
fn a_crashing_pdf_is_a_remembered_failure_and_the_next_file_works() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("loop.pdf");
    // Recurses without a depth limit in pdf-extract: overflows the worker's stack.
    std::fs::write(&bad, pdf_fixtures::self_drawing_pdf()).unwrap();
    let good = dir.path().join("ok.pdf");
    std::fs::write(
        &good,
        pdf_fixtures::arabic_pdf(pdf_fixtures::BASE, &[3, 2, 1], "x", None),
    )
    .unwrap();
    let x = extractor();
    let f = x.extract(&bad, Kind::Pdf).unwrap_err();
    assert_eq!(f.kind, FailureKind::Other, "{f}");
    assert!(f.message.contains("crashed"), "{f}");
    assert_eq!(x.extract(&good, Kind::Pdf).unwrap(), "سلام\nx\nسلام");
    assert_eq!(x.pool().stats().crashes, 1);
}
