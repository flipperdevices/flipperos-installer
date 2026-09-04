# flipperos-installer

A small installer for Rockchip **RK3576** boards (the **Flipper One** and other
RK3576 boards used for testing), designed to run from a Linux **initramfs** for
on-device installs.

It can:

- **Discover the running board** from the device tree (`/proc/device-tree`).
- **Enumerate local storage** the RK3576 boot ROM can boot from (UFS, eMMC, SD)
  via sysfs.
- **Check a UFS target's logical units** against the Flipper provisioning scheme
  and offer to **reprovision** it — writing the Configuration Descriptor over the
  kernel's UFS BSG endpoint.
- **Browse update bundles** published per channel (`release`, `testing`,
  `nightly`, and per-developer `dev/<user>/<branch>` builds), and verify each
  artifact against the SHA-256 digests in the bundle's manifest.
- **Query the image server** for available U-Boot images and exported profile
  snapshots for the board (the *custom development build* flow).
- **Mount removable storage** (SD / USB) read-only and search it for update
  bundles, offline U-Boot images and profile snapshots.
- Present a **TUI** (Cursive + Crossterm) for serial-console operation.
- Present a **GUI** (Slint + LinuxKMS) on the Flipper One 256×144 DRM screen,
  driven by the on-device buttons (a Linux input event device).
- **Run the installation**: `blkdiscard`, write a fresh GPT, `mkfs.btrfs` with a
  subvolume skeleton, `btrfs receive` the selected profile snapshots, and
  install a kernel for each.

## Architecture

Both frontends are thin views over a single shared installer state:

```
                      ┌──────────────────────────────┐
   serial console ───>│ TUI (cursive / crossterm)    │
                      └───────┬──────────────┬───────┘
                              │              ^
                              │ actions      │ snapshots
                              v              │
                      ┌───────┴──────────────┴───────┐
                      │ Controller                   │
                      │ owns AppState — the single   │
                      │ source of truth              │
                      └───────┬──────────────┬───────┘
                              ^              │
                              │ actions      │ snapshots
                              │              v
   on-device          ┌───────┴──────────────┴───────┐
   buttons ──────────>│ GUI (slint / linuxkms)       │
                      └──────────────────────────────┘
```

Both frontends render the *same* menu tree, built once in
[`core::menu`](src/core/menu.rs); only the current position in that tree is
per-frontend, so the two can sit on different screens while sharing every
selection.

- [`core::model`](src/core/model.rs) — plain data model (`AppState`, devices,
  images, snapshots, bundles).
- [`core::controller`](src/core/controller.rs) — owns `AppState`, applies all
  mutations, and broadcasts immutable snapshots to every subscribed frontend so
  the TUI and GUI mirror each other live.
- [`core::menu`](src/core/menu.rs) — the menu tree: what each screen contains,
  built once and rendered by both frontends. Only the *position* in the tree is
  per-frontend.
- [`core::board`](src/core/board.rs) / [`core::storage`](src/core/storage.rs) /
  [`core::removable`](src/core/removable.rs) — discovery.
- [`core::ufs`](src/core/ufs.rs) — UFS descriptors and attributes over the SCSI
  BSG endpoint (transport and wire format only);
  [`core::provision`](src/core/provision.rs) — the logical-unit scheme, the
  comparison against it, and the destructive rewrite.
- [`core::bundle`](src/core/bundle.rs) / [`core::archive`](src/core/archive.rs) /
  [`core::catalog`](src/core/catalog.rs) — the two kinds of image source.
- [`core::fetch`](src/core/fetch.rs) — every byte the installer reads, plus the
  SHA-256 primitives; [`core::stage`](src/core/stage.rs) — verifying artifacts
  before the target is touched.
- [`core::install`](src/core/install.rs) — the destructive install pipeline
  (honours `--dry-run`).
- [`tui`](src/tui/mod.rs) / [`gui`](src/gui/mod.rs) — the two frontends.

## Building

Built and tested with **rustc 1.95.0** (via `rustup`). The GUI depends on
**Slint 1.17**, which sets a minimum of **rustc 1.92**; the TUI-only build has a
lower floor (its highest-MSRV dependency is `uuid`, at rustc 1.85).

