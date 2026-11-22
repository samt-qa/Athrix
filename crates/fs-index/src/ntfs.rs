//! Filling the name index from an NTFS volume enumeration ([`athrix_ntfs`]) instead of a
//! directory walk, with the walk as the per-root fallback.
//!
//! - [`push_volume_tree`] copies a [`VolumeTree`] into an [`IndexBuilder`], applying exactly the
//!   exclusion rules of a [`WalkOptions`] (same compiled matcher, same paths, same "an excluded
//!   folder hides its whole subtree") and the same name normalization as [`crate::walk`].
//! - [`index_volume`] enumerates one drive (`C:\`) and returns its [`FileIndex`].
//! - [`index_roots`] indexes every root of a [`WalkOptions`]: whole NTFS drives through the
//!   volume enumeration when possible, everything else (and every drive where NTFS access fails,
//!   typically because the process is not elevated) through the walk, and reports which method
//!   each root used.
//!
//! # Ids
//!
//! A [`VolumeTree`] already stores parents before children, which is all an [`IndexBuilder`]
//! needs, so entries are copied in tree order with no reordering: when nothing is skipped, the
//! entry at tree index `i` gets id `first + i` (`first` being the builder's length before the
//! push). Skipped entries (exclusions, NTFS metadata files) are dropped in a single forward pass
//! that keeps the relative order, so ids stay monotone in tree order and parents still precede
//! children; a skipped folder's descendants are skipped with it (their parent has no id).
//!
//! # Differences from the walk
//!
//! - The enumeration sees what the walk cannot list (other users' profiles, folders that refuse
//!   listing), and lists each file once even if it has several hard links (the walk lists every
//!   link). Order of siblings differs (MFT order instead of sorted names), so ids differ between
//!   the two methods, but the set of paths and the search results are the same.
//! - USN records carry the reparse-point attribute but not its tag. A reparse-point folder with
//!   no children in the volume tree is treated as a junction / directory symlink (like the walk:
//!   `is_symlink`, not a folder); one with children (e.g. a OneDrive placeholder folder) stays a
//!   folder. Reparse-point files (mostly cloud placeholders) are plain files.
//! - NTFS metadata files in the root (`$MFT`, `$LogFile`, `$Extend`, ..., MFT records 0-15),
//!   which a directory listing never shows, are skipped.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use athrix_ntfs::{EnumOptions, EnumStats, NtfsError, VolumeTree, is_elevated};
use athrix_ntfs::{RECORD_NUMBER_MASK, Volume};

use crate::index::{EntryId, EntryMeta, FileIndex, IndexBuilder};
use crate::walk::{Exclusions, Skipped, WalkOptions, walk_root};

/// MFT records below this number are NTFS metadata files (`$MFT` = 0 ... `$Extend` = 11, 12-15
/// reserved); the root directory (5) is the tree's root and handled separately.
const FIRST_USER_RECORD: u64 = 16;

/// Errors of [`index_volume`] and [`index_roots`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NtfsIndexError {
    /// The NTFS enumeration failed (see [`NtfsError::should_fall_back`]).
    #[error(transparent)]
    Ntfs(#[from] NtfsError),
    /// Invalid options (e.g. an exclusion glob).
    #[error(transparent)]
    Index(#[from] athrix_core::Error),
}

impl NtfsIndexError {
    /// True if the directory walk should be used instead (not elevated, not NTFS, ...).
    pub fn should_fall_back(&self) -> bool {
        matches!(self, Self::Ntfs(e) if e.should_fall_back())
    }
}

/// What [`push_volume_tree`] copied and skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushStats {
    /// Entries added to the builder (including the root).
    pub pushed: usize,
    /// Entries matched by an exclusion rule (each hides its subtree).
    pub excluded: usize,
    /// Entries skipped because an ancestor was excluded or skipped.
    pub below_excluded: usize,
    /// NTFS metadata files (`$MFT`, `$Extend`, ...) skipped in the root.
    pub metadata_files: usize,
}

