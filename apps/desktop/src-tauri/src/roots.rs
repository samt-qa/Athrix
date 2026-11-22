//! Which folders are indexed: "the whole computer" (the default) or a chosen list of drives /
//! folders, persisted in `<app_data>/indexed-roots.json` so start-up uses the choice before the UI
//! loads.
//!
//! File layout: `{"version":1,"roots":null}` for the whole computer (re-resolved at every start,
//! so a new fixed drive is picked up), or `{"version":1,"roots":["C:\\","D:\\"]}`. A missing,
//! unreadable, corrupt or other-version file means the whole computer (logged).
//!
//! **External drives (task 2.5).** [`DriveSettings`] (`<app_data>/drive-settings.json`,
//! `{"version":1,"includeRemovable":true,"includeNetwork":false}`) says whether "the whole
//! computer" also takes in removable drives (USB sticks, memory cards; default on) and mapped
//! network drives (default off). [`plan`] turns the choice, the volumes present and those
//! settings into a [`RootPlan`]: the roots to index and which of them are offline right now (a
//! chosen folder on an unplugged drive stays chosen, it is just offline until it comes back).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::volumes::{Volume, VolumeKind, is_drive_root, whole_computer_roots};

/// File name of the roots setting inside the app data folder.
pub const ROOTS_SETTING_FILE: &str = "indexed-roots.json";

/// File name of the external-drive settings inside the app data folder.
pub const DRIVE_SETTINGS_FILE: &str = "drive-settings.json";

const ROOTS_FILE_VERSION: u32 = 1;
const DRIVE_SETTINGS_VERSION: u32 = 1;

/// Which drives besides the fixed ones "the whole computer" indexes (`get_drive_settings` /
/// `set_drive_settings`). Explicitly chosen roots are indexed whatever these say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveSettings {
    /// Removable drives (USB sticks, memory cards) are indexed while plugged in. Default on.
    pub include_removable: bool,
    /// Mapped network drives are indexed while connected. Default off (slow, and often huge).
    pub include_network: bool,
}

impl Default for DriveSettings {
    fn default() -> Self {
        DriveSettings {
            include_removable: true,
            include_network: false,
        }
    }
}

