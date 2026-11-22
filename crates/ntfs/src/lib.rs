//! Fast NTFS volume enumeration and USN change-journal reading.
//!
//! Instead of walking directories, this crate asks NTFS directly:
//!
//! 1. **Names and tree** — `FSCTL_ENUM_USN_DATA` streams one `USN_RECORD` per file of the
//!    volume (file reference number, parent reference, attributes, name). [`VolumeTree`]
//!    rebuilds the hierarchy from parent references and stores it in parent-before-child
//!    (breadth-first) order, which is what `athrix_fs_index::IndexBuilder` needs.
//! 2. **Size and modification time** — not part of USN records. With
//!    [`EnumOptions::metadata`] the crate reads the `$MFT` sequentially through the volume
//!    handle and decodes `$STANDARD_INFORMATION` (modified time) and the unnamed `$DATA`
//!    attribute (logical size) of every file record ([`mft`]).
//! 3. **Changes** — `FSCTL_QUERY_USN_JOURNAL` + `FSCTL_READ_USN_JOURNAL` from a saved
//!    [`JournalCursor`] produce [`Change`]s; a reset or wrapped journal is reported as
//!    [`NtfsError::RescanNeeded`].
//!
//! All of this needs a handle to `\\.\X:`, i.e. an **elevated** process. Every failure that
//! means "use the portable directory walk instead" is a typed [`NtfsError`] for which
//! [`NtfsError::should_fall_back`] is true (not elevated, not NTFS, network drive, journal
//! inactive, other platform).
//!
//! The parsers ([`record`], [`mft`], [`journal`], [`tree`]) are plain safe Rust over byte
//! slices, bounds-checked and fuzzed; the only `unsafe` code is the Win32 FFI in the private
//! `sys` module (Windows only). On other platforms [`Volume::open`] returns
//! [`NtfsError::Unsupported`].

mod bytes;
pub mod drives;
mod error;
pub mod journal;
pub mod mft;
pub mod record;
#[cfg(windows)]
#[allow(unsafe_code)]
mod sys;
pub mod tree;
mod volume;

pub use error::NtfsError;
pub use journal::{Change, ChangeBatch, ChangeKind, JournalCursor, JournalInfo, RescanReason};
pub use tree::{EntryRef, VolumeTree};
pub use volume::{EnumOptions, EnumStats, Volume, is_elevated};

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, NtfsError>;

/// `FILE_ATTRIBUTE_DIRECTORY`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `FILE_ATTRIBUTE_REPARSE_POINT` (symbolic links, junctions, but also cloud placeholders).
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Mask of the MFT record number inside a 64-bit NTFS file reference (low 48 bits; the high
/// 16 bits are the record's sequence number).
pub const RECORD_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// MFT record number of the NTFS root directory.
pub const ROOT_RECORD_NUMBER: u64 = 5;

/// Converts a Windows `FILETIME` (100 ns ticks since 1601-01-01 UTC) to seconds since the Unix
/// epoch, or `None` for 0 (unset).
pub fn filetime_to_unix_seconds(ft: i64) -> Option<i64> {
    const TICKS_PER_SEC: i64 = 10_000_000;
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    (ft > 0).then(|| ft.div_euclid(TICKS_PER_SEC) - EPOCH_DIFF_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix_seconds(0), None);
        // 1970-01-01T00:00:00Z
        assert_eq!(filetime_to_unix_seconds(116_444_736_000_000_000), Some(0));
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(
            filetime_to_unix_seconds(116_444_736_000_000_000 + 1_704_067_200 * 10_000_000),
            Some(1_704_067_200)
        );
    }
}
