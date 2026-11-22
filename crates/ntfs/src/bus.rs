//! Which bus a drive letter's disk hangs off (USB, SATA, NVMe...), to tell hot-pluggable
//! external disks from internal ones.
//!
//! `GetDriveTypeW` ([`crate::drives`]) calls most USB hard disks and SSDs "fixed", like an
//! internal disk. [`volume_bus`] asks the storage stack instead: it opens `\\.\X:` with **no
//! access rights** (no elevation needed, nothing is read from the disk), sends
//! `IOCTL_STORAGE_QUERY_PROPERTY` (`StorageDeviceProperty`: the bus type and the
//! "removable media" bit) and `IOCTL_STORAGE_GET_HOTPLUG_INFO` (the "device hotplug" bit), and
//! closes the handle before returning, so the probe itself never keeps Windows from ejecting
//! the drive. [`BusInfo::is_external`] turns the answer into "plugged in from outside".
//!
//! The replies are decoded in safe code ([`parse_device_descriptor`],
//! [`parse_hotplug_info`]); the FFI goes through the crate's private `sys` wrappers. On other
//! platforms the answer is always [`BusInfo::default`] (bus [`BusType::Unknown`]).

use std::io;

/// A storage bus (`STORAGE_BUS_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BusType {
    /// Not reported (or not asked, on other platforms).
    #[default]
    Unknown,
    /// SCSI.
    Scsi,
    /// ATAPI (optical drives, mostly).
    Atapi,
    /// Parallel ATA.
    Ata,
    /// IEEE 1394 (FireWire).
    Ieee1394,
    /// Serial Storage Architecture.
    Ssa,
    /// Fibre Channel.
    Fibre,
    /// USB (sticks, external hard disks and SSDs, card readers).
    Usb,
    /// A RAID controller.
    Raid,
    /// iSCSI.
    Iscsi,
    /// Serial Attached SCSI.
    Sas,
    /// SATA.
    Sata,
    /// Secure Digital card.
    Sd,
    /// MultiMediaCard.
    Mmc,
    /// A virtual disk.
    Virtual,
    /// A file-backed virtual disk (mounted VHD / ISO).
    FileBackedVirtual,
    /// Storage Spaces.
    Spaces,
    /// NVMe.
    Nvme,
    /// Storage-class memory.
    Scm,
    /// Universal Flash Storage.
    Ufs,
    /// NVMe over Fabrics.
    NvmeOverFabrics,
    /// A value this crate does not know.
    Other(u32),
}

impl BusType {
    /// The bus for a raw `STORAGE_BUS_TYPE` value.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0x00 => Self::Unknown,
            0x01 => Self::Scsi,
            0x02 => Self::Atapi,
            0x03 => Self::Ata,
            0x04 => Self::Ieee1394,
            0x05 => Self::Ssa,
            0x06 => Self::Fibre,
            0x07 => Self::Usb,
            0x08 => Self::Raid,
            0x09 => Self::Iscsi,
            0x0A => Self::Sas,
            0x0B => Self::Sata,
            0x0C => Self::Sd,
            0x0D => Self::Mmc,
            0x0E => Self::Virtual,
            0x0F => Self::FileBackedVirtual,
            0x10 => Self::Spaces,
            0x11 => Self::Nvme,
            0x12 => Self::Scm,
            0x13 => Self::Ufs,
            0x14 => Self::NvmeOverFabrics,
            other => Self::Other(other),
        }
    }

    /// True for the buses only external devices use: USB, FireWire, SD and MMC cards.
    pub fn is_external(self) -> bool {
        matches!(self, Self::Usb | Self::Ieee1394 | Self::Sd | Self::Mmc)
    }
}

/// What [`volume_bus`] learned about the disk under a drive letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct BusInfo {
    /// The bus the disk is attached through.
    pub bus: BusType,
    /// The device reports removable media (`STORAGE_DEVICE_DESCRIPTOR::RemovableMedia`).
    pub removable_media: bool,
    /// The device can be unplugged while running (`STORAGE_HOTPLUG_INFO::DeviceHotplug`);
    /// false if that could not be asked.
    pub device_hotplug: bool,
}