/// Copies `tree` into `builder` as one root named `root` (e.g. `C:\`, spelled as the walk would
/// have been given it), applying the exclusion rules of `opts` (its roots are ignored).
///
/// Names go through [`IndexBuilder::push`], i.e. the same Arabic-aware normalization as the
/// walk. See the [module docs](self) for the id mapping and the differences from a walk.
///
/// # Errors
/// [`athrix_core::Error::Config`] if an exclusion glob is invalid.
pub fn push_volume_tree(
    builder: &mut IndexBuilder,
    tree: &VolumeTree,
    root: &Path,
    opts: &WalkOptions,
) -> athrix_core::Result<PushStats> {
    let exclusions = Exclusions::new(opts)?;
    Ok(push_tree(builder, tree, root, &exclusions))
}

fn push_tree(
    builder: &mut IndexBuilder,
    tree: &VolumeTree,
    root: &Path,
    exclusions: &Exclusions,
) -> PushStats {
    const SKIPPED: EntryId = EntryId::MAX;
    let mut stats = PushStats::default();
    let len = tree.len();
    if len == 0 {
        return stats;
    }

    // Folders with at least one child (to tell junctions from placeholder folders).
    let mut has_children = vec![false; len];
    for i in 1..len as u32 {
        if let Some(p) = tree.parent(i) {
            has_children[p as usize] = true;
        }
    }

    // New id of each tree entry, or SKIPPED.
    let mut ids = vec![SKIPPED; len];
    // Full paths of kept folders, only when a rule needs paths.
    let needs_paths = exclusions.needs_paths();
    let mut dir_paths: HashMap<u32, PathBuf> = HashMap::new();
    let mut path = PathBuf::new();

    for e in tree.iter() {
        let i = e.index as usize;
        let is_reparse_dir = e.is_dir() && e.is_reparse_point();
        let is_symlink = is_reparse_dir && !has_children[i];
        let meta = EntryMeta {
            is_dir: e.is_dir() && !is_symlink,
            is_symlink,
            size: if e.is_dir() { 0 } else { e.size.unwrap_or(0) },
            modified: e.modified,
            created: e.created,
        };
        let Some(parent) = e.parent else {
            // The root: never excluded, as in the walk.
            ids[i] = builder.push(None, &root.to_string_lossy(), meta);
            if needs_paths {
                dir_paths.insert(e.index, root.to_path_buf());
            }
            stats.pushed += 1;
            continue;
        };
        let parent_id = ids[parent as usize];
        if parent_id == SKIPPED {
            stats.below_excluded += 1;
            continue;
        }
        if parent == 0 && e.file_ref & RECORD_NUMBER_MASK < FIRST_USER_RECORD {
            stats.metadata_files += 1;
            continue;
        }
        let name = e.name;
        let excluded = if needs_paths {
            let Some(parent_path) = dir_paths.get(&parent) else {
                // Unreachable: every kept folder has its path recorded.
                stats.below_excluded += 1;
                continue;
            };
            path.as_mut_os_string().clear();
            path.push(parent_path);
            path.push(name);
            exclusions.excludes(&path, OsStr::new(name))
        } else {
            exclusions.excludes(Path::new(name), OsStr::new(name))
        };
        if excluded {
            stats.excluded += 1;
            continue;
        }
        ids[i] = builder.push(Some(parent_id), name, meta);
        stats.pushed += 1;
        if needs_paths && meta.is_dir {
            dir_paths.insert(e.index, path.clone());
        }
    }
    stats
}

/// Enumerates the NTFS volume `volume` (`C`, `C:` or `C:\`) and indexes it under the root name
/// `C:\`, with the exclusions of `opts` (its roots are ignored). Needs an elevated process.
///
/// # Errors
/// [`NtfsIndexError::Ntfs`] if the volume cannot be enumerated (check
/// [`NtfsIndexError::should_fall_back`]), [`NtfsIndexError::Index`] for an invalid glob.
pub fn index_volume(volume: &str, opts: &WalkOptions) -> Result<FileIndex, NtfsIndexError> {
    let exclusions = Exclusions::new(opts)?;
    let volume = Volume::open(volume)?;
    let (tree, _) = volume.enumerate(&EnumOptions::default())?;
    let mut builder = IndexBuilder::with_capacity(tree.len(), name_bytes_hint(&tree));
    push_tree(&mut builder, &tree, Path::new(&volume.root()), &exclusions);
    Ok(builder.finish())
}

/// Which method [`index_roots`] may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexMethod {
    /// NTFS enumeration for whole local NTFS drives (`C:\`) when it works, the walk otherwise.
    #[default]
    Auto,
    /// The directory walk only.
    Walk,
    /// NTFS enumeration only: any root it cannot handle is an error.
    Ntfs,
}

