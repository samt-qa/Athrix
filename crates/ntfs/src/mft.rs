//! Minimal raw `$MFT` parser: just enough to recover **size** and **modification time**,
//! which USN records do not carry.
//!
//! Why raw MFT reading: the alternatives are one `FSCTL_GET_NTFS_FILE_RECORD` ioctl per file
//! (millions of syscalls, each returning a 1 KiB record) or one `GetFileInformationByHandle`
//! per file (open + close per file: as slow as the directory walk). Reading the `$MFT` stream
//! sequentially through the already open volume handle is one large sequential read
//! (≈ 1 KiB per file record, e.g. ~1 GiB for 1 M records) and is what Everything-class tools
//! do.
//!
//! What is decoded per FILE record (after update-sequence fixups):
//! * header: in-use and directory flags, sequence number, base record reference;
//! * `$STANDARD_INFORMATION` (0x10): last-write `FILETIME` (what Explorer shows as
//!   "Date modified");
//! * unnamed `$DATA` (0x80): logical size (`value length` when resident, `DataSize` of the
//!   first extent when non-resident). Extension records (base reference ≠ 0) carrying the
//!   first `$DATA` extent are attributed to their base record, so heavily fragmented files
//!   with an `$ATTRIBUTE_LIST` still get their size.
//!
//! Everything here is safe code over untrusted bytes and is fuzzed for panics.

use crate::bytes::{array, i64_at, u8_at, u16_at, u32_at, u64_at};
use crate::record::RecordError;
use crate::tree::{RefLookup, VolumeTree};
use crate::{RECORD_NUMBER_MASK, filetime_to_unix_seconds};

/// NTFS update-sequence stride (fixed, independent of the physical sector size).
pub const FIXUP_STRIDE: usize = 512;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x1;
const FLAG_DIRECTORY: u16 = 0x2;

fn mft_err(msg: &'static str) -> RecordError {
    RecordError::Mft(msg)
}

/// The fields of `NTFS_VOLUME_DATA_BUFFER` (`FSCTL_GET_NTFS_VOLUME_DATA`) needed to read the MFT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtfsVolumeData {
    /// Bytes per sector.
    pub bytes_per_sector: u32,
    /// Bytes per cluster.
    pub bytes_per_cluster: u32,
    /// Bytes per MFT file record (usually 1024, 4096 on 4Kn disks).
    pub bytes_per_file_record: u32,
    /// Valid bytes of the `$MFT` stream.
    pub mft_valid_data_length: u64,
    /// First cluster of the `$MFT`.
    pub mft_start_lcn: u64,
}

impl NtfsVolumeData {
    /// Parses `NTFS_VOLUME_DATA_BUFFER` (96 bytes) and sanity-checks the geometry.
    pub fn parse(b: &[u8]) -> Result<Self, RecordError> {
        let short = || RecordError::BadLength {
            offset: 0,
            length: 96,
            remaining: b.len(),
        };
        let d = Self {
            bytes_per_sector: u32_at(b, 40).ok_or_else(short)?,
            bytes_per_cluster: u32_at(b, 44).ok_or_else(short)?,
            bytes_per_file_record: u32_at(b, 48).ok_or_else(short)?,
            mft_valid_data_length: u64::try_from(i64_at(b, 56).ok_or_else(short)?)
                .map_err(|_| mft_err("negative MFT length"))?,
            mft_start_lcn: u64::try_from(i64_at(b, 64).ok_or_else(short)?)
                .map_err(|_| mft_err("negative MFT start"))?,
        };
        let pow2 = |v: u32, lo: u32, hi: u32| v.is_power_of_two() && (lo..=hi).contains(&v);
        if !pow2(d.bytes_per_sector, 512, 4096)
            || !pow2(d.bytes_per_cluster, 512, 2 << 20)
            || !pow2(d.bytes_per_file_record, 512, 4096)
        {
            return Err(mft_err("implausible volume geometry"));
        }
        Ok(d)
    }
}

