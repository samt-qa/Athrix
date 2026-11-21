//! Frecency: how often *and* how recently a file was opened.
//!
//! Every open adds 1 to a file's score, and the score halves every
//! [`FrecencyConfig::half_life_secs`]:
//!
//! `frecency(now) = Σ over opens at tᵢ ≤ now of 0.5^((now − tᵢ) / half_life)`
//!
//! A file opened 10 times a month ago (half-life 14 days) scores ≈ 2.3; once an hour ago ≈ 1.
//! Only `(score, reference time, open count)` is stored per file, so recording an open and
//! reading a score are O(1): the stored score is decayed to the new reference time and the new
//! open is added (an out-of-order open older than the reference time is decayed instead). The
//! sum is the same whatever order the opens are recorded in.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::path_key;

/// Records opens and answers frecency scores. The app records an open each time the user
/// opens a result; the ranker reads scores. Implementations normalize the path with
/// [`path_key`] (the M5 SQLite store will implement this trait too).
pub trait FrecencyStore {
    /// Records that `path` was opened at `at` (Unix seconds).
    fn record_open(&mut self, path: &str, at: i64);
    /// The decayed score of `path` at `now` (Unix seconds); 0 for files never opened.
    fn frecency(&self, path: &str, now: i64) -> f64;
}

/// Frecency tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrecencyConfig {
    /// Time for a score to halve, in seconds. Default 14 days.
    pub half_life_secs: f64,
    /// Compare paths case-insensitively (and `/` = `\`). Default: true on Windows.
    pub case_insensitive_paths: bool,
}

impl Default for FrecencyConfig {
    fn default() -> Self {
        FrecencyConfig {
            half_life_secs: 14.0 * 86_400.0,
            case_insensitive_paths: cfg!(windows),
        }
    }
}

/// The stored state of one file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrecencyEntry {
    /// The score at `at`.
    pub score: f64,
    /// The reference time of `score` (the latest recorded open), Unix seconds.
    pub at: i64,
    /// How many opens were recorded.
    pub opens: u64,
}

/// `0.5^(dt / half_life)` for `dt ≥ 0`; 1 for `dt ≤ 0` or a non-positive half-life.
pub(crate) fn decay(dt: i64, half_life: f64) -> f64 {
    if dt <= 0 || half_life.is_nan() || half_life <= 0.0 {
        1.0
    } else {
        (-(dt as f64) / half_life * std::f64::consts::LN_2).exp()
    }
}

/// An in-memory [`FrecencyStore`].
#[derive(Debug, Clone, Default)]
pub struct MemoryFrecencyStore {
    config: FrecencyConfig,
    entries: HashMap<String, FrecencyEntry>,
}

impl MemoryFrecencyStore {
    /// An empty store.
    pub fn new(config: FrecencyConfig) -> Self {
        MemoryFrecencyStore {
            config,
            entries: HashMap::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> FrecencyConfig {
        self.config
    }

    /// Number of files with a recorded open.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The stored entry of `path`, if any.
    pub fn entry(&self, path: &str) -> Option<FrecencyEntry> {
        let key = path_key(path, self.config.case_insensitive_paths);
        self.entries.get(key.as_ref()).copied()
    }

    /// Forgets `path` (for example when the file is deleted). Returns true if it was known.
    pub fn forget(&mut self, path: &str) -> bool {
        let key = path_key(path, self.config.case_insensitive_paths);
        self.entries.remove(key.as_ref()).is_some()
    }

    /// Drops entries whose score at `now` is below `min_score`; returns how many.
    pub fn prune(&mut self, now: i64, min_score: f64) -> usize {
        let before = self.entries.len();
        let h = self.config.half_life_secs;
        self.entries
            .retain(|_, e| e.score * decay(now - e.at, h) >= min_score);
        before - self.entries.len()
    }

    /// All entries (normalized path, state), in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &FrecencyEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    fn insert_entry(&mut self, key: String, e: FrecencyEntry) {
        self.entries.insert(key, e);
    }
}

impl FrecencyStore for MemoryFrecencyStore {
    fn record_open(&mut self, path: &str, at: i64) {
        let h = self.config.half_life_secs;
        let key = path_key(path, self.config.case_insensitive_paths).into_owned();
        let e = self.entries.entry(key).or_insert(FrecencyEntry {
            score: 0.0,
            at,
            opens: 0,
        });
        if at >= e.at {
            e.score = e.score * decay(at - e.at, h) + 1.0;
            e.at = at;
        } else {
            e.score += decay(e.at - at, h);
        }
        e.opens = e.opens.saturating_add(1);
    }