/// How one root was indexed.
#[derive(Debug)]
pub enum RootMethod {
    /// Directory walk; `fallback` is the NTFS error that made [`IndexMethod::Auto`] fall back
    /// (`None` if NTFS was not attempted: not a whole drive, other OS, links followed, or
    /// [`IndexMethod::Walk`]).
    Walk {
        /// Why NTFS was not used, if it was attempted.
        fallback: Option<NtfsError>,
    },
    /// NTFS volume enumeration.
    Ntfs {
        /// Enumeration counters and phase timings.
        stats: EnumStats,
        /// What was copied into the index.
        push: PushStats,
        /// Time spent copying the tree into the index (normalization, exclusions).
        push_time: Duration,
    },
}

impl RootMethod {
    /// `"walk"` or `"ntfs"`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Walk { .. } => "walk",
            Self::Ntfs { .. } => "ntfs",
        }
    }
}

/// What [`index_roots`] did for one root.
#[derive(Debug)]
pub struct RootReport {
    /// The root as given.
    pub root: PathBuf,
    /// Method used.
    pub method: RootMethod,
    /// Entries this root added (0 if it could not be read).
    pub entries: usize,
    /// Wall time for this root.
    pub elapsed: Duration,
}

/// The result of [`index_roots`].
#[derive(Debug)]
pub struct IndexedRoots {
    /// The combined index (roots in the order given).
    pub index: FileIndex,
    /// One report per root, in order.
    pub roots: Vec<RootReport>,
}

