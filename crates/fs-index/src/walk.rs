//! Parallel directory walk (via `jwalk`) that fills an [`IndexBuilder`].
//!
//! This is the cross-platform path; on Windows, [`crate::ntfs`] fills the same builder from an
//! NTFS volume enumeration with the same exclusion rules.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use athrix_core::{Error, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use jwalk::WalkDirGeneric;

use crate::index::{EntryId, EntryMeta, FileIndex, IndexBuilder};

/// What to walk and what to skip.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    roots: Vec<PathBuf>,
    exclude_globs: Vec<String>,
    exclude_prefixes: Vec<PathBuf>,
    follow_links: bool,
}

impl WalkOptions {
    /// Walks the given roots, in order.
    pub fn new<I, P>(roots: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        WalkOptions {
            roots: roots.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }

    /// Excludes entries matching a glob (excluded folders are not descended into).
    ///
    /// A pattern without `/` or `\` matches the entry's name (`node_modules`, `*.tmp`); a pattern
    /// with one matches the full path (`**/target/debug`). Matching is case-insensitive on
    /// Windows.
    pub fn exclude_glob(mut self, pattern: impl Into<String>) -> Self {
        self.exclude_globs.push(pattern.into());
        self
    }

    /// Excludes a path and everything below it.
    pub fn exclude_prefix(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclude_prefixes.push(path.into());
        self
    }

    /// Follows symbolic links and junctions (off by default; loops are detected by `jwalk`).
    pub fn follow_links(mut self, follow: bool) -> Self {
        self.follow_links = follow;
        self
    }

    /// The roots, in order.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether symbolic links and junctions are followed.
    pub fn follows_links(&self) -> bool {
        self.follow_links
    }
}

/// Compiled exclusion rules, shared with the walker threads (and the live watcher).
#[derive(Debug)]
pub(crate) struct Exclusions {
    names: GlobSet,
    paths: GlobSet,
    /// Normalized (see [`prefix_key`]) path prefixes.
    prefixes: Vec<String>,
}

impl Exclusions {
    pub(crate) fn new(opts: &WalkOptions) -> Result<Self> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in &opts.exclude_globs {
            let glob = GlobBuilder::new(pattern)
                .case_insensitive(cfg!(windows))
                .build()
                .map_err(|e| Error::Config {
                    key: "exclude".into(),
                    message: e.to_string(),
                })?;
            if pattern.contains(['/', '\\']) {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        let build = |b: GlobSetBuilder| {
            b.build().map_err(|e| Error::Config {
                key: "exclude".into(),
                message: e.to_string(),
            })
        };
        Ok(Exclusions {
            names: build(names)?,
            paths: build(paths)?,
            prefixes: opts
                .exclude_prefixes
                .iter()
                .map(|p| prefix_key(p))
                .collect(),
        })
    }

    /// True if some rule looks at the full path (path globs or prefixes), not only the name.
    pub(crate) fn needs_paths(&self) -> bool {
        !self.paths.is_empty() || !self.prefixes.is_empty()
    }

    pub(crate) fn excludes(&self, path: &Path, name: &std::ffi::OsStr) -> bool {
        if !self.names.is_empty() && self.names.is_match(name) {
            return true;
        }
        if !self.paths.is_empty() && self.paths.is_match(path) {
            return true;
        }
        if !self.prefixes.is_empty() {
            let key = prefix_key(path);
            return self.prefixes.iter().any(|p| {
                key.starts_with(p.as_str())
                    && (key.len() == p.len() || key.as_bytes()[p.len()] == b'/')
            });
        }
        false
    }
}

/// Path as a comparable string: `/` separators, no trailing separator, lowercase on Windows.
fn prefix_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

/// Per-entry state computed on the walker threads.
type State = ((), EntryMeta);

/// Walks every root in `opts` in parallel and builds the name index.
///
/// Entries that cannot be read (permission denied, vanished mid-walk, broken metadata) are
/// skipped or kept without metadata; they never fail the walk. Access-denied errors are
/// expected on whole-drive walks (system folders), so they are logged at `debug` only; other
/// errors at `warn`. One `info` line at the end reports how many folders could not be listed.
/// A root that cannot be read is skipped the same way. Symlinks and junctions are indexed as
/// entries but not followed unless [`WalkOptions::follow_links`] is set.
///
/// # Errors
/// [`Error::Config`] if an exclusion glob is invalid.
pub fn walk(opts: &WalkOptions) -> Result<FileIndex> {
    let exclusions = Arc::new(Exclusions::new(opts)?);
    let mut builder = IndexBuilder::new();
    let mut skipped = Skipped::default();
    for root in &opts.roots {
        walk_root(
            root,
            opts.follow_links,
            &exclusions,
            &mut builder,
            &mut skipped,
        );
    }
    let index = builder.finish();
    tracing::info!(
        entries = index.len(),
        skipped_folders = skipped.folders,
        access_denied = skipped.access_denied,
        "walk finished; {} folder(s) could not be listed ({} access denied)",
        skipped.folders,
        skipped.access_denied,
    );
    Ok(index)
}

/// The roots and exclusion rules of a [`WalkOptions`], compiled once and shared by the live
/// watcher ([`crate::watch`]) and the live index ([`crate::live`]) so that a path changed after
/// the walk is judged exactly like the walk judged it.
#[derive(Debug)]
pub(crate) struct Scope {
    /// Each root with its [`path_key`].
    pub(crate) roots: Vec<(PathBuf, String)>,
    exclusions: Arc<Exclusions>,
    pub(crate) follow_links: bool,
}

impl Scope {
    pub(crate) fn new(opts: &WalkOptions) -> Result<Self> {
        Ok(Scope {
            roots: opts
                .roots
                .iter()
                .map(|r| (r.clone(), path_key(r)))
                .collect(),
            exclusions: Arc::new(Exclusions::new(opts)?),
            follow_links: opts.follow_links,
        })
    }

