//! Public volume API: Windows implementation and a stub for other platforms.

use std::time::Duration;

use crate::Result;
use crate::journal::{ChangeBatch, JournalCursor, JournalInfo};
use crate::tree::VolumeTree;

/// Options for [`Volume::enumerate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumOptions {
    /// Also read the `$MFT` to fill sizes and modification times (one extra sequential read
    /// of ~1 KiB per MFT record). If that fails, enumeration still succeeds without metadata
    /// and [`EnumStats::metadata_error`] says why.
    pub metadata: bool,
    /// Output buffer size for `FSCTL_ENUM_USN_DATA` (bytes).
    pub enum_buffer: usize,
    /// Chunk size for sequential `$MFT` reads (bytes).
    pub mft_chunk: usize,
}

impl Default for EnumOptions {
    fn default() -> Self {
        Self {
            metadata: true,
            enum_buffer: 1 << 20,
            mft_chunk: 4 << 20,
        }
    }
}

/// Counters and timings of one enumeration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnumStats {
    /// USN records returned by `FSCTL_ENUM_USN_DATA`.
    pub records: u64,
    /// `FSCTL_ENUM_USN_DATA` calls.
    pub enum_calls: u64,
    /// Time spent enumerating USN records.
    pub enum_time: Duration,
    /// Time spent resolving parents and ordering the tree.
    pub tree_time: Duration,
    /// Time spent reading and parsing the `$MFT` (zero without metadata).
    pub mft_time: Duration,
    /// `$MFT` bytes read.
    pub mft_bytes: u64,
    /// MFT records that failed fixups/parsing (changed during the scan, or corrupt).
    pub mft_bad_records: u64,
    /// Tree entries whose metadata was found in the MFT.
    pub mft_matched: u64,
    /// Why metadata could not be read, if it was requested and failed.
    pub metadata_error: Option<String>,
}

/// Parses `C`, `C:`, `C:\` or `C:/` (any case) into an upper-case drive letter.
#[cfg_attr(not(windows), allow(dead_code))] // the stub never parses volumes
pub(crate) fn parse_drive_letter(s: &str) -> Option<char> {
    let mut chars = s.trim().chars();
    let letter = chars.next()?.to_ascii_uppercase();
    if !letter.is_ascii_uppercase() {
        return None;
    }
    match chars.as_str() {
        "" | ":" | ":\\" | ":/" => Some(letter),
        _ => None,
    }
}

/// True if the current process runs elevated (Administrator token). Raw volume access, and
/// therefore every [`Volume`] operation, needs it.
pub fn is_elevated() -> Result<bool> {
    imp::is_elevated()
}

/// An open NTFS volume (`\\.\X:`).
#[derive(Debug)]
pub struct Volume {
    inner: imp::Volume,
}

impl Volume {
    /// Opens a local NTFS volume by drive letter (`"C"`, `"C:"` or `"C:\\"`).
    ///
    /// Checks are ordered so that the fallback reasons need no privileges: invalid letter →
    /// [`crate::NtfsError::InvalidVolume`], network share → [`crate::NtfsError::NetworkDrive`], other file
    /// system → [`crate::NtfsError::NotNtfs`], then opening the raw volume →
    /// [`crate::NtfsError::AccessDenied`] when not elevated.
    pub fn open(volume: &str) -> Result<Self> {
        Ok(Self {
            inner: imp::Volume::open(volume)?,
        })
    }

    /// The volume name, e.g. `C:`.
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// The root path, e.g. `C:\`, used as the name of the tree's root entry.
    pub fn root(&self) -> String {
        format!("{}\\", self.name())
    }

    /// Queries the USN journal (`FSCTL_QUERY_USN_JOURNAL`).
    ///
    /// Errors with [`crate::NtfsError::JournalInactive`] if the volume has no journal.
    pub fn query_journal(&self) -> Result<JournalInfo> {
        self.inner.query_journal()
    }

