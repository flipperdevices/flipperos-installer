//! The installer engine: partition, format, receive snapshots, install kernels.
//!
//! Every destructive step goes through [`Step::exec`], which honours
//! [`Config::dry_run`]: in dry-run mode commands are logged but not executed, so
//! the whole flow can be exercised safely on a development host.
//!
//! The high-level flow mirrors what the user selected in either frontend:
//!   1. `blkdiscard` the whole target device.
//!   2. Write a fresh GPT reserving the RK3576 bootloader area.
//!   3. Write the bootloader. On UFS it goes only to the spare boot LU, and the
//!      boot ROM is switched over to it once the image verifies; everywhere else
//!      it goes to the reserved boot area at the start of the loader partition.
//!      On UFS the Falcon boot menu then goes to that reserved area instead, the
//!      bootloader having no need of it.
//!   4. `mkfs.btrfs` on the root partition and create the subvolume skeleton.
//!   5. If the build ships a `/home` seed, `btrfs receive` it to a transient
//!      base and snapshot a writable `@home` from it so /home starts populated.
//!   6. For each selected profile: `btrfs receive` its snapshot stream, then
//!      chroot into it and run `kernel-install` for every installed kernel.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::core::controller::{Config, Controller};
use crate::core::layout::{self, Layout};
use crate::core::model::{
    human_bytes, FetchMode, PackFile, ProfilePack, Source, StorageDevice, StorageKind, UbootBuild,
};
use crate::core::{fetch, stage, storage, ufs};

// GPT layout used by FlipperOS images, as byte offsets from the start of disk.
// These are absolute byte offsets, independent of the device's sector size;
// `write_gpt` converts them to LBAs using the target's native block size.
// loader:   [32 KiB, 60 MiB)  (holds idbloader + U-Boot)
// metadata: [60 MiB, 64 MiB)
// root:     [64 MiB, end]     (Btrfs)
//
// The RK3576 boot ROM reads `idbloader`/U-Boot starting at byte offset 32 KiB,
// which is the start of the `loader` partition (p1), so the image is written
// to that partition from its beginning.
//
// On UFS the boot ROM reads the bootloader from a boot LU instead and ignores
// this partition entirely, which is what leaves it free for the Falcon boot menu
// (see [`install_boot_menu`]). The menu is by far the largest thing that goes
// here, so it is what now sets the floor under `METADATA_START`.
const LOADER_START: u64 = 32 * 1024;
const METADATA_START: u64 = 60 * 1024 * 1024;
const ROOT_START: u64 = 64 * 1024 * 1024;
/// Room the loader partition offers whatever is written to it from its start.
/// Both ends are fixed by [`write_gpt`], so an image that does not fit here does
/// not fit on any target.
const LOADER_CAPACITY: u64 = METADATA_START - LOADER_START;
/// The loader (U-Boot) partition is the first partition.
const LOADER_PART_INDEX: u32 = 1;
/// The Btrfs root is the third partition.
const ROOT_PART_INDEX: u32 = 3;

/// Dedicated top-level directory that holds the read-only `*_stock` golden
/// bases. The build scripts nest every received stock snapshot under it (see
/// flipperone-linux-build-scripts@80dbfc8), keeping the Btrfs top level for
/// profile roots and shared subvolumes; the installer mirrors that layout.
const STOCK_SNAPSHOTS_DIR: &str = "@stock-snapshots";

/// Shared `/home` subvolume. Normally created empty by the layout skeleton, but
/// prepopulated from the build's `home_*_pack.zst` seed when one is available.
const HOME_SUBVOL: &str = "@home";

/// Mountpoint for the Btrfs top level of the target, where the subvolume
/// skeleton is built and the packs are received.
const TARGET_MNT: &str = "/run/flipperos-install";

/// Mountpoint for a single profile root, mounted with `-o subvol=` so the
/// chroot's `/` has a real FSROOT (see [`install_kernel`]). Deliberately a
/// *sibling* of [`TARGET_MNT`] rather than a path under it, so unmounting one
/// never drags the other down.
const PROFILE_MNT: &str = "/run/flipperos-install-root";

/// POSIX shell glue that chroots into a deployed profile and runs
/// `kernel-install` for every installed kernel. Written to a temp path and
/// executed once per profile at install time.
const INSTALL_KERNEL_SH: &str = include_str!("../../scripts/flipperos-install-kernel.sh");

type Result<T> = std::result::Result<T, String>;

/// Coarse, step-based progress reporter for an install run. Each [`Ticker::begin`]
/// marks the start of a discrete step and moves the bar to the fraction of steps
/// completed so far; long steps can additionally animate within their own slice
/// via [`Ticker::step_span`] + [`Controller::set_progress`].
pub(crate) struct Ticker {
    step: u32,
    total: u32,
}

impl Ticker {
    pub(crate) fn new(total: u32) -> Self {
        Ticker { step: 0, total }
    }

    /// Advance to the next step: log `msg` and set the bar to the fraction of
    /// steps completed *before* this one (so the step's own slice is left free
    /// to fill in as it progresses).
    pub(crate) fn begin(&mut self, ctrl: &Controller, msg: &str) {
        self.step += 1;
        ctrl.set_progress((self.step - 1) as f32 / self.total as f32);
        ctrl.log(msg.to_string());
    }

    /// `(base, span)` of the current step's slice of the overall bar, so a step
    /// can report intra-step progress as `set_progress(base + span * frac)`.
    pub(crate) fn step_span(&self) -> (f32, f32) {
        (
            (self.step - 1) as f32 / self.total as f32,
            1.0 / self.total as f32,
        )
    }
}

/// How a digest that does not match the manifest is treated while the install is
/// already writing.
///
/// In [`FetchMode::VerifyFirst`] every artifact was verified before the target
/// was touched, so a mismatch now means the bytes changed underneath us and the
/// run is failed. In [`FetchMode::Stream`] the operator declined the up-front
/// check, and the verdict only arrives once the data has landed, so it can only
/// be reported.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnMismatch {
    Fail,
    Warn,
}

impl OnMismatch {
    fn for_mode(mode: FetchMode) -> Self {
        match mode {
            FetchMode::VerifyFirst => OnMismatch::Fail,
            FetchMode::Stream => OnMismatch::Warn,
        }
    }
}

