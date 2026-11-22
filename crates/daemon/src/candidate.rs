//! Which files are worth extracting, and how urgent each one is.

use std::path::{Component, Path, PathBuf};

use athrix_content_index::FileMeta;
use athrix_extract::Kind;

/// A file to extract: path, the size and modification time the scanner saw, and its format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Full path, in the name index's form.
    pub path: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in seconds since the Unix epoch (the name index's unit). Only
    /// compared for equality by the content index, so it just has to be consistent.
    pub mtime: i64,
    /// Format, from the extension.
    pub kind: Kind,
}

impl Candidate {
    /// The key the content index stores for this file.
    pub fn key(&self) -> String {
        path_key(&self.path)
    }

    /// Size and time as the content index wants them.
    pub fn meta(&self) -> FileMeta {
        FileMeta {
            size: self.size,
            mtime: self.mtime,
        }
    }
}

/// The string stored in the content index for `path`. Lossy for the rare non-Unicode path,
/// but always the same for the same path, so updates and deletes still match.
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Rules that keep junk out of the content index: size cap, system and tool folders, hidden
/// and temporary files. Matching is case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFilter {
    /// Files larger than this are not extracted (default 64 MiB, the extractor's own input
    /// limit).
    pub max_size: u64,
    /// Folder names skipped wherever they appear, lowercase (default: the name index's
    /// default exclusions, [`athrix_fs_index::exclude::DEFAULT_EXCLUDED_NAMES`] — `.git`,
    /// `node_modules`, `$Recycle.Bin`, `System Volume Information`, the Windows staging
    /// folders — plus `AppData`, `__pycache__`, ...).
    pub excluded_dir_names: Vec<String>,
    /// Folder names skipped only right below a drive or file-system root (default:
    /// `Windows`, `Program Files`, `ProgramData`, `Recovery`, ...), so a user folder that
    /// happens to be called "Windows" is still indexed.
    pub excluded_root_dirs: Vec<String>,
    /// Paths skipped with everything below them.
    pub excluded_prefixes: Vec<PathBuf>,
    /// Skip files and folders whose name starts with `.` (default true).
    pub skip_dot_files: bool,
}

/// Folder names the content index skips on top of the name index's defaults
/// ([`athrix_fs_index::exclude::DEFAULT_EXCLUDED_NAMES`]): application data and package
/// caches hold no documents worth reading.
const CONTENT_EXCLUDED_DIRS: &[&str] = &[
    "appdata",
    "__pycache__",
    "bower_components",
    "site-packages",
];

/// Folder names skipped right below a drive root: the system and program folders.
const DEFAULT_EXCLUDED_ROOT_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "recovery",
    "perflogs",
    "msocache",
    "config.msi",
];

/// The default [`CandidateFilter::excluded_dir_names`]: the name index's default exclusions
/// (shared with the app and the CLI) plus [`CONTENT_EXCLUDED_DIRS`], lowercase.
fn default_excluded_dir_names() -> Vec<String> {
    let mut names: Vec<String> = athrix_fs_index::exclude::DEFAULT_EXCLUDED_NAMES
        .iter()
        .chain(CONTENT_EXCLUDED_DIRS)
        .map(|s| s.to_lowercase())
        .collect();
    names.dedup();
    names
}

const JUNK_SUFFIXES: &[&str] = &[
    ".tmp",
    ".temp",
    ".part",
    ".partial",
    ".crdownload",
    ".download",
    ".swp",
];

impl Default for CandidateFilter {
    fn default() -> Self {
        CandidateFilter {
            max_size: 64 * 1024 * 1024,
            excluded_dir_names: default_excluded_dir_names(),
            excluded_root_dirs: DEFAULT_EXCLUDED_ROOT_DIRS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            excluded_prefixes: Vec::new(),
            skip_dot_files: true,
        }
    }
}

impl CandidateFilter {
    /// True if a file with this name is a temporary, lock or system file, or hidden by a
    /// leading dot: Office owner files (`~$report.docx`), LibreOffice locks
    /// (`.~lock.x.odt#`), `~*.tmp`, editor backups (`x~`), partial downloads, `desktop.ini`,
    /// `Thumbs.db`.
    pub fn is_junk_name(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        if self.skip_dot_files && lower.starts_with('.') {
            return true;
        }
        lower.starts_with("~$")
            || lower.starts_with(".~lock.")
            || lower.ends_with('~')
            || lower.ends_with('#')
            || JUNK_SUFFIXES.iter().any(|ext| lower.ends_with(ext))
            || lower == "desktop.ini"
            || lower == "thumbs.db"
    }

    /// True if `dir` (a folder) or anything below it must not be indexed.
    pub fn is_excluded_dir(&self, dir: &Path) -> bool {
        if self
            .excluded_prefixes
            .iter()
            .any(|p| starts_with_ci(dir, p))
        {
            return true;
        }
        let mut depth = 0usize;
        for c in dir.components() {
            let Component::Normal(name) = c else {
                continue;
            };
            let name = name.to_string_lossy();
            if self.skip_dot_files && name.starts_with('.') {
                return true;
            }
            let eq = |d: &String| d.to_lowercase() == name.to_lowercase();
            if self.excluded_dir_names.iter().any(eq) {
                return true;
            }
            if depth == 0 && self.excluded_root_dirs.iter().any(eq) {
                return true;
            }
            depth += 1;
        }
        false
    }

