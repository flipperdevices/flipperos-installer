# flipperos-installer

A tiny, statically-linked installer for Rockchip **RK3576** boards (the
**Flipper One** and other RK3576 boards used for testing), designed to run from
a Linux **initramfs** for on-device installs.

It can:

- **Discover the running board** from the device tree (`/proc/device-tree`).
- **Enumerate local storage** the RK3576 boot ROM can boot from (UFS, eMMC, SD)
  via sysfs.
- **Query the image server** for available U-Boot images and exported profile
  snapshots for the board.
- **Search removable storage** (SD / USB) for offline U-Boot images and profile
  snapshots.
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
   serial  ─────▶│  TUI (cursive/crossterm)     │─┐
   console       └──────────────────────────────┘ │  actions
                                                   ▼
                 ┌──────────────────────────────┐  Controller ── AppState
   on-device ───▶│  GUI (slint/linuxkms)        │─┐  (single source of truth)
   buttons       └──────────────────────────────┘ │  snapshots ▲
                                                   └────────────┘
```

- [`core::model`](src/core/model.rs) — plain data model (`AppState`, devices,
  images, snapshots).
- [`core::controller`](src/core/controller.rs) — owns `AppState`, applies all
  mutations, and broadcasts immutable snapshots to every subscribed frontend so
  the TUI and GUI mirror each other live.
- [`core::board`](src/core/board.rs) / [`core::storage`](src/core/storage.rs) /
  [`core::removable`](src/core/removable.rs) / [`core::server`](src/core/server.rs)
  — discovery.
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
# Default: both frontends, dynamic host build (for development).
cargo build

# Slim, TUI-only static build for the device:
rustup target add aarch64-unknown-linux-musl
cargo build --release --no-default-features --features tui \
    --target aarch64-unknown-linux-musl
```

The GUI links against `libinput`, `libudev`, `libxkbcommon`, `libfontconfig` and
`libfreetype` (the last two pulled in by Slint for font discovery/rendering),
all located via `pkg-config`. The `drm` crate talks to the kernel directly via
ioctls (pregenerated bindings), so no `libdrm` is needed. Install the build
tools and dev packages (Debian/Ubuntu):

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

# On-device screen only, real install, custom server:
sudo ./flipperos-installer --gui --no-dry-run \
    --server https://images.flipperos.example \
    --kms-device /dev/dri/by-path/platform-2acf0000.spi-cs-0-card
```

By default the installer runs in **dry-run** mode: every destructive command is
logged but not executed. Pass `--no-dry-run` to actually flash.

## Image sources

The installer reads the image server's two-level catalog (default base
`https://dl-linux-images.flipp.dev`, override with `--server`):

- **U-Boot:** `/u-boot/manifest.json` lists build directories; each build's
  `manifest.json` contains `<board>/u-boot-rockchip.bin`. The installer flashes
  `<board>/u-boot-rockchip.bin` for the detected board (`flipper-one`, else
  `generic`).
- **Snapshots (rootfs):** `/rootfs/manifest.json` lists build directories; each
  build's `manifest.json` contains per-profile packs
  `<Profile>_<build>_stock_pack.zst` (full) and `<Profile>_<build>_stock_inc_pack.zst`
  (incremental delta vs. Minimal).

Both lists are presented **newest first**. The operator picks one U-Boot build
and one snapshot build. **Minimal is always deployed** (from its full pack); any
extra profiles the operator selects are streamed from their **incremental** packs
on top of Minimal. Packs are zstd-compressed `btrfs send` streams, decompressed
in-process and piped into `btrfs receive`.

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
