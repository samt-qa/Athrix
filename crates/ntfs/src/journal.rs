//! USN change journal: cursor validation and mapping of records to index changes.

use crate::bytes::{i64_at, u64_at};
use crate::record::{RecordError, UsnRecord, split_output};

/// `USN_REASON_DATA_OVERWRITE`.
pub const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
/// `USN_REASON_DATA_EXTEND`.
pub const REASON_DATA_EXTEND: u32 = 0x0000_0002;
/// `USN_REASON_DATA_TRUNCATION`.
pub const REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
/// `USN_REASON_NAMED_DATA_*` (alternate data streams).
pub const REASON_NAMED_DATA: u32 = 0x0000_0070;
/// `USN_REASON_FILE_CREATE`.
pub const REASON_FILE_CREATE: u32 = 0x0000_0100;
/// `USN_REASON_FILE_DELETE`.
pub const REASON_FILE_DELETE: u32 = 0x0000_0200;
/// `USN_REASON_RENAME_OLD_NAME`.
pub const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// `USN_REASON_RENAME_NEW_NAME`.
pub const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// `USN_REASON_BASIC_INFO_CHANGE` (attributes or timestamps).
pub const REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
/// `USN_REASON_HARD_LINK_CHANGE`.
pub const REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
/// `USN_REASON_REPARSE_POINT_CHANGE`.
pub const REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
/// `USN_REASON_CLOSE` (last record for a handle; reasons accumulated until then).
pub const REASON_CLOSE: u32 = 0x8000_0000;

const REASON_DATA: u32 =
    REASON_DATA_OVERWRITE | REASON_DATA_EXTEND | REASON_DATA_TRUNCATION | REASON_NAMED_DATA;

/// Where to resume reading a volume's journal. Persist it next to the index snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JournalCursor {
    /// `UsnJournalID`: changes when the journal is deleted and recreated.
    pub journal_id: u64,
    /// The next USN to read.
    pub next_usn: i64,
}

/// `USN_JOURNAL_DATA_V0` as returned by `FSCTL_QUERY_USN_JOURNAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    /// `UsnJournalID`.
    pub journal_id: u64,
    /// First USN still present in the journal.
    pub first_usn: i64,
    /// USN that the next record will get.
    pub next_usn: i64,
    /// Records below this USN are invalid (journal was stamped/reset).
    pub lowest_valid_usn: i64,
    /// Largest USN the journal can assign before it must be recreated.
    pub max_usn: i64,
    /// Target maximum size in bytes.
    pub maximum_size: u64,
    /// Bytes added/removed when the journal grows/trims.
    pub allocation_delta: u64,
}

/// Size of `USN_JOURNAL_DATA_V0`.
pub const JOURNAL_DATA_V0_LEN: usize = 56;

impl JournalInfo {
    /// Parses `USN_JOURNAL_DATA_V0` (also accepts the longer V1/V2 structs).
    pub fn parse(b: &[u8]) -> Result<Self, RecordError> {
        let bad = RecordError::BadLength {
            offset: 0,
            length: JOURNAL_DATA_V0_LEN,
            remaining: b.len(),
        };
        let u = |o| u64_at(b, o).ok_or(bad.clone());
        let i = |o| i64_at(b, o).ok_or(bad.clone());
        Ok(Self {
            journal_id: u(0)?,
            first_usn: i(8)?,
            next_usn: i(16)?,
            lowest_valid_usn: i(24)?,
            max_usn: i(32)?,
            maximum_size: u(40)?,
            allocation_delta: u(48)?,
        })
    }

    /// A cursor positioned at the end of the journal ("changes from now on"). Take it
    /// **before** enumerating the volume so no change between the scan and the first read is
    /// lost (re-applying a change already seen by the scan is harmless).
    pub fn cursor_at_end(&self) -> JournalCursor {
        JournalCursor {
            journal_id: self.journal_id,
            next_usn: self.next_usn,
        }
    }

    /// Checks that `saved` can still be resumed against the current journal state.
    pub fn check(&self, saved: &JournalCursor) -> Result<(), RescanReason> {
        if saved.journal_id != self.journal_id {
            return Err(RescanReason::JournalRecreated {
                saved_id: saved.journal_id,
                current_id: self.journal_id,
            });
        }
        if saved.next_usn < self.first_usn || saved.next_usn < self.lowest_valid_usn {
            return Err(RescanReason::JournalWrapped);
        }
        if saved.next_usn > self.next_usn {
            return Err(RescanReason::CursorAhead);
        }
        Ok(())
    }
}