impl DriveSettings {
    /// True if "the whole computer" takes in a drive of `kind`.
    pub fn includes(&self, kind: VolumeKind) -> bool {
        match kind {
            VolumeKind::Fixed => true,
            VolumeKind::Removable => self.include_removable,
            VolumeKind::Network => self.include_network,
            VolumeKind::Unknown => false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveSettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: DriveSettings,
}

/// The persisted [`DriveSettings`]. A missing, corrupt or other-version file means the
/// defaults (logged).
#[derive(Debug, Default)]
pub struct DriveSettingsStore {
    inner: Mutex<(Option<PathBuf>, DriveSettings)>,
}

impl DriveSettingsStore {
    /// Points the setting at `file` and loads it.
    pub fn load_from(&self, file: PathBuf) {
        let settings = read_drive_settings(&file);
        *self.lock() = (Some(file), settings);
    }

    /// The current settings.
    pub fn get(&self) -> DriveSettings {
        self.lock().1
    }

    /// Replaces the settings and writes them, if a file is set. The new value is kept even if
    /// writing fails (the error is returned for logging).
    pub fn set(&self, settings: DriveSettings) -> io::Result<()> {
        let mut s = self.lock();
        s.1 = settings;
        match &s.0 {
            Some(file) => {
                let body = DriveSettingsFile {
                    version: DRIVE_SETTINGS_VERSION,
                    settings,
                };
                write_atomic(
                    file,
                    &serde_json::to_vec_pretty(&body).map_err(io::Error::other)?,
                )
            }
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Option<PathBuf>, DriveSettings)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn read_drive_settings(file: &Path) -> DriveSettings {
    let bytes = match fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return DriveSettings::default(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the drive settings; using the defaults");
            return DriveSettings::default();
        }
    };
    match serde_json::from_slice::<DriveSettingsFile>(&bytes) {
        Ok(f) if f.version == DRIVE_SETTINGS_VERSION => f.settings,
        Ok(f) => {
            tracing::warn!(
                version = f.version,
                "drive settings have another version; ignoring them"
            );
            DriveSettings::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "drive settings are corrupt; using the defaults");
            DriveSettings::default()
        }
    }
}

/// What to index right now: every root of the choice, and which of them are offline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RootPlan {
    /// The roots, in path order for "the whole computer", in the chosen order otherwise.
    /// Includes the offline ones.
    pub roots: Vec<PathBuf>,
    /// The roots whose drive is not present now (a subset of `roots`).
    pub offline: Vec<PathBuf>,
}

impl RootPlan {
    /// The roots that can be walked now (`roots` minus `offline`).
    pub fn online(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .filter(|r| !self.offline.contains(r))
            .cloned()
            .collect()
    }
}

/// The [`RootPlan`] of `choice`, given the volumes present now and the drive settings.
///
/// - The whole computer: every fixed drive, plus every removable / network drive that
///   `settings` takes in. `remembered` are drive roots seen earlier in this session (removable
///   or network drives that were indexed): those that are gone now stay in the plan, offline,
///   so their entries are only hidden and plugging the drive back in is a re-walk of that
///   drive alone. No drive at all falls back to the home folder (like [`resolve`]).
/// - Chosen roots: all of them, those whose drive is missing marked offline
///   ([`root_available`]).
pub fn plan(
    choice: &RootsChoice,
    volumes: &[Volume],
    settings: DriveSettings,
    remembered: &[PathBuf],
) -> Result<RootPlan, String> {
    match choice {
        RootsChoice::Custom(roots) => Ok(RootPlan {
            roots: roots.clone(),
            offline: roots
                .iter()
                .filter(|r| !root_available(r, volumes))
                .cloned()
                .collect(),
        }),
        RootsChoice::WholeComputer => {
            let fixed = resolve(choice, volumes)?;
            if !cfg!(windows) {
                return Ok(RootPlan {
                    roots: fixed,
                    offline: Vec::new(),
                });
            }
            let mut roots = fixed;
            for v in volumes {
                if v.kind != VolumeKind::Fixed
                    && settings.includes(v.kind)
                    && is_drive_root(&v.path)
                {
                    roots.push(PathBuf::from(&v.path));
                }
            }
            let mut offline = Vec::new();
            for r in remembered {
                let key = path_key(r);
                if !roots.iter().any(|x| path_key(x) == key) {
                    offline.push(r.clone());
                    roots.push(r.clone());
                }
            }
            roots.sort_by_key(|r| path_key(r));
            Ok(RootPlan { roots, offline })
        }
    }
}

/// True if the drive (or mount) `root` lies on is present in `volumes`: on Windows the drive
/// letter must be listed (UNC paths count as present); elsewhere a root under `/media/x`,
/// `/mnt/x` or `/Volumes/x` needs that mount listed, anything else lives on `/`.
pub fn root_available(root: &Path, volumes: &[Volume]) -> bool {
    let s = root.to_string_lossy();
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return volumes
            .iter()
            .any(|v| v.path.len() >= 2 && v.path[..2].eq_ignore_ascii_case(&s[..2]));
    }
    for prefix in ["/media/", "/mnt/", "/Volumes/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let mount = match rest.find('/') {
                Some(i) => &s[..prefix.len() + i],
                None => &s[..],
            };
            return volumes.iter().any(|v| v.path == mount);
        }
    }
    true
}

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
    /// The folders that choice indexes now (offline ones excluded).
    pub roots: Vec<String>,
    /// Chosen folders (or remembered external drives) whose drive is not plugged in now: they
    /// stay in the choice and come back on their own.
    pub offline: Vec<String>,
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
    write_atomic(file, &json)
}

