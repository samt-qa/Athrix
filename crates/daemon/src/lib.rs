//! Background services of Athrix. For now: the **content-indexing scheduler** (task 4.5),
//! which keeps the full-text [`ContentIndex`](athrix_content_index::ContentIndex) in step
//! with the files the name index knows about.
//!
//! # Design
//!
//! ```text
//!  IndexSnapshot ──enqueue_snapshot──┐                       ┌─► worker 1 ─┐
//!  WatchBatch ─────watch_batch───────┼─► priority queue ─────┼─► worker 2 ─┼─► writer ─► ContentIndex
//!  Change (USN) ───apply_changes─────┘   (dedupe by path)    └─► worker n ─┘   (single, commits)
//!                                             ▲
//!                       control thread: SystemProbe (battery, CPU) → auto-pause; progress
//! ```
//!
//! - **Candidates.** Only files whose format the [`Extractor`] supports, at most
//!   [`CandidateFilter::max_size`], outside system and tool folders (`Windows`,
//!   `Program Files`, `AppData`, `node_modules`, dot-folders...) and not temporary or lock
//!   files (`~$x.docx`, `.~lock.x#`, `*.tmp`, partial downloads). See [`CandidateFilter`].
//! - **Queue.** A binary heap keyed by: watcher changes before the initial backlog, then files
//!   in the priority folders (Desktop, Documents, Downloads) first, then recently modified
//!   first (day / week / month / year buckets), then small before large, then newest. Each
//!   path is queued once; queuing it again keeps the newest metadata and the more urgent
//!   priority. A path being extracted is never handed to a second worker, so versions of one
//!   file reach the writer in order.
//! - **Backlog.** The files of a snapshot ([`ContentScheduler::enqueue_snapshot`]) do not go
//!   into that heap: a million of them would take half a gigabyte. They wait in a compact
//!   list in the same order of urgency (about 60 bytes each), served whenever the queue has
//!   nothing to give. A backlog file's size and time are read from disk when its turn comes.
//! - **Workers.** `max(1, cores / 4)` threads by default
//!   ([`SchedulerConfig::workers`]), at background priority. For each file a worker skips it
//!   if the same version failed before or if [`ContentIndex::is_current`](athrix_content_index::ContentIndex::is_current) says the index
//!   already holds it, and otherwise extracts it through the [`Extractor`] trait. The default
//!   [`InProcessExtractor`] calls [`athrix_extract::extract`]; [`WorkerExtractor`] sends PDFs
//!   to the isolated `athrix-extract-worker` process through a shared
//!   [`WorkerPool`](athrix_extract::WorkerPool) (plug it in with
//!   [`SchedulerBuilder::extractor`]).
//! - **Writer.** One thread owns all index writes (`add_or_update`, `delete`) and commits
//!   every [`SchedulerConfig::commit_every`] changes or [`SchedulerConfig::commit_interval`],
//!   and also when the queue runs dry, when paused, and on shutdown. A bounded channel between
//!   workers and writer applies back-pressure.
//! - **Changes.** A watcher path is re-read from disk: an existing file is re-extracted, a
//!   missing path is deleted together with every file this session queued below it (renames
//!   are a delete plus an add), and a folder with no known files below it (renamed or copied
//!   in) is listed and its entries queued.
//! - **Politeness.** [`ContentScheduler::pause`] / [`ContentScheduler::resume`], plus
//!   auto-pause from a [`SystemProbe`] sampled every [`Politeness::probe_interval`]: on
//!   battery, or when *other* programs use more than [`Politeness::busy_above`] of the CPU for
//!   [`Politeness::busy_samples`] samples in a row (it resumes under
//!   [`Politeness::resume_below`]). After [`Politeness::backoff_after`] I/O or index errors
//!   in a row, workers sleep between jobs (100 ms doubling up to
//!   [`Politeness::max_backoff`]).
//! - **Failures.** A file that fails never stops the run. It is counted by [`FailureKind`]
//!   and remembered (path, size, mtime, reason; at most
//!   [`SchedulerConfig::max_remembered_failures`]) so the same version is not retried; a new
//!   mtime or size retries it. I/O failures (locked or vanished files) are not remembered,
//!   so they are retried on the next scan. The memory lives for the process only: after a
//!   restart, failed files are tried once more, which costs one failed extraction each.
//! - **Shutdown.** [`ContentScheduler::shutdown`] (or drop) lets each worker finish its file,
//!   commits everything extracted, and joins all threads. Queued work is dropped; the next
//!   startup scan finds it again and skips what is already current.
//!
//! # Default probe and its dependencies
//!
//! [`DefaultProbe`] reads CPU load with `sysinfo` 0.36 (the version the desktop app already
//! builds) and AC/battery state with `starship-battery` 0.10 (feature `battery`, on by
//! default), both of which keep `unsafe` out of this crate. Thread priority is lowered with
//! `thread-priority` (on Windows `THREAD_MODE_BACKGROUND_BEGIN`, which also lowers I/O
//! priority).
//!
//! # Intended app API (wiring is the next step)
//!
//! ```no_run
//! use std::sync::Arc;
//! use athrix_content_index::ContentIndex;
//! use athrix_daemon::{ContentScheduler, SchedulerConfig};
//! # fn demo(live: &athrix_fs_index::LiveIndex) -> Result<(), Box<dyn std::error::Error>> {
//! let index = Arc::new(ContentIndex::open_or_create("C:/ProgramData/Athrix/content")?);
//! let scheduler = ContentScheduler::builder(Arc::clone(&index))
//!     .config(SchedulerConfig::default())
//!     .on_progress(|p| {
//!         // e.g. emit a Tauri event: "Indexing contents: 12,340 / 80,000"
//!         println!("{} / {} ({:.0} docs/s)", p.processed(), p.total(), p.docs_per_sec);
//!     })
//!     .start()?;
//!
//! // 1. Initial backlog from the name index (also after a rescan, or when
//! //    `index.rebuilt()` is Some: everything is then re-extracted).
//! scheduler.enqueue_snapshot(&live.snapshot());
//!
//! // 2. In the name-index watcher callback, after `live.apply_paths(...)`:
//! //    if !scheduler.watch_batch(&batch) { /* re-walk, then enqueue_snapshot again */ }
//!
//! // 3. UI: scheduler.pause() / scheduler.resume() / scheduler.progress()
//! //    / scheduler.failures(); searches go straight to `index.search(...)`.
//!
//! // 4. On exit:
//! let _final = scheduler.shutdown();
//! # Ok(()) }
//! ```
//!
//! The scheduler is `Send + Sync`; the app keeps it in its state next to the `LiveIndex`
//! and shares the same `Arc<ContentIndex>` with the search command. Paths must be in the same
//! form everywhere (the name index's), since the content index keys documents by path
//! string.

mod backlog;
mod candidate;
mod extractor;
mod probe;
mod progress;
mod queue;
mod scheduler;
mod worker_extractor;

pub use candidate::{Candidate, CandidateFilter, default_priority_folders, path_key};
pub use extractor::{ExtractFailure, Extractor, InProcessExtractor};
pub use probe::{DefaultProbe, NullProbe, SystemProbe, SystemState};
pub use progress::{FailureCounts, FailureKind, PauseReasons, Progress, RunState};
pub use scheduler::{
    ContentScheduler, FailedFile, Politeness, ProgressCallback, SchedulerBuilder, SchedulerConfig,
};
pub use worker_extractor::{WorkerExtractor, worker_failure};
