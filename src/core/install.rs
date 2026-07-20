//! The installer engine: partition, format, receive snapshots, install kernels.
//!
//! Every destructive step goes through [`Step::exec`], which honours
//! [`Config::dry_run`]: in dry-run mode commands are logged but not executed, so
//! the whole flow can be exercised safely on a development host.
//!
//! The high-level flow mirrors what the user selected in either frontend:
//!   1. `blkdiscard` the whole target device.
//!   2. Write a fresh GPT reserving the RK3576 bootloader area.
//!   3. Write the selected U-Boot image directly to the reserved boot area.
//!   4. `mkfs.btrfs` on the root partition and create the subvolume skeleton.
//!   5. For each selected profile: `btrfs receive` its snapshot stream, then
//!      chroot into it and run `kernel-install` for every installed kernel.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::core::controller::{Config, Controller};
use crate::core::layout::{self, Layout};
use crate::core::model::{PackFile, ProfilePack, Source, StorageDevice, StorageKind, UbootBuild};

// GPT layout used by FlipperOS images, as byte offsets from the start of disk.
// These are absolute byte offsets, independent of the device's sector size;
// `write_gpt` converts them to LBAs using the target's native block size.
// loader:   [32 KiB, 60 MiB)  (holds idbloader + U-Boot)
// metadata: [60 MiB, 64 MiB)
// root:     [64 MiB, end]     (Btrfs)
//
// The RK3576 mask ROM reads `idbloader`/U-Boot starting at byte offset 32 KiB,
// which is the start of the `loader` partition (p1), so the image is written
// to that partition from its beginning.
const LOADER_START: u64 = 32 * 1024;
const METADATA_START: u64 = 60 * 1024 * 1024;
const ROOT_START: u64 = 64 * 1024 * 1024;
/// The loader (U-Boot) partition is the first partition.
const LOADER_PART_INDEX: u32 = 1;
/// The Btrfs root is the third partition.
const ROOT_PART_INDEX: u32 = 3;

/// POSIX shell glue that chroots into a deployed profile and runs
/// `kernel-install` for every installed kernel. Written to a temp path and
/// executed once per profile at install time.
const INSTALL_KERNEL_SH: &str = include_str!("../../scripts/flipperos-install-kernel.sh");

type Result<T> = std::result::Result<T, String>;