/// Apply a digest verdict according to `policy`, logging what happened.
pub(crate) fn apply_verdict(
    ctrl: &Controller,
    what: &str,
    verdict: fetch::Verdict,
    policy: OnMismatch,
) -> Result<()> {
    if let Some(msg) = verdict.message(what) {
        ctrl.log(msg.clone());
        if matches!(verdict, fetch::Verdict::Mismatch(_, _)) {
            if policy == OnMismatch::Fail {
                return Err(msg);
            }
            // Streaming: the bytes are already on the target, so say so plainly
            // rather than letting a warning imply the install is still sound.
            ctrl.log(format!(
                "warning: {what} was already written to the target; \
                 do not boot this installation"
            ));
        }
    } else {
        ctrl.log(format!("{what}: sha256 verified"));
    }
    Ok(())
}

/// A `Read` adapter that reports the running total of bytes read to a callback
/// after each read, used to surface transfer progress for the long profile
/// receive step.
struct ProgressReader<'a, R> {
    inner: R,
    read: u64,
    on_progress: &'a mut dyn FnMut(u64),
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.read += n as u64;
            (self.on_progress)(self.read);
        }
        Ok(n)
    }
}

/// Build a throttled byte-progress callback for a receive step: it animates the
/// bar within this step's `(base, span)` slice on each 1% change and logs a
/// `<what>: N%` line every 10%. `total` is the expected (compressed) byte count;
/// when it is 0 (unknown) the bar is left alone and throughput is logged every
/// 32 MiB so the step is never silent.
pub(crate) fn receive_progress(
    ctrl: &Controller,
    what: String,
    base: f32,
    span: f32,
    total: u64,
) -> impl FnMut(u64) + '_ {
    let mut last_pct: i64 = -1;
    let mut last_bucket: i64 = -1;
    move |read: u64| {
        if total > 0 {
            let read = read.min(total);
            let pct = (read * 100 / total) as i64;
            if pct == last_pct {
                return;
            }
            last_pct = pct;
            ctrl.set_progress(base + span * (read as f32 / total as f32));
            let bucket = pct / 10;
            if bucket != last_bucket {
                last_bucket = bucket;
                ctrl.log(format!(
                    "{what}: {pct}% ({} / {})",
                    human_bytes(read),
                    human_bytes(total),
                ));
            }
        } else {
            let bucket = (read / (32 * 1024 * 1024)) as i64;
            if bucket != last_bucket {
                last_bucket = bucket;
                ctrl.log(format!("{what}: {} received", human_bytes(read)));
            }
        }
    }
}