/// Writes `bytes` to a temporary file next to `file`, then renames it over `file`.
fn write_atomic(file: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
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

    fn vol(path: &str, kind: VolumeKind) -> Volume {
        Volume {
            path: path.into(),
            name: path.trim_end_matches('\\').into(),
            label: None,
            kind,
            serial: None,
        }
    }

    #[test]
    fn drive_settings_default_round_trip_and_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(DRIVE_SETTINGS_FILE);
        let store = DriveSettingsStore::default();
        store.load_from(file.clone());
        assert_eq!(
            store.get(),
            DriveSettings {
                include_removable: true,
                include_network: false
            },
            "defaults: removable on, network off"
        );
        let changed = DriveSettings {
            include_removable: false,
            include_network: true,
        };
        store.set(changed).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"version": 1, "includeRemovable": false, "includeNetwork": true})
        );
        let again = DriveSettingsStore::default();
        again.load_from(file.clone());
        assert_eq!(again.get(), changed);
        for bad in [
            &b"{oops"[..],
            br#"{"version":2,"includeRemovable":false,"includeNetwork":true}"#,
        ] {
            fs::write(&file, bad).unwrap();
            let s = DriveSettingsStore::default();
            s.load_from(file.clone());
            assert_eq!(s.get(), DriveSettings::default());
        }
    }

    #[test]
    fn the_whole_computer_takes_external_drives_per_the_settings() {
        let vols = [
            vol("C:\\", VolumeKind::Fixed),
            vol("E:\\", VolumeKind::Removable),
            vol("Z:\\", VolumeKind::Network),
            vol("X:\\", VolumeKind::Unknown),
        ];
        let roots = |settings, remembered: &[PathBuf]| {
            plan(&RootsChoice::WholeComputer, &vols, settings, remembered).unwrap()
        };
        let p = roots(DriveSettings::default(), &[]);
        if cfg!(windows) {
            assert_eq!(p.roots, [PathBuf::from("C:\\"), PathBuf::from("E:\\")]);
            assert!(p.offline.is_empty());
            let none = DriveSettings {
                include_removable: false,
                include_network: false,
            };
            assert_eq!(roots(none, &[]).roots, [PathBuf::from("C:\\")]);
            let all = DriveSettings {
                include_removable: true,
                include_network: true,
            };
            assert_eq!(roots(all, &[]).roots.len(), 3, "unknown kinds never");
            // A stick seen earlier and unplugged since stays known, offline.
            let p = roots(DriveSettings::default(), &[PathBuf::from("F:\\")]);
            assert_eq!(
                p.roots,
                [
                    PathBuf::from("C:\\"),
                    PathBuf::from("E:\\"),
                    PathBuf::from("F:\\")
                ]
            );
            assert_eq!(p.offline, [PathBuf::from("F:\\")]);
            assert_eq!(p.online(), [PathBuf::from("C:\\"), PathBuf::from("E:\\")]);
            // ...and is not offline while it is plugged in.
            assert!(
                roots(DriveSettings::default(), &[PathBuf::from("E:\\")])
                    .offline
                    .is_empty()
            );
        } else {
            assert_eq!(p.roots, [PathBuf::from("/")]);
        }
    }

    #[test]
    fn chosen_roots_on_missing_drives_are_offline_not_dropped() {
        let vols = [
            vol("C:\\", VolumeKind::Fixed),
            vol("/media/usb", VolumeKind::Removable),
        ];
        let chosen =
            |paths: &[&str]| RootsChoice::Custom(paths.iter().map(PathBuf::from).collect());
        let p = plan(
            &chosen(&[
                "C:\\Users",
                "E:\\Photos",
                "/media/usb/docs",
                "/media/other/x",
                "/home/me",
            ]),
            &vols,
            DriveSettings::default(),
            &[],
        )
        .unwrap();
        assert_eq!(p.roots.len(), 5, "every chosen root stays");
        assert_eq!(
            p.offline,
            [PathBuf::from("E:\\Photos"), PathBuf::from("/media/other/x")]
        );
        assert!(
            root_available(Path::new("c:\\data"), &vols),
            "letters ignore case"
        );
        assert!(
            root_available(Path::new("\\\\server\\share"), &vols),
            "UNC: assumed present"
        );
        assert!(root_available(Path::new("/media/usb"), &vols));
        // The settings do not touch chosen roots.
        let off = DriveSettings {
            include_removable: false,
            include_network: false,
        };
        assert_eq!(
            plan(&chosen(&["/media/usb/docs"]), &vols, off, &[])
                .unwrap()
                .roots
                .len(),
            1
        );
    }

    #[test]
    fn resolves_the_whole_computer_from_volumes() {
        let vols = vec![
            vol("C:\\", VolumeKind::Fixed),
            vol("D:\\", VolumeKind::Fixed),
            vol("E:\\", VolumeKind::Removable),
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
