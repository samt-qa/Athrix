//! File-name index for Athrix: parallel directory walk, a compact in-memory index and fast,
//! Arabic-aware name search.
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use athrix_fs_index::{walk, Query, WalkOptions};
//!
//! let index = walk(&WalkOptions::new(["D:/Documents"]).exclude_glob("node_modules"))?;
//! let cancel = AtomicBool::new(false);
//! for id in index.search(&Query::parse("تقرير *.pdf")?, 100, &cancel)? {
//!     println!("{}", index.path(id).display());
//! }
//! # Ok::<(), athrix_core::Error>(())
//! ```
//!
//! To keep the index current after the walk, wrap it in a [`LiveIndex`] and feed it the
//! batches of a [`Watcher`] (see [`live`] for the design and the id rules, and [`watch`] for
//! the debouncing).
//!
//! Fuzzy terms (`fuzzy:rprt`, or the `fuzzy:` switch for every word) also match acronyms and
//! typos and are ranked best first; see [`Query::is_ranked`] and [`Query::fuzzy_highlights`].
//!
//! On Windows, whole NTFS drives can instead be read from the volume's MFT when the process is
//! elevated, with the walk as the per-root fallback: see [`ntfs::index_roots`].
//!
//! Still to come (see `docs/03-PROGRESS-TRACKER.md`, M1/M2): USN journal replay and general
//! ranking.

mod exec;
mod fuzzy;
mod index;
pub mod live;
pub mod ntfs;
mod search;
mod snapshot;
mod walk;
pub mod watch;

pub use exec::kind_extensions;
pub use index::{EntryId, EntryMeta, FileIndex, IndexBuilder};
pub use live::{ApplyStats, Change, IndexSnapshot, LiveIndex};
pub use search::{Query, QueryError};
pub use walk::{WalkOptions, walk};
pub use watch::{WatchBatch, WatchConfig, Watcher};