/// Applies the update-sequence-array fixups of a FILE (or INDX) record in place.
///
/// Fails on inconsistent array bounds or a torn write (sector tail ≠ update sequence number).
pub fn apply_fixups(rec: &mut [u8]) -> Result<(), RecordError> {
    let usa_offset = u16_at(rec, 4).ok_or_else(|| mft_err("record too short"))? as usize;
    let usa_count = u16_at(rec, 6).ok_or_else(|| mft_err("record too short"))? as usize;
    if usa_count == 0 {
        return Err(mft_err("empty update sequence array"));
    }
    let sectors = usa_count - 1;
    if sectors * FIXUP_STRIDE > rec.len()
        || usa_offset
            .checked_add(usa_count * 2)
            .is_none_or(|end| end > rec.len())
    {
        return Err(mft_err("update sequence array out of bounds"));
    }
    let usn: [u8; 2] = array(rec, usa_offset).ok_or_else(|| mft_err("bad USA"))?;
    for i in 1..=sectors {
        let tail = i * FIXUP_STRIDE - 2;
        let fix: [u8; 2] = array(rec, usa_offset + 2 * i).ok_or_else(|| mft_err("bad USA"))?;
        let slot = rec
            .get_mut(tail..tail + 2)
            .ok_or_else(|| mft_err("bad USA"))?;
        if slot != usn {
            return Err(mft_err("torn write: update sequence mismatch"));
        }
        slot.copy_from_slice(&fix);
    }
    Ok(())
}

/// What one FILE record contributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileRecordInfo {
    /// Sequence number (high 16 bits of the file reference).
    pub sequence: u16,
    /// Base record file reference (0 for a base record, else this is an extension record).
    pub base_ref: u64,
    /// Record is in use.
    pub in_use: bool,
    /// Record is a directory.
    pub is_dir: bool,
    /// Last-write `FILETIME` from `$STANDARD_INFORMATION`.
    pub modified_filetime: Option<i64>,
    /// Logical size of the unnamed `$DATA` stream (only from its first extent).
    pub data_size: Option<u64>,
    /// The record has an `$ATTRIBUTE_LIST` (attributes continue in extension records).
    pub has_attribute_list: bool,
}

/// Parses a fixed-up FILE record. Returns `None` for records that are not `FILE` records
/// (never initialized, zeroed, or `BAAD`).
pub fn parse_file_record(rec: &[u8]) -> Result<Option<FileRecordInfo>, RecordError> {
    if rec.get(0..4) != Some(&b"FILE"[..]) {
        return Ok(None);
    }
    let short = || mft_err("record header truncated");
    let sequence = u16_at(rec, 16).ok_or_else(short)?;
    let first_attr = u16_at(rec, 20).ok_or_else(short)? as usize;
    let flags = u16_at(rec, 22).ok_or_else(short)?;
    let bytes_used = u32_at(rec, 24).ok_or_else(short)? as usize;
    let base_ref = u64_at(rec, 32).ok_or_else(short)?;
    let mut info = FileRecordInfo {
        sequence,
        base_ref,
        in_use: flags & FLAG_IN_USE != 0,
        is_dir: flags & FLAG_DIRECTORY != 0,
        ..FileRecordInfo::default()
    };
    if !info.in_use {
        return Ok(Some(info));
    }
    let limit = bytes_used.min(rec.len());
    let mut off = first_attr;
    while off.checked_add(4).is_some_and(|e| e <= limit) {
        let ty = u32_at(rec, off).ok_or_else(short)?;
        if ty == ATTR_END {
            break;
        }
        let len =
            u32_at(rec, off + 4).ok_or_else(|| mft_err("attribute header truncated"))? as usize;
        if len < 16 || off.checked_add(len).is_none_or(|e| e > limit) {
            return Err(mft_err("bad attribute length"));
        }
        let attr = &rec[off..off + len];
        let non_resident = u8_at(attr, 8).ok_or_else(short)? != 0;
        let name_len = u8_at(attr, 9).ok_or_else(short)?;
        match ty {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                let value = resident_value(attr)?;
                info.modified_filetime = i64_at(value, 8);
            }
            ATTR_ATTRIBUTE_LIST => info.has_attribute_list = true,
            ATTR_DATA if name_len == 0 => {
                if non_resident {
                    let lowest_vcn = u64_at(attr, 16).ok_or_else(|| mft_err("short $DATA"))?;
                    if lowest_vcn == 0 {
                        info.data_size =
                            Some(u64_at(attr, 48).ok_or_else(|| mft_err("short $DATA"))?);
                    }
                } else {
                    info.data_size = Some(resident_value(attr)?.len() as u64);
                }
            }
            _ => {}
        }
        off += len;
    }
    Ok(Some(info))
}

