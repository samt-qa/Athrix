//! Parser for `USN_RECORD_V2` / `USN_RECORD_V3` buffers as returned by `FSCTL_ENUM_USN_DATA`
//! and `FSCTL_READ_USN_JOURNAL`.
//!
//! The buffer is untrusted: every length and offset is validated against the record and the
//! buffer before use. The parser never panics on any input (fuzzed with proptest).

use crate::bytes::{i64_at, u16_at, u32_at, u64_at, utf16le_lossy};

/// Fixed part of `USN_RECORD_V2` (before `FileName`).
pub const V2_HEADER_LEN: usize = 60;
/// Fixed part of `USN_RECORD_V3` (before `FileName`).
pub const V3_HEADER_LEN: usize = 76;

/// A malformed USN record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecordError {
    /// The buffer is shorter than its 8-byte leading cursor (next FRN / next USN).
    #[error("output buffer shorter than its 8-byte header")]
    MissingCursor,
    /// `RecordLength` is smaller than the fixed header, larger than the remaining buffer, or
    /// the header itself is truncated.
    #[error("bad record length {length} at offset {offset} (remaining {remaining})")]
    BadLength {
        /// Offset of the record in the buffer.
        offset: usize,
        /// Claimed `RecordLength`.
        length: usize,
        /// Bytes left in the buffer.
        remaining: usize,
    },
    /// `FileNameOffset`/`FileNameLength` point outside the record or the length is odd.
    #[error("bad file name span {name_offset}+{name_len} in record of {record_len} bytes")]
    BadName {
        /// Claimed `FileNameOffset`.
        name_offset: usize,
        /// Claimed `FileNameLength`.
        name_len: usize,
        /// `RecordLength`.
        record_len: usize,
    },
    /// A V3 (128-bit) file id that does not fit the 64-bit NTFS reference (ReFS).
    #[error("128-bit file id does not fit in 64 bits")]
    WideFileId,
    /// A malformed MFT structure (see [`crate::mft`]).
    #[error("malformed MFT data: {0}")]
    Mft(&'static str),
}

/// One decoded USN record (version 2 or 3). The name is borrowed as raw UTF-16LE bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnRecord<'a> {
    /// `MajorVersion` (2 or 3).
    pub major_version: u16,
    /// File reference number (MFT record number in the low 48 bits, sequence in the high 16).
    pub file_ref: u64,
    /// File reference number of the parent directory.
    pub parent_ref: u64,
    /// Update sequence number of this record (0 for records from `FSCTL_ENUM_USN_DATA` of files
    /// never changed since the journal was created).
    pub usn: i64,
    /// `FILETIME` of the change (journal records only).
    pub timestamp: i64,
    /// `USN_REASON_*` flags (journal records only).
    pub reason: u32,
    /// `FILE_ATTRIBUTE_*` flags.
    pub attributes: u32,
    /// UTF-16LE name bytes (even length, no terminator).
    pub name_utf16le: &'a [u8],
}

impl UsnRecord<'_> {
    /// The name decoded as UTF-16 (unpaired surrogates become U+FFFD).
    pub fn name(&self) -> String {
        utf16le_lossy(self.name_utf16le)
    }

    /// True if `FILE_ATTRIBUTE_DIRECTORY` is set.
    pub fn is_dir(&self) -> bool {
        self.attributes & crate::FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// Splits an `FSCTL_ENUM_USN_DATA` / `FSCTL_READ_USN_JOURNAL` output buffer into its leading
/// 8-byte cursor (next start FRN, resp. next USN) and an iterator over the records.
pub fn split_output(buf: &[u8]) -> Result<(u64, UsnRecords<'_>), RecordError> {
    let next = u64_at(buf, 0).ok_or(RecordError::MissingCursor)?;
    Ok((next, UsnRecords::new(&buf[8..])))
}

/// Iterator over the records packed in a buffer (without the 8-byte leading cursor).
///
/// Yields `Ok(None)` for records of an unknown major version (skipped, e.g. V4 range records),
/// and stops after the first structural error.
#[derive(Debug, Clone)]
pub struct UsnRecords<'a> {
    buf: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> UsnRecords<'a> {
    /// Iterates the records in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            failed: false,
        }
    }
}

impl<'a> Iterator for UsnRecords<'a> {
    type Item = Result<Option<UsnRecord<'a>>, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.buf.len() {
            return None;
        }
        let rest = &self.buf[self.pos..];
        let offset = self.pos;
        match parse_one(rest) {
            Ok((len, rec)) => {
                self.pos += len;
                Some(Ok(rec))
            }
            Err(e) => {
                self.failed = true;
                Some(Err(match e {
                    RecordError::BadLength {
                        length, remaining, ..
                    } => RecordError::BadLength {
                        offset,
                        length,
                        remaining,
                    },
                    other => other,
                }))
            }
        }
    }
}

