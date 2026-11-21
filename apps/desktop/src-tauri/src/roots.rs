//! Which folders are indexed: "the whole computer" (the default) or a chosen list of drives /
//! folders, persisted in `<app_data>/indexed-roots.json` so start-up uses the choice before the UI
//! loads.
//!
//! File layout: `{"version":1,"roots":null}` for the whole computer (re-resolved at every start,
//! so a new fixed drive is picked up), or `{"version":1,"roots":["C:\\","D:\\"]}`. A missing,
//! unreadable, corrupt or other-version file means the whole computer (logged).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::volumes::{Volume, is_drive_root, whole_computer_roots};

/// File name of the roots setting inside the app data folder.
pub const ROOTS_SETTING_FILE: &str = "indexed-roots.json";

const ROOTS_FILE_VERSION: u32 = 1;

/// The user's choice of what to index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RootsChoice {
    /// Every fixed drive (Windows) or `/` (elsewhere), resolved at each start.
    #[default]
    WholeComputer,
    /// Exactly these folders (normalized by [`normalize_roots`]; never empty).
    Custom(Vec<PathBuf>),
}

/// What `get_index_roots` / `set_index_roots` return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRoots {
    /// The choice is "the whole computer".
    pub whole_computer: bool,
    /// The folders that choice indexes now.
    pub roots: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RootsFile {
    version: u32,
    roots: Option<Vec<String>>,
}

/// The persisted roots setting: where it lives and its current value.
#[derive(Debug, Default)]
pub struct RootsSetting {
    inner: Mutex<SettingState>,
}

#[derive(Debug, Default)]
struct SettingState {
    /// Unset: not persisted (no app data folder, or in tests).
    file: Option<PathBuf>,
    choice: RootsChoice,
}

impl RootsSetting {
    /// Points the setting at `file` and loads it (see the module docs for fallbacks).
    pub fn load_from(&self, file: PathBuf) {
        let choice = read_choice(&file);
        let mut s = self.lock();
        s.choice = choice;
        s.file = Some(file);
    }

    /// The current choice.
    pub fn choice(&self) -> RootsChoice {
        self.lock().choice.clone()
    }

