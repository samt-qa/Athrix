//! Progress counters and run state, as shown to the user.

use std::fmt;

/// Why a file could not be indexed, coarse enough to count and to show in a settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FailureKind {
    /// The format is not supported (or the file turned out to be binary).
    Unsupported,
    /// A size limit was exceeded.
    TooLarge,
    /// The document is malformed.
    Corrupt,
    /// The document is password-protected or DRM-encrypted.
    Encrypted,
    /// Reading the file failed (locked, no access, gone, device error).
    Io,
    /// Writing to the content index failed. Not remembered per file: retried next time.
    Index,
    /// Anything else (for example a crashed extraction worker process).
    Other,
}

impl FailureKind {
    /// Every kind, in display order.
    pub const ALL: [FailureKind; 7] = [
        FailureKind::Unsupported,
        FailureKind::TooLarge,
        FailureKind::Corrupt,
        FailureKind::Encrypted,
        FailureKind::Io,
        FailureKind::Index,
        FailureKind::Other,
    ];

    /// Short lowercase name, such as `"corrupt"`.
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::Unsupported => "unsupported",
            FailureKind::TooLarge => "too_large",
            FailureKind::Corrupt => "corrupt",
            FailureKind::Encrypted => "encrypted",
            FailureKind::Io => "io",
            FailureKind::Index => "index",
            FailureKind::Other => "other",
        }
    }

    pub(crate) fn slot(self) -> usize {
        self as usize
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Number of failures per [`FailureKind`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FailureCounts([u64; FailureKind::ALL.len()]);

impl FailureCounts {
    /// Failures of one kind.
    pub fn get(&self, kind: FailureKind) -> u64 {
        self.0[kind.slot()]
    }

    /// All failures.
    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// `(kind, count)` for every kind with at least one failure.
    pub fn iter(&self) -> impl Iterator<Item = (FailureKind, u64)> + '_ {
        FailureKind::ALL
            .into_iter()
            .map(|k| (k, self.get(k)))
            .filter(|&(_, n)| n > 0)
    }

    pub(crate) fn add(&mut self, kind: FailureKind) {
        self.0[kind.slot()] += 1;
    }
}

/// Why indexing is paused. Several reasons can hold at once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PauseReasons {
    /// [`ContentScheduler::pause`](crate::ContentScheduler::pause) was called.
    pub user: bool,
    /// The machine runs on battery.
    pub battery: bool,
    /// Other programs keep the CPU busy.
    pub busy: bool,
}

impl PauseReasons {
    /// True if any reason holds.
    pub fn any(&self) -> bool {
        self.user || self.battery || self.busy
    }
}

/// What the scheduler is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Extracting and indexing.
    Running,
    /// Nothing left to do; everything is committed.
    Idle,
    /// Work is waiting but paused.
    Paused(PauseReasons),
    /// [`ContentScheduler::shutdown`](crate::ContentScheduler::shutdown) was called.
    Stopped,
}

/// A snapshot of the scheduler's counters, cheap to copy.
///
/// The counters cover the lifetime of the scheduler. For a "Indexing contents: 12,340 /
/// 80,000" line, show [`processed`](Self::processed) / [`total`](Self::total).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// What the scheduler is doing.
    pub state: RunState,
    /// Jobs waiting in the queue.
    pub queued: u64,
    /// Jobs being extracted or written right now.
    pub in_flight: u64,
    /// Files extracted and added to the index.
    pub indexed: u64,
    /// Files skipped because the index already held this version.
    pub up_to_date: u64,
    /// Files skipped because the same version failed before.
    pub known_failures: u64,
    /// Files removed from the index.
    pub deleted: u64,
    /// Files that failed, by kind.
    pub failed: FailureCounts,
    /// Input bytes of the files indexed.
    pub bytes: u64,
    /// Documents indexed per second, smoothed, while running (0 when idle or paused).
    pub docs_per_sec: f64,
    /// Commits made.
    pub commits: u64,
}

impl Progress {
    /// Jobs finished, in any way.
    pub fn processed(&self) -> u64 {
        self.indexed + self.up_to_date + self.known_failures + self.deleted + self.failed.total()
    }

    /// Jobs finished plus jobs left: the denominator of a progress bar. It grows while the
    /// initial scan and the watcher add work.
    pub fn total(&self) -> u64 {
        self.processed() + self.queued + self.in_flight
    }
}