    /// Enumerates every file and directory of the volume.
    ///
    /// For live updates, call [`Volume::query_journal`] **before** this and keep
    /// [`JournalInfo::cursor_at_end`]: changes made during the scan are then replayed.
    pub fn enumerate(&self, opts: &EnumOptions) -> Result<(VolumeTree, EnumStats)> {
        self.inner.enumerate(opts)
    }

    /// Reads journal changes after `cursor`, up to about `max_bytes` of records (pass e.g.
    /// 1 MiB and call again while [`ChangeBatch::caught_up`] is false).
    ///
    /// Errors with [`crate::NtfsError::RescanNeeded`] if the journal was recreated or wrapped past the
    /// cursor, and [`crate::NtfsError::JournalInactive`] if it was disabled.
    pub fn read_changes(&self, cursor: &JournalCursor, max_bytes: usize) -> Result<ChangeBatch> {
        self.inner.read_changes(cursor, max_bytes)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_DELETE_IN_PROGRESS,
        ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, GENERIC_READ,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
        FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0,
    };

    use super::{EnumOptions, EnumStats, parse_drive_letter};
    use crate::journal::{ChangeBatch, JournalCursor, JournalInfo, parse_read_output};
    use crate::mft::{MftScan, NtfsVolumeData, apply_fixups, data_runs};
    use crate::record::{RecordError, split_output};
    use crate::sys::{self, AlignedBuf, Handle};
    use crate::tree::{TreeBuilder, VolumeTree};
    use crate::{NtfsError, Result};

    const DRIVE_UNKNOWN: u32 = 0;
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOTE: u32 = 4;

    pub(super) fn is_elevated() -> Result<bool> {
        sys::is_elevated().map_err(|source| NtfsError::Io {
            volume: String::new(),
            op: "GetTokenInformation(TokenElevation)",
            source,
        })
    }

    #[derive(Debug)]
    pub(super) struct Volume {
        name: String,
        handle: Handle,
    }

    fn code(e: &io::Error) -> Option<u32> {
        e.raw_os_error().map(|c| c as u32)
    }

    impl Volume {
        pub(super) fn open(volume: &str) -> Result<Self> {
            let letter = parse_drive_letter(volume)
                .ok_or_else(|| NtfsError::InvalidVolume(volume.to_string()))?;
            let name = format!("{letter}:");
            let root = format!("{letter}:\\");
            match sys::drive_type(&root) {
                DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR => {
                    return Err(NtfsError::InvalidVolume(volume.to_string()));
                }
                DRIVE_REMOTE => return Err(NtfsError::NetworkDrive { volume: name }),
                _ => {}
            }
            let fs = sys::filesystem_name(&root).map_err(|source| NtfsError::Io {
                volume: name.clone(),
                op: "GetVolumeInformationW",
                source,
            })?;
            if !fs.eq_ignore_ascii_case("NTFS") {
                return Err(NtfsError::NotNtfs {
                    volume: name,
                    filesystem: fs,
                });
            }
            let handle = sys::open_device(&format!(r"\\.\{letter}:"), GENERIC_READ)
                .map_err(|e| os_error(&name, "CreateFileW(\\\\.\\X:)", e))?;
            Ok(Self { name, handle })
        }

        pub(super) fn name(&self) -> &str {
            &self.name
        }

        fn err(&self, op: &'static str, e: io::Error) -> NtfsError {
            os_error(&self.name, op, e)
        }

        fn parse_err(&self, source: RecordError) -> NtfsError {
            NtfsError::Parse {
                volume: self.name.clone(),
                source,
            }
        }

        fn volume_data(&self) -> Result<NtfsVolumeData> {
            let mut out = [0u8; 128];
            let n = sys::ioctl::<()>(&self.handle, FSCTL_GET_NTFS_VOLUME_DATA, None, &mut out)
                .map_err(|e| self.err("FSCTL_GET_NTFS_VOLUME_DATA", e))?;
            NtfsVolumeData::parse(&out[..n]).map_err(|e| self.parse_err(e))
        }

