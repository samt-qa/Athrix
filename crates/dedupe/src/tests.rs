use super::*;
use std::fs;

struct Fixture {
    dir: tempfile::TempDir,
    candidates: Vec<Candidate>,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            dir: tempfile::tempdir().unwrap(),
            candidates: Vec::new(),
        }
    }

    /// Writes `name` with `bytes` and adds it as a candidate; returns its index.
    fn file(&mut self, name: &str, bytes: &[u8]) -> usize {
        let path = self.dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        self.add(path, bytes.len() as u64)
    }

    fn add(&mut self, path: PathBuf, size: u64) -> usize {
        self.candidates.push(Candidate {
            key: path.to_string_lossy().to_lowercase(),
            path,
            size,
        });
        self.candidates.len() - 1
    }

    fn run(&self, cache: Option<&HashCache>) -> Report {
        find_duplicates(
            &self.candidates,
            cache,
            &Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap()
    }
}

fn members(r: &Report) -> Vec<Vec<usize>> {
    r.groups.iter().map(|g| g.members.clone()).collect()
}

/// `len` pseudo-random bytes from `seed`.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn identical_files_group_and_same_size_different_ones_do_not() {
    let mut f = Fixture::new();
    let a = f.file("a.txt", b"hello world");
    let b = f.file("b.txt", b"hello world");
    let _c = f.file("c.txt", b"hello WORLD"); // same size, other bytes
    let d = f.file("d.txt", b"hello world");
    let _e = f.file("e.txt", b"unique size here");
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![a, b, d]]);
    assert_eq!(r.groups[0].size, 11);
    assert_eq!(r.groups[0].wasted(), 22);
    assert_eq!(r.wasted(), 22);
    assert!(!r.incomplete);
    assert_eq!(r.skipped, 0);
    assert_eq!(r.groups[0].hash, *blake3::hash(b"hello world").as_bytes());
}

#[test]
fn zero_byte_files_are_never_duplicates() {
    let mut f = Fixture::new();
    f.file("empty1", b"");
    f.file("empty2", b"");
    let r = f.run(None);
    assert!(r.groups.is_empty());
    assert_eq!(r.bytes_read, 0);
}

#[test]
fn equal_ends_with_a_different_middle_are_told_apart_by_the_full_hash() {
    let len = 3 * PARTIAL_CHUNK as usize + 17;
    let base = noise(len, 1);
    let mut other = base.clone();
    other[len / 2] ^= 0xFF; // same first and last 64 KiB
    let mut f = Fixture::new();
    let a = f.file("a.bin", &base);
    f.file("b.bin", &other);
    let c = f.file("c.bin", &base);
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![a, c]]);
    // Three partial reads (2 × 64 KiB each) plus three full reads.
    assert_eq!(r.bytes_read, 3 * 2 * PARTIAL_CHUNK + 3 * len as u64);
}

#[test]
fn different_tails_need_no_full_hash() {
    let len = 4 * PARTIAL_CHUNK as usize;
    let base = noise(len, 2);
    let mut tail = base.clone();
    tail[len - 1] ^= 1;
    let mut f = Fixture::new();
    f.file("a.bin", &base);
    f.file("b.bin", &tail);
    let r = f.run(None);
    assert!(r.groups.is_empty());
    assert_eq!(r.bytes_read, 2 * 2 * PARTIAL_CHUNK, "partial reads only");
}

#[test]
fn groups_come_biggest_wasted_space_first() {
    let mut f = Fixture::new();
    let small: Vec<usize> = (0..4).map(|i| f.file(&format!("s{i}"), b"xy")).collect();
    let big = noise(1000, 3);
    let b1 = f.file("b1", &big);
    let b2 = f.file("b2", &big);
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![b1, b2], small]);
}