/// Why a saved [`JournalCursor`] cannot be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RescanReason {
    /// The journal was deleted and recreated (different id): changes in between are lost.
    #[error("journal was recreated (saved id {saved_id:#x}, current {current_id:#x})")]
    JournalRecreated {
        /// Id in the saved cursor.
        saved_id: u64,
        /// Current id.
        current_id: u64,
    },
    /// Records after the saved position were already purged (journal wrapped / trimmed).
    #[error("journal wrapped: records after the saved position were purged")]
    JournalWrapped,
    /// The saved position is beyond the journal end (volume restored from a backup?).
    #[error("saved position is beyond the end of the journal")]
    CursorAhead,
}

/// What a journal record means for a name index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// A new file or directory (`name`/`parent_ref` are its location).
    Created,
    /// The file was deleted (`name`/`parent_ref` are its last location).
    Deleted,
    /// First half of a rename/move: the old name and parent.
    RenamedFrom,
    /// Second half of a rename/move: the new name and parent.
    RenamedTo,
    /// Contents changed (size and modification time may differ).
    DataChanged,
    /// Attributes, timestamps, security, reparse data, ... changed.
    MetadataChanged,
}

impl ChangeKind {
    /// Classifies accumulated `USN_REASON_*` flags. When several apply, the most structural
    /// wins: delete > rename-old > create > rename-new > data > metadata. (A file created
    /// and deleted while one handle was open is reported as `Deleted`; a consumer simply
    /// ignores deletes of unknown references.)
    pub fn from_reason(reason: u32) -> Self {
        if reason & REASON_FILE_DELETE != 0 {
            Self::Deleted
        } else if reason & REASON_RENAME_OLD_NAME != 0 {
            Self::RenamedFrom
        } else if reason & REASON_FILE_CREATE != 0 {
            Self::Created
        } else if reason & REASON_RENAME_NEW_NAME != 0 {
            Self::RenamedTo
        } else if reason & REASON_DATA != 0 {
            Self::DataChanged
        } else {
            Self::MetadataChanged
        }
    }
}

/// One journal record mapped for the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// USN of the record.
    pub usn: i64,
    /// What happened.
    pub kind: ChangeKind,
    /// File reference of the changed file.
    pub file_ref: u64,
    /// Parent directory reference (old parent for `RenamedFrom`).
    pub parent_ref: u64,
    /// Name (old name for `RenamedFrom`).
    pub name: String,
    /// `FILE_ATTRIBUTE_*` flags.
    pub attributes: u32,
    /// Raw `USN_REASON_*` flags.
    pub reason: u32,
    /// `FILETIME` of the change.
    pub timestamp: i64,
}

impl Change {
    /// Maps a journal record.
    pub fn from_record(rec: &UsnRecord<'_>) -> Self {
        Self {
            usn: rec.usn,
            kind: ChangeKind::from_reason(rec.reason),
            file_ref: rec.file_ref,
            parent_ref: rec.parent_ref,
            name: rec.name(),
            attributes: rec.attributes,
            reason: rec.reason,
            timestamp: rec.timestamp,
        }
    }

    /// True if the changed entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & crate::FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// True if this is the `USN_REASON_CLOSE` record that ends a burst of changes.
    pub fn is_close(&self) -> bool {
        self.reason & REASON_CLOSE != 0
    }
}

/// A batch of changes read from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    /// Changes in USN order.
    pub changes: Vec<Change>,
    /// Cursor to resume from (persist it once the changes are applied).
    pub next: JournalCursor,
    /// True if the journal end was reached.
    pub caught_up: bool,
}

