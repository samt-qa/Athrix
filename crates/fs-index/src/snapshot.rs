//! Saving a [`FileIndex`] to disk and loading it back (index snapshot, task 1.4).
//!
//! At startup the app loads the last snapshot so results are available immediately, then
//! re-walks (or, later, replays the NTFS USN journal) in the background.
//!
//! # Format (version 2)
//!
//! All integers are little-endian. `n` is the entry count, `u` the number of distinct names,
//! `s`/`m`/`c` the lengths of the size, modification-time and creation-time overflow lists
//! (see `index.rs` for the layout these columns mirror).
//!
//! | Part | Size | Content |
//! |---|---|---|
//! | magic | 8 | `ATHRXIDX` |
//! | format version | 4 | `u32`, currently [`FORMAT_VERSION`] |
//! | normalizer version | 4 | `u32`, see [`normalizer_version`] |
//! | counts | 7 × 8 | `u64` each: `n`, `u`, original-names bytes, normalized-names bytes, `s`, `m`, `c` |
//! | name ids | 4n | `u32` per entry, `< u` |
//! | parents | 4n | `u32` per entry, `u32::MAX` for a root |
//! | flags | n | `u8` per entry (bit 0 directory, bit 1 symlink) |
//! | sizes | 4n | `u32` per entry, `u32::MAX` = in the overflow list |
//! | mtimes, ctimes | 4n + 4n | `u32` Unix seconds; `u32::MAX` unknown, `u32::MAX - 1` = in the overflow list |
//! | size overflow | 12s | `(u32 id, u64 size)`, ids increasing |
//! | mtime, ctime overflow | 12m + 12c | `(u32 id, i64 seconds)`, ids increasing |
//! | original name ends | 4u | `u32` end offset of each distinct original name |
//! | original names | bytes | UTF-8 |
//! | normalized name ends | 4u | `u32` end offset of each distinct normalized name |
//! | normalized names | bytes | UTF-8 |
//! | checksum | 4 | CRC-32 (IEEE) of every preceding byte |
//!
//! Version 1 (two per-entry name arenas, `u64` sizes, `i64` modification times and no creation
//! times) is rejected like any other version, so the app re-walks once after the upgrade.
//!
//! A snapshot whose format or normalizer version differs from this build is rejected, so the
//! caller simply re-walks. Loading validates the whole structure (lengths, offsets, UTF-8
//! boundaries, parent links, flags, checksum) and returns an error rather than panicking on
//! corrupt or hostile input.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use athrix_core::{Error, Result};

use crate::index::{Arena, FLAG_DIR, FLAG_SYMLINK, FileIndex, NO_PARENT, Names, Overflow};

const MAGIC: [u8; 8] = *b"ATHRXIDX";

/// Version of the on-disk layout. Bump on any change to the format above.
///
/// - 2: distinct names stored once, packed `u32` sizes and times, creation times.
const FORMAT_VERSION: u32 = 2;

/// Manual epoch mixed into [`normalizer_version`]. Bump it when the name normalization
/// (`athrix_text_ar::normalize_name_into`) changes, even if the probe strings below catch it.
///
/// - 2: names fold ة → ه, ؤ → و, ئ → ي (decision 7). Snapshots of epoch 1 keep those letters
///   in their normalized names, so they are rejected and the index is rebuilt.
const NORMALIZER_EPOCH: u32 = 2;

/// Names covering the normalization rules (diacritics, alef/yaa/taa marbuta/kaf variants,
/// tatweel, ligatures, digits, invisible marks, case folding, separators). Their normalized
/// forms are hashed into [`normalizer_version`], so most behavior changes in the normalizer
/// invalidate old snapshots automatically.
const NORMALIZER_PROBES: &[&str] = &[
    "مُحَاضَرَةُ.PDF",
    "إأآٱا",
    "ى ي ئ ؤ ء",
    "مدرسة مدرسه",
    "فاتورة مسؤول مسئولية",
    "ك ک گ",
    "مـــلف",
    "ﻻ لا",
    "٠١٢٣٤٥٦٧٨٩ ۰۱۲ 0123",
    "\u{200F}a\u{200C}b\u{200D}c\u{FEFF}",
    "Report_2026-final (3).tar.gz",
    "ÉCOLE Straße İstanbul",
    "الفِقْه وَالتَّفْسِير",
];