/// Run the whole installation. Called on a worker thread by the controller.
pub fn run(ctrl: &Arc<Controller>) -> Result<()> {
    let state = ctrl.snapshot();
    let cfg = ctrl.config();

    let device = state
        .target()
        .cloned()
        .ok_or("no target device selected")?;
    let mut uboot = state
        .selected_uboot()
        .cloned()
        .ok_or("no u-boot build selected")?;
    let mut build = state
        .selected_build()
        .cloned()
        .ok_or("no snapshot build selected")?;
    if !build.loaded {
        return Err("snapshot build profiles not loaded yet".to_string());
    }
    if !uboot.loaded {
        return Err("u-boot build manifest not loaded yet".to_string());
    }
    if build.minimal().is_none() {
        return Err("snapshot build has no Minimal profile".to_string());
    }
    // The boot menu goes to the loader partition, and only a UFS target has one to
    // spare — everywhere else U-Boot itself occupies it. Dropping it here is what
    // keeps it out of the staging plan too, so a non-UFS run never fetches an
    // image it has nowhere to put.
    if device.kind != StorageKind::Ufs {
        uboot.boot_menu = None;
    }
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

    // What the provisioning check found when the target was selected. Logged
    // before the guards, so it is on screen to explain a refusal; nothing here
    // aborts the run by itself — the boot-LU guard below is what refuses a device
    // the bootloader cannot be written to.
    if let Some(status) = &state.ufs {
        ctrl.log(format!(
            "ufs provisioning: {} (scheme: {})",
            status.short_label(),
            status.scheme_origin
        ));
        for m in status.critical().chain(status.warnings()) {
            ctrl.log(format!("warning: ufs {}", m.what));
        }
    }

    // An earlier attempt that failed midway can leave our own mountpoints
    // behind. Clear them before the in-use guard, so a retry is not refused
    // because of our own leftovers (a foreign mount still makes it refuse).
    release_own_mounts(cfg, ctrl);
    // Our own read-only media mounts would otherwise make the target look busy.
    // Dropping them here also fails fast if the artifacts we are about to read
    // live on the disk we are about to wipe.
    crate::core::removable::unmount_ours(Some(&device.path));
    guard_target(cfg, ctrl, &device)?;
    // Decided once, before anything is written: the guard below checks the boot LU
    // this run will write, and `install_uboot` writes that same one.
    let boot_lu = ufs_boot_lu_plan(ctrl, &device);
    guard_ufs_boot_lu(cfg, ctrl, &device, &uboot, boot_lu.as_ref())?;
    guard_boot_menu_fits(cfg, ctrl, &uboot)?;
    stage::guard_not_on_target(&uboot, &build, &extras, &device.path)?;

    // Resolve the Btrfs layout: prefer one shipped with the images, else the
    // built-in default.
    let sources: Vec<&Source> = vec![&build.source];
    let (fs_layout, layout_origin) = layout::resolve(cfg, &state.board.board_id, &sources);
    ctrl.log(format!(
        "btrfs layout: {layout_origin} — label '{}', {} subvolume(s)",
        fs_layout.label,
        fs_layout.subvolumes.len()
    ));

    // How a digest mismatch is treated once we are writing, and whether the
    // artifacts are checked before that point at all.
    let fetch_mode = state.selection.fetch;
    let policy = OnMismatch::for_mode(fetch_mode);
    let plan = stage::plan(&uboot, &build, &extras);

    // A locally supplied `*.tar.zst` is unpacked into the scratch dir first; the
    // bundle already describes its files at the paths they will occupy.
    let pending_archive = state
        .bundle
        .as_ref()
        .map(|b| b.reference.clone())
        .filter(|r| r.archive.is_some());

    // 4 fixed steps, then receive + snapshot + kernel per deployed profile, plus
    // one receive step for the shared /home seed when the build ships one, one for
    // the boot menu when the target takes one, one staging step per artifact when
    // verifying up front, and one for unpacking a local archive.
    let deployed = 1 + extras.len();
    let verify_steps = match fetch_mode {
        FetchMode::VerifyFirst => plan.len() as u32,
        FetchMode::Stream => 0,
    };
    let total_steps = 4
        + uboot.boot_menu.is_some() as u32
        + pending_archive.is_some() as u32
        + verify_steps
        + deployed as u32 * 3
        + build.home_pack.is_some() as u32;
    let mut ticker = Ticker::new(total_steps);

    if let Some(reference) = &pending_archive {
        stage::unpack_bundle(cfg, ctrl, reference, &mut ticker)?;
    }

    // Verify (and, for remote artifacts, download) everything before the first
    // destructive command, so a bad or truncated artifact cannot leave the
    // operator with a wiped device. `_staged` owns the scratch files: keeping it
    // alive until the end of the run is what keeps them on disk.
    let _staged = match fetch_mode {
        FetchMode::VerifyFirst => {
            let staged = stage::run(cfg, ctrl, &plan, &mut ticker)?;
            staged.localise_uboot(&mut uboot);
            staged.localise_build(&mut build);
            Some(staged)
        }
        FetchMode::Stream => {
            ctrl.log(
                "streaming without up-front verification; \
                 digests are checked as data is written"
                    .to_string(),
            );
            None
        }
    };
    // The Minimal pack and the extras were cloned out of `build` before staging
    // rewrote its locations, so take them again from the staged copy.
    let minimal = build
        .minimal()
        .cloned()
        .ok_or("snapshot build has no Minimal profile")?;
    let extras: Vec<ProfilePack> = build
        .extra_profiles()
        .filter(|p| state.selection.profiles.iter().any(|n| n == &p.name))
        .cloned()
        .collect();

    let root_part = partition_path(&device.path, ROOT_PART_INDEX);

    // 1. Wipe.
    ticker.begin(ctrl, &format!("blkdiscard {}", device.path));
    wait_for_exclusive_access(cfg, ctrl, &device.path);
    exec(cfg, ctrl, Command::new("blkdiscard").arg("-f").arg(&device.path))?;

    // 2. Partition.
    ticker.begin(ctrl, "writing GPT");
    write_gpt(
        cfg,
        ctrl,
        &device.path,
        device.size_bytes,
        device.logical_block_size,
    )?;

    // 3. Bootloader.
    ticker.begin(ctrl, &format!("installing u-boot {}", uboot.label));
    install_uboot(cfg, ctrl, &device, &uboot, boot_lu.as_ref(), policy)?;

    // 3a. Boot menu, onto the loader partition the bootloader left free. A no-op
    // unless the target is UFS and the build ships one.
    install_boot_menu(cfg, ctrl, &device, &uboot, policy, &mut ticker)?;

    // 4. Filesystem + subvolumes.
    ticker.begin(ctrl, &format!("mkfs.btrfs {root_part}"));
    let mnt = make_filesystem(cfg, ctrl, &root_part, &fs_layout)?;

    // The target is mounted from here on. Run the remaining steps in an inner
    // block so we can always unmount afterwards, even when a btrfs command
    // fails partway through.
    let deploy = (|| -> Result<()> {
        // The golden `*_stock` bases are all received under a dedicated
        // @stock-snapshots directory; create it before the first receive.
        exec(
            cfg,
            ctrl,
            Command::new("mkdir")
                .arg("-p")
                .arg(format!("{mnt}/{STOCK_SNAPSHOTS_DIR}")),
        )?;

        // Prepopulate the shared /home from its seed pack, if the build ships
        // one. The seed is a full `btrfs send` of @home. A received subvolume is
        // read-only and carries a `received_uuid`; we can't simply clear the ro
        // flag (btrfs refuses while received_uuid is set, and forcing it would
        // leave a writable @home still advertising a received_uuid that a
        // hand-crafted incremental could target). Instead: receive the seed into
        // a transient staging subvolume, snapshot a writable @home from it (a
        // snapshot is never assigned a received_uuid), then delete the staging
        // base — copy-on-write keeps the data with @home, and no golden base for
        // the version-independent @home lingers.
        if let Some(home_pack) = &build.home_pack {
            ticker.begin(ctrl, "receiving /home seed");
            let (base, span) = ticker.step_span();
            let mut on_progress = receive_progress(
                ctrl,
                "receiving /home seed".to_string(),
                base,
                span,
                home_pack.size_bytes,
            );
            // The seed stream names its subvolume @home (from `btrfs send
            // $TOP/@home`), so it lands at @stock-snapshots/@home.
            let stock_dir = format!("{mnt}/{STOCK_SNAPSHOTS_DIR}");
            let staged = format!("{stock_dir}/{HOME_SUBVOL}");
            receive_pack(
                cfg,
                ctrl,
                &stock_dir,
                home_pack,
                "/home seed",
                policy,
                &mut on_progress,
            )?;
            // Drop the empty placeholder the skeleton created (if it did) so the
            // writable @home snapshot can take its place; a custom layout that
            // omits @home leaves nothing to remove.
            if fs_layout.subvolumes.iter().any(|s| s.name == HOME_SUBVOL) {
                exec(
                    cfg,
                    ctrl,
                    Command::new("btrfs")
                        .arg("subvolume")
                        .arg("delete")
                        .arg(format!("{mnt}/{HOME_SUBVOL}")),
                )?;
            }
            make_writable_snapshot(cfg, ctrl, &mnt, HOME_SUBVOL, HOME_SUBVOL)?;
            // Remove the transient received base (received_uuid and all); @home
            // retains the shared extents.
            exec(
                cfg,
                ctrl,
                Command::new("btrfs")
                    .arg("subvolume")
                    .arg("delete")
                    .arg(&staged),
            )?;
        }

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
            policy,
            &mut ticker,
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
                policy,
                &mut ticker,
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
    policy: OnMismatch,
    ticker: &mut Ticker,
) -> Result<()> {
    ticker.begin(ctrl, &format!("receiving {} ({kind})", profile.name));

    // The receive is by far the longest step: decompressing and streaming a
    // multi-hundred-MiB pack. Animate the bar within this step's slice and log a
    // periodic percentage so neither frontend sits on a frozen bar. Progress is
    // measured against the compressed pack size (what we read off the wire/disk).
    let (base, span) = ticker.step_span();
    let what = format!("{} ({kind})", profile.name);
    let mut on_progress = receive_progress(
        ctrl,
        format!("receiving {what}"),
        base,
        span,
        pack.size_bytes,
    );
    let stock_dir = format!("{mnt}/{STOCK_SNAPSHOTS_DIR}");
    receive_pack(cfg, ctrl, &stock_dir, pack, &what, policy, &mut on_progress)?;

    ticker.begin(ctrl, &format!("snapshotting {}", profile.root_subvol()));
    make_writable_snapshot(cfg, ctrl, mnt, &profile.stock_subvol(), &profile.root_subvol())?;

    ticker.begin(ctrl, &format!("installing kernel for {}", profile.name));
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

    let in_use = storage::device_in_use(&device.path);
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

/// Refuse a UFS target whose boot LU cannot hold the whole U-Boot image.
///
/// The boot ROM reads DRAM init and the SPL from a boot LU, so [`install_uboot`]
/// writes the image there in full and verifies its digest. A device still
/// carrying the factory 4 MiB boot LU cannot hold one, and finding that out at
/// the U-Boot step would mean failing with the target already wiped — so it is
/// checked here, before the first destructive command. Reprovisioning the device
/// to the Flipper scheme is what fixes it.
fn guard_ufs_boot_lu(
    cfg: &Config,
    ctrl: &Controller,
    device: &StorageDevice,
    uboot: &UbootBuild,
    boot_lu: Option<&BootLuPlan>,
) -> Result<()> {
    if device.kind != StorageKind::Ufs {
        return Ok(());
    }
    // In dry-run nothing is written, so an unusable boot LU is a warning; a real
    // run refuses, the same split `guard_target` makes.
    let refuse = |reason: String| refuse_unless_dry_run(cfg, ctrl, reason);

    let Some(plan) = boot_lu else {
        return refuse(format!(
            "{} is UFS but has no boot LU, so the boot ROM would have nothing to \
             boot; reprovision the device first",
            device.path
        ));
    };
    let letter = ufs::boot_lu_name(plan.id);
    let needed = LOADER_START + uboot.size_bytes;
    match ufs::block_size_bytes(&plan.node) {
        // A manifest that publishes no size leaves nothing to compare against.
        _ if uboot.size_bytes == 0 => ctrl.log(format!(
            "warning: the u-boot manifest gives no image size, so {} cannot be \
             checked for room up front",
            plan.node
        )),
        None => ctrl.log(format!(
            "warning: cannot read the size of {}; it will not be checked for room",
            plan.node
        )),
        Some(size) if size < needed => {
            return refuse(format!(
                "UFS boot LU {letter} ({}) is only {}, too small for a {} u-boot image at \
                 offset {}; reprovision the device to the Flipper scheme first",
                plan.node,
                human_bytes(size),
                human_bytes(uboot.size_bytes),
                human_bytes(LOADER_START)
            ))
        }
        Some(size) => ctrl.log(format!(
            "UFS boot LU {letter} ({}): {} for a {} u-boot image",
            plan.node,
            human_bytes(size),
            human_bytes(uboot.size_bytes)
        )),
    }
    Ok(())
}

/// Refuse a boot menu image the loader partition cannot hold.
///
/// Checked before the first destructive command for the same reason
/// [`guard_ufs_boot_lu`] is: discovering it at the write step would mean failing
/// with the target already wiped. The image is the largest thing the installer
/// writes outside the filesystem and it grows with every Falcon build, so this is
/// the check that will eventually ask for a bigger partition.
fn guard_boot_menu_fits(cfg: &Config, ctrl: &Controller, uboot: &UbootBuild) -> Result<()> {
    let Some(menu) = &uboot.boot_menu else {
        return Ok(());
    };
    match menu.size_bytes {
        // A manifest that publishes no size leaves nothing to compare against.
        0 => ctrl.log(
            "warning: the manifest gives no boot menu image size, so the loader \
             partition cannot be checked for room up front"
                .to_string(),
        ),
        size if size > LOADER_CAPACITY => {
            return refuse_unless_dry_run(
                cfg,
                ctrl,
                format!(
                    "the loader partition holds {}, too small for a {} boot menu image; \
                     the partition layout has to grow before this build can be installed",
                    human_bytes(LOADER_CAPACITY),
                    human_bytes(size)
                ),
            )
        }
        size => ctrl.log(format!(
            "loader partition: {} for a {} boot menu image",
            human_bytes(LOADER_CAPACITY),
            human_bytes(size)
        )),
    }
    Ok(())
}

/// Report a guard's verdict: a real run refuses, a dry run only says it would.
///
/// Dry runs write nothing, so refusing one would keep the operator from
/// exercising the rest of the pipeline over a device that is never touched.
fn refuse_unless_dry_run(cfg: &Config, ctrl: &Controller, reason: String) -> Result<()> {
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] warning: {reason} — a real run would refuse it"
        ));
        Ok(())
    } else {
        Err(reason)
    }
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
    boot_lu: Option<&BootLuPlan>,
    policy: OnMismatch,
) -> Result<()> {
    // Everywhere but UFS the RK3576 boot ROM reads the bootloader from byte offset
    // 32 KiB of the disk, which the GPT makes the start of the loader partition
    // (p1), so the image is written onto that partition from its beginning. Wait
    // for the freshly created node before opening it.
    if device.kind != StorageKind::Ufs {
        let loader = partition_path(&device.path, LOADER_PART_INDEX);
        wait_for_device(cfg, ctrl, &loader)?;
        write_source_to_offset(
            cfg,
            ctrl,
            "u-boot image",
            &build.image_location,
            &build.source,
            &loader,
            0,
            build.sha256.as_deref(),
            policy,
        )?;
        return Ok(());
    }

    // On UFS the boot ROM fetches DRAM init + SPL from whichever boot LU
    // `bBootLunEn` selects and never looks at the main LU's loader partition, so
    // that is the *only* copy — the partition is left to the boot menu. The image
    // goes to the boot LU the boot ROM is not reading, at the same 32 KiB offset,
    // and only a verified write flips the flag over to it — an interrupted or
    // corrupted update therefore leaves the board booting the bootloader it
    // booted before.
    let Some(plan) = boot_lu else {
        ctrl.log(format!(
            "warning: {} is UFS but no boot LU was found — the boot ROM may fail to \
             load the bootloader; check the device's UFS provisioning",
            device.path
        ));
        return Ok(());
    };

    ctrl.log(format!(
        "UFS target: writing u-boot to boot LU {} ({}), currently booting {}",
        ufs::boot_lu_name(plan.id),
        plan.node,
        ufs::boot_lu_name(plan.active)
    ));
    wait_for_device(cfg, ctrl, &plan.node)?;
    let verified = write_source_to_offset(
        cfg,
        ctrl,
        "u-boot image",
        &build.image_location,
        &build.source,
        &plan.node,
        LOADER_START,
        build.sha256.as_deref(),
        policy,
    )?;

    if plan.id == plan.active {
        // Nothing to switch: this device has only the one boot LU, so the new
        // bootloader is live the moment it lands (see `ufs_boot_lu_plan`).
        return Ok(());
    }
    if !verified {
        // Only reachable in `stream` mode, where the fetch policy has decided a
        // digest mismatch is a warning rather than a failure (`verify first` has
        // already returned an error by this point). The run may go on, but the
        // switch must not: pointing the boot ROM at an image we know is wrong is
        // how a board stops booting at all.
        ctrl.log(format!(
            "warning: the u-boot image on boot LU {} does not match its digest, so \
             boot LU {} stays active — this installation must not be booted",
            ufs::boot_lu_name(plan.id),
            ufs::boot_lu_name(plan.active)
        ));
        return Ok(());
    }
    activate_boot_lu(cfg, ctrl, &device.path, plan.id)
}