This repository uses a git submodule for the on-device font
([flipctl-fonts](https://github.com/flipperdevices/flipctl-fonts), tracking the
`dev` branch). Clone recursively, or initialise it after cloning:

```sh
git clone --recurse-submodules <repo-url>
# or, in an existing checkout:
git submodule update --init --recursive
```

```sh
# Default: both frontends, host build (for development).
cargo build

# Release build for the device (RK3576 is ARMv8):
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu

# Slim, TUI-only variant (no GUI shared-library dependencies):
cargo build --release --no-default-features --features tui \
    --target aarch64-unknown-linux-gnu
```

The binary links dynamically against the system C library and, for the GUI,
against `libinput`, `libudev`, `libxkbcommon`, `libfontconfig` and `libfreetype`
(the last two pulled in by Slint for font discovery/rendering), all located via
`pkg-config`. These shared libraries must therefore be present in the initramfs
alongside the binary (the TUI-only build needs none of them). The `drm` crate
talks to the kernel directly via ioctls (pregenerated bindings), so no `libdrm`
is needed. Install the build tools and dev packages (Debian/Ubuntu):

```sh
sudo apt install pkg-config libinput-dev libudev-dev libxkbcommon-dev \
    libfontconfig-dev libfreetype-dev
```

The GUI's text is rendered with the compiled-in **HaxrCorp 4090 (FlipCTL)** pixel
font (vendored as the `third_party/flipctl-fonts` submodule and embedded by the
Slint compiler), so no system fonts are required on the device.

Cargo feature flags:

| Feature | Frontend                          |
|---------|-----------------------------------|
| `tui`   | Cursive/Crossterm (serial)        |
| `gui`   | Slint/LinuxKMS (on-device screen) |

## Running

```sh
# Both frontends concurrently (default), safe dry-run:
sudo ./flipperos-installer

# Serial console only:
sudo ./flipperos-installer --tui

# On-device screen only, real install:
sudo ./flipperos-installer --gui --no-dry-run \
    --kms-device /dev/dri/by-path/platform-2acf0000.spi-cs-0-card

# Install from a bundle sitting on a USB stick, streaming rather than staging:
sudo ./flipperos-installer --no-dry-run --fetch stream \
    --bundle /mnt/usb/flipperone-update-20260812-83ddb68-nightly-15.tar.zst

# Hand-picked U-Boot + rootfs pair from the image server:
sudo ./flipperos-installer --custom --server https://images.flipperos.example
```

By default the installer runs in **dry-run** mode: every destructive command is
logged but not executed. Pass `--no-dry-run` to actually flash. `--help` lists
every flag; the bundle-specific ones are described under
[Update bundles](#update-bundles).

Once a real install has finished, both frontends offer to reboot: `<ReBoot>`
(Alt+B) in the serial console's button bar, and the RUN soft button on the
device, where it replaces Install. Use it instead of power-cycling — it closes
both frontends first, which is what leaves the serial console usable afterwards
(a terminal still in raw mode / the alternate screen survives a power cut). The
action is absent in a dry run, since nothing was written to boot into.

Two examples double as development probes, and both run without a screen or a
serial console:

```sh
# Browse the real bucket and resolve the newest bundle.
cargo run --example bundle_probe --no-default-features

# Drive a whole installation headlessly, in dry-run mode.
cargo run --example dry_run --no-default-features
```

## Update bundles

An **update bundle** is one artifact set that pins a U-Boot build and a rootfs
build together, with a SHA-256 digest for every file. Installing from a bundle is
the default. Bundles are published per channel:

```
bundles/<channel>/<build>/               channel = release | testing | nightly
bundles/dev/<user>/<branch>/<build>/     per-developer topic branches
```

and each build directory holds a `manifest.json`, the flashable
`u-boot/<board>/u-boot-rockchip.bin` for every supported board, the matching
`boot-menu/<board>/bootmenu-falcon.itb` (see [The boot menu](#the-boot-menu)),
the `profile-packs/` (the same `<Profile>_<build>_stock[_inc]_pack.zst` and
`home_<build>_pack.zst` files the image server publishes), MCU firmware the
installer ignores, and a `*.tar.zst` of the whole tree.

Directory listing is not available on the public object host, so the installer
**lists** through the bucket's object API (`--bundle-bucket`, or
`--bundle-list-url` to override the endpoint outright) and **downloads** from the
public base URL (`--bundle-url`). Builds are ordered by directory name,
descending; the build number shown in the UI comes from the manifest once a
bundle is selected, so nothing depends on the shape of the directory name.

A bundle can equally come from a local directory holding a `manifest.json`, or
from a `*.tar.zst` — passed with `--bundle` or found on removable media, which
the installer mounts read-only during discovery (`--no-automount` opts out).
A local archive is unpacked into the scratch directory before installing; a
remote install never reads the archive, only the individual files its manifest
lists and the operator's profile selection call for.

### Verifying

The **Fetch** row chooses when artifacts are checked:

- `verify first` (the default) downloads exactly what the run needs — the U-Boot
  image, the boot menu on a UFS target, the Minimal full pack, each selected
  incremental and the `/home` seed —
  into `--cache-dir`, compares each against its manifest digest, and only then
  starts partitioning. A bad or truncated artifact therefore cannot leave a wiped
  device behind. The space needed (~0.9–1.4 GiB, the higher end of it on UFS,
  which also stages the boot menu) is checked up front.
- `stream` writes as it downloads, hashing on the way through. The verdict
  necessarily arrives after the bytes have landed, so a mismatch is reported as a
  warning that says the target must not be booted.

Either way, an artifact whose manifest publishes no digest is installed with a
warning that verification was skipped. Both modes also work for the *custom
development build* flow, whose manifests now carry digests too.

## Image sources

The *custom development build* flow reads the image server's two-level catalog
(default base `https://dl-linux-images.flipp.dev`, override with `--server`):

- **U-Boot:** `/u-boot/manifest.json` lists build directories; each build's
  `manifest.json` contains `<board>/u-boot-rockchip.bin` and, beside it,
  `<board>/bootmenu-falcon.itb`. The installer flashes
  `<board>/u-boot-rockchip.bin` for the detected board (`flipper-one`, else
  `generic`), and takes the boot menu from the same directory.
- **Snapshots (rootfs):** `/rootfs/manifest.json` lists build directories; each
  build's `manifest.json` contains per-profile packs
  `<Profile>_<build>_stock_pack.zst` (full) and `<Profile>_<build>_stock_inc_pack.zst`
  (incremental delta vs. Minimal).

Both lists are presented **newest first**, and the operator picks one U-Boot build
and one snapshot build — any combination, which is what makes this the *custom*
flow rather than the default one.

Either way — bundle or custom pair — **Minimal is always deployed** (from its full
pack); any extra profiles the operator selects are received from their
**incremental** packs on top of Minimal. Packs are zstd-compressed `btrfs send`
streams, decompressed in-process and piped into `btrfs receive`.

A removable-media mirror of the same `u-boot/` and `rootfs/` tree is picked up
automatically and merged into the lists.

## UFS provisioning

A UFS device is divided into **logical units** by its manufacturer, and the layout
we need differs from the one they ship. When a UFS device is selected as the
install target, the installer reads its Configuration Descriptor and compares it
against the scheme in [config/flipperos-ufs.toml](config/flipperos-ufs.toml):

| LU | Role | Memory type | Size |
|----|------|-------------|------|
| 0  | main system — GPT, loader partition (the boot menu), Btrfs | Normal | all remaining |
| 1  | U-Boot, flagged **Boot LU A** | Enhanced1 | 16 MiB |
| 2  | U-Boot, flagged **Boot LU B** | Enhanced1 | 16 MiB |
| 3  | recovery — kernel + initrd for on-device rescue | Enhanced1 | 128 MiB |

If the layout differs in a way that matters — which units exist, their size,
memory type or boot flag, whether the boot feature is on, or whether the boot ROM
is pointed at a boot LU at all — both frontends raise a confirmation prompt.
Reprovisioning is
**destructive**: the device rebuilds its whole mapping and everything on it is
lost. Differences that do not change what the device *is* (the WriteBooster size,
data reliability, provisioning type) are logged and tolerated. The full report is
on the target-device level's details popup.

A **factory-blank** device is the case that has to be handled without a target at
all. It has no logical units, so it presents no block device: nothing in
`/sys/block`, nothing in the target list, and so — if provisioning were keyed on a
disk — nothing to select in order to provision it. Provisioning is therefore keyed
on the UFS *host controller*, found from the SCSI host (`proc_name` is `ufshcd`)
with its `/dev/bsg/ufs-bsg<host>` endpoint, which exists from `ufshcd_init` onwards
regardless of logical units. Such a device still identifies itself — the SCSI
vendor and model of its well-known UFS DEVICE logical unit — so it appears as
`Provision UFS… <vendor> <model>` and is offered as soon as discovery finds it.
Offering it unprompted is safe precisely because it is blank, and the prompt says
so rather than claiming data loss:

```
Provision UFS?
  BIWIN BWU2A0526B128G has no logical units yet. Provisioning creates them:

  Logical units, now and wanted:
  LU 0: absent → 118.7 GiB
  LU 1: absent → 16.0 MiB boot A
  LU 2: absent → 16.0 MiB boot B
  LU 3: absent → 128.0 MiB
```

Once it is provisioned, LU 0 shows up as an ordinary `/dev/sd*`, is selected
automatically, and the install proceeds as it would on any other target.

Provisioning goes through `/dev/bsg/ufs-bsg<host>` (needs `CONFIG_SCSI_UFS_BSG`) and
takes four steps: write the Configuration Descriptor, set `bBootLunEn`, set
**`fDeviceInit`** so the device rebuilds its logical units, and rescan the SCSI host
so the kernel picks up their new capacities. `fDeviceInit` is the same step
Rockchip's downstream USB-plug loader performs after provisioning
(`ufshcd_complete_dev_init` inside `_ufs_start`, see
`drivers/ufs/ufs-rockchip-usbplug.c` in `rockchip-linux/u-boot`), which is why no
power cycle is needed even though a device applies a new configuration only when it
initialises. The installer then re-reads the descriptor and refuses to call the job
done unless it reads back as the scheme.

Every descriptor field we do not vary is set to the value that loader writes, so a
device provisioned by either agrees with the other.

Two details are worth knowing before touching this code. The UPIU header's
`data_segment_length` **must** be set by us for a WRITE DESCRIPTOR: the kernel fills
it in on its own query path but not on the raw BSG one, and a device that receives a
zero-length data segment answers a perfectly good descriptor with `INVALID VALUE`,
having never seen it (`ufs-utils` sets it in `prepare_upiu` for the same reason). And
the rescan must delete only *data* logical units: the well-known LUNs live under the
same SCSI target, the driver holds pointers to them, and deleting
`hba->ufs_device_wlun` earned a NULL dereference in `rpm_drop_usage_count` from
`ufshcd_err_handler`.

### The A/B bootloader pair

On UFS the boot LU is the **only** place the bootloader goes. The boot ROM reads
DRAM init and the SPL from whichever LU `bBootLunEn` selects and never looks at
the main LU's loader partition, so a copy there would be dead weight — the
installer writes none, and puts [the boot menu](#the-boot-menu) in that partition
instead. Every other kind of target keeps its bootloader on the loader partition,
which is where its boot ROM reads it from.

Because each boot LU holds 16 MiB, a whole `u-boot-rockchip.bin` fits in one, so a
UFS bootloader update is fail-safe:

1. read `bBootLunEn` to see which boot LU the boot ROM currently reads;
2. write the image to the **other** one, in full;
3. verify it against the manifest digest;
4. only then point `bBootLunEn` at it, and read the attribute back.

An interrupted or corrupted update therefore leaves the board booting exactly what
it booted before. A digest mismatch leaves the old boot LU active — the install
fails outright under `verify first`, and warns under `stream`, where the bytes have
already landed. A device with only one boot LU is written in place, with a warning
that says so. An install onto a device whose boot LU is too small for the image is
refused **before** anything is erased.

Which side is live is consequently not fixed, so the provisioning check accepts
either — otherwise every second install would offer to wipe the device.

LU 1–3 are meant to end up RPMB write-protected; that is separate work, so
`bLUWriteProtect` is left clear for now (setting it would make them read-only
after a power-on reset, which would break the installer's own U-Boot write).

`--no-ufs-check` skips the check entirely, `--reprovision-ufs` reprovisions a
mismatched target without asking (for unattended and factory runs), and
`--ufs-scheme <PATH>` tries a different scheme without a rebuild.

## The boot menu

`bootmenu-falcon.itb` is a FIT image holding a Falcon-mode Linux kernel and an
initramfs that draws the graphical boot menu. It is written raw to the **start of
the loader partition** (`/dev/sda1`), and **only when the target is UFS** — that
is the one case where the bootloader lives elsewhere and leaves the partition
free. Nothing changes for eMMC, SD or USB targets, whose loader partition is
occupied by U-Boot and has nowhere else to put it.

Where it comes from:

- bundle: `boot-menu/<board>/bootmenu-falcon.itb`, for the same board directory
  the bootloader is taken from;
- custom development build: `<board>/bootmenu-falcon.itb`, published beside
  `<board>/u-boot-rockchip.bin` in the U-Boot build directory.

It is verified against its manifest digest exactly like the bootloader, and it is
staged with the other artifacts under `verify first`. A build that ships no boot
menu is still installable: the loader partition is left empty, with a warning, and
the board boots through full U-Boot as it did before.

The loader partition runs from 32 KiB to 60 MiB, so it holds 58.6 MiB. The image
is ~39 MiB today and grows with the menu, so an install that would not fit is
refused **before** anything is erased, rather than failing with the target already
wiped. That check is what will eventually ask for a larger partition.

## Btrfs layout

The shared, top-level Btrfs subvolume skeleton (`boot`, `@home`, `@var-log`,
`@var-cache`, `@snapshots`, with `boot` kept uncompressed and the journal dir
NODATACOW) is described by a small TOML file that mirrors the build recipe. See
[config/flipperos-btrfs.toml](config/flipperos-btrfs.toml).

At install time the installer prefers a `btrfs-layout.toml` shipped with the
images and falls back to the copy compiled into the binary. Per-profile roots
(`@Minimal`, `@Desktop`, …) are not listed there — they are received from the
selected snapshot packs.

## Status

This is an early scaffold: the architecture, discovery, both frontends and the
dry-run install pipeline are in place. GPT partitioning, the U-Boot and boot menu
writes and UFS provisioning are done in-process (the `gpt` crate, and `SG_IO`
ioctls for UFS);
the remaining destructive steps shell out to
`blkdiscard`, `mkfs.btrfs`, `btrfs` and `chattr`. Kernel installation is driven
by an embedded POSIX shell script
([scripts/flipperos-install-kernel.sh](scripts/flipperos-install-kernel.sh))
that `chroot`s into each deployed profile and runs the profile's own
`kernel-install` for every installed kernel, writing into the shared `/boot`
subvolume. These tools (`mount`/`umount`, `chroot`, `sh`, and the profile's
`kernel-install`) must be present in the initramfs / profile.

## Licensing

The installer's own source in this repository is **MIT-licensed** (see
[`LICENSES/MIT.txt`](LICENSES/MIT.txt); the repository follows the
[REUSE](https://reuse.software) specification, so `reuse lint` is green).

However, the shipped binary links the [Slint](https://slint.dev) GUI toolkit,
which we use under the **GNU GPL v3.0** option of its tri-license (wehold no
separate Slint agreement, and want the binary to be buildable purely from
public sources). Linking GPL code makes the **combined binary as a whole
GPL-3.0-only**. MIT is GPL-compatible, so our own sources are unaffected and
may still be reused under MIT on their own.

Full license texts for every crate linked into the binary are collected in
[`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md). Regenerate it from the
current `Cargo.lock` whenever dependencies change:

```sh
cargo install cargo-about --features cli   # once
sh scripts/gen-third-party-licenses.sh
```

The bundled fonts live in the `third_party/flipctl-fonts` submodule and carry
their own licenses (HaxrCorp 4090 — CC BY-SA 3.0; Born2bSportyV2 — The Unlicense;
Busy9px — MIT); see the `LICENSE` file in each font's folder.