        pub(super) fn query_journal(&self) -> Result<JournalInfo> {
            let mut out = [0u8; 128];
            let n = sys::ioctl::<()>(&self.handle, FSCTL_QUERY_USN_JOURNAL, None, &mut out)
                .map_err(|e| self.err("FSCTL_QUERY_USN_JOURNAL", e))?;
            JournalInfo::parse(&out[..n]).map_err(|e| self.parse_err(e))
        }

        pub(super) fn enumerate(&self, opts: &EnumOptions) -> Result<(VolumeTree, EnumStats)> {
            let mut stats = EnumStats::default();
            let vd = self.volume_data().ok();
            let estimate = vd.map_or(1 << 16, |d| {
                (d.mft_valid_data_length / u64::from(d.bytes_per_file_record)) as usize
            });

            let t = Instant::now();
            let mut builder = TreeBuilder::with_capacity(estimate, 24);
            let mut buf = AlignedBuf::new(opts.enum_buffer.max(64 << 10));
            let mut input = MFT_ENUM_DATA_V0 {
                StartFileReferenceNumber: 0,
                LowUsn: 0,
                HighUsn: i64::MAX,
            };
            loop {
                let n = match sys::ioctl(
                    &self.handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&input),
                    buf.bytes_mut(),
                ) {
                    Ok(n) => n,
                    Err(e) if code(&e) == Some(ERROR_HANDLE_EOF) => break,
                    Err(e) => return Err(self.err("FSCTL_ENUM_USN_DATA", e)),
                };
                stats.enum_calls += 1;
                let (next, records) =
                    split_output(&buf.bytes()[..n]).map_err(|e| self.parse_err(e))?;
                let mut any = false;
                for rec in records {
                    if let Some(rec) = rec.map_err(|e| self.parse_err(e))? {
                        builder.push_record(&rec);
                        stats.records += 1;
                        any = true;
                    }
                }
                if !any || next <= input.StartFileReferenceNumber {
                    break; // no progress: defensive stop instead of looping forever
                }
                input.StartFileReferenceNumber = next;
            }
            stats.enum_time = t.elapsed();

            let t = Instant::now();
            let mut tree = builder.finish(&format!("{}\\", self.name));
            stats.tree_time = t.elapsed();

            if opts.metadata {
                let t = Instant::now();
                match vd
                    .ok_or_else(|| "FSCTL_GET_NTFS_VOLUME_DATA failed".to_string())
                    .and_then(|vd| self.read_mft(vd, opts, &mut tree, &mut stats))
                {
                    Ok(()) => tree.mark_metadata(),
                    Err(e) => stats.metadata_error = Some(e),
                }
                stats.mft_time = t.elapsed();
            }
            Ok((tree, stats))
        }

        /// Sequentially reads the `$MFT` stream and merges sizes/mtimes into `tree`.
        fn read_mft(
            &self,
            vd: NtfsVolumeData,
            opts: &EnumOptions,
            tree: &mut VolumeTree,
            stats: &mut EnumStats,
        ) -> std::result::Result<(), String> {
            let cluster = u64::from(vd.bytes_per_cluster);
            let sector = vd.bytes_per_sector as usize;
            let rs = vd.bytes_per_file_record as usize;

            // Record 0 describes the $MFT itself.
            let mut first = AlignedBuf::new(rs.max(sector));
            let start = vd
                .mft_start_lcn
                .checked_mul(cluster)
                .ok_or("MFT start overflows")?;
            let got = sys::read_at(&self.handle, start, first.bytes_mut())
                .map_err(|e| format!("reading $MFT record 0: {e}"))?;
            if got < rs {
                return Err("short read of $MFT record 0".into());
            }
            let rec0 = &mut first.bytes_mut()[..rs];
            apply_fixups(rec0).map_err(|e| format!("$MFT record 0: {e}"))?;
            let (runs, data_size) =
                data_runs(rec0, cluster).map_err(|e| format!("$MFT record 0: {e}"))?;

            let chunk = opts
                .mft_chunk
                .clamp(1 << 16, 64 << 20)
                .next_multiple_of(4096);
            let mut buf = AlignedBuf::new(chunk);
            let mut scan = MftScan::new(tree, rs);
            let mut remaining = data_size.min(vd.mft_valid_data_length);
            for run in runs {
                if remaining == 0 {
                    break;
                }
                let run_bytes = run.clusters.saturating_mul(cluster).min(remaining);
                remaining -= run_bytes;
                let Some(lcn) = run.lcn else {
                    scan.skip(run_bytes);
                    continue;
                };
                let base = lcn.checked_mul(cluster).ok_or("run offset overflows")?;
                let mut pos = 0u64;
                while pos < run_bytes {
                    let want = (run_bytes - pos).min(chunk as u64) as usize;
                    // Raw volume reads must be sector multiples; parse only `want` bytes.
                    let read_len = want.next_multiple_of(sector).min(buf.len());
                    let got =
                        sys::read_at(&self.handle, base + pos, &mut buf.bytes_mut()[..read_len])
                            .map_err(|e| format!("reading $MFT at byte {}: {e}", base + pos))?;
                    if got < want {
                        return Err(format!("short $MFT read at byte {}", base + pos));
                    }
                    scan.feed(tree, &mut buf.bytes_mut()[..want]);
                    stats.mft_bytes += want as u64;
                    pos += want as u64;
                }
            }
            stats.mft_bad_records = scan.bad_records;
            stats.mft_matched = scan.matched;
            Ok(())
        }