/// Write the Falcon boot menu onto the loader partition, from its start.
///
/// Only UFS gets one: there the boot ROM reads the bootloader from a boot LU and
/// never looks at this partition, whereas on every other kind of device U-Boot
/// occupies it and has nowhere else to go.
///
/// A build that ships no boot menu leaves the partition empty rather than falling
/// back to a copy of U-Boot the boot ROM would never read. The board then boots
/// through full U-Boot as it always did, only without the graphical menu.
fn install_boot_menu(
    cfg: &Config,
    ctrl: &Controller,
    device: &StorageDevice,
    build: &UbootBuild,
    policy: OnMismatch,
    ticker: &mut Ticker,
) -> Result<()> {
    if device.kind != StorageKind::Ufs {
        return Ok(());
    }
    let Some(menu) = &build.boot_menu else {
        ctrl.log(format!(
            "warning: u-boot {} ships no boot menu image, so the loader partition is \
             left empty and the board boots through full u-boot",
            build.label
        ));
        return Ok(());
    };
    ticker.begin(ctrl, "installing boot menu");
    let loader = partition_path(&device.path, LOADER_PART_INDEX);
    wait_for_device(cfg, ctrl, &loader)?;
    write_source_to_offset(
        cfg,
        ctrl,
        "boot menu image",
        &menu.location,
        &menu.source,
        &loader,
        0,
        menu.sha256.as_deref(),
        policy,
    )?;
    Ok(())
}