/// Run the whole installation. Called on a worker thread by the controller.
pub fn run(ctrl: &Arc<Controller>) -> Result<()> {
    let state = ctrl.snapshot();
    let cfg = ctrl.config();

    let device = state
        .target()
        .cloned()
        .ok_or("no target device selected")?;
    let uboot = state
        .selected_uboot()
        .cloned()
        .ok_or("no u-boot build selected")?;
    let build = state
        .selected_build()
        .cloned()
        .ok_or("no snapshot build selected")?;
    if !build.loaded {
        return Err("snapshot build profiles not loaded yet".to_string());
    }
    let minimal = build
        .minimal()
        .cloned()
        .ok_or("snapshot build has no Minimal profile")?;
    // Extra profiles the user opted into, in build order.
    let extras: Vec<ProfilePack> = build
        .extra_profiles()
        .filter(|p| state.selection.profiles.iter().any(|n| n == &p.name))
        .cloned()
        .collect();

    ctrl.log(format!(
        "target {} ({}), u-boot {}, build {}, profiles: Minimal{}{}{}",
        device.path,
        device.kind.as_str(),
        uboot.label,
        build.label,
        if extras.is_empty() { "" } else { " + " },
        extras
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", "),
        if cfg.dry_run { "  [DRY RUN]" } else { "" },
    ));

    guard_target(cfg, ctrl, &device)?;

    // Resolve the Btrfs layout: prefer one shipped with the images, else the
    // built-in default.
    let sources: Vec<&Source> = vec![&build.source];
    let (fs_layout, layout_origin) = layout::resolve(cfg, &state.board.board_id, &sources);
    ctrl.log(format!(
        "btrfs layout: {layout_origin} — label '{}', {} subvolume(s)",
        fs_layout.label,
        fs_layout.subvolumes.len()
    ));

    // 4 fixed steps, then receive + snapshot + kernel per deployed profile.
    let deployed = 1 + extras.len();
    let total_steps = 4 + deployed as u32 * 3;
    let mut step = 0u32;
    let mut tick = |ctrl: &Controller, msg: &str| {
        step += 1;
        ctrl.set_progress(step as f32 / total_steps as f32);
        ctrl.log(msg.to_string());
    };

    let root_part = partition_path(&device.path, ROOT_PART_INDEX);

    // 1. Wipe.
    tick(ctrl, &format!("blkdiscard {}", device.path));
    exec(cfg, ctrl, Command::new("blkdiscard").arg("-f").arg(&device.path))?;

    // 2. Partition.
    tick(ctrl, "writing GPT");
    write_gpt(
        cfg,
        ctrl,
        &device.path,
        device.size_bytes,
        device.logical_block_size,
    )?;

    // 3. Bootloader.
    tick(ctrl, &format!("installing u-boot {}", uboot.label));
    install_uboot(cfg, ctrl, &device, &uboot)?;

    // 4. Filesystem + subvolumes.
    tick(ctrl, &format!("mkfs.btrfs {root_part}"));
    let mnt = make_filesystem(cfg, ctrl, &root_part, &fs_layout)?;

    // The target is mounted from here on. Run the remaining steps in an inner
    // block so we can always unmount afterwards, even when a btrfs command
    // fails partway through.
    let deploy = (|| -> Result<()> {
        // 5a. Minimal base: full stock pack.
        let minimal_full = minimal
            .full
            .as_ref()
            .ok_or("Minimal profile has no full pack")?;
        deploy_profile(
            cfg,
            ctrl,
            &mnt,
            &root_part,
            &fs_layout,
            &minimal,
            minimal_full,
            "full",
            &mut tick,
        )?;

        // 5b. Extra profiles: incremental packs on top of Minimal.
        for p in &extras {
            let inc = p
                .incremental
                .as_ref()
                .ok_or_else(|| format!("profile {} has no incremental pack", p.name))?;
            deploy_profile(
                cfg,
                ctrl,
                &mnt,
                &root_part,
                &fs_layout,
                p,
                inc,
                "incremental",
                &mut tick,
            )?;
        }
        Ok(())
    })();

    // Always unmount, then surface the deployment error (if any) in preference
    // to any unmount error.
    if deploy.is_err() {
        ctrl.log("install failed — unmounting target".to_string());
    }
    let unmounted = unmount(cfg, ctrl, &mnt);
    deploy?;
    unmounted?;
    Ok(())
}

/// Receive one profile's pack, snapshot a writable root from it, and run
/// `kernel-install` inside it.
#[allow(clippy::too_many_arguments)]
fn deploy_profile(
    cfg: &Config,
    ctrl: &Controller,
    mnt: &str,
    root_part: &str,
    layout: &Layout,
    profile: &ProfilePack,
    pack: &PackFile,
    kind: &str,
    tick: &mut impl FnMut(&Controller, &str),
) -> Result<()> {
    tick(ctrl, &format!("receiving {} ({kind})", profile.name));
    receive_pack(cfg, ctrl, mnt, pack)?;

    tick(ctrl, &format!("snapshotting {}", profile.root_subvol()));
    make_writable_snapshot(cfg, ctrl, mnt, &profile.stock_subvol(), &profile.root_subvol())?;

    tick(ctrl, &format!("installing kernel for {}", profile.name));
    install_kernel(
        cfg,
        ctrl,
        mnt,
        root_part,
        layout.boot_subvol(),
        &profile.root_subvol(),
        &profile.name,
    )
}

