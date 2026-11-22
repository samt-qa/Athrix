//! The app's name index, read from its snapshot (`<app_data>/index/names.snap`).
//!
//! The desktop app saves the snapshot from time to time (atomically: a temporary file, then a
//! rename), so what the server sees can be a few minutes behind the disk; every result is
//! checked against the disk anyway ([`crate::paths::Guard::check`]), so deleted files drop
//! out. The snapshot is loaded on first use and loaded again when its file or the allowed
//! folders change.
//!
//! **Only the allowed part is kept.** The snapshot holds every indexed file (hundreds of MB for
//! a whole drive) while the assistant may only see a few folders, so it is read as a stream
//! ([`load_allowed`]): one pass over the entries, in file order (parents always come before
//! their children), deciding each entry from its parent and, on the way down to an allowed
//! folder, from its path. Entries outside are skipped without being stored; the allowed
//! folders, their contents and the folders leading to them are copied into a small
//! [`FileIndex`]. Memory while loading is about 5 bytes per snapshot entry plus the kept part,
//! instead of the whole snapshot. A snapshot in a format this reader does not know is loaded
//! whole by [`FileIndex::load`] and cut down the same way.
//!
//! **Released when idle.** [`Names::release_if_idle`] drops the loaded part after a quiet
//! period (the server calls it from a timer thread, see [`crate::server::ServerOptions`]); the
//! next search loads it again.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use athrix_fs_index::{EntryId, EntryMeta, FileIndex, IndexBuilder, Query};

use crate::paths::{Guard, key};

/// Folder of the snapshot inside the app data folder.
pub const SNAPSHOT_DIR: &str = "index";
/// File name of the snapshot.
pub const SNAPSHOT_FILE: &str = "names.snap";

/// The allowed part of the loaded snapshot.
#[derive(Debug, Default)]
pub struct Names {
    loaded: Option<Loaded>,
    last_used: Option<Instant>,
}

#[derive(Debug)]
struct Loaded {
    /// Modification time and length of the snapshot file it was read from.
    stamp: (Option<SystemTime>, u64),
    /// Keys of the allowed folders it was cut for.
    folders: Vec<String>,
    /// The kept entries: allowed folders, everything under them, and their ancestors.
    index: FileIndex,
    /// `mask[id]`: entry `id` of `index` is an allowed folder or under one (not an ancestor).
    mask: Vec<bool>,
}

/// Where an entry stands relative to the allowed folders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// Neither under an allowed folder nor on the way to one.
    Outside,
    /// On the way down to an allowed folder (its children need a look).
    Ancestor,
    /// An allowed folder or under one.
    Inside,
}

impl Names {
    /// The allowed part of the snapshot in `data_dir`, (re)loaded if its file or the allowed
    /// folders changed, with its mask (`true`: under an allowed folder). Errors are English
    /// messages for the tool result.
    pub fn get(&mut self, data_dir: &Path, guard: &Guard) -> Result<(&FileIndex, &[bool]), String> {
        let file = data_dir.join(SNAPSHOT_DIR).join(SNAPSHOT_FILE);
        let meta = std::fs::metadata(&file).map_err(|_| {
            format!(
                "Athrix has not saved a name index yet ({} is missing): open Athrix and let it \
                 index once",
                file.display()
            )
        })?;
        let stamp = (meta.modified().ok(), meta.len());
        let folders: Vec<String> = guard.folders().map(|p| key(&p.to_string_lossy())).collect();
        if self
            .loaded
            .as_ref()
            .is_none_or(|l| l.stamp != stamp || l.folders != folders)
        {
            // Free the old part before reading the new one.
            self.loaded = None;
            let (index, mask) = load_allowed(&file, guard)
                .map_err(|e| format!("cannot read the name index {}: {e}", file.display()))?;
            self.loaded = Some(Loaded {
                stamp,
                folders,
                index,
                mask,
            });
        }
        // Idle time counts from the end of a (possibly long) load.
        self.touch();
        let loaded = self.loaded.as_ref().expect("loaded above");
        Ok((&loaded.index, &loaded.mask))
    }

