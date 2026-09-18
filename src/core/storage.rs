//! Enumeration of local block storage via sysfs.
//!
//! The RK3576 boot ROM can boot from UFS, eMMC and SD. We walk `/sys/block`,
//! skip virtual/loop/ram devices and partitions, and classify each whole disk.
//! Classification starts from the device node name: an `mmcblk*` is told apart
//! by asking the MMC core what the card is (see [`mmc_kind`]), and an `sd*` by
//! its sysfs bus topology — backed by USB it is USB, backed by a UFS host it is
//! UFS.
//!
//! Note that `/sys/block/<disk>/removable` answers none of this. The mmc block
//! driver never sets it, so it reads 0 for eMMC and SD alike, and plenty of USB
//! disks report 0 as well. It is an input to the `sd*` heuristic and nothing
//! more; whether a device is removable *media* is decided by its class, via
//! [`StorageKind::is_removable`].

use std::fs;
use std::path::{Path, PathBuf};

use crate::core::model::{StorageDevice, StorageKind};
use crate::core::ufs;

const SYS_BLOCK: &str = "/sys/block";
/// Linux reports sizes in 512-byte sectors regardless of physical block size.
const SECTOR_SIZE: u64 = 512;

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
        let block_removable = read_u64(&sysdir.join("removable")).unwrap_or(0) == 1;
        let model = read_trimmed(&sysdir.join("device/model"))
            .or_else(|| read_trimmed(&sysdir.join("device/name")))
            .unwrap_or_else(|| "unknown".to_string());
        let kind = classify(&name, &sysdir, block_removable);

        out.push(StorageDevice {
            path: format!("/dev/{name}"),
            kind,
            model,
            size_bytes,
            // Derived from the class rather than copied from the sysfs flag
            // above, which lies for exactly the devices we care about.
            removable: kind.is_removable(),
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
///
/// `block_removable` is `/sys/block/<disk>/removable`; only the `sd*` branch
/// uses it, and only as a tie-breaker.
fn classify(name: &str, sysdir: &Path, block_removable: bool) -> StorageKind {
    // Taken before the bus topology is resolved: the card device hangs off the
    // block device's own `device` link, so the mmc branch needs no canonical
    // path, and asking for one would be the only thing tying it to a real sysfs.
    if name.starts_with("mmcblk") {
        return mmc_kind(
            read_trimmed(&sysdir.join("device/type")).as_deref(),
            read_trimmed(&sysdir.join("device/removable")).as_deref(),
        );
    }

    // Resolve the real sysfs path to inspect the bus topology.
    let real = fs::canonicalize(sysdir).unwrap_or_else(|_| sysdir.to_path_buf());
    let chain = real.to_string_lossy().to_lowercase();

    if name.starts_with("sd") {
        if chain.contains("usb") {
            return StorageKind::Usb;
        }
        // A SCSI disk that is not USB on an RK3576 is the UFS host.
        if chain.contains("ufs") || has_ufs_host(&real) {
            return StorageKind::Ufs;
        }
        return if block_removable {
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

/// Decide whether an `mmcblk*` device is a soldered eMMC or a card in a slot,
/// from the attributes the MMC core publishes about it.
///
/// `card_type` is `<card>/type`, printed by `mmc_type_show`
/// (`drivers/mmc/core/bus.c`) as `MMC`, `SD`, `SDIO`, `SD-combo` or `unknown`.
/// It is the card's own answer to the initialisation commands — the medium
/// itself, which is precisely the question being asked — and the mmc bus
/// attaches it to every card it enumerates.
///
/// `slot_removable` is `<card>/removable` (`removable` / `fixed` / `unknown`),
/// and describes the *slot* rather than what is in it, so it only breaks the tie
/// for a kernel that has stopped publishing `type`: a fixed slot holds a
/// soldered eMMC, a pluggable one all but certainly a card.
///
/// `/sys/block/<disk>/removable` is deliberately absent from both: the mmc block
/// driver never sets it, so it reads 0 for every card and used to make every SD
/// card here an eMMC.
fn mmc_kind(card_type: Option<&str>, slot_removable: Option<&str>) -> StorageKind {
    if let Some(t) = card_type {
        if t.eq_ignore_ascii_case("MMC") {
            return StorageKind::Emmc;
        }
        // `SD-combo` is an SD memory card that also speaks SDIO, and the block
        // device is its memory half. A pure `SDIO` card exposes no block device
        // and so cannot reach here, but treat it as a card rather than let an
        // unexpected spelling fall through to the slot.
        if t.eq_ignore_ascii_case("SD")
            || t.eq_ignore_ascii_case("SD-combo")
            || t.eq_ignore_ascii_case("SDIO")
        {
            return StorageKind::SdCard;
        }
    }

    // Unrecognised: err towards a card. That keeps the device out of the
    // automatic target selection, so a mystery device is never pre-selected to
    // be wiped — it can still be chosen by hand.
    match slot_removable {
        Some(s) if s.eq_ignore_ascii_case("fixed") => StorageKind::Emmc,
        _ => StorageKind::SdCard,
    }
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

/// Given a whole-disk UFS device node (e.g. `/dev/sda`), find the block node of
/// the logical unit flagged as boot LU `boot_lun_id`
/// ([`crate::core::ufs::BOOT_LUN_A`] or [`crate::core::ufs::BOOT_LUN_B`]) on the
/// *same physical device*, if present.
///
/// All logical units of one UFS device share a SCSI `host:channel:target` and
/// differ only by LUN, appearing as sibling directories under the target dir in
/// sysfs. On RK3576 the boot LUs are normal (small) LUs rather than well-known
/// LUNs, so we can't derive them from the LUN number; instead we read each
/// sibling's UFS unit descriptor and pick the one whose `bBootLunID` matches.
/// Which of the pair the boot ROM actually reads is the device's `bBootLunEn`
/// attribute, not this flag. Returns `None` when no such LU exists (the caller
/// then warns).
pub fn find_ufs_boot_lu(disk: &str, boot_lun_id: u8) -> Option<String> {
    let name = disk.trim_start_matches("/dev/");
    // The target LU's SCSI device dir, e.g. `.../target0:0:0/0:0:0:0`; its
    // parent is the SCSI target shared by every LU of this physical device.
    let scsi_dev = fs::canonicalize(Path::new(SYS_BLOCK).join(name).join("device")).ok()?;
    let target_dir = scsi_dev.parent()?;

    for entry in fs::read_dir(target_dir).ok()?.flatten() {
        let lu = entry.path();
        if read_boot_lun_id(&lu) != Some(boot_lun_id) {
            continue;
        }
        // Resolve this LU's block node; skip it if it *is* the target (a data LU
        // that also happens to be flagged bootable) — that's not a separate area.
        if let Some(node) = ufs::lu_block_node(&lu) {
            if node != disk {
                return Some(node);
            }
        }
    }
    None
}

/// Given a whole-disk UFS device node, find the block node of the data logical
/// unit numbered `lun` on the *same physical device*, if present.
///
/// The sibling of [`find_ufs_boot_lu`], and it walks the same set of directories,
/// but a data LU is identified by its LUN rather than by a descriptor flag: the
/// Flipper provisioning scheme in `config/flipperos-ufs.toml` is what says which
/// number means what. A node that *is* the target disk is skipped, so asking for
/// a LUN the device does not have can never hand back the LU being installed to.
pub fn find_ufs_data_lu(disk: &str, lun: u32) -> Option<String> {
    let name = disk.trim_start_matches("/dev/");
    let scsi_dev = fs::canonicalize(Path::new(SYS_BLOCK).join(name).join("device")).ok()?;
    let target_dir = scsi_dev.parent()?;

    for entry in fs::read_dir(target_dir).ok()?.flatten() {
        if ufs::lun_of(&entry.file_name().to_string_lossy()) != Some(lun) {
            continue;
        }
        if let Some(node) = ufs::lu_block_node(&entry.path()) {
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
fn read_boot_lun_id(lu_dir: &Path) -> Option<u8> {
    let raw = read_trimmed(&lu_dir.join("unit_descriptor/boot_lun_id"))?;
    match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => raw.parse().ok(),
    }
}

/// Report the reasons the whole-disk `disk` (or any of its partitions) is
/// currently in use: a mounted filesystem, active swap, or a device-mapper / MD
/// / RAID holder. An empty result means the disk is free to repartition. Used
/// as a safety gate before destructive operations so we never wipe a disk that
/// backs a live mount — most importantly the removable media the snapshots are
/// being read from, or the running system.
pub fn device_in_use(disk: &str) -> Vec<String> {
    let mut reasons = Vec::new();

    // Mounts: `/proc/mounts` columns are `source mountpoint fstype …`.
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let mut cols = line.split_whitespace();
            if let (Some(src), Some(mnt)) = (cols.next(), cols.next()) {
                if src == disk || is_partition_of(src, disk) {
                    reasons.push(format!("{src} is mounted at {mnt}"));
                }
            }
        }
    }

    // Swap: the first column of `/proc/swaps` (past its header) is the device.
    if let Ok(swaps) = fs::read_to_string("/proc/swaps") {
        for line in swaps.lines().skip(1) {
            if let Some(src) = line.split_whitespace().next() {
                if src == disk || is_partition_of(src, disk) {
                    reasons.push(format!("{src} is an active swap device"));
                }
            }
        }
    }

    // Device-mapper / MD holders on the whole disk or any of its partitions.
    reasons.extend(holder_reasons(disk));
    reasons
}

/// The partition device nodes of whole-disk `disk`, in partition-number order.
///
/// Read from sysfs: a child directory of `/sys/block/<disk>` that contains a
/// `partition` file is a partition of it.
pub fn partitions(disk: &str) -> Vec<String> {
    let name = disk.trim_start_matches("/dev/");
    let block = Path::new(SYS_BLOCK).join(name);
    let Ok(entries) = fs::read_dir(&block) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, String)> = Vec::new();
    for entry in entries.flatten() {
        let child = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        if !dir.join("partition").is_file() {
            continue;
        }
        let number = read_u64(&dir.join("partition")).unwrap_or(u64::MAX);
        found.push((number, format!("/dev/{child}")));
    }
    found.sort();
    found.into_iter().map(|(_, node)| node).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Flipper One's microSD, attribute for attribute as the board reports
    /// it. The block layer calls it non-removable, which is what used to make it
    /// an eMMC in the target list.
    #[test]
    fn a_card_the_block_layer_calls_fixed_is_still_an_sd_card() {
        assert_eq!(mmc_kind(Some("SD"), Some("removable")), StorageKind::SdCard);
    }

    #[test]
    fn the_card_type_outranks_the_slot() {
        // A soldered eMMC stays an eMMC even where the host describes a slot it
        // could be pulled from, and vice versa.
        assert_eq!(mmc_kind(Some("MMC"), Some("removable")), StorageKind::Emmc);
        assert_eq!(mmc_kind(Some("MMC"), Some("fixed")), StorageKind::Emmc);
        assert_eq!(mmc_kind(Some("SD"), Some("fixed")), StorageKind::SdCard);
    }

    #[test]
    fn every_spelling_of_a_card_reads_as_one() {
        for t in ["SD", "sd", "SD-combo", "sd-combo", "SDIO"] {
            assert_eq!(mmc_kind(Some(t), None), StorageKind::SdCard, "{t}");
        }
        assert_eq!(mmc_kind(Some("mmc"), None), StorageKind::Emmc);
    }

    /// Without a usable card type the slot is all there is to go on.
    #[test]
    fn an_unreadable_card_type_falls_back_to_the_slot() {
        assert_eq!(mmc_kind(Some("unknown"), Some("fixed")), StorageKind::Emmc);
        assert_eq!(mmc_kind(None, Some("fixed")), StorageKind::Emmc);
        assert_eq!(mmc_kind(None, Some("removable")), StorageKind::SdCard);
        // Nothing to go on at all: assume a card, so it is never auto-selected
        // as the device to wipe.
        assert_eq!(mmc_kind(None, None), StorageKind::SdCard);
    }

    /// Pins *which* files `classify` reads, and that it does so without needing
    /// the sysfs symlink farm.
    #[test]
    fn classify_reads_the_card_attributes_under_the_block_device() {
        let dir = Scratch::new("mmc-type");
        fs::create_dir_all(dir.path("device")).unwrap();
        fs::write(dir.path("device/type"), "SD\n").unwrap();
        fs::write(dir.path("device/removable"), "removable\n").unwrap();
        assert_eq!(
            classify("mmcblk0", &dir.0, false),
            StorageKind::SdCard,
            "the block layer's flag must not get a vote"
        );

        let bare = Scratch::new("mmc-bare");
        assert_eq!(classify("mmcblk0", &bare.0, false), StorageKind::SdCard);
    }

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("flipperos-storage-{name}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