/// Refuse to touch a device that is not boot-ROM capable (avoids nuking a USB
/// stick that just happens to hold the snapshots) or that is currently in use
/// (a mounted partition, active swap, or an LVM/MD/dm holder) — wiping a live
/// disk, e.g. the media the snapshots are being read from, would corrupt it.
fn guard_target(cfg: &Config, ctrl: &Controller, device: &StorageDevice) -> Result<()> {
    if !device.boot_rom_capable() {
        return Err(format!(
            "{} ({}) is not a boot-ROM capable target",
            device.path,
            device.kind.as_str()
        ));
    }

    let in_use = crate::core::storage::device_in_use(&device.path);
    if !in_use.is_empty() {
        let detail = in_use.join("; ");
        // In dry-run we never touch the disk, so warn but let the flow proceed;
        // a real run refuses outright.
        if cfg.dry_run {
            ctrl.log(format!(
                "[dry-run] warning: {} is in use ({detail}) — a real run would refuse it",
                device.path
            ));
        } else {
            return Err(format!("{} is in use: {detail}", device.path));
        }
    }
    Ok(())
}

/// Whole-disk path + 1-based index -> partition node path
/// (`/dev/mmcblk0`, 3 -> `/dev/mmcblk0p3`; `/dev/sda`, 3 -> `/dev/sda3`).
fn partition_path(disk: &str, index: u32) -> String {
    let name = disk.trim_start_matches("/dev/");
    if name.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        format!("{disk}p{index}")
    } else {
        format!("{disk}{index}")
    }
}

fn write_gpt(
    cfg: &Config,
    ctrl: &Controller,
    disk: &str,
    size_bytes: u64,
    sector_size: u64,
) -> Result<()> {
    // Three-partition FlipperOS layout (loader / metadata / root), written
    // entirely in-process with the `gpt` crate (no sgdisk).
    //
    // GPT geometry is expressed in logical blocks, so it must be laid out in
    // the device's *native* sector size (512 on eMMC/SD, 4096 on UFS). The
    // crate only knows 512 and 4096, so anything else is an error rather than a
    // silent fall-back to a wrong size.
    let lb_size = gpt::disk::LogicalBlockSize::try_from(sector_size)
        .map_err(|_| format!("{disk}: unsupported logical block size {sector_size} (must be 512 or 4096)"))?;
    let s = sector_size;
    let loader_first = LOADER_START / s;
    let metadata_first = METADATA_START / s;
    let root_first = ROOT_START / s;

    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] GPT on {disk} ({sector_size}-byte sectors): loader[LBA {loader_first}..{}], metadata[{metadata_first}..{}], root[{root_first}..end]",
            metadata_first - 1,
            root_first - 1,
        ));
        return Ok(());
    }
    ctrl.log(format!("writing GPT to {disk} (loader / metadata / root)"));

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .map_err(|e| format!("open {disk}: {e}"))?;

    // Fresh protective MBR so the disk is recognised as GPT by other tools.
    let total_sectors = size_bytes / s;
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
        u32::try_from(total_sectors.saturating_sub(1)).unwrap_or(0xFFFF_FFFF),
    );
    mbr.overwrite_lba0(&mut file)
        .map_err(|e| format!("write protective MBR to {disk}: {e}"))?;

    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .logical_block_size(lb_size)
        .create_from_device(file, None)
        .map_err(|e| format!("initialise GPT on {disk}: {e}"))?;

    // Derive the last usable LBA from the freshly-created empty table.
    let free = gdisk.find_free_sectors();
    let (region_start, region_len) = free
        .first()
        .copied()
        .ok_or("no free space for partitions")?;
    let last_usable = region_start + region_len - 1;
    if root_first > last_usable {
        return Err(format!("{disk} is too small for the FlipperOS layout"));
    }

    // Partition type GUIDs matching the current FlipperOS images.
    let loader_type: gpt::partition_types::Type = "3DE21764-95BD-54BD-A5C3-4ABE786F38A8"
        .parse()
        .map_err(|e| format!("loader type guid: {e}"))?;
    let metadata_type: gpt::partition_types::Type = "8DA63339-0007-60C0-C436-083AC8230908"
        .parse()
        .map_err(|e| format!("metadata type guid: {e}"))?;
    let root_type: gpt::partition_types::Type = "B921B045-1DF0-41C3-AF44-4C6F280D3FAE"
        .parse()
        .map_err(|e| format!("root type guid: {e}"))?;

    // GPT attribute bit 2 = "Legacy BIOS Bootable".
    const LEGACY_BIOS_BOOTABLE: u64 = 1 << 2;

    let mut parts: std::collections::BTreeMap<u32, gpt::partition::Partition> =
        std::collections::BTreeMap::new();
    parts.insert(
        1,
        gpt::partition::Partition {
            part_type_guid: loader_type,
            part_guid: uuid::Uuid::new_v4(),
            first_lba: loader_first,
            last_lba: metadata_first - 1,
            flags: 0,
            name: "loader".to_string(),
        },
    );
    parts.insert(
        2,
        gpt::partition::Partition {
            part_type_guid: metadata_type,
            part_guid: uuid::Uuid::new_v4(),
            first_lba: metadata_first,
            last_lba: root_first - 1,
            flags: 0,
            name: "metadata".to_string(),
        },
    );
    parts.insert(
        ROOT_PART_INDEX,
        gpt::partition::Partition {
            part_type_guid: root_type,
            part_guid: uuid::Uuid::new_v4(),
            first_lba: root_first,
            last_lba: last_usable,
            flags: LEGACY_BIOS_BOOTABLE,
            name: "root".to_string(),
        },
    );

    gdisk
        .update_partitions(parts)
        .map_err(|e| format!("set partitions on {disk}: {e}"))?;

    let dev = gdisk.write().map_err(|e| format!("write GPT to {disk}: {e}"))?;
    dev.sync_all().map_err(|e| format!("sync {disk}: {e}"))?;
    drop(dev);

    // Ask the kernel to re-read the partition table so the nodes appear.
    exec(cfg, ctrl, Command::new("partprobe").arg(disk)).ok();
    settle(cfg, ctrl);
    Ok(())
}