fn resident_value(attr: &[u8]) -> Result<&[u8], RecordError> {
    let bad = || mft_err("bad resident value");
    let len = u32_at(attr, 16).ok_or_else(bad)? as usize;
    let off = u16_at(attr, 20).ok_or_else(bad)? as usize;
    attr.get(off..off.checked_add(len).ok_or_else(bad)?)
        .ok_or_else(bad)
}

/// One run of a non-resident attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// First logical cluster, `None` for a sparse run.
    pub lcn: Option<u64>,
    /// Length in clusters.
    pub clusters: u64,
}

/// Decodes an NTFS mapping-pairs (run list) array.
pub fn parse_runlist(b: &[u8]) -> Result<Vec<DataRun>, RecordError> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut lcn: i64 = 0;
    loop {
        let Some(&header) = b.get(pos) else {
            return Err(mft_err("run list not terminated"));
        };
        if header == 0 {
            return Ok(runs);
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(mft_err("bad run header"));
        }
        pos += 1;
        let len_bytes = b
            .get(pos..pos + len_size)
            .ok_or_else(|| mft_err("run list truncated"))?;
        let clusters = le_uint(len_bytes);
        pos += len_size;
        let run_lcn = if off_size == 0 {
            None
        } else {
            let off_bytes = b
                .get(pos..pos + off_size)
                .ok_or_else(|| mft_err("run list truncated"))?;
            pos += off_size;
            lcn = lcn
                .checked_add(le_int(off_bytes))
                .ok_or_else(|| mft_err("run offset overflow"))?;
            Some(u64::try_from(lcn).map_err(|_| mft_err("negative LCN"))?)
        };
        if clusters == 0 {
            return Err(mft_err("empty run"));
        }
        runs.push(DataRun {
            lcn: run_lcn,
            clusters,
        });
    }
}

fn le_uint(b: &[u8]) -> u64 {
    b.iter()
        .rev()
        .fold(0u64, |acc, &x| (acc << 8) | u64::from(x))
}

fn le_int(b: &[u8]) -> i64 {
    let u = le_uint(b);
    let bits = (b.len() * 8) as u32;
    if bits == 0 || bits >= 64 {
        return u as i64;
    }
    // Sign-extend from `bits`.
    let shift = 64 - bits;
    ((u << shift) as i64) >> shift
}