    /// Marks the loaded part as just used (the idle period starts again).
    pub fn touch(&mut self) {
        self.last_used = Some(Instant::now());
    }

    /// Whether a snapshot part is in memory.
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    /// Approximate heap bytes held (0 when released).
    pub fn heap_bytes(&self) -> usize {
        self.loaded
            .as_ref()
            .map_or(0, |l| l.index.heap_bytes() + l.mask.capacity())
    }

    /// Drops the loaded part; the next [`Names::get`] reads the snapshot again.
    pub fn release(&mut self) {
        self.loaded = None;
    }

    /// Drops the loaded part if [`Names::get`] was not called for `idle`. Returns whether
    /// something was released.
    pub fn release_if_idle(&mut self, idle: Duration) -> bool {
        let quiet = self.last_used.is_none_or(|t| t.elapsed() >= idle);
        if quiet && self.loaded.is_some() {
            self.loaded = None;
            true
        } else {
            false
        }
    }
}

fn classify(path: &Path, guard: &Guard) -> Place {
    let k = key(&path.to_string_lossy());
    if guard.admits_key(&k) {
        Place::Inside
    } else if guard.is_ancestor_key(&k) {
        Place::Ancestor
    } else {
        Place::Outside
    }
}

/// Builds the kept index while entries arrive in snapshot order (parents first).
struct Cutter<'g> {
    guard: &'g Guard,
    builder: IndexBuilder,
    mask: Vec<bool>,
    /// New id of each snapshot entry seen so far, [`DROPPED`] when it is outside.
    remap: Vec<u32>,
    /// Full paths of the kept ancestors (few), by new id.
    ancestor_paths: HashMap<u32, PathBuf>,
}

const DROPPED: u32 = u32::MAX;

