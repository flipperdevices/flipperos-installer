//! Enumeration of local block storage via sysfs.
//!
//! The RK3576 mask ROM can boot from UFS, eMMC and SD. We walk `/sys/block`,
//! skip virtual/loop/ram devices and partitions, and classify each whole disk.
//! Classification is heuristic and based on the device node name plus the sysfs
//! topology (a device whose parent bus is `mmc` with a non-removable flag is
//! eMMC, a removable one is an SD card, `sd*` backed by USB is USB, `sd*` backed
//! by a UFS host is UFS).

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::model::{StorageDevice, StorageKind};

const SYS_BLOCK: &str = "/sys/block";
/// Linux reports sizes in 512-byte sectors regardless of physical block size.
const SECTOR_SIZE: u64 = 512;

/// Enumerate candidate whole-disk storage devices.
pub fn enumerate() -> Vec<StorageDevice> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(SYS_BLOCK) {
        Ok(e) => e,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_virtual(&name) {
            continue;
        }
        let sysdir = entry.path();
        let size_bytes = read_u64(&sysdir.join("size")).unwrap_or(0) * SECTOR_SIZE;
        if size_bytes == 0 {
            continue;
        }
        let removable = read_u64(&sysdir.join("removable")).unwrap_or(0) == 1;
        let model = read_trimmed(&sysdir.join("device/model"))
            .or_else(|| read_trimmed(&sysdir.join("device/name")))
            .unwrap_or_else(|| "unknown".to_string());
        let kind = classify(&name, &sysdir, removable);

        out.push(StorageDevice {
            path: format!("/dev/{name}"),
            kind,
            model,
            size_bytes,
            removable,
        });
    }

    // Deterministic ordering: boot-capable first, then by device path.
    out.sort_by(|a, b| {
        b.boot_rom_capable()
            .cmp(&a.boot_rom_capable())
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Skip loop/ram/zram/dm/md/virtual devices we never want to flash.
fn is_virtual(name: &str) -> bool {
    const PREFIXES: [&str; 6] = ["loop", "ram", "zram", "dm-", "md", "sr"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Best-effort classification of a whole-disk device.
fn classify(name: &str, sysdir: &Path, removable: bool) -> StorageKind {
    // Resolve the real sysfs path to inspect the bus topology.
    let real = fs::canonicalize(sysdir).unwrap_or_else(|_| sysdir.to_path_buf());
    let chain = real.to_string_lossy().to_lowercase();

    if name.starts_with("mmcblk") {
        // mmc-backed: eMMC is non-removable, SD is removable.
        return if removable {
            StorageKind::SdCard
        } else {
            StorageKind::Emmc
        };
    }

    if name.starts_with("sd") {
        if chain.contains("usb") {
            return StorageKind::Usb;
        }
        // A SCSI disk that is not USB on an RK3576 is the UFS host.
        if chain.contains("ufs") || has_ufs_host(&real) {
            return StorageKind::Ufs;
        }
        return if removable {
            StorageKind::Usb
        } else {
            StorageKind::Other
        };
    }

    if name.starts_with("nvme") {
        return StorageKind::Other;
    }

    StorageKind::Other
}

/// Walk up the sysfs chain looking for a UFS host controller marker.
fn has_ufs_host(real: &Path) -> bool {
    let mut cur: Option<PathBuf> = Some(real.to_path_buf());
    while let Some(dir) = cur {
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            if name.contains("ufs") {
                return true;
            }
        }
        cur = dir.parent().map(|p| p.to_path_buf());
        if cur.as_deref() == Some(Path::new("/")) {
            break;
        }
    }
    false
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}