    /// The index of the (innermost) root that contains `path`, or `None` if it is outside every
    /// root. A root contains itself.
    pub(crate) fn root_of(&self, key: &str) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, (_, root))| key_has_prefix(key, root))
            .max_by_key(|(_, (_, root))| root.len())
            .map(|(i, _)| i)
    }

    /// True if `path` (with key `key`) is inside a root and neither it nor any of its ancestors
    /// below that root is excluded. Roots themselves are never excluded, as in [`walk`].
    pub(crate) fn admits(&self, path: &Path, key: &str) -> bool {
        let Some(root) = self.root_of(key) else {
            return false;
        };
        let rest = &key[self.roots[root].1.len()..];
        let depth = rest.bytes().filter(|&b| b == b'/').count();
        path.ancestors().take(depth).all(|a| match a.file_name() {
            Some(name) => !self.exclusions.excludes(a, name),
            None => true,
        })
    }

    /// True if `key` is exactly one of the roots.
    pub(crate) fn is_root(&self, key: &str) -> bool {
        self.roots.iter().any(|(_, r)| r == key)
    }

    /// Walks the subtree at `dir` (which must be in scope) with the same rules as [`walk`]. The
    /// returned index has `dir` as its only root, named by its full path.
    pub(crate) fn walk_subtree(&self, dir: &Path) -> FileIndex {
        let mut builder = IndexBuilder::new();
        let mut skipped = Skipped::default();
        walk_root(
            dir,
            self.follow_links,
            &self.exclusions,
            &mut builder,
            &mut skipped,
        );
        if skipped.folders > 0 {
            tracing::debug!(
                dir = %dir.display(),
                skipped_folders = skipped.folders,
                "subtree walk could not list some folders"
            );
        }
        builder.finish()
    }

    /// Reads the metadata of `path` the way the walker records it.
    pub(crate) fn read_meta(&self, path: &Path) -> std::io::Result<EntryMeta> {
        let md = if self.follow_links {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }?;
        Ok(meta_from(&md))
    }
}

/// Comparable form of a path for the live layer: `/` separators, no trailing separator, and
/// case-folded per character on Windows (the file system there is case-insensitive).
pub(crate) fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push('/'),
            c if cfg!(windows) => out.extend(c.to_lowercase()),
            c => out.push(c),
        }
    }
    while out.ends_with('/') {
        out.pop();
    }
    out
}

/// True if `key` equals `prefix` or lies below it.
pub(crate) fn key_has_prefix(key: &str, prefix: &str) -> bool {
    key.starts_with(prefix) && (key.len() == prefix.len() || key.as_bytes()[prefix.len()] == b'/')
}