impl<'g> Cutter<'g> {
    fn new(guard: &'g Guard, entries: usize) -> Cutter<'g> {
        Cutter {
            guard,
            builder: IndexBuilder::new(),
            mask: Vec::new(),
            remap: Vec::with_capacity(entries),
            ancestor_paths: HashMap::new(),
        }
    }

    /// What the next entry, whose parent is `parent` (an earlier snapshot id), needs:
    /// `None` to skip it, `Some(place)` to read its name and call [`Cutter::push`]. `Some` with
    /// [`Place::Ancestor`] means "decide from the path".
    fn need(&mut self, parent: Option<u32>) -> Option<Place> {
        let Some(p) = parent else {
            return Some(Place::Ancestor);
        };
        match self.remap[p as usize] {
            DROPPED => {
                self.remap.push(DROPPED);
                None
            }
            new if self.mask[new as usize] => Some(Place::Inside),
            _ => Some(Place::Ancestor),
        }
    }

    /// Adds the next entry (after [`Cutter::need`] returned `Some(place)`).
    fn push(&mut self, parent: Option<u32>, place: Place, name: &str, meta: EntryMeta) {
        let new_parent = parent.map(|p| self.remap[p as usize]);
        let place = match place {
            Place::Inside => Place::Inside,
            _ => {
                let path = match new_parent.and_then(|p| self.ancestor_paths.get(&p)) {
                    Some(base) => base.join(name),
                    None => PathBuf::from(name),
                };
                let place = classify(&path, self.guard);
                if place == Place::Ancestor {
                    let id = u32::try_from(self.builder.len()).unwrap_or(DROPPED);
                    self.ancestor_paths.insert(id, path);
                }
                place
            }
        };
        if place == Place::Outside {
            self.remap.push(DROPPED);
            return;
        }
        let id = self.builder.push(new_parent, name, meta);
        self.mask.push(place == Place::Inside);
        self.remap.push(id);
    }

    fn finish(self) -> (FileIndex, Vec<bool>) {
        let mut mask = self.mask;
        mask.shrink_to_fit();
        (self.builder.finish(), mask)
    }
}

/// The allowed part of the snapshot `file` for `guard`: the allowed folders, everything under
/// them and the folders leading to them, with the mask of the entries under an allowed folder.
/// Streams the snapshot (see the [module docs](self)); falls back to loading it whole when its
/// format is unknown to this reader.
///
/// # Errors
/// When the file cannot be read or is not a valid snapshot.
pub fn load_allowed(file: &Path, guard: &Guard) -> io::Result<(FileIndex, Vec<bool>)> {
    if let Some(cut) = stream::read(file, guard)? {
        return Ok(cut);
    }
    let full = FileIndex::load(file).map_err(io::Error::other)?;
    Ok(cut_loaded(&full, guard))
}

/// [`load_allowed`] over an index already in memory.
fn cut_loaded(full: &FileIndex, guard: &Guard) -> (FileIndex, Vec<bool>) {
    let mut cut = Cutter::new(guard, full.len());
    for id in 0..full.len() as EntryId {
        let parent = full.parent(id);
        if let Some(place) = cut.need(parent) {
            cut.push(parent, place, full.name(id), full.meta(id));
        }
    }
    cut.finish()
}

/// A streaming reader of the snapshot format written by `FileIndex::save` (`athrix-fs-index`,
/// `snapshot.rs`, format version 1): header, then the columns parents, flags, sizes, mtimes,
/// name ends, names, normalized ends, normalized names, CRC-32. Each needed column gets its own
/// buffered reader, so the entries are read in one pass without holding any column.
///
/// Every structural invariant the cut index relies on is checked (lengths, parent order,
/// flags, offsets, UTF-8 of every name read). Unlike the full loader it does not verify the
/// checksum or the normalizer version: names are normalized again as they are copied, and
/// every result is checked against the disk before it is returned.
mod stream {
    use super::*;
    use std::io::{Seek, SeekFrom};

    const MAGIC: [u8; 8] = *b"ATHRXIDX";
    /// The only format version this reader knows.
    const FORMAT_VERSION: u32 = 1;
    const HEADER_LEN: u64 = 8 + 4 + 4 + 8 + 8 + 8;
    const CHECKSUM_LEN: u64 = 4;
    const BYTES_PER_ENTRY: u64 = 4 + 1 + 8 + 8 + 4 + 4;
    const NO_PARENT: u32 = u32::MAX;
    const KNOWN_FLAGS: u8 = 0b11;
    const FLAG_DIR: u8 = 1;
    const FLAG_SYMLINK: u8 = 2;
    const BUFFER: usize = 64 * 1024;