/// Which boot LU of a UFS device the next bootloader goes to.
struct BootLuPlan {
    /// Block node of the logical unit to write.
    node: String,
    /// Its `bBootLunID` — which one of the pair it is.
    id: u8,
    /// The boot LU the boot ROM reads right now; `0` when booting is disabled.
    active: u8,
}

/// Pick the boot LU to write on a UFS target: the one the boot ROM is *not*
/// reading, so a failed or corrupted write cannot take the board down with it.
/// Only after the image is written and its digest checked does `bBootLunEn` move
/// over ([`activate_boot_lu`]).
///
/// Falls back to the LU that is active when the device has no second one — the
/// update is then not fail-safe, which is worth saying out loud. Returns `None`
/// when the device has no boot LU at all, which [`guard_ufs_boot_lu`] refuses.
fn ufs_boot_lu_plan(ctrl: &Controller, device: &StorageDevice) -> Option<BootLuPlan> {
    if device.kind != StorageKind::Ufs {
        return None;
    }
    let active = match read_boot_lun_en(&device.path) {
        Ok(active) => active,
        Err(e) => {
            // Without the BSG endpoint the flag can be neither read nor moved, so
            // fall back to the historical behaviour: write boot LU A in place and
            // switch nothing. Reporting it as the active one is what makes
            // `install_uboot` skip the switch.
            ctrl.log(format!(
                "warning: cannot read which UFS boot LU is active ({e}); writing boot \
                 LU A in place, without the fail-safe switch"
            ));
            let node = storage::find_ufs_boot_lu(&device.path, ufs::BOOT_LUN_A)?;
            return Some(BootLuPlan {
                node,
                id: ufs::BOOT_LUN_A,
                active: ufs::BOOT_LUN_A,
            });
        }
    };

    // The spare side first, then the active one.
    for id in [spare_boot_lu(active), active] {
        if id == ufs::BOOT_LUN_NONE {
            continue;
        }
        if let Some(node) = storage::find_ufs_boot_lu(&device.path, id) {
            if id == active {
                ctrl.log(format!(
                    "warning: {} has no spare boot LU, so u-boot is replaced in place \
                     and an interrupted write would leave the board unbootable",
                    device.path
                ));
            }
            return Some(BootLuPlan { node, id, active });
        }
    }
    None
}

/// The side of the boot pair to write, given which one the boot ROM reads. With
/// booting disabled there is nothing to preserve, so A is as good as B.
fn spare_boot_lu(active: u8) -> u8 {
    if active == ufs::BOOT_LUN_A {
        ufs::BOOT_LUN_B
    } else {
        ufs::BOOT_LUN_A
    }
}