/// Extracts the run list and stream size of the unnamed `$DATA` attribute of a fixed-up
/// record (used for record 0, the `$MFT` itself). Fails if the runs do not cover the whole
/// stream (they continue in an extension record: extremely fragmented MFT, not supported).
pub fn data_runs(rec: &[u8], bytes_per_cluster: u64) -> Result<(Vec<DataRun>, u64), RecordError> {
    let first_attr = u16_at(rec, 20).ok_or_else(|| mft_err("short record"))? as usize;
    let limit = (u32_at(rec, 24).ok_or_else(|| mft_err("short record"))? as usize).min(rec.len());
    let mut off = first_attr;
    while off.checked_add(4).is_some_and(|e| e <= limit) {
        let ty = u32_at(rec, off).ok_or_else(|| mft_err("short record"))?;
        if ty == ATTR_END {
            break;
        }
        let len = u32_at(rec, off + 4).ok_or_else(|| mft_err("short attribute"))? as usize;
        if len < 16 || off.checked_add(len).is_none_or(|e| e > limit) {
            return Err(mft_err("bad attribute length"));
        }
        let attr = &rec[off..off + len];
        if ty == ATTR_DATA && u8_at(attr, 9) == Some(0) && u8_at(attr, 8) == Some(1) {
            let runs_off = u16_at(attr, 32).ok_or_else(|| mft_err("short $DATA"))? as usize;
            let data_size = u64_at(attr, 48).ok_or_else(|| mft_err("short $DATA"))?;
            let runs = parse_runlist(attr.get(runs_off..).ok_or_else(|| mft_err("bad runs"))?)?;
            let covered = runs
                .iter()
                .try_fold(0u64, |acc, r| acc.checked_add(r.clusters))
                .and_then(|c| c.checked_mul(bytes_per_cluster))
                .ok_or_else(|| mft_err("run length overflow"))?;
            if covered < data_size {
                return Err(mft_err("$MFT runs continue in an extension record"));
            }
            return Ok((runs, data_size));
        }
        off += len;
    }
    Err(mft_err("no non-resident unnamed $DATA attribute"))
}

/// Splits a byte stream of consecutive MFT records (fed in arbitrary chunk sizes) into whole
/// records and merges their metadata into a [`VolumeTree`].
#[derive(Debug)]
pub struct MftScan {
    record_size: usize,
    next_record: u64,
    carry: Vec<u8>,
    lookup: RefLookup,
    /// Records that failed fixups or parsing (torn writes during the scan, corruption).
    pub bad_records: u64,
    /// In-use base records matched to a tree entry.
    pub matched: u64,
}

impl MftScan {
    /// Starts a scan at record 0 for `tree`.
    pub fn new(tree: &VolumeTree, record_size: usize) -> Self {
        Self {
            record_size: record_size.max(1),
            next_record: 0,
            carry: Vec::new(),
            lookup: tree.ref_lookup(),
            bad_records: 0,
            matched: 0,
        }
    }

    /// Number of the next record expected.
    pub fn next_record(&self) -> u64 {
        self.next_record
    }

    /// Skips `bytes` of the stream (sparse run): records in it are treated as absent.
    pub fn skip(&mut self, bytes: u64) {
        let total = self.carry.len() as u64 + bytes;
        self.carry.clear();
        self.next_record += total / self.record_size as u64;
        let rem = (total % self.record_size as u64) as usize;
        self.carry.resize(rem, 0);
    }

    /// Feeds the next bytes of the `$MFT` stream (modified in place by fixups).
    pub fn feed(&mut self, tree: &mut VolumeTree, mut data: &mut [u8]) {
        let rs = self.record_size;
        if !self.carry.is_empty() {
            let need = rs - self.carry.len();
            let take = need.min(data.len());
            let (head, rest) = data.split_at_mut(take);
            self.carry.extend_from_slice(head);
            data = rest;
            if self.carry.len() < rs {
                return;
            }
            let mut rec = std::mem::take(&mut self.carry);
            self.record(tree, &mut rec);
            rec.clear();
            self.carry = rec;
        }
        let mut chunks = data.chunks_exact_mut(rs);
        for rec in &mut chunks {
            self.record(tree, rec);
        }
        self.carry.extend_from_slice(chunks.into_remainder());
    }