/// Parses the record at the start of `b`. Returns its `RecordLength` (always > 0, ≤ `b.len()`)
/// and the record, or `None` for an unknown version.
pub fn parse_one(b: &[u8]) -> Result<(usize, Option<UsnRecord<'_>>), RecordError> {
    let bad_len = |length: usize| RecordError::BadLength {
        offset: 0,
        length,
        remaining: b.len(),
    };
    let record_len = u32_at(b, 0).ok_or_else(|| bad_len(0))? as usize;
    let major = u16_at(b, 4).ok_or_else(|| bad_len(record_len))?;
    // Any record must at least hold RecordLength + versions, and must fit in the buffer.
    if record_len < 8 || record_len > b.len() {
        return Err(bad_len(record_len));
    }
    let rec = &b[..record_len];
    let header = match major {
        2 => V2_HEADER_LEN,
        3 => V3_HEADER_LEN,
        _ => return Ok((record_len, None)),
    };
    if record_len < header {
        return Err(bad_len(record_len));
    }
    // The header is in bounds from here on, so the unwraps below cannot fail; use `?`-style
    // fallbacks anyway to keep this function provably panic-free.
    let h = |r: Option<u64>| r.ok_or_else(|| bad_len(record_len));
    let (file_ref, parent_ref, base) = if major == 2 {
        (h(u64_at(rec, 8))?, h(u64_at(rec, 16))?, 24)
    } else {
        let wide = |off| -> Result<u64, RecordError> {
            let lo = h(u64_at(rec, off))?;
            let hi = h(u64_at(rec, off + 8))?;
            if hi != 0 {
                return Err(RecordError::WideFileId);
            }
            Ok(lo)
        };
        (wide(8)?, wide(24)?, 40)
    };
    let usn = i64_at(rec, base).ok_or_else(|| bad_len(record_len))?;
    let timestamp = i64_at(rec, base + 8).ok_or_else(|| bad_len(record_len))?;
    let reason = u32_at(rec, base + 16).ok_or_else(|| bad_len(record_len))?;
    let attributes = u32_at(rec, base + 28).ok_or_else(|| bad_len(record_len))?;
    let name_len = u16_at(rec, base + 32).ok_or_else(|| bad_len(record_len))? as usize;
    let name_offset = u16_at(rec, base + 34).ok_or_else(|| bad_len(record_len))? as usize;
    let bad_name = RecordError::BadName {
        name_offset,
        name_len,
        record_len,
    };
    if name_len % 2 != 0 || name_offset < header {
        return Err(bad_name);
    }
    let name_utf16le = rec
        .get(name_offset..name_offset + name_len)
        .ok_or(bad_name)?;
    Ok((
        record_len,
        Some(UsnRecord {
            major_version: major,
            file_ref,
            parent_ref,
            usn,
            timestamp,
            reason,
            attributes,
            name_utf16le,
        }),
    ))
}