/// Read `bBootLunEn`: which boot LU the boot ROM reads.
fn read_boot_lun_en(disk: &str) -> std::result::Result<u8, String> {
    let bsg = ufs::Bsg::open_for_disk(disk)?;
    Ok(bsg.read_attr(ufs::ATTR_BOOT_LUN_EN)? as u8)
}

/// Point `bBootLunEn` at the boot LU just written. The last step of a UFS
/// bootloader update, and the only one that changes what the board boots.
fn activate_boot_lu(cfg: &Config, ctrl: &Controller, disk: &str, id: u8) -> Result<()> {
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] would switch the active UFS boot LU to {}",
            ufs::boot_lu_name(id)
        ));
        return Ok(());
    }
    let bsg = ufs::Bsg::open_for_disk(disk)?;
    bsg.write_attr(ufs::ATTR_BOOT_LUN_EN, u32::from(id))?;
    // Read it back: this is the switch the board's next boot depends on.
    let now = bsg.read_attr(ufs::ATTR_BOOT_LUN_EN)? as u8;
    if now != id {
        return Err(format!(
            "the active UFS boot LU is still {} after asking for {}",
            ufs::boot_lu_name(now),
            ufs::boot_lu_name(id)
        ));
    }
    ctrl.log(format!("active UFS boot LU switched to {}", ufs::boot_lu_name(id)));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receive_pack(
    cfg: &Config,
    ctrl: &Controller,
    target: &str,
    pack: &PackFile,
    what: &str,
    policy: OnMismatch,
    on_progress: &mut dyn FnMut(u64),
) -> Result<()> {
    // The packs are zstd-compressed `btrfs send` streams. Decompress in-process
    // (libzstd via the `zstd` crate) and pipe the stream into `btrfs receive` at
    // `target`, which recreates the sent subvolume there. Stock packs go under
    // @stock-snapshots (incrementals find their Minimal parent in the same
    // directory); the /home seed is received at the Btrfs top level.
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] zstd -d {} | btrfs receive {target}",
            pack.location
        ));
        return Ok(());
    }
    // Count bytes and hash on the compressed source, before the decoder: the
    // reported progress then lines up with the known compressed pack size, and
    // the digest covers the pack exactly as the manifest describes it.
    let mut sha = fetch::Sha256::new();
    let reader = ProgressReader {
        inner: fetch::Digesting {
            inner: open_source(&pack.location, &pack.source)?,
            sha: &mut sha,
        },
        read: 0,
        on_progress,
    };
    let decoder = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| format!("zstd {}: {e}", pack.location))?;
    let mut recv = Command::new("btrfs");
    recv.arg("receive").arg(target);
    pump_reader_into(ctrl, decoder, recv, &pack.location)?;
    apply_verdict(ctrl, what, fetch::verify(sha, pack.sha256.as_deref()), policy)
}

fn make_writable_snapshot(
    cfg: &Config,
    ctrl: &Controller,
    mnt: &str,
    stock: &str,
    root: &str,
) -> Result<()> {
    // The received `*_stock` subvolume (under @stock-snapshots) is the RO golden
    // base; snapshot a writable deployable root from it at the top level
    // (matching the build recipe).
    exec(
        cfg,
        ctrl,
        Command::new("btrfs")
            .arg("subvolume")
            .arg("snapshot")
            .arg(format!("{mnt}/{STOCK_SNAPSHOTS_DIR}/{stock}"))
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

    let mnt = TARGET_MNT.to_string();
    exec(cfg, ctrl, Command::new("mkdir").arg("-p").arg(&mnt))?;
    // Mount the Btrfs top level (subvolid=5) so we can create the shared
    // subvolumes, the @stock-snapshots receive target, and the profile roots
    // relative to it.
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

    // The target is mounted from here on, but the caller only takes over the
    // unmount once we return `Ok` — so a failure while building the skeleton
    // has to clean up after itself, or it strands the mount and leaves the disk
    // claimed for the next attempt (which then fails in `blkdiscard`).
    if let Err(e) = create_skeleton(cfg, ctrl, &mnt, layout) {
        ctrl.log("filesystem setup failed — unmounting target".to_string());
        if let Err(u) = umount_recursive(cfg, ctrl, &mnt) {
            ctrl.log(format!("warning: could not unmount {mnt}: {u}"));
        }
        return Err(e);
    }
    Ok(mnt)
}

/// Create the shared, top-level subvolume skeleton from the layout config on
/// the freshly mounted target.
fn create_skeleton(cfg: &Config, ctrl: &Controller, mnt: &str, layout: &Layout) -> Result<()> {
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
    Ok(())
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
    let root = PROFILE_MNT.to_string();
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
        std::io::Cursor::new(INSTALL_KERNEL_SH.as_bytes()),
        sh,
        "kernel-install script",
    );

    // Always unmount the profile root (recursively, in case the script left an
    // API mount behind), surfacing the script error in preference to any
    // unmount error.
    let unmounted = umount_recursive(cfg, ctrl, &root);
    ran?;
    unmounted?;
    Ok(())
}

fn unmount(cfg: &Config, ctrl: &Controller, mnt: &str) -> Result<()> {
    // Flush first, but never let a failing `sync` skip the unmount itself — a
    // stranded mount keeps the target claimed and breaks the next attempt.
    exec(cfg, ctrl, &mut Command::new("sync")).ok();
    umount_recursive(cfg, ctrl, mnt)
}

/// Tear down any mountpoint of ours left over from an earlier attempt, so a
/// retry starts from a clean slate.
///
/// Only *our* mountpoints are touched: anything else sitting on the target is a
/// foreign user that must make [`guard_target`] refuse, not something to
/// silently unmount out from under whoever owns it.
fn release_own_mounts(cfg: &Config, ctrl: &Controller) {
    // The profile root first: it is a subvolume of the same filesystem as the
    // top-level mount, and dropping it first lets the top level go quietly.
    for mnt in [PROFILE_MNT, TARGET_MNT] {
        if mountpoints_under(mnt).is_empty() {
            continue;
        }
        ctrl.log(format!("clearing stale mount at {mnt} from an earlier attempt"));
        if let Err(e) = umount_recursive(cfg, ctrl, mnt) {
            ctrl.log(format!("warning: could not clear {mnt}: {e}"));
        }
    }
}

