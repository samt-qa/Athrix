//! A cheap probe of the drive letters, for noticing drives that are plugged in or removed
//! while an app runs (polling a few times a minute).
//!
//! [`probe_drives`] costs one `GetLogicalDrives` call plus, per letter, `GetDriveTypeW` and
//! (for local drives only) `GetVolumeInformationW` for the serial number. None of these read
//! the disk once the volume is mounted, so polling an unchanged machine is a few
//! microseconds. Network drives are never asked for their serial (a disconnected share can
//! block for seconds), and optical drives neither (a disc could spin up).
//!
//! On other platforms the probe is empty: callers list mounts another way.

/// What kind of drive a letter is (`GetDriveTypeW`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveType {
    /// An internal disk, or an external one that reports itself as fixed (most USB hard
    /// disks and SSDs).
    Fixed,
    /// A USB stick, memory card...
    Removable,
    /// A mapped network share.
    Remote,
    /// A CD / DVD drive.
    CdRom,
    /// A RAM disk.
    RamDisk,
    /// Anything else (no root directory, unknown).
    Unknown,
}

/// One drive letter as seen by [`probe_drives`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriveProbe {
    /// The root, e.g. `E:\`.
    pub root: String,
    /// Its kind.
    pub kind: DriveType,
    /// The volume serial number, for local drives whose volume could be read (`None` for an
    /// empty card-reader slot, network and optical drives). Tells two USB sticks mounted on
    /// the same letter apart.
    pub serial: Option<u32>,
}

/// The drive letters present now, in letter order. Empty on other platforms.
pub fn probe_drives() -> Vec<DriveProbe> {
    imp::probe()
}

#[cfg(windows)]
mod imp {
    use super::{DriveProbe, DriveType};
    use crate::sys;

    // GetDriveTypeW results.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;

    pub(super) fn probe() -> Vec<DriveProbe> {
        let mask = sys::logical_drives();
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| {
                let root = format!("{}:\\", char::from(b'A' + i));
                let kind = match sys::drive_type(&root) {
                    DRIVE_FIXED => DriveType::Fixed,
                    DRIVE_REMOVABLE => DriveType::Removable,
                    DRIVE_REMOTE => DriveType::Remote,
                    DRIVE_CDROM => DriveType::CdRom,
                    DRIVE_RAMDISK => DriveType::RamDisk,
                    _ => DriveType::Unknown,
                };
                let serial = match kind {
                    DriveType::Fixed | DriveType::Removable | DriveType::RamDisk => {
                        sys::volume_serial(&root).ok()
                    }
                    _ => None,
                };
                DriveProbe { root, kind, serial }
            })
            .collect()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::DriveProbe;

    pub(super) fn probe() -> Vec<DriveProbe> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_this_machine() {
        let drives = probe_drives();
        if cfg!(windows) {
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            let sys = drives
                .iter()
                .find(|d| d.root[..2].eq_ignore_ascii_case(&system))
                .expect("the system drive is probed");
            assert_eq!(sys.kind, DriveType::Fixed);
            assert!(sys.serial.is_some(), "{sys:?}");
            assert!(drives.windows(2).all(|w| w[0].root < w[1].root));
            // Same answer twice in a row (nothing plugged in meanwhile, in all likelihood).
            assert_eq!(
                probe_drives().iter().find(|d| d.root == sys.root),
                Some(sys)
            );
        } else {
            assert!(drives.is_empty());
        }
    }
}