fn install_uboot(
    cfg: &Config,
    ctrl: &Controller,
    device: &StorageDevice,
    build: &UbootBuild,
) -> Result<()> {
    // Write the image directly onto the loader partition (p1) from its start.
    // The GPT places p1 at the RK3576 mask-ROM offset, so this lands the
    // bootloader exactly where the boot ROM expects it. Wait for the freshly
    // created node before opening it.
    let loader = partition_path(&device.path, LOADER_PART_INDEX);
    wait_for_device(cfg, ctrl, &loader)?;
    write_source_to_offset(cfg, ctrl, &build.image_location, &build.source, &loader, 0, false)?;

    // On UFS the RK3576 mask ROM fetches DRAM init + SPL from Boot LU A, and
    // ignores the copy on the main LU's loader partition. Duplicate the leading
    // part of the image onto Boot LU A at the same 32 KiB offset. The boot LU is
    // small (typically 4 MiB) while the image is ~10 MiB, so we fill it and
    // discard the tail: only the DRAM init + SPL at the front is needed there.
    if device.kind == StorageKind::Ufs {
        match crate::core::storage::find_ufs_boot_lu_a(&device.path) {
            Some(boot_lu) => {
                ctrl.log(format!("UFS target: mirroring u-boot to Boot LU A ({boot_lu})"));
                wait_for_device(cfg, ctrl, &boot_lu)?;
                write_source_to_offset(
                    cfg,
                    ctrl,
                    &build.image_location,
                    &build.source,
                    &boot_lu,
                    LOADER_START,
                    true,
                )?;
            }
            None => ctrl.log(format!(
                "warning: {} is UFS but no Boot LU A was found — the mask ROM may \
                 fail to load the bootloader; check the device's UFS provisioning",
                device.path
            )),
        }
    }
    Ok(())
}

fn receive_pack(cfg: &Config, ctrl: &Controller, mnt: &str, pack: &PackFile) -> Result<()> {
    // The packs are zstd-compressed `btrfs send` streams. Decompress in-process
    // (libzstd via the `zstd` crate) and pipe the stream into `btrfs receive` at
    // the top level, which recreates the profile's stock subvolume from the
    // stream.
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] zstd -d {} | btrfs receive {mnt}",
            pack.location
        ));
        return Ok(());
    }
    let reader = open_source(&pack.location, &pack.source)?;
    let decoder = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| format!("zstd {}: {e}", pack.location))?;
    let mut recv = Command::new("btrfs");
    recv.arg("receive").arg(mnt);
    pump_reader_into(ctrl, Box::new(decoder), recv, &pack.location)
}