const HEADER_LEN: u64 = 8 + 4 + 4 + 7 * 8;
const CHECKSUM_LEN: u64 = 4;
/// Fixed bytes per entry: name id 4 + parent 4 + flags 1 + size 4 + mtime 4 + ctime 4.
const BYTES_PER_ENTRY: u64 = 4 + 4 + 1 + 4 + 4 + 4;
/// Bytes per distinct name: two end offsets.
const BYTES_PER_NAME: u64 = 4 + 4;
/// Bytes per overflow item: `u32` id + 8-byte value.
const BYTES_PER_OVERFLOW: u64 = 4 + 8;
/// Entries converted per I/O chunk.
const CHUNK: usize = 16 * 1024;
const IO_BUFFER: usize = 1 << 20;

/// Fingerprint of the name normalizer this build uses, stored in every snapshot.
///
/// It is a CRC-32 over [`NORMALIZER_EPOCH`] and the normalized forms of fixed probe names, so
/// a snapshot written by a build that normalizes names differently is rejected on load.
fn normalizer_version() -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&NORMALIZER_EPOCH.to_le_bytes());
    let mut buf = String::new();
    for probe in NORMALIZER_PROBES {
        athrix_text_ar::normalize_name_into(probe, &mut buf);
        h.update(buf.as_bytes());
        h.update(&[0]);
    }
    h.finalize()
}

impl FileIndex {
    /// Saves the index to `path` atomically: the data is written and flushed to `path.tmp`,
    /// which is then renamed over `path`, so a crash never leaves a partial snapshot.
    ///
    /// # Errors
    /// [`Error::Io`] if the file cannot be written or renamed (the temporary file is removed).
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = tmp_path(path);
        let result = write_snapshot(self, &tmp)
            .map_err(|e| Error::io(&tmp, e))
            .and_then(|()| fs::rename(&tmp, path).map_err(|e| Error::io(path, e)));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Loads an index saved by [`FileIndex::save`].
    ///
    /// # Errors
    /// [`Error::Io`] if the file cannot be read, and [`Error::Io`] with kind
    /// [`io::ErrorKind::InvalidData`] if it is not a snapshot, was written by an incompatible
    /// format or normalizer version, or is corrupt. In every such case the caller should
    /// discard it and rebuild the index.
    pub fn load(path: &Path) -> Result<FileIndex> {
        read_snapshot(path).map_err(|e| Error::io(path, e))
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid index snapshot: {}", message.into()),
    )
}

/// A buffered writer that checksums everything written through it.
struct HashWriter {
    inner: BufWriter<File>,
    hasher: crc32fast::Hasher,
    buf: Vec<u8>,
}

impl HashWriter {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.hasher.update(bytes);
        self.inner.write_all(bytes)
    }

    /// Writes a column chunk by chunk, converting each value with `to_le`.
    fn put_col<T: Copy, const N: usize>(
        &mut self,
        col: &[T],
        to_le: impl Fn(T) -> [u8; N],
    ) -> io::Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        for chunk in col.chunks(CHUNK) {
            buf.clear();
            for &x in chunk {
                buf.extend_from_slice(&to_le(x));
            }
            self.put(&buf)?;
        }
        self.buf = buf;
        Ok(())
    }
}

/// An overflow item as written: `u32` id, then the 8-byte value.
fn pair(id: u32, value: [u8; 8]) -> [u8; 12] {
    let mut out = [0; 12];
    out[..4].copy_from_slice(&id.to_le_bytes());
    out[4..].copy_from_slice(&value);
    out
}

/// An overflow item as read: `u32` id and the 8-byte value.
fn split(b: [u8; 12]) -> (u32, [u8; 8]) {
    let mut v = [0; 8];
    v.copy_from_slice(&b[4..]);
    (u32::from_le_bytes([b[0], b[1], b[2], b[3]]), v)
}

