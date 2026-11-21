//! Parallel directory walk (via `jwalk`) that fills an [`IndexBuilder`].
//!
//! This is the cross-platform fallback; the NTFS MFT reader (M2) will feed the same builder.

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
}

/// Compiled exclusion rules, shared with the walker threads.
#[derive(Debug)]
struct Exclusions {
    names: GlobSet,
    paths: GlobSet,
    /// Normalized (see [`prefix_key`]) path prefixes.
    prefixes: Vec<String>,
}

impl Exclusions {
    fn new(opts: &WalkOptions) -> Result<Self> {
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

    fn excludes(&self, path: &Path, name: &std::ffi::OsStr) -> bool {
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
/// skipped or kept without metadata, with a `tracing` warning; they never fail the walk.
/// A root that cannot be read is skipped the same way. Symlinks and junctions are indexed as
/// entries but not followed unless [`WalkOptions::follow_links`] is set.
///
/// # Errors
/// [`Error::Config`] if an exclusion glob is invalid.
pub fn walk(opts: &WalkOptions) -> Result<FileIndex> {
    let exclusions = Arc::new(Exclusions::new(opts)?);
    let mut builder = IndexBuilder::new();
    for root in &opts.roots {
        walk_root(root, opts.follow_links, &exclusions, &mut builder);
    }
    Ok(builder.finish())
}

fn read_meta(path: &Path, follow_links: bool) -> Option<EntryMeta> {
    let md = if follow_links {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    match md {
        Ok(md) => Some(EntryMeta {
            is_dir: md.is_dir(),
            is_symlink: md.file_type().is_symlink(),
            size: if md.is_dir() { 0 } else { md.len() },
            modified: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_secs()).ok()),
        }),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot read metadata");
            None
        }
    }
}

fn walk_root(
    root: &Path,
    follow_links: bool,
    exclusions: &Arc<Exclusions>,
    builder: &mut IndexBuilder,
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
                    tracing::warn!(error = %e, "skipping unreadable entry");
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
                tracing::warn!(error = %e, "skipping unreadable entry");
                continue;
            }
        };
        if let Some(err) = &entry.read_children_error {
            tracing::warn!(path = %entry.path().display(), error = %err, "cannot list folder");
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
            .search(&Query::parse("محاضرة"), 10, &AtomicBool::new(false))
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