fn meta_from(md: &std::fs::Metadata) -> EntryMeta {
    EntryMeta {
        is_dir: md.is_dir(),
        is_symlink: md.file_type().is_symlink(),
        size: if md.is_dir() { 0 } else { md.len() },
        modified: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok()),
    }
}

fn read_meta(path: &Path, follow_links: bool) -> Option<EntryMeta> {
    let md = if follow_links {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    match md {
        Ok(md) => Some(meta_from(&md)),
        Err(e) => {
            if is_access_denied(&e) {
                tracing::debug!(path = %path.display(), error = %e, "cannot read metadata");
            } else {
                tracing::warn!(path = %path.display(), error = %e, "cannot read metadata");
            }
            None
        }
    }
}

/// Folders a walk could not list.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Skipped {
    /// All folders whose children could not be read.
    pub(crate) folders: usize,
    /// Of those, the ones refused with "access denied".
    pub(crate) access_denied: usize,
}

/// True for "access denied" / "permission denied", which whole-drive walks hit by the hundred
/// (System Volume Information, other users' profiles...) and which are not worth a warning.
fn is_access_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
}

/// Logs a jwalk error: `debug` for access denied, `warn` otherwise.
fn log_entry_error(e: &jwalk::Error) {
    if e.io_error().is_some_and(is_access_denied) {
        tracing::debug!(error = %e, "skipping unreadable entry");
    } else {
        tracing::warn!(error = %e, "skipping unreadable entry");
    }
}