fn make_writable_snapshot(
    cfg: &Config,
    ctrl: &Controller,
    mnt: &str,
    stock: &str,
    root: &str,
) -> Result<()> {
    // The received `*_stock` subvolume is the RO golden base; snapshot a writable
    // deployable root from it (matching the build recipe).
    exec(
        cfg,
        ctrl,
        Command::new("btrfs")
            .arg("subvolume")
            .arg("snapshot")
            .arg(format!("{mnt}/{stock}"))
            .arg(format!("{mnt}/{root}")),
    )
}

fn make_filesystem(cfg: &Config, ctrl: &Controller, part: &str, layout: &Layout) -> Result<String> {
    // Writing the GPT and re-reading it is asynchronous: udev may not have
    // created the partition node yet, so `mkfs.btrfs` can race and fail with
    // "No such file or directory". Wait for the node to appear first.
    wait_for_device(cfg, ctrl, part)?;
    exec(
        cfg,
        ctrl,
        Command::new("mkfs.btrfs")
            .arg("-f")
            .arg("-L")
            .arg(&layout.label)
            .arg(part),
    )?;

    let mnt = "/run/flipperos-install".to_string();
    exec(cfg, ctrl, Command::new("mkdir").arg("-p").arg(&mnt))?;
    // Mount the Btrfs top level (subvolid=5) so we create the shared subvolumes
    // and receive profile roots directly on it.
    exec(
        cfg,
        ctrl,
        Command::new("mount")
            .arg("-t")
            .arg("btrfs")
            .arg("-o")
            .arg(&layout.options)
            .arg(part)
            .arg(&mnt),
    )?;

    // Shared, top-level subvolume skeleton from the layout config.
    for sv in &layout.subvolumes {
        ctrl.log(format!("creating subvolume {}", sv.name));
        exec(
            cfg,
            ctrl,
            Command::new("btrfs")
                .arg("subvolume")
                .arg("create")
                .arg(format!("{mnt}/{}", sv.name)),
        )?;
        if let Some(compression) = &sv.compression {
            exec(
                cfg,
                ctrl,
                Command::new("btrfs")
                    .arg("property")
                    .arg("set")
                    .arg(format!("{mnt}/{}", sv.name))
                    .arg("compression")
                    .arg(compression),
            )?;
        }
        for dir in &sv.nodatacow {
            let path = format!("{mnt}/{}/{}", sv.name, dir);
            exec(cfg, ctrl, Command::new("mkdir").arg("-p").arg(&path))?;
            // +C only affects files created afterwards, hence the empty dir.
            exec(cfg, ctrl, Command::new("chattr").arg("+C").arg(&path))?;
        }
    }
    Ok(mnt)
}

fn install_kernel(
    cfg: &Config,
    ctrl: &Controller,
    mnt: &str,
    root_part: &str,
    boot_subvol: &str,
    root_subvol: &str,
    profile_name: &str,
) -> Result<()> {
    // chroot into the deployed profile root and run its own `kernel-install` for
    // every installed kernel, writing into the shared /boot subvolume. All the
    // real logic (kernel-install, plugins, BLS entry-token) lives in the profile.
    let boot = format!("{mnt}/{boot_subvol}");
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] mount -o subvol={root_subvol} {root_part}; \
             chroot: kernel-install add (all kernels) -> {boot}"
        ));
        return Ok(());
    }

    // Mount the profile's root subvolume at its own mountpoint instead of
    // chrooting into the bare subvolume *directory* under the top-level mount.
    // A real `-o subvol=` mount gives the chroot a `/` whose FSROOT is
    // `/<root_subvol>` — which the profile's flipper-bls BLS plugin reads via
    // `findmnt` to discover the current root subvol. A chroot into the subvolume
    // directory has no /proc/self/mountinfo entry for `/`, so that lookup fails
    // with "cannot determine current root subvol".
    let root = format!("{mnt}-root");
    std::fs::create_dir_all(&root).map_err(|e| format!("mkdir {root}: {e}"))?;
    exec(
        cfg,
        ctrl,
        Command::new("mount")
            .arg("-o")
            .arg(format!("subvol={root_subvol}"))
            .arg(root_part)
            .arg(&root),
    )?;

    ctrl.log(format!("installing kernels for {profile_name} (chroot {root})"));
    // Feed the embedded script to `sh` on stdin (`-s`) rather than staging a
    // temp file, which avoids needing a writable/executable scratch path.
    // Positional args after `-s` become $1/$2/$3 inside the script.
    let mut sh = Command::new("sh");
    sh.arg("-s").arg(&root).arg(&boot).arg(profile_name);
    let ran = pump_reader_into(
        ctrl,
        Box::new(std::io::Cursor::new(INSTALL_KERNEL_SH.as_bytes())),
        sh,
        "kernel-install script",
    );

    // Always unmount the profile root (recursively, in case the script left an
    // API mount behind), surfacing the script error in preference to any
    // unmount error.
    let unmounted = exec(cfg, ctrl, Command::new("umount").arg("-R").arg(&root));
    ran?;
    unmounted?;
    Ok(())
}

