//! Discovery of removable media (SD / USB) that may hold update bundles or
//! mirror the image catalog. Media that is already mounted is picked up as it
//! is; anything else is mounted read-only by [`automount_ro`], so an operator
//! who just plugged in a card does not have to mount it by hand.
//!
//! Returns the backing device and mount point of each candidate; the
//! [`catalog`](crate::core::catalog) and [`bundle`](crate::core::bundle) modules
//! then read from those roots.

use std::collections::HashMap;
use std::fs;
use std::process::{Command, Stdio};

use crate::core::model::StorageKind;
use crate::core::storage;

/// Where we mount media ourselves. One subdirectory per device node.
pub const MEDIA_MNT_BASE: &str = "/run/flipperos-media";

/// `(device, mountpoint)` for every mounted SD/USB filesystem.
pub fn media_roots() -> Vec<(String, String)> {
    let removable = removable_devices();
    let mut out = Vec::new();
    for mount in mounts() {
        if let Some(kind) = backing_kind(&mount.device, &removable) {
            if matches!(kind, StorageKind::SdCard | StorageKind::Usb) {
                out.push((mount.device, mount.mountpoint));
            }
        }
    }
    out
}

/// Mount every not-yet-mounted partition of every removable disk read-only
/// under [`MEDIA_MNT_BASE`], and return the roots we mounted.
///
/// Read-only throughout: this runs during discovery, before the operator has
/// chosen anything, so it must not be able to modify a card that merely happens
/// to be plugged in. It also deliberately ignores [`Config::dry_run`] — mounting
/// read-only is not destructive, and a dry run needs to exercise discovery to be
/// worth anything.
///
/// Partitions that cannot be mounted (no filesystem, a type this kernel lacks)
/// are skipped quietly: on a multi-partition card most of them are expected to
/// fail.
///
/// [`Config::dry_run`]: crate::core::controller::Config::dry_run
pub fn automount_ro() -> Vec<(String, String)> {
    let already: Vec<String> = mounts().into_iter().map(|m| m.device).collect();
    let mut mounted = Vec::new();

    for (disk, kind) in removable_devices() {
        if !matches!(kind, StorageKind::SdCard | StorageKind::Usb) {
            continue;
        }
        // Try the partitions, and the whole disk too: a card can carry a bare
        // filesystem with no partition table at all.
        let mut candidates = storage::partitions(&disk);
        if candidates.is_empty() {
            candidates.push(disk.clone());
        }
        for part in candidates {
            if already.iter().any(|d| d == &part) {
                continue;
            }
            let node = part.trim_start_matches("/dev/").replace('/', "_");
            let target = format!("{MEDIA_MNT_BASE}/{node}");
            if fs::create_dir_all(&target).is_err() {
                continue;
            }
            let ok = Command::new("mount")
                .arg("-o")
                .arg("ro,noatime")
                .arg(&part)
                .arg(&target)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                mounted.push((part, target));
            } else {
                // Nothing mountable here; don't leave an empty directory behind.
                let _ = fs::remove_dir(&target);
            }
        }
    }
    mounted
}

/// Unmount the media we mounted ourselves. With `disk`, only the mounts backed
/// by that whole disk — used before wiping a target so our own read-only mounts
/// do not make it look busy.
pub fn unmount_ours(disk: Option<&str>) {
    for mount in mounts() {
        if !mount.mountpoint.starts_with(MEDIA_MNT_BASE) {
            continue;
        }
        if let Some(disk) = disk {
            if whole_disk(&mount.device) != disk {
                continue;
            }
        }
        let _ = Command::new("umount")
            .arg(&mount.mountpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir(&mount.mountpoint);
    }
}

/// Mountpoints of every filesystem backed by `disk` (any of its partitions).
pub fn mounts_on(disk: &str) -> Vec<String> {
    mounts()
        .into_iter()
        .filter(|m| whole_disk(&m.device) == disk || m.device == disk)
        .map(|m| m.mountpoint)
        .collect()
}

struct Mount {
    device: String,
    mountpoint: String,
}

/// Parse `/proc/mounts` for real block-device-backed filesystems.
fn mounts() -> Vec<Mount> {
    let content = match fs::read_to_string("/proc/mounts") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let device = cols.next()?;
            let mountpoint = cols.next()?;
            if !device.starts_with("/dev/") {
                return None;
            }
            Some(Mount {
                device: device.to_string(),
                mountpoint: unescape_mount(mountpoint),
            })
        })
        .collect()
}

fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// Map removable whole-disk devices to their kind for quick lookup.
fn removable_devices() -> HashMap<String, StorageKind> {
    // Pass 0: source media may legitimately be smaller than an install target.
    storage::enumerate(0)
        .into_iter()
        .filter(|d| d.removable)
        .map(|d| (d.path, d.kind))
        .collect()
}

/// Determine the storage kind of the whole disk backing a partition device.
fn backing_kind(device: &str, removable: &HashMap<String, StorageKind>) -> Option<StorageKind> {
    if let Some(kind) = removable.get(device) {
        return Some(*kind);
    }
    let whole = whole_disk(device);
    removable.get(&whole).copied()
}

/// Reduce a partition node to its whole-disk node.
fn whole_disk(device: &str) -> String {
    let name = device.trim_start_matches("/dev/");
    if let Some(idx) = name.find('p') {
        if name[idx + 1..].chars().all(|c| c.is_ascii_digit())
            && name[..idx].chars().any(|c| c.is_ascii_digit())
        {
            return format!("/dev/{}", &name[..idx]);
        }
    }
    let base: String = name.chars().take_while(|c| !c.is_ascii_digit()).collect();
    format!("/dev/{base}")
}