    fn record(&mut self, tree: &mut VolumeTree, rec: &mut [u8]) {
        let number = self.next_record;
        self.next_record += 1;
        if rec.get(0..4) != Some(&b"FILE"[..]) {
            return;
        }
        let info = match apply_fixups(rec).and_then(|()| parse_file_record(rec)) {
            Ok(Some(info)) if info.in_use => info,
            Ok(_) => return,
            Err(_) => {
                self.bad_records += 1;
                return;
            }
        };
        if info.base_ref == 0 {
            let file_ref = (u64::from(info.sequence) << 48) | (number & RECORD_NUMBER_MASK);
            if let Some(i) = self.lookup.get(file_ref) {
                self.matched += 1;
                let size = if info.is_dir { Some(0) } else { info.data_size };
                tree.set_metadata(
                    i,
                    size,
                    info.modified_filetime.and_then(filetime_to_unix_seconds),
                );
            }
        } else if let (Some(size), false) = (info.data_size, info.is_dir) {
            // First $DATA extent stored in an extension record: belongs to the base file.
            if let Some(i) = self.lookup.get(info.base_ref) {
                tree.set_metadata(i, Some(size), None);
            }
        }
    }
}

/// Test helpers that build FILE records.
#[cfg(test)]
pub(crate) mod build {
    use super::*;

    pub(crate) struct Rec {
        pub(crate) buf: Vec<u8>,
        pos: usize,
    }

    impl Rec {
        pub(crate) fn new(size: usize, seq: u16, flags: u16, base_ref: u64) -> Self {
            let mut buf = vec![0u8; size];
            buf[0..4].copy_from_slice(b"FILE");
            let usa_count = (size / FIXUP_STRIDE + 1) as u16;
            buf[4..6].copy_from_slice(&48u16.to_le_bytes());
            buf[6..8].copy_from_slice(&usa_count.to_le_bytes());
            buf[16..18].copy_from_slice(&seq.to_le_bytes());
            let first_attr = (48 + 2 * usa_count as usize).next_multiple_of(8);
            buf[20..22].copy_from_slice(&(first_attr as u16).to_le_bytes());
            buf[22..24].copy_from_slice(&flags.to_le_bytes());
            buf[28..32].copy_from_slice(&(size as u32).to_le_bytes());
            buf[32..40].copy_from_slice(&base_ref.to_le_bytes());
            Self {
                buf,
                pos: first_attr,
            }
        }

        fn attr(&mut self, bytes: &[u8]) -> &mut Self {
            self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
            self.pos += bytes.len();
            self
        }

        pub(crate) fn std_info(&mut self, modified: i64) -> &mut Self {
            let mut a = vec![0u8; 24 + 48];
            a[0..4].copy_from_slice(&ATTR_STANDARD_INFORMATION.to_le_bytes());
            let alen = a.len() as u32;
            a[4..8].copy_from_slice(&alen.to_le_bytes());
            a[16..20].copy_from_slice(&48u32.to_le_bytes());
            a[20..22].copy_from_slice(&24u16.to_le_bytes());
            a[24 + 8..24 + 16].copy_from_slice(&modified.to_le_bytes());
            self.attr(&a)
        }

        pub(crate) fn resident_data(&mut self, len: usize) -> &mut Self {
            let mut a = vec![0u8; (24 + len).next_multiple_of(8)];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            let alen = a.len() as u32;
            a[4..8].copy_from_slice(&alen.to_le_bytes());
            a[16..20].copy_from_slice(&(len as u32).to_le_bytes());
            a[20..22].copy_from_slice(&24u16.to_le_bytes());
            self.attr(&a)
        }