/// Wait until the whole-disk `disk` can be claimed exclusively, i.e. nothing
/// holds it or any of its partitions any more.
///
/// Unmounting is not synchronous with the kernel releasing the block device.
/// Our own busy-mount fallback (`umount -l`) detaches the mountpoint from the
/// tree — so it disappears from `/proc/mounts`, and [`guard_target`] sees a
/// clean disk — while the filesystem, and with it the exclusive claim on the
/// whole disk, lives on until the last reference goes away. The next step is
/// `blkdiscard`, whose BLKDISCARD ioctl claims the disk itself (`-f` only skips
/// util-linux's own `O_EXCL` open, not the kernel's claim), so it fails with
/// EBUSY. Waiting here turns that into a short pause instead of a failed run.
///
/// Best-effort: if the disk is still claimed when the deadline passes we log
/// why and carry on, letting the real tool report the real error.
fn wait_for_exclusive_access(cfg: &Config, ctrl: &Controller, disk: &str) {
    if cfg.dry_run {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut waited = false;
    loop {
        let err = match claim_exclusively(disk) {
            // Dropping the handle releases the claim again; nothing else is
            // competing for the target at this point.
            Ok(_) => {
                if waited {
                    ctrl.log(format!("{disk} released"));
                }
                return;
            }
            Err(e) => e,
        };
        if !waited {
            waited = true;
            ctrl.log(format!(
                "{disk} is still claimed ({err}) — waiting for it to be released…"
            ));
        }
        if std::time::Instant::now() >= deadline {
            ctrl.log(format!(
                "warning: {disk} is still claimed after 10 s ({err}); a mount from an \
                 earlier attempt may not have been fully released yet"
            ));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// `O_EXCL` on a block device is a claim on the whole disk (and fails if any of
/// its partitions is claimed), which is exactly the check `blkdiscard` and the
/// BLKDISCARD ioctl perform. Linux keeps this flag value on every architecture.
const O_EXCL: i32 = 0o200;

/// Try to open `disk` with an exclusive claim, the same way the wipe will.
fn claim_exclusively(disk: &str) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().read(true).custom_flags(O_EXCL).open(disk)
}

/// Recursively unmount `target` and everything mounted beneath it, deepest
/// first, falling back to a lazy detach when a plain unmount is busy.
///
/// `umount -R` is a util-linux extension that BusyBox's `umount` does not
/// implement, and the installer runs in a BusyBox initramfs — so we enumerate
/// the mount table ourselves instead of relying on the flag.
fn umount_recursive(cfg: &Config, ctrl: &Controller, target: &str) -> Result<()> {
    if cfg.dry_run {
        ctrl.log(format!("[dry-run] umount -R {target}"));
        return Ok(());
    }
    let mut failed: Option<String> = None;
    for mp in mountpoints_under(target) {
        // Plain unmount first; if the mount is busy, detach lazily so we never
        // leave the target mounted. Only a lazy detach that also fails is an error.
        if exec(cfg, ctrl, Command::new("umount").arg(&mp)).is_err() {
            if let Err(e) = exec(cfg, ctrl, Command::new("umount").arg("-l").arg(&mp)) {
                failed.get_or_insert(e);
            }
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Mountpoints at or under `dir`, deepest first (children before parents), read
/// from `/proc/mounts`. The trailing-slash prefix test avoids matching a sibling
/// whose name merely extends `dir` (e.g. `/run/x` must not swallow `/run/x-root`).
fn mountpoints_under(dir: &str) -> Vec<String> {
    let content = match std::fs::read_to_string("/proc/mounts") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    mountpoints_in(&content, dir)
}

/// [`mountpoints_under`] against an already-read mount table.
fn mountpoints_in(mounts: &str, dir: &str) -> Vec<String> {
    let prefix = format!("{dir}/");
    let mut mps: Vec<String> = mounts
        .lines()
        // `/proc/mounts` columns: device mountpoint fstype options dump pass.
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(unescape_mount)
        .filter(|mp| mp == dir || mp.starts_with(&prefix))
        .collect();
    mps.sort();
    mps.dedup();
    // Deepest first: a child mountpoint is always a longer string than its parent.
    mps.sort_by_key(|mp| std::cmp::Reverse(mp.len()));
    mps
}

/// Decode the octal escapes the kernel writes into `/proc/mounts` mountpoints.
fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
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
/// local file for removable media and unpacked bundles. See
/// [`crate::core::fetch`] for the timeout and resume behaviour.
fn open_source(location: &str, source: &Source) -> Result<Box<dyn Read + Send>> {
    fetch::open(location, source)
}

/// Stream a source (HTTP URL for [`Source::Server`], local path otherwise)
/// directly onto `device`, starting at `offset` bytes, then flush to disk. A
/// short write is an error, so the digest always covers the whole image.
///
/// Returns whether the bytes now on the device are trustworthy: true when the
/// digest matched, or when the manifest published none to compare against, and
/// false only for an outright mismatch (which is an error unless the fetch policy
/// downgrades it to a warning). Callers that make a written image *live* — the
/// UFS boot-LU switch — must not do so when this is false.
#[allow(clippy::too_many_arguments)]
fn write_source_to_offset(
    cfg: &Config,
    ctrl: &Controller,
    what: &str,
    location: &str,
    source: &Source,
    device: &str,
    offset: u64,
    expect: Option<&str>,
    policy: OnMismatch,
) -> Result<bool> {
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] write {location} -> {device} @ offset {offset} B"
        ));
        return Ok(true);
    }
    ctrl.log(format!("writing {location} -> {device} @ offset {offset} B"));

    let mut sha = fetch::Sha256::new();
    let mut reader = fetch::Digesting {
        inner: open_source(location, source)?,
        sha: &mut sha,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .open(device)
        .map_err(|e| format!("open {device}: {e}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {device} to {offset}: {e}"))?;
    let written = io::copy(&mut reader, &mut file)
        .map_err(|e| format!("write {location} to {device}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync {device}: {e}"))?;
    ctrl.log(format!("wrote {written} bytes to {device}"));
    drop(reader);
    let verdict = fetch::verify(sha, expect);
    let trustworthy = !matches!(verdict, fetch::Verdict::Mismatch(_, _));
    apply_verdict(ctrl, what, verdict, policy)?;
    Ok(trustworthy)
}

/// Spawn `cmd` and pump an arbitrary reader into its stdin, then wait.
fn pump_reader_into(
    ctrl: &Controller,
    mut reader: impl Read,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::BootMenu;
    use std::io::Cursor;

    #[test]
    fn the_bootloader_goes_to_the_other_boot_lu() {
        // The whole point of the pair: never write the LU the board is booting
        // from, so an interrupted update cannot take it down.
        assert_eq!(spare_boot_lu(ufs::BOOT_LUN_A), ufs::BOOT_LUN_B);
        assert_eq!(spare_boot_lu(ufs::BOOT_LUN_B), ufs::BOOT_LUN_A);
        // Nothing is live yet on a device with booting disabled, so either side
        // will do; A keeps a freshly provisioned device predictable.
        assert_eq!(spare_boot_lu(ufs::BOOT_LUN_NONE), ufs::BOOT_LUN_A);
    }

    /// A U-Boot build carrying a boot menu image of `size` bytes.
    fn build_with_boot_menu(size: u64) -> UbootBuild {
        UbootBuild {
            id: "u".into(),
            label: "u".into(),
            mtime: String::new(),
            image_location: "https://example.invalid/u-boot-rockchip.bin".into(),
            manifest_location: "https://example.invalid/manifest.json".into(),
            source: Source::Server,
            size_bytes: 9_961_984,
            sha256: None,
            details: None,
            boot_menu: Some(BootMenu {
                location: "https://example.invalid/bootmenu-falcon.itb".into(),
                source: Source::Server,
                size_bytes: size,
                sha256: None,
            }),
            loaded: true,
        }
    }

    #[test]
    fn a_boot_menu_too_big_for_the_loader_partition_is_refused() {
        let ctrl = Controller::new(Config {
            automount: false,
            ..Config::default()
        });
        // `Config::default()` is a dry run; a real run is what refuses.
        let cfg = Config {
            dry_run: false,
            automount: false,
            ..Config::default()
        };

        // The largest image that still fits, and the first one that does not.
        guard_boot_menu_fits(&cfg, &ctrl, &build_with_boot_menu(LOADER_CAPACITY)).unwrap();
        let err = guard_boot_menu_fits(&cfg, &ctrl, &build_with_boot_menu(LOADER_CAPACITY + 1))
            .unwrap_err();
        assert!(err.contains("too small for a"), "{err}");

        // A dry run writes nothing, so it only says a real run would refuse.
        let dry = Config {
            automount: false,
            ..Config::default()
        };
        assert!(dry.dry_run);
        guard_boot_menu_fits(&dry, &ctrl, &build_with_boot_menu(LOADER_CAPACITY + 1)).unwrap();

        // A manifest that publishes no size leaves nothing to compare against.
        guard_boot_menu_fits(&cfg, &ctrl, &build_with_boot_menu(0)).unwrap();
    }

    #[test]
    fn progress_reader_reports_cumulative_bytes() {
        let mut seen: Vec<u64> = Vec::new();
        let mut cb = |n: u64| seen.push(n);
        let mut r = ProgressReader {
            inner: Cursor::new(vec![0u8; 10]),
            read: 0,
            on_progress: &mut cb,
        };
        let mut buf = [0u8; 4];
        let mut total = 0usize;
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(total, 10);
        // The callback sees a strictly increasing running total that ends at the
        // full length (EOF's zero-length read reports nothing).
        assert_eq!(seen.last().copied(), Some(10));
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn ticker_step_spans_tile_the_bar() {
        let mut t = Ticker::new(4);
        t.step = 1;
        let (base, span) = t.step_span();
        assert!(base.abs() < 1e-6);
        assert!((span - 0.25).abs() < 1e-6);
        // The last step's slice ends exactly at a full bar.
        t.step = 4;
        let (base, span) = t.step_span();
        assert!((base - 0.75).abs() < 1e-6);
        assert!((base + span - 1.0).abs() < 1e-6);
    }

    /// A mount table as it looks mid-install: the target's top level, the
    /// profile root mounted alongside it, and the chroot's API mounts under
    /// that root.
    const MOUNTS: &str = "\
proc /proc proc rw 0 0
/dev/mmcblk0p3 /run/flipperos-install btrfs rw,subvolid=5 0 0
/dev/mmcblk0p3 /run/flipperos-install-root btrfs rw,subvol=/@minimal 0 0
devtmpfs /run/flipperos-install-root/dev devtmpfs rw 0 0
devpts /run/flipperos-install-root/dev/pts devpts rw 0 0
/dev/mmcblk0p3 /run/flipperos-install-root/boot btrfs rw,subvol=/@boot 0 0
/dev/sda1 /media/usb\\0401 vfat ro 0 0
";

    #[test]
    fn mountpoints_are_listed_deepest_first() {
        let mps = mountpoints_in(MOUNTS, PROFILE_MNT);
        // Children before their parent, so each unmount sees an idle mountpoint.
        assert_eq!(
            mps,
            vec![
                "/run/flipperos-install-root/dev/pts",
                "/run/flipperos-install-root/boot",
                "/run/flipperos-install-root/dev",
                "/run/flipperos-install-root",
            ]
        );
    }

    #[test]
    fn target_mountpoint_does_not_swallow_the_profile_root() {
        // PROFILE_MNT merely *extends* TARGET_MNT's name; unmounting the target
        // must not pull in the profile root (or its chroot mounts) as well.
        assert_eq!(mountpoints_in(MOUNTS, TARGET_MNT), vec![TARGET_MNT]);
    }

    #[test]
    fn mountpoint_escapes_are_decoded() {
        assert_eq!(mountpoints_in(MOUNTS, "/media"), vec!["/media/usb 1"]);
    }
}