/// If `root` is a whole drive (`C:\` or `C:/`), its volume name (`C:`).
pub fn whole_drive(root: &Path) -> Option<String> {
    let s = root.to_str()?;
    let b = s.as_bytes();
    (b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
        .then(|| format!("{}:", char::from(b[0].to_ascii_uppercase())))
}

/// Indexes every root of `opts` into one [`FileIndex`] (same layout as [`crate::walk`]: one root
/// entry per root, in order), using the NTFS enumeration where `method` allows and the walk
/// elsewhere. With [`IndexMethod::Auto`], any NTFS failure falls back to the walk for that root
/// and is kept in its [`RootReport`]. On other platforms than Windows, `Auto` is the walk.
///
/// # Errors
/// [`NtfsIndexError::Index`] for an invalid exclusion glob; with [`IndexMethod::Ntfs`],
/// [`NtfsIndexError::Ntfs`] for the first root that cannot be enumerated (a root that is not a
/// whole drive is [`NtfsError::InvalidVolume`]).
pub fn index_roots(
    opts: &WalkOptions,
    method: IndexMethod,
) -> Result<IndexedRoots, NtfsIndexError> {
    let exclusions = std::sync::Arc::new(Exclusions::new(opts)?);
    let mut builder = IndexBuilder::new();
    let mut reports = Vec::with_capacity(opts.roots().len());
    let mut skipped = Skipped::default();
    for root in opts.roots() {
        let start = Instant::now();
        let before = builder.len();
        let try_ntfs = match method {
            IndexMethod::Walk => false,
            IndexMethod::Ntfs => true,
            IndexMethod::Auto => {
                cfg!(windows) && !opts.follows_links() && whole_drive(root).is_some()
            }
        };
        let mut fallback = None;
        if try_ntfs {
            match enumerate_root(root) {
                Ok((tree, stats)) => {
                    let t = Instant::now();
                    let push = push_tree(&mut builder, &tree, root, &exclusions);
                    drop(tree);
                    reports.push(RootReport {
                        root: root.clone(),
                        method: RootMethod::Ntfs {
                            stats,
                            push,
                            push_time: t.elapsed(),
                        },
                        entries: builder.len() - before,
                        elapsed: start.elapsed(),
                    });
                    continue;
                }
                Err(e) if method == IndexMethod::Ntfs => return Err(e.into()),
                Err(e) => {
                    if e.should_fall_back() {
                        tracing::info!(root = %root.display(), error = %e, "NTFS enumeration unavailable; walking");
                    } else {
                        tracing::warn!(root = %root.display(), error = %e, "NTFS enumeration failed; walking");
                    }
                    fallback = Some(e);
                }
            }
        }
        walk_root(
            root,
            opts.follows_links(),
            &exclusions,
            &mut builder,
            &mut skipped,
        );
        reports.push(RootReport {
            root: root.clone(),
            method: RootMethod::Walk { fallback },
            entries: builder.len() - before,
            elapsed: start.elapsed(),
        });
    }
    let index = builder.finish();
    tracing::info!(
        entries = index.len(),
        skipped_folders = skipped.folders,
        access_denied = skipped.access_denied,
        "indexing finished"
    );
    Ok(IndexedRoots {
        index,
        roots: reports,
    })
}

/// Enumerates `root` if it is a whole drive.
fn enumerate_root(root: &Path) -> Result<(VolumeTree, EnumStats), NtfsError> {
    let Some(volume) = whole_drive(root) else {
        return Err(NtfsError::InvalidVolume(root.display().to_string()));
    };
    Volume::open(&volume)?.enumerate(&EnumOptions::default())
}

fn name_bytes_hint(tree: &VolumeTree) -> usize {
    tree.name_bytes() / tree.len().max(1) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use athrix_ntfs::tree::TreeBuilder;
    use athrix_ntfs::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

    const ROOT: u64 = (5 << 48) | 5;
    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;
    const FILE: u32 = 0x20;

    fn r(rec: u64) -> u64 {
        (1 << 48) | rec
    }

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\" } else { "/vol" })
    }

    /// `(record, parent record (5 = root), attributes, name)`, children before parents.
    fn sample() -> VolumeTree {
        let mut b = TreeBuilder::new();
        let recs: &[(u64, u64, u32, &str)] = &[
            (0, 5, FILE, "$MFT"),
            (11, 5, DIR, "$Extend"),
            (40, 30, FILE, "report.pdf"),
            (30, 5, DIR, "Docs"),
            (31, 5, DIR, "Users"),
            (32, 31, DIR, "me"),
            (33, 32, DIR, "AppData"),
            (34, 33, DIR, "Local"),
            (35, 34, DIR, "Temp"),
            (36, 35, FILE, "junk.tmp"),
            (37, 5, DIR, "$RECYCLE.BIN"),
            (38, 37, FILE, "deleted.txt"),
            (39, 30, DIR | FILE_ATTRIBUTE_REPARSE_POINT, "link"),
            (41, 30, DIR | FILE_ATTRIBUTE_REPARSE_POINT, "OneDrive"),
            (42, 41, FILE | FILE_ATTRIBUTE_REPARSE_POINT, "مُحَاضَرَة.docx"),
            (43, 30, DIR, ".git"),
            (44, 43, FILE, "HEAD"),
        ];
        for &(rec, parent, attrs, name) in recs {
            let parent = if parent == 5 { ROOT } else { r(parent) };
            b.push(r(rec), parent, attrs, name);
        }
        let mut t = b.finish(&root().to_string_lossy());
        for i in 0..t.len() as u32 {
            t.set_metadata(
                i,
                Some(u64::from(i) * 10),
                Some(1_700_000_000 + i64::from(i)),
                Some(1_600_000_000 + i64::from(i)),
            );
        }
        t.mark_metadata();
        t
    }

    fn paths(ix: &FileIndex) -> Vec<String> {
        let mut v: Vec<String> = (0..ix.len() as EntryId)
            .map(|id| ix.path(id).to_string_lossy().replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    fn rel(ix: &FileIndex) -> Vec<String> {
        let root = root();
        let mut v: Vec<String> = (0..ix.len() as EntryId)
            .filter(|&id| ix.parent(id).is_some())
            .map(|id| {
                ix.path(id)
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn ids_are_tree_indexes_when_nothing_is_skipped() {
        let mut b = TreeBuilder::new();
        b.push(r(30), ROOT, DIR, "a");
        b.push(r(31), r(30), FILE, "b.txt");
        b.push(r(32), ROOT, FILE, "c.txt");
        let tree = b.finish(&root().to_string_lossy());
        let mut builder = IndexBuilder::new();
        builder.push(None, "/other", EntryMeta::default());
        let stats =
            push_volume_tree(&mut builder, &tree, &root(), &WalkOptions::default()).unwrap();
        assert_eq!(stats.pushed, 4);
        let ix = builder.finish();
        for i in 0..tree.len() as u32 {
            let id = 1 + i;
            assert_eq!(ix.name(id), tree.name(i));
            assert_eq!(ix.parent(id), tree.parent(i).map(|p| p + 1));
        }
    }

    #[test]
    fn skips_metadata_files_exclusions_and_their_subtrees() {
        let tree = sample();
        let opts = WalkOptions::default()
            .exclude_glob(".git")
            .exclude_glob("$RECYCLE.BIN")
            .exclude_glob("**/AppData/Local/Temp");
        let mut builder = IndexBuilder::new();
        let stats = push_volume_tree(&mut builder, &tree, &root(), &opts).unwrap();
        let ix = builder.finish();
        assert_eq!(
            rel(&ix),
            [
                "Docs",
                "Docs/OneDrive",
                "Docs/OneDrive/مُحَاضَرَة.docx",
                "Docs/link",
                "Docs/report.pdf",
                "Users",
                "Users/me",
                "Users/me/AppData",
                "Users/me/AppData/Local",
            ]
        );
        assert_eq!(stats.metadata_files, 2);
        assert_eq!(stats.excluded, 3);
        assert_eq!(stats.below_excluded, 3);
        assert_eq!(stats.pushed, ix.len());
        for id in 1..ix.len() as EntryId {
            assert!(ix.parent(id).unwrap() < id);
        }
        // Prefix exclusions too.
        let opts = WalkOptions::default().exclude_prefix(root().join("Users").join("me"));
        let mut builder = IndexBuilder::new();
        push_volume_tree(&mut builder, &tree, &root(), &opts).unwrap();
        let ix = builder.finish();
        assert!(rel(&ix).contains(&"Users".to_string()));
        assert!(!rel(&ix).iter().any(|p| p.starts_with("Users/me")));
    }

    #[test]
    fn metadata_junctions_and_normalized_names() {
        let tree = sample();
        let mut builder = IndexBuilder::new();
        push_volume_tree(&mut builder, &tree, &root(), &WalkOptions::default()).unwrap();
        let ix = builder.finish();
        let find = |name: &str| {
            (0..ix.len() as EntryId)
                .find(|&id| ix.name(id) == name)
                .unwrap()
        };
        let tree_index = |name: &str| tree.iter().find(|e| e.name == name).unwrap().index;
        let link = ix.meta(find("link"));
        assert!(link.is_symlink && !link.is_dir);
        let one = ix.meta(find("OneDrive"));
        assert!(one.is_dir && !one.is_symlink && one.size == 0);
        let doc = find("مُحَاضَرَة.docx");
        let i = tree_index("مُحَاضَرَة.docx");
        assert_eq!(ix.normalized_name(doc), "محاضره docx");
        assert_eq!(ix.meta(doc).size, u64::from(i) * 10);
        assert_eq!(ix.meta(doc).modified, Some(1_700_000_000 + i64::from(i)));
        assert_eq!(ix.meta(doc).created, Some(1_600_000_000 + i64::from(i)));
        assert!(!ix.meta(doc).is_symlink);
        assert!(ix.meta(0).is_dir);
        assert_eq!(ix.name(0), root().to_string_lossy());
        assert!(paths(&ix).iter().all(|p| !p.contains("$MFT")));
    }

    #[test]
    fn whole_drives() {
        assert_eq!(whole_drive(Path::new(r"c:\")), Some("C:".into()));
        assert_eq!(whole_drive(Path::new("D:/")), Some("D:".into()));
        assert_eq!(whole_drive(Path::new("D:")), None);
        assert_eq!(whole_drive(Path::new(r"D:\x")), None);
        assert_eq!(whole_drive(Path::new("/")), None);
    }

    #[test]
    fn forced_ntfs_on_a_folder_is_a_fallback_error_and_auto_walks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let opts = WalkOptions::new([dir.path()]);
        let err = index_roots(&opts, IndexMethod::Ntfs).unwrap_err();
        assert!(err.should_fall_back(), "{err:?}");
        let done = index_roots(&opts, IndexMethod::Auto).unwrap();
        assert_eq!(done.index.len(), 2);
        assert_eq!(done.roots.len(), 1);
        assert_eq!(done.roots[0].entries, 2);
        assert_eq!(done.roots[0].method.name(), "walk");
    }

    #[test]
    fn invalid_volume_falls_back() {
        let err = index_volume("not a drive", &WalkOptions::default()).unwrap_err();
        assert!(err.should_fall_back(), "{err:?}");
        let err = index_volume("C:", &WalkOptions::default().exclude_glob("a[")).unwrap_err();
        assert!(matches!(err, NtfsIndexError::Index(_)));
    }
}
