# flipperos-installer

A small installer for Rockchip **RK3576** boards (the **Flipper One** and other
RK3576 boards used for testing), designed to run from a Linux **initramfs** for
on-device installs.

It can:

- **Discover the running board** from the device tree (`/proc/device-tree`).
- **Enumerate local storage** the RK3576 boot ROM can boot from (UFS, eMMC, SD)
  via sysfs.
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
`u-boot/<board>/u-boot-rockchip.bin` for every supported board, the
`profile-packs/` (the same `<Profile>_<build>_stock[_inc]_pack.zst` and
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
  image, the Minimal full pack, each selected incremental and the `/home` seed —
  into `--cache-dir`, compares each against its manifest digest, and only then
  starts partitioning. A bad or truncated artifact therefore cannot leave a wiped
  device behind. The space needed (~0.8–1.3 GiB) is checked up front.
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
  `manifest.json` contains `<board>/u-boot-rockchip.bin`. The installer flashes
  `<board>/u-boot-rockchip.bin` for the detected board (`flipper-one`, else
  `generic`).
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
dry-run install pipeline are in place. GPT partitioning and the U-Boot write are
done in-process (`gpt` crate); the remaining destructive steps shell out to
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