    /// Replaces the choice and writes it to the file, if one is set. The new choice is kept
    /// even if writing fails (the error is returned so the UI can say it won't survive a restart).
    pub fn set(&self, choice: RootsChoice) -> io::Result<()> {
        let mut s = self.lock();
        s.choice = choice;
        match &s.file {
            Some(file) => write_choice(file, &s.choice),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SettingState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The folders `choice` indexes, given the volumes of this computer. The whole computer with no
/// fixed drive found falls back to the user's home folder.
pub fn resolve(choice: &RootsChoice, volumes: &[Volume]) -> Result<Vec<PathBuf>, String> {
    match choice {
        RootsChoice::Custom(roots) => Ok(roots.clone()),
        RootsChoice::WholeComputer => {
            let roots = whole_computer_roots(volumes);
            if roots.is_empty() {
                tracing::warn!("no fixed drive found; indexing the home folder instead");
                home_roots()
            } else {
                Ok(roots)
            }
        }
    }
}

/// Cleans a list of folders from the UI: trims, turns `d:` / `d:/` into `D:\`, rejects
/// relative paths, drops duplicates (case-insensitively on Windows) and folders inside another
/// listed folder (they would be indexed twice). Errors (English) on a relative path or an
/// empty result.
pub fn normalize_roots(roots: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for raw in roots {
        let root = normalize_root(raw)?;
        let key = path_key(&root);
        out.push((root, key));
    }
    let keys: Vec<String> = out.iter().map(|(_, k)| k.clone()).collect();
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut kept_keys: Vec<&str> = Vec::new();
    for (i, (root, key)) in out.iter().enumerate() {
        let duplicate = kept_keys.contains(&key.as_str());
        let nested = keys
            .iter()
            .enumerate()
            .any(|(j, other)| j != i && other != key && key_has_prefix(key, other));
        if !duplicate && !nested {
            kept.push(root.clone());
            kept_keys.push(key);
        }
    }
    if kept.is_empty() {
        return Err("choose at least one drive or folder to index".to_owned());
    }
    Ok(kept)
}

/// One folder from the UI, see [`normalize_roots`].
pub fn normalize_root(raw: &str) -> Result<PathBuf, String> {
    let s = raw.trim();
    if is_drive_root(s) {
        return Ok(PathBuf::from(format!("{}:\\", s[..1].to_ascii_uppercase())));
    }
    let path = PathBuf::from(s);
    if s.is_empty() || !path.is_absolute() {
        return Err(format!(
            "\"{raw}\" is not an absolute path or a drive like D:"
        ));
    }
    Ok(path)
}

/// Comparable form of a path: `/` separators, no trailing separator, lowercase on Windows
/// (`C:\Users\` → `c:/users`, `/` → ``). Same rules as the walker's exclusion prefixes.
pub fn path_key(path: &Path) -> String {
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

/// True if `key` equals `prefix` or lies below it (both from [`path_key`]).
pub fn key_has_prefix(key: &str, prefix: &str) -> bool {
    key.starts_with(prefix) && (key.len() == prefix.len() || key.as_bytes()[prefix.len()] == b'/')
}

/// The user's home folder (`USERPROFILE` on Windows, `HOME` elsewhere).
///
/// Read from the environment instead of `std::env::home_dir`, which is deprecated on the MSRV
/// (Rust 1.85) and would fail clippy with `-D warnings` there.
pub fn home_roots() -> Result<Vec<PathBuf>, String> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|p| !p.is_empty())
        .map(|p| vec![PathBuf::from(p)])
        .ok_or_else(|| "cannot find the user's home folder".to_owned())
}

fn read_choice(file: &Path) -> RootsChoice {
    let bytes = match fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return RootsChoice::WholeComputer,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the indexed-roots setting; indexing the whole computer");
            return RootsChoice::WholeComputer;
        }
    };
    match serde_json::from_slice::<RootsFile>(&bytes) {
        Ok(f) if f.version == ROOTS_FILE_VERSION => match f.roots {
            None => RootsChoice::WholeComputer,
            Some(roots) => match normalize_roots(&roots) {
                Ok(roots) => RootsChoice::Custom(roots),
                Err(e) => {
                    tracing::warn!(error = %e, "invalid indexed-roots setting; indexing the whole computer");
                    RootsChoice::WholeComputer
                }
            },
        },
        Ok(f) => {
            tracing::warn!(
                version = f.version,
                "indexed-roots setting has another version; ignoring it"
            );
            RootsChoice::WholeComputer
        }
        Err(e) => {
            tracing::warn!(error = %e, "indexed-roots setting is corrupt; indexing the whole computer");
            RootsChoice::WholeComputer
        }
    }
}