/// True if `ids` increase strictly and stay below `n`.
fn ids_ok(mut ids: impl Iterator<Item = u32>, n: usize) -> bool {
    let mut prev: Option<u32> = None;
    ids.all(|id| {
        let ok = (id as usize) < n && prev.is_none_or(|p| p < id);
        prev = Some(id);
        ok
    })
}

fn write_snapshot(ix: &FileIndex, path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = HashWriter {
        inner: BufWriter::with_capacity(IO_BUFFER, file),
        hasher: crc32fast::Hasher::new(),
        buf: Vec::with_capacity(CHUNK * 8),
    };
    w.put(&MAGIC)?;
    w.put(&FORMAT_VERSION.to_le_bytes())?;
    w.put(&normalizer_version().to_le_bytes())?;
    let o = &ix.overflow;
    for count in [
        ix.len(),
        ix.names.len(),
        ix.names.orig.text.len(),
        ix.names.norm.text.len(),
        o.sizes.len(),
        o.mtimes.len(),
        o.ctimes.len(),
    ] {
        w.put(&(count as u64).to_le_bytes())?;
    }
    w.put_col(&ix.name_ids, u32::to_le_bytes)?;
    w.put_col(&ix.parents, u32::to_le_bytes)?;
    w.put(&ix.flags)?;
    w.put_col(&ix.sizes, u32::to_le_bytes)?;
    w.put_col(&ix.mtimes, u32::to_le_bytes)?;
    w.put_col(&ix.ctimes, u32::to_le_bytes)?;
    w.put_col(&o.sizes, |(id, v)| pair(id, v.to_le_bytes()))?;
    w.put_col(&o.mtimes, |(id, v)| pair(id, v.to_le_bytes()))?;
    w.put_col(&o.ctimes, |(id, v)| pair(id, v.to_le_bytes()))?;
    for arena in [&ix.names.orig, &ix.names.norm] {
        w.put_col(&arena.ends, u32::to_le_bytes)?;
        w.put(arena.text.as_bytes())?;
    }
    let checksum = w.hasher.clone().finalize();
    w.inner.write_all(&checksum.to_le_bytes())?;
    let file = w
        .inner
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()
}

/// A buffered reader that checksums everything read through it.
struct HashReader {
    inner: BufReader<File>,
    hasher: crc32fast::Hasher,
}

impl HashReader {
    fn take(&mut self, out: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(out)?;
        self.hasher.update(out);
        Ok(())
    }

