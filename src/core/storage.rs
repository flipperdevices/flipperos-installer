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

/// Value of the UFS unit-descriptor `bBootLunID` attribute that marks a logical
/// unit as Boot LU A (the one the RK3576 mask ROM reads early bootloader from).
/// `0` means not bootable, `2` means Boot LU B.
const BOOT_LUN_ID_A: u64 = 1;

/// Smallest device we consider a viable *install target*. Anything smaller
/// cannot hold the main GPT+Btrfs image; in particular it filters out the tiny
/// UFS boot LUs (typically 4 MiB), so the operator never has to pick between a
/// UFS device's logical units in the target list.
pub const MIN_TARGET_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Enumerate candidate whole-disk storage devices at least `min_size_bytes`
/// large. Pass `0` to list every device regardless of size (e.g. when mapping
/// removable *source* media, which may be smaller than an install target).
pub fn enumerate(min_size_bytes: u64) -> Vec<StorageDevice> {
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
        if size_bytes < min_size_bytes.max(1) {
            continue;
        }
        // `size` above is always in 512-byte units, but the *native* logical
        // block size (needed to write a recognisable GPT) is separate — 512 on
        // most eMMC/SD, 4096 on UFS. Fall back to 512 if the queue attribute is
        // missing (e.g. an unusual virtual device).
        let logical_block_size =
            read_u64(&sysdir.join("queue/logical_block_size")).unwrap_or(SECTOR_SIZE);
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
            logical_block_size,
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

/// Given a whole-disk UFS device node (e.g. `/dev/sda`), find the block node
/// backing Boot LU A (the "W-LU-A") on the *same physical device*, if present.
///
/// All logical units of one UFS device share a SCSI `host:channel:target` and
/// differ only by LUN, appearing as sibling directories under the target dir in
/// sysfs. On RK3576 the boot LU is a normal (small) LU rather than a well-known
/// LUN, so we can't derive it from the LUN number; instead we read each
/// sibling's UFS unit descriptor and pick the one whose `bBootLunID` marks it as
/// Boot LU A. Returns `None` when no such LU exists (the caller then warns).
pub fn find_ufs_boot_lu_a(disk: &str) -> Option<String> {
    let name = disk.trim_start_matches("/dev/");
    // The target LU's SCSI device dir, e.g. `.../target0:0:0/0:0:0:0`; its
    // parent is the SCSI target shared by every LU of this physical device.
    let scsi_dev = fs::canonicalize(Path::new(SYS_BLOCK).join(name).join("device")).ok()?;
    let target_dir = scsi_dev.parent()?;

    for entry in fs::read_dir(target_dir).ok()?.flatten() {
        let lu = entry.path();
        if read_boot_lun_id(&lu) != Some(BOOT_LUN_ID_A) {
            continue;
        }
        // Resolve this LU's block node; skip it if it *is* the target (a data LU
        // that also happens to be flagged bootable) — that's not a separate area.
        if let Some(node) = lu_block_node(&lu) {
            if node != disk {
                return Some(node);
            }
        }
    }
    None
}

/// Read a UFS LU's `unit_descriptor/boot_lun_id` (`bBootLunID`). The kernel
/// prints single-byte descriptor fields as `0x%02X`, so accept a `0x` prefix as
/// well as a plain decimal value.
fn read_boot_lun_id(lu_dir: &Path) -> Option<u64> {
    let raw = read_trimmed(&lu_dir.join("unit_descriptor/boot_lun_id"))?;
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => raw.parse().ok(),
    }
}

/// The `/dev/...` node for a UFS LU given its sysfs SCSI-device directory.
fn lu_block_node(lu_dir: &Path) -> Option<String> {
    let entry = fs::read_dir(lu_dir.join("block")).ok()?.flatten().next()?;
    Some(format!("/dev/{}", entry.file_name().to_string_lossy()))
}

/// Report the reasons the whole-disk `disk` (or any of its partitions) is
/// currently in use: a mounted filesystem, active swap, or a device-mapper / MD
/// / RAID holder. An empty result means the disk is free to repartition. Used
/// as a safety gate before destructive operations so we never wipe a disk that
/// backs a live mount — most importantly the removable media the snapshots are
/// being read from, or the running system. If a source we need cannot be read,
/// that is reported as a reason too, so an unanswerable check blocks the wipe
/// instead of reading as "free".
pub fn device_in_use(disk: &str) -> Vec<String> {
    let mut reasons = Vec::new();

    // Mounts: `/proc/mounts` columns are `source mountpoint fstype …`.
    // A read failure means we cannot tell whether the disk is live, so report
    // that as a reason rather than silently returning "free" to the caller.
    match fs::read_to_string("/proc/mounts") {
        Ok(mounts) => {
            for line in mounts.lines() {
                let mut cols = line.split_whitespace();
                if let (Some(src), Some(mnt)) = (cols.next(), cols.next()) {
                    if src == disk || is_partition_of(src, disk) {
                        reasons.push(format!("{src} is mounted at {mnt}"));
                    }
                }
            }
        }
        Err(e) => reasons.push(format!(
            "cannot read /proc/mounts to check for mounts ({e})"
        )),
    }

    // Swap: the first column of `/proc/swaps` (past its header) is the device.
    match fs::read_to_string("/proc/swaps") {
        Ok(swaps) => {
            for line in swaps.lines().skip(1) {
                if let Some(src) = line.split_whitespace().next() {
                    if src == disk || is_partition_of(src, disk) {
                        reasons.push(format!("{src} is an active swap device"));
                    }
                }
            }
        }
        Err(e) => reasons.push(format!(
            "cannot read /proc/swaps to check for active swap ({e})"
        )),
    }

    // Device-mapper / MD holders on the whole disk or any of its partitions.
    reasons.extend(holder_reasons(disk));
    reasons
}

/// Whether `node` is a partition of whole-disk `disk` (`/dev/sda1` of
/// `/dev/sda`, `/dev/mmcblk0p2` of `/dev/mmcblk0`) — a name suffix that is an
/// optional `p` followed by digits, rejecting sibling disks like `/dev/sdaa`.
fn is_partition_of(node: &str, disk: &str) -> bool {
    let suffix = match node.strip_prefix(disk) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };
    let digits = suffix.strip_prefix('p').unwrap_or(suffix);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Reasons drawn from sysfs `holders/` links: a non-empty `holders` directory on
/// the whole disk or a partition means an LVM/MD/dm/crypt device sits on top.
fn holder_reasons(disk: &str) -> Vec<String> {
    let mut out = Vec::new();
    let name = disk.trim_start_matches("/dev/");
    let block = Path::new(SYS_BLOCK).join(name);

    // The whole disk plus each partition (a sub-dir carrying a `partition` file).
    let mut dirs = vec![block.clone()];
    if let Ok(entries) = fs::read_dir(&block) {
        for e in entries.flatten() {
            let p = e.path();
            if p.join("partition").exists() {
                dirs.push(p);
            }
        }
    }

    for dir in dirs {
        let owner = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(holders) = fs::read_dir(dir.join("holders")) {
            for h in holders.flatten() {
                out.push(format!(
                    "/dev/{owner} is held by {}",
                    h.file_name().to_string_lossy()
                ));
            }
        }
    }
    out
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