#[test]
fn missing_changed_and_folder_candidates_are_skipped() {
    let mut f = Fixture::new();
    let a = f.file("a", b"same");
    let b = f.file("b", b"same");
    let gone = f.dir.path().join("gone");
    f.add(gone, 4);
    // The index thinks it has 4 bytes; it now has 5.
    let changed = f.dir.path().join("changed");
    fs::write(&changed, b"same!").unwrap();
    f.add(changed, 4);
    let folder = f.dir.path().join("folder");
    fs::create_dir(&folder).unwrap();
    f.add(folder, 4);
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![a, b]]);
    assert_eq!(r.skipped, 3);
}

#[cfg(windows)]
#[test]
fn locked_files_are_skipped() {
    use std::os::windows::fs::OpenOptionsExt;
    let mut f = Fixture::new();
    let a = f.file("a", b"locked?");
    let b = f.file("b", b"locked?");
    let c = f.file("c", b"locked?");
    // No sharing at all: other opens fail with a sharing violation.
    let _lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&f.candidates[c].path)
        .unwrap();
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![a, b]]);
    assert_eq!(r.skipped, 1);
}

#[cfg(unix)]
#[test]
fn unreadable_files_are_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let a = f.file("a", b"secret");
    let b = f.file("b", b"secret");
    let c = f.file("c", b"secret");
    let path = f.candidates[c].path.clone();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if File::open(&path).is_ok() {
        return; // running as root: permissions are not enforced
    }
    let r = f.run(None);
    assert_eq!(members(&r), vec![vec![a, b]]);
    assert_eq!(r.skipped, 1);
}

#[test]
fn the_cache_avoids_reading_again_and_ignores_changed_files() {
    let len = 3 * PARTIAL_CHUNK as usize;
    let data = noise(len, 4);
    let mut f = Fixture::new();
    let a = f.file("a.bin", &data);
    let b = f.file("b.bin", &data);
    let cache = HashCache::in_memory().unwrap();
    let first = f.run(Some(&cache));
    assert_eq!(members(&first), vec![vec![a, b]]);
    assert!(first.bytes_read > 0);
    assert_eq!(cache.len(), 2);
    let second = f.run(Some(&cache));
    assert_eq!(members(&second), members(&first));
    assert_eq!(second.bytes_read, 0);
    assert_eq!(second.cache_hits, 2);
    // Rewrite b with other bytes of the same size and a later mtime: its row is stale.
    let mut other = data.clone();
    other[10] ^= 1;
    let path = &f.candidates[b].path;
    fs::write(path, &other).unwrap();
    let later = std::time::SystemTime::now() + Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let third = f.run(Some(&cache));
    assert!(third.groups.is_empty());
    assert_eq!(third.cache_hits, 1);
}

#[test]
fn a_spent_budget_returns_what_is_confirmed_and_a_second_call_continues() {
    let len = 3 * PARTIAL_CHUNK as usize;
    let mut f = Fixture::new();
    // Two pairs of different sizes, each pair identical.
    let x = noise(len, 5);
    let y = noise(len + 1, 6);
    let x1 = f.file("x1", &x);
    let x2 = f.file("x2", &x);
    let y1 = f.file("y1", &y);
    let y2 = f.file("y2", &y);
    let cache = HashCache::in_memory().unwrap();
    let tight = Options {
        max_bytes: 1,
        threads: 1,
        ..Options::default()
    };
    let no = AtomicBool::new(false);
    let mut rounds = 0;
    let report = loop {
        rounds += 1;
        assert!(rounds < 50, "never finished");
        let r = find_duplicates(&f.candidates, Some(&cache), &tight, &no).unwrap();
        if !r.incomplete {
            break r;
        }
    };
    assert!(rounds > 1, "the 1-byte budget must stop the first call");
    assert_eq!(members(&report), vec![vec![y1, y2], vec![x1, x2]]);
}

#[test]
fn a_set_cancel_flag_cancels() {
    let mut f = Fixture::new();
    f.file("a", b"abc");
    f.file("b", b"abc");
    let cancel = AtomicBool::new(true);
    assert_eq!(
        find_duplicates(&f.candidates, None, &Options::default(), &cancel),
        Err(Cancelled)
    );
}
