//! The default exclusions: the one list of folders that the desktop app, the command-line tool
//! and the content-indexing daemon all leave out, so they index the same thing.
//!
//! - [`DEFAULT_EXCLUDED_NAMES`]: folder names skipped wherever they appear. Tool noise
//!   ([`TOOL_FOLDER_NAMES`]: `.git`, `node_modules`) plus the Windows system folders that only
//!   matter when whole drives are indexed ([`SYSTEM_FOLDER_NAMES`]: the recycle bin, `System
//!   Volume Information` and the update / reset staging folders, which hold hundreds of
//!   thousands of copies of system files nobody searches for).
//! - [`DEFAULT_EXCLUDED_PATH_GLOBS`]: the per-user temp folder and the Windows component store
//!   (`Windows\WinSxS`, ~100k hard-linked copies of system files).
//! - [`UNIX_EXCLUDED_PREFIXES`]: pseudo file systems skipped when `/` is indexed.
//!
//! [`WalkOptions::with_default_exclusions`] applies all of them. The content daemon starts from
//! [`DEFAULT_EXCLUDED_NAMES`] and adds its own, stricter rules (it also skips `AppData`, program
//! folders...).

use crate::walk::WalkOptions;

/// Tool folders skipped wherever they appear: version-control data and package caches.
pub const TOOL_FOLDER_NAMES: [&str; 2] = [".git", "node_modules"];

/// Windows system folders skipped wherever they appear (they sit at drive roots): the recycle
/// bin, `System Volume Information`, and Windows' update / reset staging folders.
pub const SYSTEM_FOLDER_NAMES: [&str; 6] = [
    "$RECYCLE.BIN",
    "System Volume Information",
    "$WinREAgent",
    "$Windows.~BT",
    "$Windows.~WS",
    "$SysReset",
];

/// Folder names never indexed: [`TOOL_FOLDER_NAMES`] then [`SYSTEM_FOLDER_NAMES`]. Matched
/// against each entry's name (case-insensitive on Windows).
pub const DEFAULT_EXCLUDED_NAMES: [&str; 8] = [
    TOOL_FOLDER_NAMES[0],
    TOOL_FOLDER_NAMES[1],
    SYSTEM_FOLDER_NAMES[0],
    SYSTEM_FOLDER_NAMES[1],
    SYSTEM_FOLDER_NAMES[2],
    SYSTEM_FOLDER_NAMES[3],
    SYSTEM_FOLDER_NAMES[4],
    SYSTEM_FOLDER_NAMES[5],
];

/// Path globs never indexed, under any root:
/// - the per-user temp folder (`AppData\Local\Temp`);
/// - the Windows component store (`Windows\WinSxS`): ~100k hard-linked copies of system files,
///   often a tenth of a system drive's entries, never what a user is looking for;
/// - Windows' launch traces and application-compatibility databases (`Windows\Prefetch`,
///   `Windows\appcompat`): rewritten every time a program starts, unreadable without
///   administrator rights, so watching them only produced "access denied" noise.
pub const DEFAULT_EXCLUDED_PATH_GLOBS: [&str; 4] = [
    "**/AppData/Local/Temp",
    "**/Windows/WinSxS",
    "**/Windows/Prefetch",
    "**/Windows/appcompat",
];

/// Pseudo file systems skipped when `/` is indexed (Linux and other Unixes): `/proc`, `/sys`,
/// `/dev`. Not used on Windows.
pub const UNIX_EXCLUDED_PREFIXES: [&str; 3] = ["/proc", "/sys", "/dev"];

impl WalkOptions {
    /// Adds the default exclusions (see the [module docs](crate::exclude)):
    /// [`DEFAULT_EXCLUDED_NAMES`], [`DEFAULT_EXCLUDED_PATH_GLOBS`] and, on Unix,
    /// [`UNIX_EXCLUDED_PREFIXES`].
    pub fn with_default_exclusions(mut self) -> Self {
        for glob in DEFAULT_EXCLUDED_NAMES
            .iter()
            .chain(&DEFAULT_EXCLUDED_PATH_GLOBS)
        {
            self = self.exclude_glob(*glob);
        }
        if cfg!(unix) {
            for prefix in UNIX_EXCLUDED_PREFIXES {
                self = self.exclude_prefix(prefix);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::walk;
    use std::collections::BTreeSet;
    use std::fs;

    #[test]
    fn the_names_are_the_tool_then_the_system_folders() {
        let joined: Vec<&str> = TOOL_FOLDER_NAMES
            .iter()
            .chain(&SYSTEM_FOLDER_NAMES)
            .copied()
            .collect();
        assert_eq!(DEFAULT_EXCLUDED_NAMES.as_slice(), joined.as_slice());
        let unique: BTreeSet<String> = DEFAULT_EXCLUDED_NAMES
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        assert_eq!(unique.len(), DEFAULT_EXCLUDED_NAMES.len(), "no duplicates");
        for glob in DEFAULT_EXCLUDED_PATH_GLOBS {
            assert!(glob.contains('/'), "{glob} must be a path glob");
        }
    }

    #[test]
    fn default_exclusions_skip_tools_system_and_temp_folders() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for d in [
            "docs",
            ".git/objects",
            "src/node_modules/pkg",
            "$RECYCLE.BIN/S-1",
            "System Volume Information",
            "$WinREAgent/Scratch",
            "Users/me/AppData/Local/Temp/x",
            "Users/me/AppData/Local/keep",
            "Windows/WinSxS/amd64",
            "Windows/Prefetch",
            "Windows/appcompat/Programs",
            "Windows/System32",
        ] {
            fs::create_dir_all(p.join(d)).unwrap();
        }
        fs::write(p.join("docs").join("تقرير.pdf"), b"x").unwrap();
        let ix = walk(&WalkOptions::new([p]).with_default_exclusions()).unwrap();
        let paths: BTreeSet<String> = (1..ix.len() as crate::EntryId)
            .map(|id| {
                ix.path(id)
                    .strip_prefix(p)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        let expected: BTreeSet<String> = [
            "docs",
            "docs/تقرير.pdf",
            "src",
            "Users",
            "Users/me",
            "Users/me/AppData",
            "Users/me/AppData/Local",
            "Users/me/AppData/Local/keep",
            "Windows",
            "Windows/System32",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(paths, expected);
    }
}