impl BusInfo {
    /// True if the disk is plugged in from outside and may be unplugged at any time: an
    /// external bus ([`BusType::is_external`]), removable media, or a hot-pluggable device.
    ///
    /// The hotplug bit is not trusted for (S)ATA disks: motherboards commonly flag their
    /// internal SATA ports as hot-plug capable, and an internal disk must not be mistaken for
    /// an external one.
    pub fn is_external(&self) -> bool {
        self.bus.is_external()
            || self.removable_media
            || (self.device_hotplug && !matches!(self.bus, BusType::Ata | BusType::Sata))
    }
}

/// The bus of the disk holding the volume mounted on `root` (`E:\`, `E:` or `e:/`).
///
/// Cheap (two `DeviceIoControl`s on a handle opened with no access rights and closed before
/// returning; no elevation, no disk read), but not free: callers polling often should cache
/// the answer per volume. Fails for anything but a drive-letter root, for an empty card
/// reader slot and for volumes the storage stack cannot describe (spanned dynamic
/// volumes...). Other platforms get `Ok(BusInfo::default())` for any drive-letter root.
pub fn volume_bus(root: &str) -> io::Result<BusInfo> {
    let letter = drive_letter(root).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a drive root: {root:?}"),
        )
    })?;
    imp::query(letter)
}

/// The drive letter of `root` if it is `X:`, `X:\` or `X:/`.
fn drive_letter(root: &str) -> Option<char> {
    let b = root.as_bytes();
    let ok = (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')))
        && b[0].is_ascii_alphabetic()
        && b[1] == b':';
    ok.then(|| char::from(b[0].to_ascii_uppercase()))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(b))
}

/// Decodes the start of a `STORAGE_DEVICE_DESCRIPTOR` (the `IOCTL_STORAGE_QUERY_PROPERTY`
/// reply for `StorageDeviceProperty`): the bus type (offset 28) and `RemovableMedia`
/// (offset 10). `None` if `bytes` is too short to hold them, by its length or by the
/// descriptor's own `Size` (offset 4).
pub fn parse_device_descriptor(bytes: &[u8]) -> Option<(BusType, bool)> {
    const BUS_TYPE_AT: usize = 28;
    let size = usize::try_from(u32_at(bytes, 4)?).ok()?;
    if size < BUS_TYPE_AT + 4 {
        return None;
    }
    let bus = BusType::from_raw(u32_at(bytes, BUS_TYPE_AT)?);
    let removable = *bytes.get(10)? != 0;
    Some((bus, removable))
}