    fn u32(&mut self) -> io::Result<u32> {
        let mut b = [0; 4];
        self.take(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> io::Result<u64> {
        let mut b = [0; 8];
        self.take(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Reads `len` raw bytes. `len` was checked against the file size by the caller.
    fn bytes(&mut self, len: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0; len];
        self.take(&mut v)?;
        Ok(v)
    }

    /// Reads `n` values of `N` bytes each, converting chunk by chunk with `from_le`.
    fn col<T, const N: usize>(
        &mut self,
        n: usize,
        from_le: impl Fn([u8; N]) -> T,
    ) -> io::Result<Vec<T>> {
        let mut out = Vec::with_capacity(n);
        let mut buf = vec![0; CHUNK.min(n) * N];
        let mut left = n;
        while left > 0 {
            let k = left.min(CHUNK);
            let bytes = &mut buf[..k * N];
            self.take(bytes)?;
            out.extend(bytes.chunks_exact(N).map(|c| {
                let mut a = [0; N];
                a.copy_from_slice(c);
                from_le(a)
            }));
            left -= k;
        }
        Ok(out)
    }
}

fn read_snapshot(path: &Path) -> io::Result<FileIndex> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < HEADER_LEN + CHECKSUM_LEN {
        return Err(invalid(format!("file too short ({file_len} bytes)")));
    }
    let mut r = HashReader {
        inner: BufReader::with_capacity(IO_BUFFER, file),
        hasher: crc32fast::Hasher::new(),
    };

    let mut magic = [0; 8];
    r.take(&mut magic)?;
    if magic != MAGIC {
        return Err(invalid("not an Athrix index snapshot (bad magic)"));
    }
    let format = r.u32()?;
    if format != FORMAT_VERSION {
        return Err(invalid(format!(
            "format version {format}, this build reads {FORMAT_VERSION}"
        )));
    }
    let normalizer = r.u32()?;
    if normalizer != normalizer_version() {
        return Err(invalid(
            "written with a different name normalizer; rebuild the index",
        ));
    }
    let mut counts = [0u64; 7];
    for c in &mut counts {
        *c = r.u64()?;
    }
    let [
        count,
        distinct,
        names_len,
        normalized_len,
        big_sizes,
        big_mtimes,
        big_ctimes,
    ] = counts;
    if count >= u64::from(NO_PARENT) || distinct >= u64::from(u32::MAX) {
        return Err(invalid(format!(
            "entry count {count} or name count {distinct} out of range"
        )));
    }
    for len in [names_len, normalized_len] {
        if len > u64::from(u32::MAX) {
            return Err(invalid(format!("name arena of {len} bytes exceeds 4 GiB")));
        }
    }
    for len in [big_sizes, big_mtimes, big_ctimes] {
        if len > count {
            return Err(invalid(format!(
                "overflow list of {len} items for {count} entries"
            )));
        }
    }
    // Checked against the real size before allocating anything, so a forged header cannot
    // trigger a huge allocation. All terms are bounded above, so this cannot overflow.
    let expected = HEADER_LEN
        + count * BYTES_PER_ENTRY
        + (big_sizes + big_mtimes + big_ctimes) * BYTES_PER_OVERFLOW
        + distinct * BYTES_PER_NAME
        + names_len
        + normalized_len
        + CHECKSUM_LEN;
    if expected != file_len {
        return Err(invalid(format!(
            "file is {file_len} bytes, header implies {expected}"
        )));
    }
    let n = count as usize;
    let u = distinct as usize;

    let name_ids = r.col(n, u32::from_le_bytes)?;
    let parents = r.col(n, u32::from_le_bytes)?;
    let flags = r.bytes(n)?;
    let sizes = r.col(n, u32::from_le_bytes)?;
    let mtimes = r.col(n, u32::from_le_bytes)?;
    let ctimes = r.col(n, u32::from_le_bytes)?;
    let overflow = Overflow {
        sizes: r.col(big_sizes as usize, |b| {
            let (id, v) = split(b);
            (id, u64::from_le_bytes(v))
        })?,
        mtimes: r.col(big_mtimes as usize, |b| {
            let (id, v) = split(b);
            (id, i64::from_le_bytes(v))
        })?,
        ctimes: r.col(big_ctimes as usize, |b| {
            let (id, v) = split(b);
            (id, i64::from_le_bytes(v))
        })?,
    };
    let orig_ends = r.col(u, u32::from_le_bytes)?;
    let orig = r.bytes(names_len as usize)?;
    let norm_ends = r.col(u, u32::from_le_bytes)?;
    let norm = r.bytes(normalized_len as usize)?;

    let computed = r.hasher.clone().finalize();
    let mut stored = [0; 4];
    r.inner.read_exact(&mut stored)?;
    if u32::from_le_bytes(stored) != computed {
        return Err(invalid("checksum mismatch (file is corrupt)"));
    }

    // The checksum only catches accidental damage; validate every invariant the index
    // accessors rely on so that no later call can panic.
    for (i, &p) in parents.iter().enumerate() {
        // Parents always precede their children (as `IndexBuilder` enforces), which also
        // rules out cycles in `FileIndex::path`.
        if p != NO_PARENT && p as usize >= i {
            return Err(invalid(format!("entry {i} has invalid parent {p}")));
        }
    }
    if let Some(i) = name_ids.iter().position(|&nid| nid as usize >= u) {
        return Err(invalid(format!(
            "entry {i} has invalid name id {}",
            name_ids[i]
        )));
    }
    if let Some(i) = flags
        .iter()
        .position(|&f| f & !(FLAG_DIR | FLAG_SYMLINK) != 0)
    {
        return Err(invalid(format!(
            "entry {i} has unknown flags {:#x}",
            flags[i]
        )));
    }
    if !ids_ok(overflow.sizes.iter().map(|x| x.0), n)
        || !ids_ok(overflow.mtimes.iter().map(|x| x.0), n)
        || !ids_ok(overflow.ctimes.iter().map(|x| x.0), n)
    {
        return Err(invalid("overflow list ids out of order or range"));
    }
    let names = Names {
        orig: arena(orig, orig_ends, "names")?,
        norm: arena(norm, norm_ends, "normalized names")?,
    };

    Ok(FileIndex {
        names,
        name_ids,
        parents,
        flags,
        sizes,
        mtimes,
        ctimes,
        overflow,
        memos: Default::default(),
    })
}

/// Validates and builds a name arena: UTF-8 text, end offsets non-decreasing, on character
/// boundaries, and the last one equal to the text length.
fn arena(text: Vec<u8>, ends: Vec<u32>, what: &str) -> io::Result<Arena> {
    let text = String::from_utf8(text).map_err(|_| invalid(format!("{what} are not UTF-8")))?;
    let mut prev = 0u32;
    for (i, &end) in ends.iter().enumerate() {
        if end < prev || !text.is_char_boundary(end as usize) {
            return Err(invalid(format!(
                "{what}: bad end offset {end} for entry {i}"
            )));
        }
        prev = end;
    }
    if prev as usize != text.len() {
        return Err(invalid(format!(
            "{what}: offsets end at {prev}, text is {} bytes",
            text.len()
        )));
    }
    Ok(Arena { text, ends })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryMeta, IndexBuilder};
    use proptest::prelude::*;

    fn sample() -> FileIndex {
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let mut b = IndexBuilder::new();
        let root = b.push(None, "D:\\Data", dir);
        let docs = b.push(Some(root), "مُحَاضَرَات", dir);
        b.push(
            Some(docs),
            "تَقْرِير_2026.PDF",
            EntryMeta {
                size: 42,
                modified: Some(1_700_000_000),
                ..Default::default()
            },
        );
        b.push(
            Some(root),
            "link",
            EntryMeta {
                is_symlink: true,
                modified: Some(-5),
                created: Some(1_600_000_000),
                ..Default::default()
            },
        );
        b.push(None, "/other root", dir);
        b.push(Some(4), "", EntryMeta::default());
        b.push(
            Some(4),
            "link",
            EntryMeta {
                size: 1 << 40,
                created: Some(i64::from(u32::MAX) + 7),
                ..Default::default()
            },
        );
        b.finish()
    }

    fn assert_same(a: &FileIndex, b: &FileIndex) {
        assert_eq!(a.names.orig.text, b.names.orig.text);
        assert_eq!(a.names.orig.ends, b.names.orig.ends);
        assert_eq!(a.names.norm.text, b.names.norm.text);
        assert_eq!(a.names.norm.ends, b.names.norm.ends);
        assert_eq!(a.name_ids, b.name_ids);
        assert_eq!(a.parents, b.parents);
        assert_eq!(a.flags, b.flags);
        assert_eq!(a.sizes, b.sizes);
        assert_eq!(a.mtimes, b.mtimes);
        assert_eq!(a.ctimes, b.ctimes);
        assert_eq!(a.overflow, b.overflow);
        for id in 0..a.len() as u32 {
            assert_eq!(a.meta(id), b.meta(id));
        }
    }

    fn sample_bytes() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ix.bin");
        sample().save(&path).unwrap();
        fs::read(&path).unwrap()
    }