    fn frecency(&self, path: &str, now: i64) -> f64 {
        if self.entries.is_empty() {
            return 0.0;
        }
        let key = path_key(path, self.config.case_insensitive_paths);
        self.entries.get(key.as_ref()).map_or(0.0, |e| {
            e.score * decay(now - e.at, self.config.half_life_secs)
        })
    }
}

const HEADER: &str = "athrix-frecency 1";

/// A [`MemoryFrecencyStore`] persisted to a small text file.
///
/// Format (UTF-8): a header line `athrix-frecency 1`, then one line per file:
/// `score<TAB>at<TAB>opens<TAB>path`, the path with `%`, tab, CR and LF written as `%25`,
/// `%09`, `%0D`, `%0A`. Unreadable lines are skipped. [`save`](Self::save) writes a temporary
/// file next to the target and renames it over, so a crash never leaves a half-written file.
/// The M5 SQLite store replaces this.
#[derive(Debug, Clone)]
pub struct FileFrecencyStore {
    mem: MemoryFrecencyStore,
    path: PathBuf,
    dirty: bool,
}

impl FileFrecencyStore {
    /// Loads `path`, or starts empty when it does not exist.
    pub fn open(path: impl Into<PathBuf>, config: FrecencyConfig) -> io::Result<Self> {
        let path = path.into();
        let mut mem = MemoryFrecencyStore::new(config);
        match fs::File::open(&path) {
            Ok(f) => load(io::BufReader::new(f), &mut mem)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(FileFrecencyStore {
            mem,
            path,
            dirty: false,
        })
    }

    /// The file this store saves to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The in-memory state.
    pub fn memory(&self) -> &MemoryFrecencyStore {
        &self.mem
    }

    /// Mutable in-memory state (`forget`, `prune`); marks the store as changed.
    pub fn memory_mut(&mut self) -> &mut MemoryFrecencyStore {
        self.dirty = true;
        &mut self.mem
    }

    /// True when there are changes not saved yet.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Writes the file (atomically: temporary file + rename).
    pub fn save(&mut self) -> io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut w = BufWriter::new(fs::File::create(&tmp)?);
            writeln!(w, "{HEADER}")?;
            let mut rows: Vec<_> = self.mem.iter().collect();
            rows.sort_by(|a, b| a.0.cmp(b.0));
            for (p, e) in rows {
                writeln!(w, "{}\t{}\t{}\t{}", e.score, e.at, e.opens, escape(p))?;
            }
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }

    /// Saves only when something changed.
    pub fn save_if_dirty(&mut self) -> io::Result<()> {
        if self.dirty { self.save() } else { Ok(()) }
    }
}

impl FrecencyStore for FileFrecencyStore {
    fn record_open(&mut self, path: &str, at: i64) {
        self.mem.record_open(path, at);
        self.dirty = true;
    }

    fn frecency(&self, path: &str, now: i64) -> f64 {
        self.mem.frecency(path, now)
    }
}

fn load(r: impl BufRead, mem: &mut MemoryFrecencyStore) -> io::Result<()> {
    let mut lines = r.lines();
    match lines.next().transpose()? {
        None => return Ok(()),
        Some(h) if h.trim_end() == HEADER => {}
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an Athrix frecency file (or a newer version)",
            ));
        }
    }
    for line in lines {
        let line = line?;
        let mut it = line.splitn(4, '\t');
        let (Some(score), Some(at), Some(opens), Some(path)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let (Ok(score), Ok(at), Ok(opens)) = (score.parse(), at.parse(), opens.parse()) else {
            continue;
        };
        let score: f64 = score;
        if !score.is_finite() || score < 0.0 {
            continue;
        }
        let Some(path) = unescape(path) else { continue };
        let key = path_key(&path, mem.config.case_insensitive_paths).into_owned();
        mem.insert_entry(key, FrecencyEntry { score, at, opens });
    }
    Ok(())
}