/// Decodes a `STORAGE_HOTPLUG_INFO` (the `IOCTL_STORAGE_GET_HOTPLUG_INFO` reply):
/// `DeviceHotplug` (offset 6). `None` if `bytes` is too short.
pub fn parse_hotplug_info(bytes: &[u8]) -> Option<bool> {
    bytes.get(6).map(|&b| b != 0)
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_HOTPLUG_INFO, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    };

    use super::{BusInfo, parse_device_descriptor, parse_hotplug_info};
    use crate::sys;

    pub(super) fn query(letter: char) -> io::Result<BusInfo> {
        // No access rights: storage queries need none, and such a handle needs no elevation
        // and reads nothing. It is dropped (closed) at the end of this function.
        let handle = sys::open_device(&format!("\\\\.\\{letter}:"), 0)?;
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        // The fixed part of the descriptor is 40 bytes; the vendor / product strings that
        // follow are not needed, and a short buffer still gets the fixed part.
        let mut out = [0u8; 512];
        let n = sys::ioctl(
            &handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query),
            &mut out,
        )?;
        let (bus, removable_media) = parse_device_descriptor(&out[..n]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "short storage device descriptor",
            )
        })?;
        let mut hp = [0u8; 8];
        let device_hotplug =
            sys::ioctl::<u8>(&handle, IOCTL_STORAGE_GET_HOTPLUG_INFO, None, &mut hp)
                .ok()
                .and_then(|n| parse_hotplug_info(&hp[..n]))
                .unwrap_or(false);
        drop(handle);
        Ok(BusInfo {
            bus,
            removable_media,
            device_hotplug,
        })
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    use super::BusInfo;

    pub(super) fn query(_letter: char) -> io::Result<BusInfo> {
        Ok(BusInfo::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(bus: u32, removable: bool) -> Vec<u8> {
        let mut d = vec![0u8; 40];
        d[0..4].copy_from_slice(&1u32.to_le_bytes()); // Version
        d[4..8].copy_from_slice(&40u32.to_le_bytes()); // Size
        d[10] = u8::from(removable);
        d[28..32].copy_from_slice(&bus.to_le_bytes());
        d
    }

    #[test]
    fn descriptors_decode() {
        assert_eq!(
            parse_device_descriptor(&descriptor(7, false)),
            Some((BusType::Usb, false))
        );
        assert_eq!(
            parse_device_descriptor(&descriptor(0x11, false)),
            Some((BusType::Nvme, false))
        );
        assert_eq!(
            parse_device_descriptor(&descriptor(0x0C, true)),
            Some((BusType::Sd, true))
        );
        assert_eq!(
            parse_device_descriptor(&descriptor(0x99, false)),
            Some((BusType::Other(0x99), false))
        );
        // Too short, by length or by the descriptor's own size.
        assert_eq!(parse_device_descriptor(&descriptor(7, false)[..31]), None);
        let mut small = descriptor(7, false);
        small[4..8].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(parse_device_descriptor(&small), None);
        assert_eq!(parse_device_descriptor(&[]), None);
    }

    #[test]
    fn hotplug_info_decodes() {
        assert_eq!(parse_hotplug_info(&[8, 0, 0, 0, 0, 0, 1, 0]), Some(true));
        assert_eq!(parse_hotplug_info(&[8, 0, 0, 0, 1, 1, 0, 0]), Some(false));
        assert_eq!(parse_hotplug_info(&[8, 0, 0]), None);
    }

    #[test]
    fn bus_values_round_trip() {
        for raw in 0..=0x14 {
            assert_ne!(BusType::from_raw(raw), BusType::Other(raw), "{raw}");
        }
        assert_eq!(BusType::from_raw(4), BusType::Ieee1394);
        assert_eq!(BusType::from_raw(0x0D), BusType::Mmc);
        assert_eq!(BusType::from_raw(0x0B), BusType::Sata);
    }

    #[test]
    fn external_classification() {
        let info = |bus, removable_media, device_hotplug| BusInfo {
            bus,
            removable_media,
            device_hotplug,
        };
        for bus in [BusType::Usb, BusType::Ieee1394, BusType::Sd, BusType::Mmc] {
            assert!(info(bus, false, false).is_external(), "{bus:?}");
        }
        for bus in [
            BusType::Sata,
            BusType::Nvme,
            BusType::Ata,
            BusType::Raid,
            BusType::Spaces,
            BusType::Unknown,
        ] {
            assert!(!info(bus, false, false).is_external(), "{bus:?}");
        }
        assert!(
            info(BusType::Scsi, true, false).is_external(),
            "removable media"
        );
        assert!(
            info(BusType::Nvme, false, true).is_external(),
            "hotplug NVMe"
        );
        assert!(info(BusType::Sas, false, true).is_external(), "hotplug SAS");
        // Internal SATA ports are often flagged hot-pluggable.
        assert!(!info(BusType::Sata, false, true).is_external());
        assert!(!info(BusType::Ata, false, true).is_external());
        assert!(!BusInfo::default().is_external());
    }

    #[test]
    fn only_drive_roots_are_probed() {
        assert_eq!(drive_letter("e:\\"), Some('E'));
        assert_eq!(drive_letter("C:"), Some('C'));
        assert_eq!(drive_letter("d:/"), Some('D'));
        for not in ["C:\\Users", "/", "", "CD:", "1:\\", "\\\\server\\share"] {
            assert_eq!(drive_letter(not), None, "{not}");
            assert_eq!(
                volume_bus(not).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn probes_the_system_drive() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let info = volume_bus(&system).expect("the system drive is describable");
        eprintln!("{system} -> {info:?}");
        if cfg!(windows) {
            // The system disk is internal on any development or CI machine.
            assert!(!info.bus.is_external(), "{info:?}");
            assert!(!info.is_external(), "{info:?}");
            assert_ne!(info.bus, BusType::Unknown, "{info:?}");
        } else {
            assert_eq!(info, BusInfo::default());
        }
        // Every drive letter answers or fails cleanly (and quickly).
        for d in crate::drives::probe_drives() {
            eprintln!("{} {:?} -> {:?}", d.root, d.kind, volume_bus(&d.root));
        }
    }
}