        pub(super) fn read_changes(
            &self,
            cursor: &JournalCursor,
            max_bytes: usize,
        ) -> Result<ChangeBatch> {
            let info = self.query_journal()?;
            info.check(cursor)
                .map_err(|reason| NtfsError::RescanNeeded {
                    volume: self.name.clone(),
                    reason,
                })?;
            let mut buf = AlignedBuf::new(max_bytes.clamp(64 << 10, 16 << 20));
            let mut batch = ChangeBatch {
                changes: Vec::new(),
                next: *cursor,
                caught_up: false,
            };
            let mut read = 0usize;
            loop {
                if batch.next.next_usn >= info.next_usn {
                    batch.caught_up = true; // reached the end as of the query
                    break;
                }
                let input = READ_USN_JOURNAL_DATA_V0 {
                    StartUsn: batch.next.next_usn,
                    ReasonMask: u32::MAX,
                    ReturnOnlyOnClose: 0,
                    Timeout: 0,
                    BytesToWaitFor: 0,
                    UsnJournalID: cursor.journal_id,
                };
                let n = sys::ioctl(
                    &self.handle,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&input),
                    buf.bytes_mut(),
                )
                .map_err(|e| match code(&e) {
                    Some(ERROR_JOURNAL_ENTRY_DELETED) => NtfsError::RescanNeeded {
                        volume: self.name.clone(),
                        reason: crate::RescanReason::JournalWrapped,
                    },
                    _ => self.err("FSCTL_READ_USN_JOURNAL", e),
                })?;
                let (next, changes) =
                    parse_read_output(&buf.bytes()[..n]).map_err(|e| self.parse_err(e))?;
                let progressed = next > batch.next.next_usn;
                batch.next.next_usn = next.max(batch.next.next_usn);
                let empty = changes.is_empty();
                batch.changes.extend(changes);
                read += n;
                if empty || !progressed {
                    batch.caught_up = true;
                    break;
                }
                if read >= max_bytes {
                    break;
                }
            }
            Ok(batch)
        }
    }

    /// Maps OS errors to the typed fallback errors.
    fn os_error(volume: &str, op: &'static str, e: io::Error) -> NtfsError {
        match code(&e) {
            Some(ERROR_ACCESS_DENIED) => NtfsError::AccessDenied {
                volume: volume.to_string(),
                elevated: sys::is_elevated().unwrap_or(false),
            },
            Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                NtfsError::JournalInactive {
                    volume: volume.to_string(),
                }
            }
            _ => NtfsError::Io {
                volume: volume.to_string(),
                op,
                source: e,
            },
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::EnumOptions;
    use crate::journal::{ChangeBatch, JournalCursor, JournalInfo};
    use crate::tree::VolumeTree;
    use crate::{EnumStats, NtfsError, Result};

    pub(super) fn is_elevated() -> Result<bool> {
        Err(NtfsError::Unsupported)
    }

    /// Uninhabited: a volume can never be opened on this platform.
    #[derive(Debug)]
    pub(super) struct Volume {
        never: std::convert::Infallible,
    }

    impl Volume {
        pub(super) fn open(_volume: &str) -> Result<Self> {
            Err(NtfsError::Unsupported)
        }

        pub(super) fn name(&self) -> &str {
            match self.never {}
        }

        pub(super) fn query_journal(&self) -> Result<JournalInfo> {
            match self.never {}
        }

        pub(super) fn enumerate(&self, _opts: &EnumOptions) -> Result<(VolumeTree, EnumStats)> {
            match self.never {}
        }

        pub(super) fn read_changes(
            &self,
            _cursor: &JournalCursor,
            _max_bytes: usize,
        ) -> Result<ChangeBatch> {
            match self.never {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NtfsError;

    #[test]
    fn drive_letters() {
        assert_eq!(parse_drive_letter("c"), Some('C'));
        assert_eq!(parse_drive_letter("D:"), Some('D'));
        assert_eq!(parse_drive_letter("d:\\"), Some('D'));
        assert_eq!(parse_drive_letter("E:/"), Some('E'));
        assert_eq!(parse_drive_letter(""), None);
        assert_eq!(parse_drive_letter("1:"), None);
        assert_eq!(parse_drive_letter("C:\\Windows"), None);
        assert_eq!(parse_drive_letter(r"\\server\share"), None);
        assert_eq!(parse_drive_letter("é:"), None);
    }

    #[test]
    fn invalid_volume_is_a_fallback() {
        let e = Volume::open("not a drive").unwrap_err();
        #[cfg(windows)]
        assert!(matches!(e, NtfsError::InvalidVolume(_)), "{e:?}");
        #[cfg(not(windows))]
        assert!(matches!(e, NtfsError::Unsupported), "{e:?}");
        assert!(e.should_fall_back());
    }

    /// Without elevation, opening the system volume must fail with the typed fallback error;
    /// elevated, it must succeed.
    #[cfg(windows)]
    #[test]
    fn open_reports_elevation_state() {
        let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let elevated = is_elevated().unwrap();
        match Volume::open(&sys_drive) {
            Ok(v) => assert!(elevated, "opened {} without elevation?", v.name()),
            Err(NtfsError::AccessDenied { elevated: e, .. }) => {
                assert!(!elevated && !e);
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    /// Needs Administrator: `cargo test -p athrix-ntfs -- --ignored` from an elevated shell.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs an elevated (Administrator) process"]
    fn elevated_enumerate_system_drive() {
        let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let v = Volume::open(&sys_drive).unwrap();
        let journal = v.query_journal().unwrap();
        let (tree, stats) = v.enumerate(&EnumOptions::default()).unwrap();
        assert!(tree.len() > 10_000, "{stats:?}");
        assert!(tree.has_metadata(), "{stats:?}");
        let windows = tree
            .iter()
            .find(|e| e.parent == Some(0) && e.name.eq_ignore_ascii_case("Windows"))
            .expect("C:\\Windows");
        assert!(windows.is_dir());
        // A known file must have a plausible size and mtime.
        let lookup_path = std::path::PathBuf::from(format!("{sys_drive}\\Windows\\explorer.exe"));
        let i = (0..tree.len() as u32)
            .find(|&i| tree.path(i) == lookup_path)
            .expect("explorer.exe");
        let e = tree.get(i);
        let meta = std::fs::metadata(&lookup_path).unwrap();
        assert_eq!(e.size, Some(meta.len()));
        assert!(e.modified.is_some());
        // Journal: nothing lost between query and now.
        let batch = v.read_changes(&journal.cursor_at_end(), 1 << 20).unwrap();
        assert!(batch.next.next_usn >= journal.next_usn);
    }
}