    fn invalid(message: impl Into<String>) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid index snapshot: {}", message.into()),
        )
    }

    fn section(file: &Path, offset: u64) -> io::Result<BufReader<File>> {
        let mut f = File::open(file)?;
        f.seek(SeekFrom::Start(offset))?;
        Ok(BufReader::with_capacity(BUFFER, f))
    }

    fn bytes<const N: usize>(r: &mut impl Read) -> io::Result<[u8; N]> {
        let mut b = [0; N];
        r.read_exact(&mut b)?;
        Ok(b)
    }

    /// Most roots looked at by [`all_roots_allowed`]; a snapshot with more is cut normally.
    const MAX_ROOTS: usize = 1024;

    /// Whether every root of the snapshot (an entry without a parent, named by its full path)
    /// is an allowed folder or lies under one, so the whole snapshot is kept. Reads the parent
    /// and name-end columns, then the names of the roots only.
    fn all_roots_allowed(
        file: &Path,
        count: u64,
        names_len: u64,
        guard: &Guard,
    ) -> io::Result<bool> {
        let mut parents = section(file, HEADER_LEN)?;
        let mut ends = section(file, HEADER_LEN + 21 * count)?;
        let mut roots: Vec<(u32, u32)> = Vec::new();
        let mut prev_end = 0u32;
        for _ in 0..count {
            let parent = u32::from_le_bytes(bytes(&mut parents)?);
            let end = u32::from_le_bytes(bytes(&mut ends)?);
            if parent == NO_PARENT {
                if roots.len() == MAX_ROOTS || end < prev_end || u64::from(end) > names_len {
                    return Ok(false);
                }
                roots.push((prev_end, end));
            }
            prev_end = end;
        }
        let names_at = HEADER_LEN + 25 * count;
        let mut names = File::open(file)?;
        let mut name = Vec::new();
        for (start, end) in roots.iter().copied() {
            names.seek(SeekFrom::Start(names_at + u64::from(start)))?;
            name.resize((end - start) as usize, 0);
            names.read_exact(&mut name)?;
            let Ok(text) = std::str::from_utf8(&name) else {
                return Ok(false);
            };
            if classify(Path::new(text), guard) != Place::Inside {
                return Ok(false);
            }
        }
        Ok(!roots.is_empty())
    }

    /// `Ok(None)` when the format version is not one this reader knows.
    pub(super) fn read(file: &Path, guard: &Guard) -> io::Result<Option<(FileIndex, Vec<bool>)>> {
        let mut head = File::open(file)?;
        let file_len = head.metadata()?.len();
        if file_len < HEADER_LEN + CHECKSUM_LEN {
            return Err(invalid(format!("file too short ({file_len} bytes)")));
        }
        if bytes::<8>(&mut head)? != MAGIC {
            return Err(invalid("not an Athrix index snapshot (bad magic)"));
        }
        if u32::from_le_bytes(bytes(&mut head)?) != FORMAT_VERSION {
            return Ok(None);
        }
        let _normalizer = bytes::<4>(&mut head)?;
        let count = u64::from_le_bytes(bytes(&mut head)?);
        let names_len = u64::from_le_bytes(bytes(&mut head)?);
        let normalized_len = u64::from_le_bytes(bytes(&mut head)?);
        drop(head);
        if count >= u64::from(NO_PARENT) {
            return Err(invalid(format!("entry count {count} out of range")));
        }
        if names_len > u64::from(u32::MAX) || normalized_len > u64::from(u32::MAX) {
            return Err(invalid("name arena exceeds 4 GiB"));
        }
        let expected =
            HEADER_LEN + count * BYTES_PER_ENTRY + names_len + normalized_len + CHECKSUM_LEN;
        if expected != file_len {
            return Err(invalid(format!(
                "file is {file_len} bytes, header implies {expected}"
            )));
        }

        if all_roots_allowed(file, count, names_len, guard)? {
            // Everything is kept: the plain loader is faster (no names normalized again). It
            // refuses a snapshot written with another name normalizer (an app of another
            // version); the cut below normalizes again, so it reads that one too.
            if let Ok(index) = FileIndex::load(file) {
                let mask = vec![true; index.len()];
                return Ok(Some((index, mask)));
            }
        }

        let mut parents = section(file, HEADER_LEN)?;
        let mut flags = section(file, HEADER_LEN + 4 * count)?;
        let mut sizes = section(file, HEADER_LEN + 5 * count)?;
        let mut mtimes = section(file, HEADER_LEN + 13 * count)?;
        let mut ends = section(file, HEADER_LEN + 21 * count)?;
        let mut names = section(file, HEADER_LEN + 25 * count)?;

        let n = usize::try_from(count).map_err(|_| invalid("too many entries"))?;
        let mut cut = Cutter::new(guard, n);
        let mut name = Vec::new();
        let mut prev_end = 0u32;
        for i in 0..count {
            let parent = match u32::from_le_bytes(bytes(&mut parents)?) {
                NO_PARENT => None,
                p if u64::from(p) < i => Some(p),
                p => return Err(invalid(format!("entry {i} has invalid parent {p}"))),
            };
            let end = u32::from_le_bytes(bytes(&mut ends)?);
            if end < prev_end || u64::from(end) > names_len {
                return Err(invalid(format!(
                    "names: bad end offset {end} for entry {i}"
                )));
            }
            let len = end - prev_end;
            prev_end = end;
            let Some(place) = cut.need(parent) else {
                // Outside: read past its name and metadata without keeping them. (Reading,
                // not `seek_relative`: a seek past an empty buffer is a system call each time.)
                name.resize(len as usize, 0);
                names.read_exact(&mut name)?;
                bytes::<1>(&mut flags)?;
                bytes::<8>(&mut sizes)?;
                bytes::<8>(&mut mtimes)?;
                continue;
            };
            name.resize(len as usize, 0);
            names.read_exact(&mut name)?;
            let text = std::str::from_utf8(&name)
                .map_err(|_| invalid(format!("name of entry {i} is not UTF-8")))?;
            let [flag] = bytes::<1>(&mut flags)?;
            if flag & !KNOWN_FLAGS != 0 {
                return Err(invalid(format!("entry {i} has unknown flags {flag:#x}")));
            }
            let size = u64::from_le_bytes(bytes(&mut sizes)?);
            let mtime = i64::from_le_bytes(bytes(&mut mtimes)?);
            let meta = EntryMeta {
                is_dir: flag & FLAG_DIR != 0,
                is_symlink: flag & FLAG_SYMLINK != 0,
                size,
                modified: (mtime != i64::MIN).then_some(mtime),
            };
            cut.push(parent, place, text, meta);
        }
        if u64::from(prev_end) != names_len {
            return Err(invalid(format!(
                "names: offsets end at {prev_end}, text is {names_len} bytes"
            )));
        }
        Ok(Some(cut.finish()))
    }
}