fn unmount(cfg: &Config, ctrl: &Controller, mnt: &str) -> Result<()> {
    exec(cfg, ctrl, &mut Command::new("sync"))?;
    // A recursive unmount handles any nested btrfs subvolumes; fall back to a
    // lazy detach if the mount is still busy (e.g. after a partial receive).
    if exec(cfg, ctrl, Command::new("umount").arg("-R").arg(mnt)).is_err() {
        exec(cfg, ctrl, Command::new("umount").arg("-l").arg(mnt)).ok();
    }
    Ok(())
}

/// Give udev/the kernel a moment to create partition nodes.
fn settle(cfg: &Config, ctrl: &Controller) {
    exec(cfg, ctrl, Command::new("udevadm").arg("settle")).ok();
}

/// Poll for a (freshly-created) block device node to appear, up to ~10 s.
/// The GPT re-read + udev node creation is asynchronous, so callers that need
/// to open a partition immediately (e.g. `mkfs`) must wait for it.
fn wait_for_device(cfg: &Config, ctrl: &Controller, path: &str) -> Result<()> {
    if cfg.dry_run {
        return Ok(());
    }
    let node = std::path::Path::new(path);
    if node.exists() {
        return Ok(());
    }
    // Nudge udev, then poll for the node.
    settle(cfg, ctrl);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if node.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("timed out waiting for {path} to appear"))
}

/// Run a command, honouring dry-run. On dry-run we only log the command line.
fn exec(cfg: &Config, ctrl: &Controller, cmd: &mut Command) -> Result<()> {
    let rendered = render(cmd);
    if cfg.dry_run {
        ctrl.log(format!("[dry-run] {rendered}"));
        return Ok(());
    }
    ctrl.log(format!("$ {rendered}"));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `{rendered}`: {e}"))?;
    drain_child(ctrl, &mut child);
    let status = child
        .wait()
        .map_err(|e| format!("wait `{rendered}`: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{rendered}` exited with {status}"))
    }
}

/// Forward a child's stdout/stderr line-by-line into the activity log, so no
/// command output ever reaches the terminal the TUI is drawing on.
fn drain_child(ctrl: &Controller, child: &mut std::process::Child) {
    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::scope(|s| {
        if let Some(out) = stdout {
            s.spawn(|| {
                for line in BufReader::new(out).lines().map_while(std::result::Result::ok) {
                    ctrl.log(line);
                }
            });
        }
        if let Some(err) = stderr {
            s.spawn(|| {
                for line in BufReader::new(err).lines().map_while(std::result::Result::ok) {
                    ctrl.log(line);
                }
            });
        }
    });
}

/// Open a byte stream for a source: an HTTP GET for [`Source::Server`], or the
/// local file for removable media.
fn open_source(location: &str, source: &Source) -> Result<Box<dyn Read + Send>> {
    match source {
        Source::Server => {
            let resp = ureq::get(location)
                .call()
                .map_err(|e| format!("GET {location}: {e}"))?;
            Ok(Box::new(resp.into_reader()))
        }
        Source::Removable { .. } => {
            let file = std::fs::File::open(location)
                .map_err(|e| format!("open {location}: {e}"))?;
            Ok(Box::new(file))
        }
    }
}

