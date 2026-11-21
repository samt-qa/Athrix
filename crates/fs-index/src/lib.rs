//! File-name index for Athrix: parallel directory walk, a compact in-memory index and fast,
//! Arabic-aware name search.
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use athrix_fs_index::{walk, Query, WalkOptions};
//!
//! let index = walk(&WalkOptions::new(["D:/Documents"]).exclude_glob("node_modules"))?;
//! let cancel = AtomicBool::new(false);
//! for id in index.search(&Query::parse("تقرير *.pdf"), 100, &cancel)? {
//!     println!("{}", index.path(id).display());
//! }
//! # Ok::<(), athrix_core::Error>(())
//! ```
//!
//! Still to come (see `docs/03-PROGRESS-TRACKER.md`, M1/M2): snapshots, live watching,
//! NTFS MFT/USN, path matching and ranking.

mod index;
mod search;
mod walk;

pub use index::{EntryId, EntryMeta, FileIndex, IndexBuilder};
pub use search::Query;
pub use walk::{WalkOptions, walk};