        pub(crate) fn nonresident_data(
            &mut self,
            name_len: u8,
            lowest_vcn: u64,
            size: u64,
            runs: &[u8],
        ) -> &mut Self {
            let mut a = vec![0u8; (64 + runs.len()).next_multiple_of(8)];
            a[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            let alen = a.len() as u32;
            a[4..8].copy_from_slice(&alen.to_le_bytes());
            a[8] = 1;
            a[9] = name_len;
            a[16..24].copy_from_slice(&lowest_vcn.to_le_bytes());
            a[32..34].copy_from_slice(&64u16.to_le_bytes());
            a[48..56].copy_from_slice(&size.to_le_bytes());
            a[64..64 + runs.len()].copy_from_slice(runs);
            self.attr(&a)
        }

        pub(crate) fn attribute_list(&mut self) -> &mut Self {
            let mut a = vec![0u8; 24];
            a[0..4].copy_from_slice(&ATTR_ATTRIBUTE_LIST.to_le_bytes());
            a[4..8].copy_from_slice(&24u32.to_le_bytes());
            a[20..22].copy_from_slice(&24u16.to_le_bytes());
            self.attr(&a)
        }

        /// Terminates the attribute list and applies the "on disk" sector protection.
        pub(crate) fn finish(&mut self) -> Vec<u8> {
            let end = ATTR_END.to_le_bytes();
            self.attr(&end);
            let used = (self.pos + 4) as u32;
            self.buf[24..28].copy_from_slice(&used.to_le_bytes());
            let mut b = self.buf.clone();
            let usa_count = u16::from_le_bytes([b[6], b[7]]) as usize;
            let usn = [0x34u8, 0x12];
            b[48..50].copy_from_slice(&usn);
            for i in 1..usa_count {
                let tail = i * FIXUP_STRIDE - 2;
                let orig = [b[tail], b[tail + 1]];
                b[48 + 2 * i..50 + 2 * i].copy_from_slice(&orig);
                b[tail..tail + 2].copy_from_slice(&usn);
            }
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build::Rec;
    use super::*;
    use crate::tree::TreeBuilder;
    use proptest::prelude::*;

    const ROOT: u64 = (5 << 48) | 5;
    const FT_2024: i64 = 116_444_736_000_000_000 + 1_704_067_200 * 10_000_000;

    #[test]
    fn fixups_restore_sector_tails() {
        let mut r = Rec::new(1024, 1, FLAG_IN_USE, 0);
        r.buf[510] = 0xAA;
        r.buf[1022] = 0xBB;
        let mut disk = r.std_info(FT_2024).finish();
        assert_eq!(&disk[510..512], &[0x34, 0x12]);
        apply_fixups(&mut disk).unwrap();
        assert_eq!(disk[510], 0xAA);
        assert_eq!(disk[1022], 0xBB);

        let mut torn = r.finish();
        torn[1022] = 0;
        assert!(apply_fixups(&mut torn).is_err());

        let mut bad = r.finish();
        bad[6..8].copy_from_slice(&100u16.to_le_bytes());
        assert!(apply_fixups(&mut bad).is_err());
    }

    #[test]
    fn parses_resident_file() {
        let mut rec = Rec::new(1024, 3, FLAG_IN_USE, 0)
            .std_info(FT_2024)
            .resident_data(300)
            .finish();
        apply_fixups(&mut rec).unwrap();
        let info = parse_file_record(&rec).unwrap().unwrap();
        assert_eq!(info.sequence, 3);
        assert!(info.in_use && !info.is_dir);
        assert_eq!(info.modified_filetime, Some(FT_2024));
        assert_eq!(info.data_size, Some(300));
        assert!(!info.has_attribute_list);
    }

    #[test]
    fn parses_nonresident_and_ignores_named_streams() {
        let mut rec = Rec::new(1024, 1, FLAG_IN_USE, 0)
            .std_info(FT_2024)
            .nonresident_data(4, 0, 999, &[0x11, 1, 1, 0]) // named stream: ignored
            .nonresident_data(0, 0, 5_000_000, &[0x11, 2, 10, 0])
            .finish();
        apply_fixups(&mut rec).unwrap();
        assert_eq!(
            parse_file_record(&rec).unwrap().unwrap().data_size,
            Some(5_000_000)
        );

        // A later extent (lowest VCN > 0) does not carry the size.
        let mut rec = Rec::new(1024, 1, FLAG_IN_USE, 7)
            .nonresident_data(0, 100, 1, &[0x11, 2, 10, 0])
            .finish();
        apply_fixups(&mut rec).unwrap();
        let info = parse_file_record(&rec).unwrap().unwrap();
        assert_eq!(info.data_size, None);
        assert_eq!(info.base_ref, 7);
    }

    #[test]
    fn not_file_records() {
        assert_eq!(parse_file_record(&[0u8; 1024]), Ok(None));
        let mut baad = vec![0u8; 1024];
        baad[0..4].copy_from_slice(b"BAAD");
        assert_eq!(parse_file_record(&baad), Ok(None));
        let mut free = Rec::new(1024, 1, 0, 0).std_info(FT_2024).finish();
        apply_fixups(&mut free).unwrap();
        assert!(!parse_file_record(&free).unwrap().unwrap().in_use);
    }

    #[test]
    fn runlists() {
        // 0x21: 1-byte length, 2-byte offset. Second run has a negative relative offset.
        let runs = parse_runlist(&[
            0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0x00, 0xFF, 0x01, 0x05, 0x00,
        ])
        .unwrap();
        assert_eq!(
            runs,
            [
                DataRun {
                    lcn: Some(0x100),
                    clusters: 0x10
                },
                DataRun {
                    lcn: Some(0x100 - 0x100),
                    clusters: 8
                },
                DataRun {
                    lcn: None,
                    clusters: 5
                },
            ]
        );
        assert!(parse_runlist(&[0x21, 0x10]).is_err());
        assert!(parse_runlist(&[0x11, 0x10, 0x05]).is_err(), "unterminated");
        assert!(parse_runlist(&[0x19, 0, 0]).is_err(), "9-byte length");
        assert!(
            parse_runlist(&[0x11, 0x01, 0x80, 0x00]).is_err(),
            "negative LCN"
        );
    }

    #[test]
    fn mft_record_zero_runs() {
        let rec = Rec::new(1024, 1, FLAG_IN_USE, 0)
            .std_info(FT_2024)
            .nonresident_data(0, 0, 8 * 4096, &[0x11, 8, 0x40, 0])
            .finish();
        let mut rec = rec;
        apply_fixups(&mut rec).unwrap();
        let (runs, size) = data_runs(&rec, 4096).unwrap();
        assert_eq!(size, 8 * 4096);
        assert_eq!(
            runs,
            [DataRun {
                lcn: Some(0x40),
                clusters: 8
            }]
        );
        // Runs shorter than the stream: continue elsewhere → unsupported.
        assert!(data_runs(&rec, 2048).is_err());
    }

    #[test]
    fn volume_data() {
        let mut b = vec![0u8; 96];
        b[40..44].copy_from_slice(&512u32.to_le_bytes());
        b[44..48].copy_from_slice(&4096u32.to_le_bytes());
        b[48..52].copy_from_slice(&1024u32.to_le_bytes());
        b[56..64].copy_from_slice(&(1i64 << 30).to_le_bytes());
        b[64..72].copy_from_slice(&786_432i64.to_le_bytes());
        let d = NtfsVolumeData::parse(&b).unwrap();
        assert_eq!(d.bytes_per_file_record, 1024);
        assert_eq!(d.mft_start_lcn, 786_432);
        b[44..48].copy_from_slice(&3000u32.to_le_bytes());
        assert!(NtfsVolumeData::parse(&b).is_err());
        assert!(NtfsVolumeData::parse(&b[..50]).is_err());
    }

    fn sample_tree() -> VolumeTree {
        let mut b = TreeBuilder::new();
        b.push((1 << 48) | 2, ROOT, 0x10, "dir"); // record 2 (a dir)
        b.push((1 << 48) | 3, (1 << 48) | 2, 0x20, "small.txt"); // record 3
        b.push((2 << 48) | 4, ROOT, 0x20, "big.bin"); // record 4, seq 2
        b.finish("C:\\")
    }

    fn stream() -> Vec<u8> {
        let rs = 1024;
        let mut s = Vec::new();
        s.extend(vec![0u8; rs]); // record 0: unused/zero
        s.extend(Rec::new(rs, 1, 0, 0).finish()); // record 1: free
        s.extend(
            Rec::new(rs, 1, FLAG_IN_USE | FLAG_DIRECTORY, 0)
                .std_info(FT_2024)
                .finish(),
        );
        s.extend(
            Rec::new(rs, 1, FLAG_IN_USE, 0)
                .std_info(FT_2024 + 10_000_000)
                .resident_data(42)
                .finish(),
        );
        // record 4: base has an attribute list, $DATA lives in extension record 6.
        s.extend(
            Rec::new(rs, 2, FLAG_IN_USE, 0)
                .std_info(FT_2024)
                .attribute_list()
                .finish(),
        );
        let mut torn = Rec::new(rs, 1, FLAG_IN_USE, 0).std_info(FT_2024).finish();
        torn[1022] ^= 0xFF; // record 5: torn write
        s.extend(torn);
        s.extend(
            Rec::new(rs, 1, FLAG_IN_USE, (2 << 48) | 4)
                .nonresident_data(0, 0, 7 << 30, &[0x21, 0x10, 0x00, 0x01, 0])
                .finish(),
        );
        s
    }

    #[test]
    fn scan_merges_metadata_with_any_chunking() {
        for chunk in [1usize, 100, 1000, 1024, 3000, 1 << 20] {
            let mut tree = sample_tree();
            let mut scan = MftScan::new(&tree, 1024);
            let mut s = stream();
            for c in s.chunks_mut(chunk) {
                scan.feed(&mut tree, c);
            }
            tree.mark_metadata();
            assert_eq!(scan.next_record(), 7, "chunk {chunk}");
            assert_eq!(scan.bad_records, 1);
            assert_eq!(scan.matched, 3);
            let by_name = |n: &str| tree.iter().find(|e| e.name == n).unwrap();
            assert_eq!(by_name("dir").size, Some(0));
            assert_eq!(by_name("dir").modified, Some(1_704_067_200));
            assert_eq!(by_name("small.txt").size, Some(42));
            assert_eq!(by_name("small.txt").modified, Some(1_704_067_201));
            assert_eq!(by_name("big.bin").size, Some(7 << 30));
            assert_eq!(by_name("big.bin").modified, Some(1_704_067_200));
        }
    }

    #[test]
    fn scan_skip_and_stale_sequence() {
        let mut tree = sample_tree();
        let mut scan = MftScan::new(&tree, 1024);
        scan.skip(3 * 1024); // records 0..3 sparse
        // Record 3 now has seq 9: a reused record, must not match entry (seq 1).
        let mut r = Rec::new(1024, 9, FLAG_IN_USE, 0).resident_data(5).finish();
        scan.feed(&mut tree, &mut r);
        assert_eq!(scan.matched, 0);
        assert_eq!(scan.next_record(), 4);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        #[test]
        fn record_parser_never_panics(mut buf in proptest::collection::vec(any::<u8>(), 0..1100)) {
            if buf.len() >= 4 { buf[0..4].copy_from_slice(b"FILE"); }
            let _ = parse_file_record(&buf);
            let _ = data_runs(&buf, 4096);
            let _ = apply_fixups(&mut buf);
            let _ = parse_file_record(&buf);
        }

        #[test]
        fn mutated_records_never_panic(
            flips in proptest::collection::vec((0usize..1024, any::<u8>()), 1..12),
        ) {
            let mut rec = Rec::new(1024, 1, FLAG_IN_USE, 0)
                .std_info(FT_2024)
                .nonresident_data(0, 0, 77, &[0x21, 0x10, 0x00, 0x01, 0])
                .finish();
            for (i, v) in flips { rec[i] = v; }
            let _ = apply_fixups(&mut rec);
            let _ = parse_file_record(&rec);
            let _ = data_runs(&rec, 4096);
            let mut tree = sample_tree();
            let mut scan = MftScan::new(&tree, 1024);
            scan.feed(&mut tree, &mut rec);
        }

        #[test]
        fn runlist_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = parse_runlist(&buf);
        }
    }
}
