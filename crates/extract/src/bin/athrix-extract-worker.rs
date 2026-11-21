//! `athrix-extract-worker`: runs document extraction out of process for
//! [`athrix_extract::WorkerPool`], so that a parser crash, stack overflow or
//! runaway allocation kills only this process.
//!
//! Build it with `--features pdf` to extract PDFs. The protocol and the
//! command line are described in the `athrix_extract::worker` module.

use std::alloc::System;
use std::process::ExitCode;

use cap::Cap;

/// Counts heap use, and enforces the `--memory-limit` cap: an allocation
/// past it fails, which aborts the process.
#[global_allocator]
static ALLOCATOR: Cap<System> = Cap::new(System, usize::MAX);

fn main() -> ExitCode {
    athrix_extract::worker::run_worker_process(athrix_extract::worker::ProcessHooks {
        allocated: || ALLOCATOR.allocated(),
        set_memory_limit: |limit| ALLOCATOR.set_limit(limit).is_ok(),
    })
}
