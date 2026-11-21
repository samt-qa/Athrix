//! Typed errors, split into "fall back to the directory walk" and genuine failures.

use crate::journal::RescanReason;
use crate::record::RecordError;

/// Errors of this crate.
///
/// Use [`NtfsError::should_fall_back`] to decide whether to use the portable walker instead,
/// and [`NtfsError::needs_rescan`] to detect a journal that cannot be resumed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NtfsError {
    /// NTFS access is only implemented on Windows.
    #[error("NTFS enumeration is not supported on this platform")]
    Unsupported,

    /// The volume argument is not a drive letter (`C`, `C:` or `C:\`) or has no file system.
    #[error("invalid volume {0:?}: expected a local drive letter such as \"C:\"")]
    InvalidVolume(String),

    /// Opening the raw volume was refused: the process is not elevated (Administrator).
    #[error(
        "access denied opening volume {volume} (process elevated: {elevated}); \
         raw NTFS access requires Administrator rights"
    )]
    AccessDenied {
        /// Volume, e.g. `C:`.
        volume: String,
        /// Whether the process token is elevated (`false` is the usual cause).
        elevated: bool,
    },

    /// The volume is not NTFS (FAT32, exFAT, ReFS, UDF, ...).
    #[error("volume {volume} uses {filesystem}, not NTFS")]
    NotNtfs {
        /// Volume, e.g. `E:`.
        volume: String,
        /// File-system name reported by Windows.
        filesystem: String,
    },

    /// The drive is a network share (no MFT or local journal available).
    #[error("volume {volume} is a network drive")]
    NetworkDrive {
        /// Volume, e.g. `Z:`.
        volume: String,
    },

    /// The volume has no active USN journal (or it is being deleted). Enumeration still works;
    /// change tracking needs the journal (`fsutil usn createjournal`) or a watcher.
    #[error("the USN journal of volume {volume} is not active")]
    JournalInactive {
        /// Volume, e.g. `D:`.
        volume: String,
    },

    /// The saved journal position cannot be resumed; the index must be rebuilt.
    #[error("full rescan needed for volume {volume}: {reason}")]
    RescanNeeded {
        /// Volume, e.g. `C:`.
        volume: String,
        /// Why the cursor is unusable.
        reason: RescanReason,
    },

    /// A record returned by the file system was malformed.
    #[error("malformed data from volume {volume}: {source}")]
    Parse {
        /// Volume, e.g. `C:`.
        volume: String,
        /// The parse failure.
        source: RecordError,
    },

    /// Another OS error.
    #[error("{op} failed on volume {volume}: {source}")]
    Io {
        /// Volume, e.g. `C:`.
        volume: String,
        /// The operation that failed (e.g. `FSCTL_ENUM_USN_DATA`).
        op: &'static str,
        /// The OS error.
        source: std::io::Error,
    },
}

impl NtfsError {
    /// True if the caller should use the portable directory walk (and a file watcher) for this
    /// volume instead: not elevated, not NTFS, network drive, unsupported platform, invalid
    /// volume, or no journal.
    pub fn should_fall_back(&self) -> bool {
        matches!(
            self,
            Self::Unsupported
                | Self::InvalidVolume(_)
                | Self::AccessDenied { .. }
                | Self::NotNtfs { .. }
                | Self::NetworkDrive { .. }
                | Self::JournalInactive { .. }
        )
    }

    /// True if the saved journal cursor is unusable and the volume must be re-enumerated.
    pub fn needs_rescan(&self) -> bool {
        matches!(self, Self::RescanNeeded { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_classification() {
        let v = || "C:".to_string();
        assert!(NtfsError::Unsupported.should_fall_back());
        assert!(
            NtfsError::AccessDenied {
                volume: v(),
                elevated: false
            }
            .should_fall_back()
        );
        assert!(
            NtfsError::NotNtfs {
                volume: v(),
                filesystem: "exFAT".into()
            }
            .should_fall_back()
        );
        assert!(NtfsError::NetworkDrive { volume: v() }.should_fall_back());
        assert!(NtfsError::JournalInactive { volume: v() }.should_fall_back());
        let rescan = NtfsError::RescanNeeded {
            volume: v(),
            reason: RescanReason::JournalWrapped,
        };
        assert!(!rescan.should_fall_back());
        assert!(rescan.needs_rescan());
        let io = NtfsError::Io {
            volume: v(),
            op: "ReadFile",
            source: std::io::Error::other("x"),
        };
        assert!(!io.should_fall_back());
    }

    #[test]
    fn messages_are_actionable() {
        let e = NtfsError::AccessDenied {
            volume: "D:".into(),
            elevated: false,
        };
        let msg = e.to_string();
        assert!(msg.contains("D:") && msg.contains("Administrator"), "{msg}");
    }
}