/// Test helpers that build well-formed records.
#[cfg(test)]
pub(crate) mod build {
    /// Encodes a `USN_RECORD_V2` (8-byte aligned, like NTFS does).
    pub(crate) fn v2(
        file_ref: u64,
        parent_ref: u64,
        usn: i64,
        reason: u32,
        attributes: u32,
        name: &str,
    ) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (super::V2_HEADER_LEN + name.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&file_ref.to_le_bytes());
        r[16..24].copy_from_slice(&parent_ref.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[32..40].copy_from_slice(&133_000_000_000_000_000i64.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attributes.to_le_bytes());
        r[56..58].copy_from_slice(&(name.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r[60..60 + name.len()].copy_from_slice(&name);
        r
    }

    /// Encodes a `USN_RECORD_V3` with 64-bit refs widened to 128 bits.
    pub(crate) fn v3(file_ref: u64, parent_ref: u64, attributes: u32, name: &str) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (super::V3_HEADER_LEN + name.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..16].copy_from_slice(&file_ref.to_le_bytes());
        r[24..32].copy_from_slice(&parent_ref.to_le_bytes());
        r[68..72].copy_from_slice(&attributes.to_le_bytes());
        r[72..74].copy_from_slice(&(name.len() as u16).to_le_bytes());
        r[74..76].copy_from_slice(&76u16.to_le_bytes());
        r[76..76 + name.len()].copy_from_slice(&name);
        r
    }

    /// Prefixes records with the 8-byte cursor.
    pub(crate) fn output(cursor: u64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = cursor.to_le_bytes().to_vec();
        for r in records {
            out.extend_from_slice(r);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_v2_and_v3() {
        let buf = build::output(
            42,
            &[
                build::v2(
                    0x0001_0000_0000_0010,
                    0x0005_0000_0000_0005,
                    7,
                    0x100,
                    0x20,
                    "a.txt",
                ),
                build::v3(0x0002_0000_0000_0011, 0x0005_0000_0000_0005, 0x10, "دليل"),
            ],
        );
        let (next, recs) = split_output(&buf).unwrap();
        assert_eq!(next, 42);
        let recs: Vec<_> = recs.map(|r| r.unwrap().unwrap()).collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].file_ref, 0x0001_0000_0000_0010);
        assert_eq!(recs[0].parent_ref, 0x0005_0000_0000_0005);
        assert_eq!(recs[0].usn, 7);
        assert_eq!(recs[0].reason, 0x100);
        assert_eq!(recs[0].name(), "a.txt");
        assert!(!recs[0].is_dir());
        assert_eq!(recs[1].major_version, 3);
        assert_eq!(recs[1].name(), "دليل");
        assert!(recs[1].is_dir());
    }

    #[test]
    fn skips_unknown_versions() {
        let mut v4 = vec![0u8; 16];
        v4[0] = 16;
        v4[4] = 4;
        let buf = build::output(0, &[v4, build::v2(1, 5, 0, 0, 0, "x")]);
        let (_, recs) = split_output(&buf).unwrap();
        let recs: Vec<_> = recs.collect();
        assert_eq!(recs[0], Ok(None));
        assert_eq!(recs[1].as_ref().unwrap().unwrap().name(), "x");
    }

    #[test]
    fn rejects_bad_lengths() {
        let mut r = build::v2(1, 5, 0, 0, 0, "abc");
        // RecordLength beyond the buffer.
        r[0..4].copy_from_slice(&1000u32.to_le_bytes());
        let mut it = UsnRecords::new(&r);
        assert!(matches!(
            it.next(),
            Some(Err(RecordError::BadLength { .. }))
        ));
        assert!(it.next().is_none(), "iterator fuses after an error");

        // Zero length must not loop forever.
        let zero = [0u8; 64];
        assert_eq!(UsnRecords::new(&zero).count(), 1);

        // Name span outside the record.
        let mut r = build::v2(1, 5, 0, 0, 0, "abc");
        r[58..60].copy_from_slice(&200u16.to_le_bytes());
        assert!(matches!(parse_one(&r), Err(RecordError::BadName { .. })));

        // Odd name length.
        let mut r = build::v2(1, 5, 0, 0, 0, "abc");
        r[56..58].copy_from_slice(&5u16.to_le_bytes());
        assert!(matches!(parse_one(&r), Err(RecordError::BadName { .. })));

        // Name overlapping the header.
        let mut r = build::v2(1, 5, 0, 0, 0, "abc");
        r[58..60].copy_from_slice(&10u16.to_le_bytes());
        assert!(matches!(parse_one(&r), Err(RecordError::BadName { .. })));

        assert_eq!(
            split_output(&[1, 2, 3]).err(),
            Some(RecordError::MissingCursor)
        );
    }

    #[test]
    fn wide_v3_ids_are_rejected() {
        let mut r = build::v3(1, 5, 0, "x");
        r[16] = 1; // high half of FileReferenceNumber
        assert_eq!(parse_one(&r).err(), Some(RecordError::WideFileId));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn never_panics_on_random_bytes(buf in proptest::collection::vec(any::<u8>(), 0..512)) {
            if let Ok((_, recs)) = split_output(&buf) {
                for r in recs.flatten().flatten() {
                    let _ = r.name();
                }
            }
            let mut total = 0usize;
            for r in UsnRecords::new(&buf) {
                total += 1;
                prop_assert!(total <= buf.len() / 8 + 1, "iterator must make progress");
                let _ = r;
            }
        }

        #[test]
        fn never_panics_on_mutated_records(
            name in "[a-zA-Z0-9 ._\u{600}-\u{6ff}]{0,20}",
            flips in proptest::collection::vec((0usize..128, any::<u8>()), 0..8),
            truncate in 0usize..160,
        ) {
            let mut buf = build::output(9, &[build::v2(17, 5, 3, 1, 0x20, &name), build::v3(18, 17, 0x10, &name)]);
            for (i, v) in flips {
                if let Some(b) = buf.get_mut(i) { *b = v; }
            }
            buf.truncate(buf.len().saturating_sub(truncate));
            if let Ok((_, recs)) = split_output(&buf) {
                for r in recs.flatten().flatten() {
                    let _ = r.name();
                }
            }
        }

        #[test]
        fn roundtrip(file_ref in any::<u64>(), parent in any::<u64>(), attrs in any::<u32>(), name in "\\PC{0,40}") {
            let buf = build::v2(file_ref, parent, 1, 2, attrs, &name);
            let (len, rec) = parse_one(&buf).unwrap();
            let rec = rec.unwrap();
            prop_assert_eq!(len, buf.len());
            prop_assert_eq!(rec.file_ref, file_ref);
            prop_assert_eq!(rec.parent_ref, parent);
            prop_assert_eq!(rec.attributes, attrs);
            prop_assert_eq!(rec.name(), name);
        }
    }
}
