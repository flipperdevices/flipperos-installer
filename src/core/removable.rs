//! Discovery of mounted removable media (SD / USB) that may mirror the image
//! catalog. Returns the backing device and mount point of each candidate; the
//! [`catalog`](crate::core::catalog) module then reads builds from those roots.

use std::collections::HashMap;
use std::fs;

use crate::core::model::StorageKind;
use crate::core::storage;

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
    storage::enumerate()
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
