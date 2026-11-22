//! The folder guard: every path the server returns or reads must lie under an allowed folder.
//!
//! A path passes [`Guard::check`] only when **all** of these hold:
//!
//! 1. it is a plain absolute path: on Windows a drive path (`C:\…`); UNC shares
//!    (`\\server\share`), verbatim / device paths (`\\?\…`, `\\.\…`), drive-relative paths
//!    (`C:foo`), alternate data streams (`file:stream`) and NUL characters are refused;
//! 2. after resolving `.` and `..` lexically (never above the root), it is an allowed folder
//!    or lies under one, compared by [`key`] (case-insensitive with either separator on
//!    Windows; byte for byte elsewhere) and by whole components (`C:\Docs` never admits
//!    `C:\Docs2`);
//! 3. it exists, and its canonical form (symbolic links, junctions, 8.3 names and `..` resolved
//!    by the OS) lies under the canonical form of the same allowed folder, so a link or
//!    junction inside an allowed folder that points elsewhere is refused.
//!
//! Allowed folders themselves must pass rule 1 and exist; others are ignored.
//! [`Guard::admits_lexically`] applies rule 2 alone, without disk access, to filter index
//! entries cheaply before the full check.

use std::path::{Component, Path, PathBuf};

/// The comparison key of a path: on Windows lower-cased with `/` turned into `\`; elsewhere
/// the path itself. Trailing separators are removed (except for a root such as `C:\` or `/`).
pub fn key(path: &str) -> String {
    let mut k = if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path.to_owned()
    };
    let sep = if cfg!(windows) { '\\' } else { '/' };
    while k.len() > 1 && k.ends_with(sep) && !is_root_key(&k) {
        k.pop();
    }
    k
}

fn is_root_key(k: &str) -> bool {
    if cfg!(windows) {
        let b = k.as_bytes();
        b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
    } else {
        k == "/"
    }
}

/// Whether the key `inner` is the key `outer` or lies under it, by whole components.
pub fn key_within(inner: &str, outer: &str) -> bool {
    let sep = if cfg!(windows) { '\\' } else { '/' };
    match inner.strip_prefix(outer) {
        Some("") => true,
        Some(rest) => outer.ends_with(sep) || rest.starts_with(sep),
        None => false,
    }
}

/// Why a path was refused (English, for the tool error; never contains file contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// Not a plain absolute local path (rule 1).
    BadPath(&'static str),
    /// Outside every allowed folder, lexically or after resolving links (rules 2, 3).
    Outside,
    /// It does not exist or cannot be resolved.
    Missing,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Denied::BadPath(why) => write!(f, "refused path: {why}"),
            Denied::Outside => {
                f.write_str("this path is outside the folders allowed in Athrix → Settings → MCP")
            }
            Denied::Missing => f.write_str("no such file or folder"),
        }
    }
}