    /// Writes `bytes` to a temporary file and loads it.
    fn load_bytes(bytes: &[u8]) -> Result<FileIndex> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ix.bin");
        fs::write(&path, bytes).unwrap();
        FileIndex::load(&path)
    }

    fn fix_checksum(bytes: &mut [u8]) {
        let body = bytes.len() - 4;
        let crc = crc32fast::hash(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
    }

    fn load_err(bytes: &[u8]) -> String {
        match load_bytes(bytes) {
            Ok(_) => panic!("corrupt snapshot was accepted"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn round_trips_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.athrix");
        let ix = sample();
        FileIndex::default().save(&path).unwrap();
        ix.save(&path).unwrap();
        assert!(!tmp_path(&path).exists());
        let loaded = FileIndex::load(&path).unwrap();
        assert_same(&ix, &loaded);
        assert_eq!(loaded.path(2), ix.path(2));
        assert_eq!(loaded.meta(3), ix.meta(3));
        assert_eq!(loaded.meta(6).size, 1 << 40);
        assert_eq!(loaded.meta(6).created, Some(i64::from(u32::MAX) + 7));
        assert_eq!(loaded.name(6), "link");
        assert_eq!(loaded.normalized_name(2), ix.normalized_name(2));
    }

    #[test]
    fn round_trips_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        FileIndex::default().save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            HEADER_LEN + CHECKSUM_LEN
        );
        assert!(FileIndex::load(&path).unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = FileIndex::load(&dir.path().join("nope")).unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn save_failure_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("ix");
        let err = sample().save(&path).unwrap_err();
        assert!(err.to_string().contains("missing-dir"), "{err}");
    }

    #[test]
    fn rejects_bad_header_and_checksum() {
        let good = sample_bytes();
        assert!(load_bytes(&good).is_ok());

        let mut b = good.clone();
        b[0] = b'X';
        assert!(load_err(&b).contains("bad magic"));

        let mut b = good.clone();
        b[8] = 1;
        assert!(load_err(&b).contains("format version 1"));

        let mut b = good.clone();
        b[12] ^= 1;
        assert!(load_err(&b).contains("normalizer"));

        let mut b = good.clone();
        b[HEADER_LEN as usize + 1] ^= 0x40;
        assert!(load_err(&b).contains("checksum"));

        assert!(load_err(&good[..good.len() - 1]).contains("header implies"));
        assert!(load_err(&good[..10]).contains("too short"));
        assert!(load_err(b"").contains("too short"));
    }

    #[test]
    fn rejects_structural_damage_even_with_a_valid_checksum() {
        let good = sample_bytes();
        let n = sample().len();
        let h = HEADER_LEN as usize;

        // Entry 1 pointing at itself.
        let parents = h + 4 * n;
        let mut b = good.clone();
        b[parents + 4..parents + 8].copy_from_slice(&1u32.to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("invalid parent"));

        // A name id past the distinct names.
        let mut b = good.clone();
        b[h + 4..h + 8].copy_from_slice(&999u32.to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("invalid name id"));

        let mut b = good.clone();
        b[h + 8 * n] = 0x80;
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("unknown flags"));

        // The size overflow item's id moved out of range.
        let overflow = h + n * BYTES_PER_ENTRY as usize;
        let mut b = good.clone();
        b[overflow..overflow + 4].copy_from_slice(&(n as u32).to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("overflow list"));

        // First name end moved inside a multi-byte character / past the next one.
        let ends = overflow + 3 * BYTES_PER_OVERFLOW as usize;
        let mut b = good.clone();
        b[ends + 4..ends + 8].copy_from_slice(&10u32.to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("bad end offset"));

        let mut b = good.clone();
        b[ends..ends + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("bad end offset"));

        // Entry count inflated: caught by the size check before any allocation.
        let mut b = good.clone();
        b[16..24].copy_from_slice(&(u64::from(u32::MAX) - 1).to_le_bytes());
        fix_checksum(&mut b);
        assert!(load_err(&b).contains("header implies"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Random byte damage, with or without a recomputed checksum, never panics; the
        /// result is either an error or an index whose accessors all work.
        #[test]
        fn never_panics_on_corruption(
            edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
            cut in any::<prop::sample::Index>(),
            truncate in any::<bool>(),
            fix in any::<bool>(),
        ) {
            let mut b = sample_bytes();
            for (at, byte) in edits {
                let i = at.index(b.len());
                b[i] = byte;
            }
            if truncate {
                b.truncate(cut.index(b.len()));
            }
            if fix && b.len() >= 4 {
                fix_checksum(&mut b);
            }
            if let Ok(ix) = load_bytes(&b) {
                for id in 0..ix.len() as u32 {
                    let _ = (ix.path(id), ix.meta(id), ix.normalized_name(id));
                }
            }
        }
    }
}