fn escape(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\t' => out.push_str("%09"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(p: &str) -> Option<String> {
    let mut out = String::with_capacity(p.len());
    let mut rest = p;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let code = rest.get(i + 1..i + 3)?;
        out.push(match code {
            "25" => '%',
            "09" => '\t',
            "0D" | "0d" => '\r',
            "0A" | "0a" => '\n',
            _ => return None,
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn cfg() -> FrecencyConfig {
        FrecencyConfig {
            half_life_secs: 10.0 * DAY as f64,
            case_insensitive_paths: true,
        }
    }

    #[test]
    fn decays_and_accumulates() {
        let mut s = MemoryFrecencyStore::new(cfg());
        assert_eq!(s.frecency("a", 0), 0.0);
        s.record_open(r"C:\Docs\تقرير.pdf", 0);
        assert!((s.frecency(r"c:/docs/تقرير.PDF", 0) - 1.0).abs() < 1e-12);
        assert!((s.frecency(r"C:\Docs\تقرير.pdf", 10 * DAY) - 0.5).abs() < 1e-12);
        s.record_open(r"C:\Docs\تقرير.pdf", 10 * DAY);
        assert!((s.frecency(r"C:\Docs\تقرير.pdf", 10 * DAY) - 1.5).abs() < 1e-12);
        // Clock going backwards never increases the score.
        assert!((s.frecency(r"C:\Docs\تقرير.pdf", 0) - 1.5).abs() < 1e-12);
        assert_eq!(s.entry(r"c:\docs\تقرير.pdf").unwrap().opens, 2);
    }

    #[test]
    fn order_of_opens_does_not_matter() {
        let times = [5 * DAY, 0, 30 * DAY, 2 * DAY];
        let mut a = MemoryFrecencyStore::new(cfg());
        let mut b = MemoryFrecencyStore::new(cfg());
        for t in times {
            a.record_open("x", t);
        }
        let mut sorted = times;
        sorted.sort();
        for t in sorted {
            b.record_open("x", t);
        }
        let now = 40 * DAY;
        assert!((a.frecency("x", now) - b.frecency("x", now)).abs() < 1e-9);
    }

    #[test]
    fn prune_and_forget() {
        let mut s = MemoryFrecencyStore::new(cfg());
        s.record_open("old", 0);
        s.record_open("new", 100 * DAY);
        assert_eq!(s.prune(100 * DAY, 0.01), 1);
        assert!(s.forget("NEW"));
        assert!(s.is_empty());
    }

    #[test]
    fn escape_round_trip() {
        for p in ["a%b\tc\nd\re", "plain", "%", "مسار/ملف"] {
            assert_eq!(unescape(&escape(p)).as_deref(), Some(p));
        }
        assert_eq!(unescape("bad%zz"), None);
        assert_eq!(unescape("bad%2"), None);
    }

    #[test]
    fn file_store_round_trip() {
        let dir = std::env::temp_dir().join(format!("athrix-frecency-{}", std::process::id()));
        let file = dir.join("frecency.txt");
        let _ = fs::remove_file(&file);
        let mut s = FileFrecencyStore::open(&file, cfg()).unwrap();
        assert!(s.memory().is_empty());
        s.record_open("C:\\a\tb.txt", 0);
        s.record_open("C:\\ملف.docx", DAY);
        assert!(s.is_dirty());
        s.save().unwrap();
        assert!(!s.is_dirty());
        let t = FileFrecencyStore::open(&file, cfg()).unwrap();
        assert_eq!(t.memory().len(), 2);
        assert!((t.frecency("c:\\a\tb.txt", 0) - 1.0).abs() < 1e-12);
        assert!((t.frecency("C:\\ملف.docx", DAY) - 1.0).abs() < 1e-12);

        fs::write(&file, "something else\n").unwrap();
        assert!(FileFrecencyStore::open(&file, cfg()).is_err());
        fs::write(
            &file,
            format!("{HEADER}\ngarbage\n1\t2\t3\tok\nNaN\t1\t1\tbad\n"),
        )
        .unwrap();
        let t = FileFrecencyStore::open(&file, cfg()).unwrap();
        assert_eq!(t.memory().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