/// Writes atomically: a temporary file next to `file`, then a rename over it.
fn write_choice(file: &Path, choice: &RootsChoice) -> io::Result<()> {
    let body = RootsFile {
        version: ROOTS_FILE_VERSION,
        roots: match choice {
            RootsChoice::WholeComputer => None,
            RootsChoice::Custom(roots) => Some(
                roots
                    .iter()
                    .map(|r| r.to_string_lossy().into_owned())
                    .collect(),
            ),
        },
    };
    let json = serde_json::to_vec_pretty(&body).map_err(io::Error::other)?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volumes::VolumeKind;

    fn abs(p: &str) -> String {
        if cfg!(windows) {
            format!("C:\\{p}")
        } else {
            format!("/{p}")
        }
    }

    #[test]
    fn persisted_roots_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join(ROOTS_SETTING_FILE);

        let setting = RootsSetting::default();
        setting.load_from(file.clone());
        assert_eq!(setting.choice(), RootsChoice::WholeComputer, "no file yet");

        let custom = RootsChoice::Custom(normalize_roots(&[abs("data"), abs("docs")]).unwrap());
        setting.set(custom.clone()).unwrap();
        let reloaded = RootsSetting::default();
        reloaded.load_from(file.clone());
        assert_eq!(reloaded.choice(), custom);

        reloaded.set(RootsChoice::WholeComputer).unwrap();
        let again = RootsSetting::default();
        again.load_from(file.clone());
        assert_eq!(again.choice(), RootsChoice::WholeComputer);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(json, serde_json::json!({"version": 1, "roots": null}));
    }

    #[test]
    fn a_bad_file_means_the_whole_computer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(ROOTS_SETTING_FILE);
        for body in [
            &b"not json"[..],
            br#"{"version":99,"roots":["C:\\"]}"#,
            br#"{"version":1,"roots":[]}"#,
            br#"{"version":1,"roots":["relative/path"]}"#,
        ] {
            fs::write(&file, body).unwrap();
            let s = RootsSetting::default();
            s.load_from(file.clone());
            assert_eq!(s.choice(), RootsChoice::WholeComputer, "{body:?}");
        }
    }

    #[test]
    fn normalizes_drives_duplicates_and_nested_folders() {
        if cfg!(windows) {
            assert_eq!(
                normalize_roots(&[
                    "d:".into(),
                    "C:/".into(),
                    "D:\\".into(),
                    " c:\\Users ".into()
                ])
                .unwrap(),
                [PathBuf::from("D:\\"), PathBuf::from("C:\\")]
            );
        }
        assert_eq!(
            normalize_roots(&[abs("a/b"), abs("a"), abs("c"), abs("a")]).unwrap(),
            [PathBuf::from(abs("a")), PathBuf::from(abs("c"))]
        );
        assert!(normalize_roots(&[]).is_err());
        assert!(normalize_roots(&["relative".into()]).is_err());
        assert!(normalize_roots(&["  ".into()]).is_err());
    }

    #[test]
    fn resolves_the_whole_computer_from_volumes() {
        let vols = vec![
            Volume {
                path: "C:\\".into(),
                name: "C:".into(),
                label: None,
                kind: VolumeKind::Fixed,
            },
            Volume {
                path: "D:\\".into(),
                name: "D:".into(),
                label: Some("Data".into()),
                kind: VolumeKind::Fixed,
            },
            Volume {
                path: "E:\\".into(),
                name: "E:".into(),
                label: None,
                kind: VolumeKind::Removable,
            },
        ];
        let whole = resolve(&RootsChoice::WholeComputer, &vols).unwrap();
        if cfg!(windows) {
            assert_eq!(whole, [PathBuf::from("C:\\"), PathBuf::from("D:\\")]);
            // No fixed drive at all: the home folder.
            assert_eq!(
                resolve(&RootsChoice::WholeComputer, &vols[2..]).unwrap(),
                home_roots().unwrap()
            );
        } else {
            assert_eq!(whole, [PathBuf::from("/")]);
        }
        let custom = RootsChoice::Custom(vec![PathBuf::from(abs("x"))]);
        assert_eq!(resolve(&custom, &vols).unwrap(), [PathBuf::from(abs("x"))]);
    }

    #[test]
    fn path_keys_compare_like_the_walker() {
        if cfg!(windows) {
            assert_eq!(path_key(Path::new("C:\\Users\\Me\\")), "c:/users/me");
            assert_eq!(path_key(Path::new("C:\\")), "c:");
        }
        assert_eq!(path_key(Path::new("/")), "");
        assert!(key_has_prefix("c:/users", "c:"));
        assert!(key_has_prefix("c:", "c:"));
        assert!(!key_has_prefix("c:/users", "c:/use"));
        assert!(key_has_prefix("/home/me", ""));
    }
}
