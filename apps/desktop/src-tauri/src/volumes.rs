//! The drives (volumes) of this computer, for the "which drives to index" setting and the
//! search-scope selector.
//!
//! Listed with `sysinfo` (a safe wrapper over the OS APIs; this crate denies `unsafe`):
//! - **Windows**: every local volume mounted on a drive letter (`C:\`, `D:\`...), fixed or
//!   removable, with its label. Network shares and optical drives are not listed by the API we
//!   use, so they are never indexed by default (they can still be passed to `start_indexing`).
//!   If the API returns nothing, existing drive letters are probed instead (kind `unknown`).
//! - **Elsewhere**: `/`, plus volumes mounted under `/media`, `/mnt` or `/Volumes`.
//!
//! "The whole computer" ([`whole_computer_roots`]) is every fixed drive on Windows, `/` elsewhere.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// What kind of storage a volume is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeKind {
    /// An internal (non-removable) disk. Indexed by default.
    Fixed,
    /// A USB stick, memory card... Not indexed by default.
    Removable,
    /// A network share. Not indexed by default.
    Network,
    /// Could not be determined. Not indexed by default.
    Unknown,
}

/// One volume, as sent to the UI by `list_volumes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    /// The root folder to index, e.g. `C:\` or `/media/usb`.
    pub path: String,
    /// Short name for the UI: `C:` on Windows, the path elsewhere.
    pub name: String,
    /// The volume label ("Data", "Windows"...), if any.
    pub label: Option<String>,
    /// Fixed, removable...
    pub kind: VolumeKind,
}

/// Lists the volumes of this computer, sorted by path (drive letter on Windows). May block for
/// a moment (it asks the OS about each volume): call it off the UI thread.
pub fn list_volumes() -> Vec<Volume> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut found: Vec<Volume> = disks
        .list()
        .iter()
        .filter(|d| is_listed_mount(d.mount_point()))
        .map(|d| {
            let label = d.name().to_string_lossy().trim().to_owned();
            volume(
                d.mount_point(),
                (!label.is_empty()).then_some(label),
                if d.is_removable() {
                    VolumeKind::Removable
                } else {
                    VolumeKind::Fixed
                },
            )
        })
        .collect();
    if cfg!(windows) && found.is_empty() {
        found = probe_drive_letters();
    }
    if !cfg!(windows) && !found.iter().any(|v| v.path == "/") {
        found.push(volume(Path::new("/"), None, VolumeKind::Fixed));
    }
    sort_and_dedup(found)
}

/// The roots of "the whole computer" among `volumes`: every fixed drive on Windows; `/`
/// elsewhere (other mounts live under it). Empty if there is none.
pub fn whole_computer_roots(volumes: &[Volume]) -> Vec<PathBuf> {
    if cfg!(windows) {
        volumes
            .iter()
            .filter(|v| v.kind == VolumeKind::Fixed && is_drive_root(&v.path))
            .map(|v| PathBuf::from(&v.path))
            .collect()
    } else {
        vec![PathBuf::from("/")]
    }
}

/// True if `path` is a drive root such as `C:\` (or `C:/`, `C:`).
pub fn is_drive_root(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')))
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
}

/// Builds a [`Volume`] for `mount`.
fn volume(mount: &Path, label: Option<String>, kind: VolumeKind) -> Volume {
    let path = mount.to_string_lossy().into_owned();
    let name = if is_drive_root(&path) {
        path[..2].to_ascii_uppercase()
    } else {
        path.clone()
    };
    let path = if is_drive_root(&path) {
        format!("{name}\\")
    } else {
        path
    };
    Volume {
        path,
        name,
        label,
        kind,
    }
}

/// Which mount points the drive picker shows: drive letters on Windows; `/` and removable or
/// extra volumes under `/media`, `/mnt`, `/Volumes` elsewhere (not `/boot`, `/snap/...`).
fn is_listed_mount(mount: &Path) -> bool {
    let s = mount.to_string_lossy();
    if cfg!(windows) {
        return is_drive_root(&s);
    }
    s == "/"
        || ["/media/", "/mnt/", "/Volumes/"]
            .iter()
            .any(|p| s.starts_with(p) && s.len() > p.len())
}