/// Parses an `FSCTL_READ_USN_JOURNAL` output buffer into the next USN and its changes.
pub fn parse_read_output(buf: &[u8]) -> Result<(i64, Vec<Change>), RecordError> {
    let (next, records) = split_output(buf)?;
    let mut changes = Vec::new();
    for rec in records {
        if let Some(rec) = rec? {
            changes.push(Change::from_record(&rec));
        }
    }
    Ok((next as i64, changes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::build;

    fn info() -> JournalInfo {
        JournalInfo {
            journal_id: 0xABC,
            first_usn: 1000,
            next_usn: 5000,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
            maximum_size: 32 << 20,
            allocation_delta: 8 << 20,
        }
    }

    #[test]
    fn parses_journal_data() {
        let i = info();
        let mut b = Vec::new();
        for v in [
            i.journal_id as i64,
            i.first_usn,
            i.next_usn,
            i.lowest_valid_usn,
            i.max_usn,
            i.maximum_size as i64,
            i.allocation_delta as i64,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(JournalInfo::parse(&b), Ok(i));
        assert!(JournalInfo::parse(&b[..55]).is_err());
    }

    #[test]
    fn cursor_validation() {
        let i = info();
        let at = |next_usn| JournalCursor {
            journal_id: 0xABC,
            next_usn,
        };
        assert_eq!(i.check(&at(1000)), Ok(()));
        assert_eq!(i.check(&at(5000)), Ok(()));
        assert_eq!(i.check(&i.cursor_at_end()), Ok(()));
        assert_eq!(i.check(&at(999)), Err(RescanReason::JournalWrapped));
        assert_eq!(i.check(&at(5001)), Err(RescanReason::CursorAhead));
        assert!(matches!(
            i.check(&JournalCursor {
                journal_id: 1,
                next_usn: 2000
            }),
            Err(RescanReason::JournalRecreated { saved_id: 1, .. })
        ));
        let stamped = JournalInfo {
            lowest_valid_usn: 3000,
            ..i
        };
        assert_eq!(stamped.check(&at(2000)), Err(RescanReason::JournalWrapped));
    }

    #[test]
    fn reason_mapping() {
        use ChangeKind::*;
        assert_eq!(ChangeKind::from_reason(REASON_FILE_CREATE), Created);
        assert_eq!(
            ChangeKind::from_reason(REASON_FILE_CREATE | REASON_CLOSE),
            Created
        );
        assert_eq!(
            ChangeKind::from_reason(REASON_FILE_CREATE | REASON_DATA_EXTEND | REASON_CLOSE),
            Created
        );
        assert_eq!(
            ChangeKind::from_reason(REASON_FILE_CREATE | REASON_FILE_DELETE | REASON_CLOSE),
            Deleted
        );
        assert_eq!(ChangeKind::from_reason(REASON_RENAME_OLD_NAME), RenamedFrom);
        assert_eq!(ChangeKind::from_reason(REASON_RENAME_NEW_NAME), RenamedTo);
        assert_eq!(
            ChangeKind::from_reason(REASON_RENAME_NEW_NAME | REASON_FILE_CREATE),
            Created
        );
        assert_eq!(ChangeKind::from_reason(REASON_DATA_OVERWRITE), DataChanged);
        assert_eq!(
            ChangeKind::from_reason(REASON_DATA_TRUNCATION | REASON_CLOSE),
            DataChanged
        );
        assert_eq!(ChangeKind::from_reason(0x0020), DataChanged); // named data extend
        assert_eq!(
            ChangeKind::from_reason(REASON_BASIC_INFO_CHANGE),
            MetadataChanged
        );
        assert_eq!(ChangeKind::from_reason(REASON_CLOSE), MetadataChanged);
    }

    #[test]
    fn parses_a_rename_sequence() {
        let dir = 0x0001_0000_0000_0030;
        let file = 0x0002_0000_0000_0040;
        let buf = build::output(
            7777,
            &[
                build::v2(file, dir, 5000, REASON_RENAME_OLD_NAME, 0x20, "old.txt"),
                build::v2(
                    file,
                    0x0005_0000_0000_0005,
                    5100,
                    REASON_RENAME_NEW_NAME,
                    0x20,
                    "new.txt",
                ),
                build::v2(
                    file,
                    0x0005_0000_0000_0005,
                    5200,
                    REASON_RENAME_NEW_NAME | REASON_CLOSE,
                    0x20,
                    "new.txt",
                ),
            ],
        );
        let (next, changes) = parse_read_output(&buf).unwrap();
        assert_eq!(next, 7777);
        let kinds: Vec<_> = changes.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ChangeKind::RenamedFrom,
                ChangeKind::RenamedTo,
                ChangeKind::RenamedTo
            ]
        );
        assert_eq!(changes[0].name, "old.txt");
        assert_eq!(changes[0].parent_ref, dir);
        assert_eq!(changes[1].name, "new.txt");
        assert!(!changes[1].is_close());
        assert!(changes[2].is_close());
        assert!(!changes[2].is_dir());
    }

    #[test]
    fn caught_up_output_is_just_the_cursor() {
        let (next, changes) = parse_read_output(&42u64.to_le_bytes()).unwrap();
        assert_eq!((next, changes.len()), (42, 0));
    }
}