/// Up to `limit` entries matching `query` among those set in `mask`: best first for a ranked
/// query, else in index order.
pub fn search(index: &FileIndex, mask: &[bool], query: &Query, limit: usize) -> Vec<EntryId> {
    if limit == 0 {
        return Vec::new();
    }
    let ids = mask
        .iter()
        .enumerate()
        .filter(|(_, m)| **m)
        .map(|(i, _)| i as EntryId);
    if query.is_ranked() {
        let mut scored: Vec<(u32, EntryId)> = ids
            .filter_map(|id| index.score(query, id).map(|s| (s, id)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, id)| id).collect()
    } else {
        ids.filter(|&id| index.is_match(query, id))
            .take(limit)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> EntryMeta {
        EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        }
    }

    fn plain(size: u64, modified: Option<i64>) -> EntryMeta {
        EntryMeta {
            size,
            modified,
            ..EntryMeta::default()
        }
    }

    /// A tree with two roots under `base`: `base/a` (with `allowed/…`, `Allowed2/…` and
    /// `other/…`) and `base/b`; `allowed` and `b` exist on disk.
    struct Tree {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        index: FileIndex,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        std::fs::create_dir_all(base.join("a").join("allowed")).unwrap();
        std::fs::create_dir_all(base.join("b")).unwrap();
        let mut b = IndexBuilder::new();
        let a = b.push(None, &base.join("a").to_string_lossy(), dir());
        let allowed = b.push(Some(a), "allowed", dir());
        let other = b.push(Some(a), "other", dir());
        let sub = b.push(Some(allowed), "تقارير", dir());
        b.push(Some(sub), "تَقْرِير 2025.PDF", plain(42, Some(1_700_000_000)));
        b.push(Some(other), "تقرير سري.pdf", plain(7, None));
        // A sibling whose name starts like the allowed folder: not admitted.
        let near = b.push(Some(a), "allowed2", dir());
        b.push(Some(near), "near.txt", plain(1, Some(-5)));
        b.push(
            Some(allowed),
            "link",
            EntryMeta {
                is_symlink: true,
                ..EntryMeta::default()
            },
        );
        let root_b = b.push(None, &base.join("b").to_string_lossy(), dir());
        b.push(Some(root_b), "b.txt", plain(3, Some(3)));
        b.push(Some(allowed), "late.txt", plain(9, Some(9)));
        Tree {
            index: b.finish(),
            base,
            _tmp: tmp,
        }
    }

    /// (path, meta, inside) of every kept entry.
    fn rows(index: &FileIndex, mask: &[bool]) -> Vec<(PathBuf, EntryMeta, bool)> {
        (0..index.len() as EntryId)
            .map(|id| (index.path(id), index.meta(id), mask[id as usize]))
            .collect()
    }

    fn save(t: &Tree) -> PathBuf {
        let file = t.base.join("names.snap");
        t.index.save(&file).unwrap();
        file
    }

    #[test]
    fn keeps_only_the_allowed_part_and_its_ancestors() {
        let t = tree();
        let file = save(&t);
        let allowed = t.base.join("a").join("allowed");
        let guard = Guard::new(&[allowed.to_string_lossy()]);
        let (index, mask) = load_allowed(&file, &guard).unwrap();
        let got = rows(&index, &mask);
        let inside: Vec<PathBuf> = got.iter().filter(|r| r.2).map(|r| r.0.clone()).collect();
        assert_eq!(
            inside,
            vec![
                allowed.clone(),
                allowed.join("تقارير"),
                allowed.join("تقارير").join("تَقْرِير 2025.PDF"),
                allowed.join("link"),
                allowed.join("late.txt"),
            ]
        );
        // The root leading to it is kept (for paths) but not searchable; nothing else is.
        let outside: Vec<PathBuf> = got.iter().filter(|r| !r.2).map(|r| r.0.clone()).collect();
        assert_eq!(outside, vec![t.base.join("a")]);
        // Metadata and normalized names survive the copy.
        let pdf = got
            .iter()
            .find(|r| r.0.ends_with("تَقْرِير 2025.PDF"))
            .unwrap();
        assert_eq!(pdf.1, plain(42, Some(1_700_000_000)));
        assert!(
            got.iter()
                .find(|r| r.0.ends_with("link"))
                .unwrap()
                .1
                .is_symlink
        );
        let q = Query::parse("تقرير").unwrap();
        let hits: Vec<PathBuf> = search(&index, &mask, &q, 10)
            .into_iter()
            .map(|id| index.path(id))
            .collect();
        assert_eq!(
            hits,
            vec![allowed.join("تقارير").join("تَقْرِير 2025.PDF")],
            "not the secret one"
        );
    }

    #[test]
    fn streaming_matches_the_full_loader() {
        let t = tree();
        let file = save(&t);
        for folders in [
            vec![t.base.join("a").join("allowed")],
            vec![t.base.join("b")],
            vec![t.base.clone()],
            vec![t.base.join("a").join("allowed"), t.base.join("b")],
        ] {
            let guard = Guard::new(
                &folders
                    .iter()
                    .map(|f| f.to_string_lossy())
                    .collect::<Vec<_>>(),
            );
            let (si, sm) = stream::read(&file, &guard).unwrap().expect("known format");
            let full = FileIndex::load(&file).unwrap();
            let (fi, fm) = cut_loaded(&full, &guard);
            assert_eq!(rows(&si, &sm), rows(&fi, &fm), "{folders:?}");
            for id in 0..si.len() as EntryId {
                assert_eq!(si.normalized_name(id), fi.normalized_name(id));
            }
        }
        // Allowing everything keeps everything.
        let guard = Guard::new(&[t.base.to_string_lossy()]);
        let (si, sm) = load_allowed(&file, &guard).unwrap();
        assert_eq!(si.len(), t.index.len());
        assert!(sm.iter().all(|&m| m));
    }

    #[test]
    fn reads_a_snapshot_of_another_normalizer_version() {
        // An app of another version normalizes names differently; its snapshot is still
        // read (names are normalized again here), whole or cut.
        let t = tree();
        let file = save(&t);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[12..16].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        std::fs::write(&file, &bytes).unwrap();
        assert!(FileIndex::load(&file).is_err());
        let all = Guard::new(&[t.base.to_string_lossy()]);
        let (index, mask) = load_allowed(&file, &all).unwrap();
        assert_eq!(
            rows(&index, &mask),
            rows(&t.index, &vec![true; t.index.len()])
        );
        let some = Guard::new(&[t.base.join("b").to_string_lossy()]);
        assert_eq!(load_allowed(&file, &some).unwrap().1, vec![true, true]);
    }

    #[test]
    fn rejects_damaged_snapshots() {
        let t = tree();
        let file = save(&t);
        let guard = Guard::new(&[t.base.join("a").join("allowed").to_string_lossy()]);
        let good = std::fs::read(&file).unwrap();
        // Truncated.
        std::fs::write(&file, &good[..good.len() - 1]).unwrap();
        assert!(load_allowed(&file, &guard).is_err());
        // Bad magic.
        let mut bad = good.clone();
        bad[0] = b'X';
        std::fs::write(&file, &bad).unwrap();
        assert!(load_allowed(&file, &guard).is_err());
        // A parent pointing forward (entry 1's parent set to 5).
        let mut bad = good.clone();
        let at = stream_parent_offset(1);
        bad[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
        std::fs::write(&file, &bad).unwrap();
        assert!(load_allowed(&file, &guard).is_err());
        // Garbage everywhere after the header never panics.
        for seed in 0..64u32 {
            let mut bad = good.clone();
            for (i, b) in bad.iter_mut().enumerate().skip(40) {
                if (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) % 7 == 0 {
                    *b ^= 0x5a;
                }
            }
            std::fs::write(&file, &bad).unwrap();
            let _ = load_allowed(&file, &guard);
        }
    }

    fn stream_parent_offset(entry: usize) -> usize {
        40 + 4 * entry
    }

    #[test]
    fn reloads_on_change_and_releases_when_idle() {
        let t = tree();
        let data = t.base.join("data");
        std::fs::create_dir_all(data.join(SNAPSHOT_DIR)).unwrap();
        t.index
            .save(&data.join(SNAPSHOT_DIR).join(SNAPSHOT_FILE))
            .unwrap();
        let g1 = Guard::new(&[t.base.join("a").join("allowed").to_string_lossy()]);
        let g2 = Guard::new(&[t.base.join("b").to_string_lossy()]);
        let mut names = Names::default();
        assert!(!names.is_loaded() && names.heap_bytes() == 0);
        let n1 = names
            .get(&data, &g1)
            .unwrap()
            .1
            .iter()
            .filter(|m| **m)
            .count();
        assert_eq!(n1, 5);
        assert!(names.is_loaded() && names.heap_bytes() > 0);
        // Other folders: cut again.
        let n2 = names
            .get(&data, &g2)
            .unwrap()
            .1
            .iter()
            .filter(|m| **m)
            .count();
        assert_eq!(n2, 2);
        // Not idle long enough.
        assert!(!names.release_if_idle(Duration::from_secs(3600)));
        assert!(names.is_loaded());
        assert!(names.release_if_idle(Duration::ZERO));
        assert!(!names.is_loaded() && names.heap_bytes() == 0);
        assert!(!names.release_if_idle(Duration::ZERO));
        // Loads again on demand.
        assert_eq!(
            names
                .get(&data, &g2)
                .unwrap()
                .1
                .iter()
                .filter(|m| **m)
                .count(),
            2
        );
        names.release();
        assert!(!names.is_loaded());
    }
}