/// Fallback when the volume API lists nothing: the drive letters whose root can be read.
fn probe_drive_letters() -> Vec<Volume> {
    (b'A'..=b'Z')
        .map(|l| format!("{}:\\", l as char))
        .filter(|root| Path::new(root).is_dir())
        .map(|root| volume(Path::new(&root), None, VolumeKind::Unknown))
        .collect()
}

fn sort_and_dedup(mut volumes: Vec<Volume>) -> Vec<Volume> {
    volumes.sort_by_key(|v| v.path.to_lowercase());
    volumes.dedup_by(|a, b| a.path.eq_ignore_ascii_case(&b.path));
    volumes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_something_sane_on_this_machine() {
        let volumes = list_volumes();
        assert!(!volumes.is_empty(), "at least the system drive / root");
        for v in &volumes {
            assert!(Path::new(&v.path).is_absolute(), "{v:?}");
            assert!(!v.name.is_empty(), "{v:?}");
        }
        let mut sorted = volumes.clone();
        sorted.sort_by_key(|v| v.path.to_lowercase());
        assert_eq!(volumes, sorted, "sorted by path");
        if cfg!(windows) {
            assert!(
                volumes
                    .iter()
                    .all(|v| is_drive_root(&v.path) && v.path.ends_with('\\'))
            );
            // The system drive is always there and fixed.
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let sys = volumes
                .iter()
                .find(|v| v.name.eq_ignore_ascii_case(&system))
                .expect("system drive listed");
            assert_eq!(sys.kind, VolumeKind::Fixed);
            assert!(whole_computer_roots(&volumes).contains(&PathBuf::from(&sys.path)));
        } else {
            assert!(volumes.iter().any(|v| v.path == "/"));
            assert_eq!(whole_computer_roots(&volumes), vec![PathBuf::from("/")]);
        }
    }

    #[test]
    fn drive_roots_are_recognized_and_named() {
        for root in ["C:\\", "d:/", "E:"] {
            assert!(is_drive_root(root), "{root}");
        }
        for not in ["C:\\Users", "/", "CD:\\", "1:\\", "", "\\\\server\\share"] {
            assert!(!is_drive_root(not), "{not}");
        }
        let v = volume(Path::new("d:\\"), Some("Data".into()), VolumeKind::Fixed);
        assert_eq!((v.path.as_str(), v.name.as_str()), ("D:\\", "D:"));
        let m = volume(Path::new("/media/usb"), None, VolumeKind::Removable);
        assert_eq!(
            (m.path.as_str(), m.name.as_str()),
            ("/media/usb", "/media/usb")
        );
    }

    #[test]
    fn whole_computer_is_the_fixed_drives() {
        let vols = sort_and_dedup(vec![
            volume(Path::new("D:\\"), None, VolumeKind::Fixed),
            volume(Path::new("C:\\"), Some("Windows".into()), VolumeKind::Fixed),
            volume(Path::new("c:\\"), None, VolumeKind::Fixed),
            volume(Path::new("E:\\"), None, VolumeKind::Removable),
            volume(Path::new("F:\\"), None, VolumeKind::Unknown),
        ]);
        assert_eq!(
            vols.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["C:", "D:", "E:", "F:"]
        );
        let roots = whole_computer_roots(&vols);
        if cfg!(windows) {
            assert_eq!(roots, [PathBuf::from("C:\\"), PathBuf::from("D:\\")]);
        } else {
            assert_eq!(roots, [PathBuf::from("/")]);
        }
    }

    #[test]
    fn serializes_for_the_ui() {
        let v = volume(Path::new("C:\\"), Some("OS".into()), VolumeKind::Removable);
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            serde_json::json!({"path": "C:\\", "name": "C:", "label": "OS", "kind": "removable"})
        );
    }
}