/// Lexically normalized absolute form of `raw` (rule 1 + `.`/`..` resolution).
pub fn normalize(raw: &str) -> Result<PathBuf, Denied> {
    if raw.contains('\0') {
        return Err(Denied::BadPath("NUL character"));
    }
    if cfg!(windows) {
        let unified = raw.replace('/', "\\");
        if unified.starts_with("\\\\") {
            return Err(Denied::BadPath(
                "UNC, device and \\\\?\\ paths are not allowed",
            ));
        }
        // A `:` past the drive letter names an alternate data stream (or is invalid).
        if unified.get(2..).is_some_and(|rest| rest.contains(':')) {
            return Err(Denied::BadPath("alternate data streams are not allowed"));
        }
    }
    let path = Path::new(raw);
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    let mut rooted = false;
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                use std::path::Prefix;
                match p.kind() {
                    Prefix::Disk(_) => out.push(p.as_os_str()),
                    _ => return Err(Denied::BadPath("only local drive paths are allowed")),
                }
            }
            Component::RootDir => {
                out.push(std::path::MAIN_SEPARATOR_STR);
                rooted = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(Denied::BadPath("`..` above the root"));
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(name) => {
                if !rooted {
                    return Err(Denied::BadPath("the path must be absolute"));
                }
                out.push(name);
                depth += 1;
            }
        }
    }
    if !rooted || (cfg!(windows) && !path.has_root()) || !out.is_absolute() {
        return Err(Denied::BadPath("the path must be absolute"));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Allowed {
    /// The normalized folder as configured.
    path: PathBuf,
    /// [`key`] of `path`.
    lexical: String,
    /// [`key`] of its canonical form.
    canonical: String,
}

/// The allowed folders, ready for checks. See the [module docs](self).
#[derive(Debug, Clone, Default)]
pub struct Guard {
    folders: Vec<Allowed>,
}

impl Guard {
    /// A guard for `folders`; folders that are not plain absolute paths of existing
    /// directories are ignored.
    pub fn new<S: AsRef<str>>(folders: &[S]) -> Guard {
        let folders = folders
            .iter()
            .filter_map(|f| {
                let path = normalize(f.as_ref().trim()).ok()?;
                let canonical = std::fs::canonicalize(&path).ok()?;
                if !canonical.is_dir() {
                    return None;
                }
                Some(Allowed {
                    lexical: key(&path.to_string_lossy()),
                    canonical: key(&canonical.to_string_lossy()),
                    path,
                })
            })
            .collect();
        Guard { folders }
    }

    /// True when no usable folder is allowed (then nothing passes).
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    /// The usable allowed folders (normalized).
    pub fn folders(&self) -> impl Iterator<Item = &Path> {
        self.folders.iter().map(|a| a.path.as_path())
    }

    /// Rule 2 only, without touching the disk: whether the already-absolute `path` (as stored
    /// in an index) is an allowed folder or lies under one.
    pub fn admits_lexically(&self, path: &str) -> bool {
        let Ok(norm) = normalize(path) else {
            return false;
        };
        let k = key(&norm.to_string_lossy());
        self.folders.iter().any(|a| key_within(&k, &a.lexical))
    }

    /// Whether the key of some allowed folder lies under (or is) the key `k`: an index entry
    /// with that key may have allowed entries below it.
    pub(crate) fn is_ancestor_key(&self, k: &str) -> bool {
        self.folders.iter().any(|a| key_within(&a.lexical, k))
    }

    /// Whether the key `k` is an allowed folder or lies under one.
    pub(crate) fn admits_key(&self, k: &str) -> bool {
        self.folders.iter().any(|a| key_within(k, &a.lexical))
    }

    /// The full check (rules 1–3). Returns the normalized path.
    pub fn check(&self, raw: &str) -> Result<PathBuf, Denied> {
        let norm = normalize(raw)?;
        let lexical = key(&norm.to_string_lossy());
        let candidates: Vec<&Allowed> = self
            .folders
            .iter()
            .filter(|a| key_within(&lexical, &a.lexical))
            .collect();
        if candidates.is_empty() {
            return Err(Denied::Outside);
        }
        let canonical = std::fs::canonicalize(&norm).map_err(|_| Denied::Missing)?;
        let canonical = key(&canonical.to_string_lossy());
        if candidates
            .iter()
            .any(|a| key_within(&canonical, &a.canonical))
        {
            Ok(norm)
        } else {
            Err(Denied::Outside)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_compare_whole_components() {
        assert!(key_within(&key("/a/b"), &key("/a")));
        assert!(key_within(&key("/a"), &key("/a/")));
        assert!(!key_within(&key("/ab"), &key("/a")));
        assert!(key_within(&key("/x"), &key("/")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_case_and_separator_insensitive() {
        assert_eq!(key("C:/Docs/"), "c:\\docs");
        assert_eq!(key("C:\\"), "c:\\");
        assert!(key_within(&key("c:/DOCS/a.txt"), &key("C:\\Docs")));
        assert!(!key_within(&key("C:\\Docs2"), &key("C:\\Docs")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_odd_paths() {
        for bad in [
            r"\\?\C:\Docs",
            r"\\.\C:\Docs",
            r"\\server\share\x",
            "//server/share/x",
            r"C:Docs",
            r"Docs\a",
            r"C:\Docs\a.txt:secret",
            r"C:\..\x",
            "C:\\Docs\0",
        ] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
        assert_eq!(
            normalize(r"C:/Docs/./sub/../a.txt").unwrap(),
            PathBuf::from(r"C:\Docs\a.txt")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_refuses_relative_and_escaping_paths() {
        for bad in ["docs/a", "/../x", "/a/../../x", "a\0"] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize("/a/./b/../c").unwrap(), PathBuf::from("/a/c"));
    }
}