    /// The candidate for a file, or `None` if a rule rejects it or `supports` says its format
    /// cannot be extracted.
    pub fn candidate(
        &self,
        path: &Path,
        size: u64,
        mtime: i64,
        supports: impl Fn(Kind) -> bool,
    ) -> Option<Candidate> {
        if size > self.max_size {
            return None;
        }
        let name = path.file_name()?.to_str()?;
        let kind = Kind::from_path(Path::new(name))?;
        if !supports(kind) || self.is_junk_name(name) {
            return None;
        }
        if path.parent().is_some_and(|dir| self.is_excluded_dir(dir)) {
            return None;
        }
        Some(Candidate {
            path: path.to_path_buf(),
            size,
            mtime,
            kind,
        })
    }
}

/// Case-insensitive, component-wise `path.starts_with(prefix)`.
pub(crate) fn starts_with_ci(path: &Path, prefix: &Path) -> bool {
    let mut p = path.components();
    for want in prefix.components() {
        let Some(got) = p.next() else {
            return false;
        };
        let a = got.as_os_str().to_string_lossy().to_lowercase();
        let b = want.as_os_str().to_string_lossy().to_lowercase();
        if a != b {
            return false;
        }
    }
    true
}

/// The folders whose files are indexed first: `Desktop`, `Documents` and `Downloads` under
/// the user's home folder (`USERPROFILE` on Windows, `HOME` elsewhere).
///
/// Folders redirected elsewhere (for example into OneDrive) are not detected; the app can pass
/// the real locations in
/// [`SchedulerConfig::priority_folders`](crate::SchedulerConfig::priority_folders).
pub fn default_priority_folders() -> Vec<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var_os(var) {
        Some(h) if !h.is_empty() => {
            let h = PathBuf::from(h);
            ["Desktop", "Documents", "Downloads"]
                .iter()
                .map(|d| h.join(d))
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_names() {
        let f = CandidateFilter::default();
        for junk in [
            "~$report.docx",
            ".~lock.sheet.ods#",
            "~WRL0001.tmp",
            ".hidden.txt",
            "notes.txt~",
            "desktop.ini",
            "Thumbs.db",
            "big.iso.crdownload",
        ] {
            assert!(f.is_junk_name(junk), "{junk}");
        }
        for ok in ["report.docx", "تقرير.txt", "a~b.md"] {
            assert!(!f.is_junk_name(ok), "{ok}");
        }
    }

    #[test]
    fn excluded_dirs() {
        let f = CandidateFilter::default();
        let r = Path::new(if cfg!(windows) { "C:\\" } else { "/" });
        assert!(f.is_excluded_dir(&r.join("Windows").join("System32")));
        assert!(f.is_excluded_dir(&r.join("Users/a/AppData/Roaming")));
        assert!(f.is_excluded_dir(&r.join("src/node_modules/x")));
        assert!(f.is_excluded_dir(&r.join("repo/.git/objects")));
        assert!(!f.is_excluded_dir(&r.join("Users/a/Documents/Windows")));
        assert!(!f.is_excluded_dir(&r.join("Users/a/Documents")));
        // The shared defaults, wherever they appear.
        assert!(f.is_excluded_dir(&r.join("$Recycle.Bin").join("S-1-5")));
        assert!(f.is_excluded_dir(&r.join("System Volume Information")));
        assert!(f.is_excluded_dir(&r.join("$WinREAgent").join("Scratch")));
        for name in athrix_fs_index::exclude::DEFAULT_EXCLUDED_NAMES {
            assert!(
                f.excluded_dir_names.contains(&name.to_lowercase()),
                "{name} shared with the name index"
            );
        }
    }

    #[test]
    fn candidate_rules() {
        let f = CandidateFilter {
            max_size: 100,
            ..CandidateFilter::default()
        };
        let all = |_| true;
        let c = f.candidate(Path::new("/d/a.txt"), 10, 5, all).unwrap();
        assert_eq!(c.kind, Kind::Text);
        assert!(f.candidate(Path::new("/d/a.txt"), 101, 5, all).is_none());
        assert!(f.candidate(Path::new("/d/a.exe"), 10, 5, all).is_none());
        assert!(
            f.candidate(Path::new("/d/a.pdf"), 10, 5, |k| k != Kind::Pdf)
                .is_none()
        );
        assert!(f.candidate(Path::new("/d/~$a.docx"), 10, 5, all).is_none());
    }

    #[test]
    fn prefix_is_case_insensitive() {
        assert!(starts_with_ci(
            Path::new("/Users/A/Docs/x"),
            Path::new("/users/a/docs")
        ));
        assert!(!starts_with_ci(
            Path::new("/Users/A/Docsx"),
            Path::new("/users/a/docs")
        ));
    }
}
