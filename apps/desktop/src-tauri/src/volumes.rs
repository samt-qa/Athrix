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
//!
//! **Hot plug (task 2.5).** On Windows every listed volume also carries its serial number (from
//! [`athrix_ntfs::drives::probe_drives`]), and mapped network drives are listed too (kind
//! `network`, indexed only if the user asks). [`signature`] is the cheap poll the drive monitor
//! ([`crate::drives`]) runs every few seconds; [`diff`] compares two lists by mount path *and*
//! serial, so another USB stick on the same letter counts as a removal plus an arrival.

use std::path::{Path, PathBuf};

use athrix_ntfs::drives::{DriveProbe, DriveType, probe_drives};
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
    /// Volume serial number (Windows, local drives). Not sent to the UI: it tells two volumes
    /// mounted on the same path apart (see [`diff`]).
    #[serde(skip)]
    pub serial: Option<u32>,
}

impl Volume {
    /// True if `self` and `other` are the same volume at the same place: same path (case
    /// aside), same kind and, when both are known, the same serial number.
    pub fn same_as(&self, other: &Volume) -> bool {
        self.path.eq_ignore_ascii_case(&other.path)
            && self.kind == other.kind
            && match (self.serial, other.serial) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// What changed between two volume lists (see [`diff`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeChanges {
    /// Volumes of the new list that were not in the old one (plugged in, or another volume on
    /// a path that was in use).
    pub added: Vec<Volume>,
    /// Volumes of the old list that are gone (unplugged, or replaced by another volume).
    pub removed: Vec<Volume>,
}

impl VolumeChanges {
    /// True if nothing was added or removed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Compares two volume lists by identity ([`Volume::same_as`]); a label change alone is no
/// change.
pub fn diff(old: &[Volume], new: &[Volume]) -> VolumeChanges {
    VolumeChanges {
        added: new
            .iter()
            .filter(|n| !old.iter().any(|o| o.same_as(n)))
            .cloned()
            .collect(),
        removed: old
            .iter()
            .filter(|o| !new.iter().any(|n| n.same_as(o)))
            .cloned()
            .collect(),
    }
}

/// A cheap fingerprint of the mounted volumes, for polling: when it changes, call
/// [`list_volumes`] (slower: it asks the OS about labels and sizes too) and [`diff`].
///
/// Windows: the drive letters with their type and serial number (a few microseconds, no disk
/// access). Elsewhere: the listed mount points (`sysinfo` reads the mount table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(Vec<(String, u8, Option<u32>)>);

/// The current [`Signature`].
pub fn signature() -> Signature {
    if cfg!(windows) {
        Signature(
            probe_drives()
                .into_iter()
                .map(|d| (d.root, drive_type_code(d.kind), d.serial))
                .collect(),
        )
    } else {
        Signature(
            list_volumes()
                .into_iter()
                .map(|v| (v.path, 0, None))
                .collect(),
        )
    }
}

fn drive_type_code(kind: DriveType) -> u8 {
    match kind {
        DriveType::Unknown => 0,
        DriveType::Fixed => 1,
        DriveType::Removable => 2,
        DriveType::Remote => 3,
        DriveType::CdRom => 4,
        DriveType::RamDisk => 5,
    }
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
    if cfg!(windows) {
        if found.is_empty() {
            found = probe_drive_letters();
        }
        add_probe_details(&mut found, &probe_drives());
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
        serial: None,
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

/// Fills in the serial numbers from the drive-letter probe and adds the mapped network drives
/// (which the volume API does not list).
fn add_probe_details(found: &mut Vec<Volume>, probed: &[DriveProbe]) {
    for v in found.iter_mut() {
        if let Some(p) = probed.iter().find(|p| p.root.eq_ignore_ascii_case(&v.path)) {
            v.serial = p.serial;
        }
    }
    for p in probed {
        if p.kind == DriveType::Remote
            && !found.iter().any(|v| v.path.eq_ignore_ascii_case(&p.root))
        {
            found.push(volume(Path::new(&p.root), None, VolumeKind::Network));
        }
    }
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

    fn with_serial(path: &str, kind: VolumeKind, serial: Option<u32>) -> Volume {
        Volume {
            serial,
            ..volume(Path::new(path), None, kind)
        }
    }

    #[test]
    fn diff_tells_plugged_removed_and_swapped_volumes_apart() {
        let c = with_serial("C:\\", VolumeKind::Fixed, Some(1));
        let usb_a = with_serial("E:\\", VolumeKind::Removable, Some(0xA));
        let usb_b = with_serial("E:\\", VolumeKind::Removable, Some(0xB));
        let before = vec![c.clone()];

        // Plugged in.
        let plugged = vec![c.clone(), usb_a.clone()];
        let d = diff(&before, &plugged);
        assert_eq!(d.added, std::slice::from_ref(&usb_a));
        assert!(d.removed.is_empty());
        // Nothing changed (a label change is no change).
        let mut relabeled = plugged.clone();
        relabeled[1].label = Some("New label".into());
        assert!(diff(&plugged, &relabeled).is_empty());
        // Another stick on the same letter: a removal plus an arrival.
        let swapped = vec![c.clone(), usb_b.clone()];
        let d = diff(&plugged, &swapped);
        assert_eq!((d.added, d.removed), (vec![usb_b], vec![usb_a.clone()]));
        // Unplugged.
        let d = diff(&plugged, &before);
        assert_eq!((d.added, d.removed), (vec![], vec![usb_a]));
        // An unknown serial on either side compares by path and kind only.
        let unknown = with_serial("E:\\", VolumeKind::Removable, None);
        assert!(diff(&plugged, &[c.clone(), unknown]).is_empty());
        // Same path, other kind (a network drive mapped where the stick was).
        let net = with_serial("E:\\", VolumeKind::Network, None);
        assert_eq!(diff(&plugged, &[c, net.clone()]).added, [net]);
    }

    #[test]
    fn probe_details_fill_serials_and_add_network_drives() {
        let mut found = vec![volume(Path::new("C:\\"), None, VolumeKind::Fixed)];
        let probed = [
            DriveProbe {
                root: "C:\\".into(),
                kind: DriveType::Fixed,
                serial: Some(7),
            },
            DriveProbe {
                root: "Z:\\".into(),
                kind: DriveType::Remote,
                serial: None,
            },
            DriveProbe {
                root: "R:\\".into(),
                kind: DriveType::CdRom,
                serial: None,
            },
        ];
        add_probe_details(&mut found, &probed);
        assert_eq!(found[0].serial, Some(7));
        assert_eq!(found.len(), 2, "network yes, optical no: {found:?}");
        assert_eq!(
            (found[1].name.as_str(), found[1].kind),
            ("Z:", VolumeKind::Network)
        );
    }

    #[test]
    fn the_signature_is_stable_while_nothing_changes() {
        assert_eq!(signature(), signature());
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