pub(crate) fn walk_root(
    root: &Path,
    follow_links: bool,
    exclusions: &Arc<Exclusions>,
    builder: &mut IndexBuilder,
    skipped: &mut Skipped,
) {
    let ex = Arc::clone(exclusions);
    let walker = WalkDirGeneric::<State>::new(root)
        .follow_links(follow_links)
        .skip_hidden(false)
        .sort(true)
        .process_read_dir(move |depth, dir, _state, children| {
            if depth.is_none() {
                // The root itself: handled (and never excluded) in the loop below.
                return;
            }
            children.retain_mut(|child| match child {
                Ok(entry) => {
                    let path = dir.join(&entry.file_name);
                    if ex.excludes(&path, &entry.file_name) {
                        return false;
                    }
                    let ft = entry.file_type;
                    let mut meta = read_meta(&path, follow_links).unwrap_or_default();
                    // Trust the directory listing for the kind: it never follows links.
                    meta.is_dir = ft.is_dir();
                    meta.is_symlink |= ft.is_symlink();
                    if meta.is_symlink && !follow_links {
                        // Covers junctions too: std reports them as symlinks on Windows.
                        entry.read_children_path = None;
                        meta.is_dir = false;
                    }
                    entry.client_state = meta;
                    true
                }
                Err(e) => {
                    log_entry_error(e);
                    false
                }
            });
        });

    // jwalk yields depth-first: every entry comes after its parent, and a directory's
    // descendants come before its next sibling. So `stack[d]` is the open directory at depth d.
    let mut stack: Vec<(Arc<Path>, EntryId)> = Vec::new();
    for item in walker {
        let entry = match item {
            Ok(e) => e,
            Err(e) => {
                log_entry_error(&e);
                continue;
            }
        };
        if let Some(err) = &entry.read_children_error {
            skipped.folders += 1;
            if err.io_error().is_some_and(is_access_denied) {
                skipped.access_denied += 1;
                tracing::debug!(path = %entry.path().display(), error = %err, "cannot list folder");
            } else {
                tracing::warn!(path = %entry.path().display(), error = %err, "cannot list folder");
            }
        }
        let depth = entry.depth;
        let id = if depth == 0 {
            let Some(meta) = read_meta(root, true) else {
                tracing::warn!(root = %root.display(), "skipping unreadable root");
                return;
            };
            builder.push(None, &root.to_string_lossy(), meta)
        } else {
            stack.truncate(depth);
            let Some((parent_path, parent)) = stack.last() else {
                tracing::warn!(path = %entry.path().display(), "entry without parent, skipped");
                continue;
            };
            debug_assert_eq!(**parent_path, *entry.parent_path);
            let name = entry.file_name.to_string_lossy();
            builder.push(Some(*parent), &name, entry.client_state)
        };
        if let Some(children) = &entry.read_children_path {
            stack.truncate(depth);
            stack.push((Arc::clone(children), id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Query;
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in [
            "docs/عربي",
            "docs/sub/deep",
            "node_modules/pkg",
            "build/out",
            "keep",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        for f in [
            "docs/report.pdf",
            "docs/عربي/مُحَاضَرَة.docx",
            "docs/sub/deep/notes.txt",
            "docs/sub/cache.tmp",
            "node_modules/pkg/index.js",
            "build/out/app.exe",
            "keep/readme.md",
        ] {
            fs::write(p.join(f), b"hello").unwrap();
        }
        dir
    }

    fn relative_paths(ix: &FileIndex, root: &Path) -> BTreeSet<String> {
        (0..ix.len() as EntryId)
            .filter(|&id| ix.parent(id).is_some())
            .map(|id| {
                ix.path(id)
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walks_everything_and_rebuilds_paths() {
        let dir = tree();
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert_eq!(paths.len(), 16, "{paths:?}");
        assert!(paths.contains("docs/عربي/مُحَاضَرَة.docx"));
        assert!(paths.contains("node_modules/pkg/index.js"));
        // Every rebuilt path exists on disk.
        for id in 0..ix.len() as EntryId {
            assert!(ix.path(id).exists(), "{:?}", ix.path(id));
        }
        let id = ix
            .search(
                &Query::parse("محاضرة").unwrap(),
                10,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(id.len(), 1);
        let meta = ix.meta(id[0]);
        assert_eq!(meta.size, 5);
        assert!(!meta.is_dir && meta.modified.is_some());
        assert!(ix.meta(0).is_dir);
    }

    #[test]
    fn exclusions_by_name_glob_path_glob_and_prefix() {
        let dir = tree();
        let opts = WalkOptions::new([dir.path()])
            .exclude_glob("node_modules")
            .exclude_glob("*.tmp")
            .exclude_glob("**/build/out")
            .exclude_prefix(dir.path().join("docs").join("sub").join("deep"));
        let paths = relative_paths(&walk(&opts).unwrap(), dir.path());
        let expected: BTreeSet<String> = [
            "build",
            "docs",
            "docs/report.pdf",
            "docs/sub",
            "docs/عربي",
            "docs/عربي/مُحَاضَرَة.docx",
            "keep",
            "keep/readme.md",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(paths, expected);
    }

    #[test]
    fn prefix_does_not_exclude_siblings_with_same_start() {
        let dir = tree();
        fs::create_dir_all(dir.path().join("keep2")).unwrap();
        let opts = WalkOptions::new([dir.path()]).exclude_prefix(dir.path().join("keep"));
        let paths = relative_paths(&walk(&opts).unwrap(), dir.path());
        assert!(paths.contains("keep2"));
        assert!(!paths.contains("keep"));
    }

    #[test]
    fn multiple_roots_and_missing_root_is_skipped() {
        let a = tree();
        let b = tree();
        let missing = a.path().join("does-not-exist");
        let ix = walk(&WalkOptions::new([a.path(), &missing, b.path()])).unwrap();
        let roots: Vec<_> = (0..ix.len() as EntryId)
            .filter(|&id| ix.parent(id).is_none())
            .collect();
        assert_eq!(roots.len(), 2);
        assert_eq!(ix.len(), 2 * 17);
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        let err = walk(&WalkOptions::new(["."]).exclude_glob("a[")).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_by_default() {
        let dir = tree();
        std::os::unix::fs::symlink(dir.path().join("docs"), dir.path().join("link")).unwrap();
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert!(paths.contains("link"));
        assert!(!paths.contains("link/report.pdf"));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_not_followed_by_default() {
        let dir = tree();
        let target = dir.path().join("docs");
        let link = dir.path().join("link");
        // `mklink /J` needs no admin rights; skip quietly if the shell refuses.
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("mklink /J unavailable; skipping");
            return;
        }
        let ix = walk(&WalkOptions::new([dir.path()])).unwrap();
        let paths = relative_paths(&ix, dir.path());
        assert!(paths.contains("link"));
        assert!(!paths.contains("link/report.pdf"), "{paths:?}");
        let followed = walk(&WalkOptions::new([dir.path()]).follow_links(true)).unwrap();
        assert!(relative_paths(&followed, dir.path()).contains("link/report.pdf"));
    }
}