/// Stream a source (HTTP URL for [`Source::Server`], local path otherwise)
/// directly onto `device`, starting at `offset` bytes, then flush to disk.
///
/// When `fill` is set, the source is expected to be larger than the target: we
/// write until the device runs out of space and treat that as success, so only
/// the leading part that fits is kept (used to seed a small UFS boot LU with the
/// front of the full U-Boot image). Otherwise a short write is an error.
fn write_source_to_offset(
    cfg: &Config,
    ctrl: &Controller,
    location: &str,
    source: &Source,
    device: &str,
    offset: u64,
    fill: bool,
) -> Result<()> {
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] write {location} -> {device} @ offset {offset} B{}",
            if fill { " (fill, discarding overflow)" } else { "" }
        ));
        return Ok(());
    }
    ctrl.log(format!("writing {location} -> {device} @ offset {offset} B"));

    let mut reader = open_source(location, source)?;
    let mut file = OpenOptions::new()
        .write(true)
        .open(device)
        .map_err(|e| format!("open {device}: {e}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {device} to {offset}: {e}"))?;
    let written = if fill {
        copy_until_full(&mut reader, &mut file)
    } else {
        io::copy(&mut reader, &mut file)
    }
    .map_err(|e| format!("write {location} to {device}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync {device}: {e}"))?;
    ctrl.log(format!("wrote {written} bytes to {device}"));
    Ok(())
}

/// Linux `errno` for "No space left on device" — what a `write(2)` past the end
/// of a block device returns.
const ENOSPC: i32 = 28;

/// Copy `reader` into `writer` until the reader is exhausted *or* the writer
/// runs out of space, returning the bytes written. Hitting the end of a
/// fixed-size block device (a short write or `ENOSPC`) ends the copy cleanly
/// rather than erroring — the caller intends to keep only the leading part.
fn copy_until_full(reader: &mut dyn Read, writer: &mut impl io::Write) -> io::Result<u64> {
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut off = 0;
        while off < n {
            match writer.write(&buf[off..n]) {
                Ok(0) => return Ok(total), // device full: nothing more accepted
                Ok(w) => {
                    off += w;
                    total += w as u64;
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(ref e) if e.raw_os_error() == Some(ENOSPC) => return Ok(total),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(total)
}

/// Spawn `cmd` and pump an arbitrary reader into its stdin, then wait.
fn pump_reader_into(
    ctrl: &Controller,
    mut reader: Box<dyn Read + Send>,
    mut cmd: Command,
    label: &str,
) -> Result<()> {
    let rendered = render(&cmd);
    ctrl.log(format!("$ {rendered} < {label}"));
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `{rendered}`: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("failed to open child stdin")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Pump the source into stdin while concurrently draining stdout/stderr into
    // the log, so the child never blocks on a full pipe and never writes to the
    // terminal.
    let copy_result = std::thread::scope(|s| {
        use std::io::{BufRead, BufReader};
        if let Some(out) = stdout {
            s.spawn(|| {
                for line in BufReader::new(out).lines().map_while(std::result::Result::ok) {
                    ctrl.log(line);
                }
            });
        }
        if let Some(err) = stderr {
            s.spawn(|| {
                for line in BufReader::new(err).lines().map_while(std::result::Result::ok) {
                    ctrl.log(line);
                }
            });
        }
        let r = io::copy(&mut reader, &mut stdin).map(|_| ());
        // Close stdin so the child sees EOF.
        drop(stdin);
        r
    });

    let status = child
        .wait()
        .map_err(|e| format!("wait `{rendered}`: {e}"))?;

    copy_result.map_err(|e| format!("streaming {label} into `{rendered}`: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{rendered}` exited with {status}"))
    }
}

fn render(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    for arg in cmd.get_args() {
        parts.push(arg.to_string_lossy().into_owned());
    }
    parts.join(" ")
}
