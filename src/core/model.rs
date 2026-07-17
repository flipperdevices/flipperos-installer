//! Data model shared by both frontends and the installer engine.
//!
//! Everything here is plain data: it is [`Clone`] so the [`Controller`] can hand
//! immutable snapshots to each frontend, and (de)serializable so it can be read
//! from the image server API and from on-disk snapshot manifests.
//!
//! [`Controller`]: crate::core::Controller

use serde::{Deserialize, Serialize};

/// Where a boot image or snapshot was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Fetched from the remote image server.
    Server,
    /// Discovered on a piece of removable media (SD/USB), already mounted.
    Removable {
        /// Backing device, e.g. `/dev/sda1`.
        device: String,
        /// Mount point the file was found under.
        mountpoint: String,
    },
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Server => "server".to_string(),
            Source::Removable { device, .. } => format!("media {device}"),
        }
    }
}

/// Class of block storage as understood by the RK3576 boot ROM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageKind {
    /// Universal Flash Storage.
    Ufs,
    /// eMMC.
    Emmc,
    /// SD/microSD card.
    SdCard,
    /// USB mass storage.
    Usb,
    /// Anything else (loopback, ram, unknown).
    Other,
}

impl StorageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageKind::Ufs => "UFS",
            StorageKind::Emmc => "eMMC",
            StorageKind::SdCard => "SD",
            StorageKind::Usb => "USB",
            StorageKind::Other => "other",
        }
    }

    /// Whether the RK3576 mask ROM can boot from this class of device.
    pub fn boot_rom_capable(&self) -> bool {
        matches!(
            self,
            StorageKind::Ufs | StorageKind::Emmc | StorageKind::SdCard
        )
    }
}

/// A discovered block device that could be a flash target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageDevice {
    /// Whole-disk node, e.g. `/dev/mmcblk0` or `/dev/sda`.
    pub path: String,
    pub kind: StorageKind,
    pub model: String,
    pub size_bytes: u64,
    pub removable: bool,
    /// Native logical block (sector) size in bytes, as reported by the kernel.
    /// 512 for most eMMC/SD, typically 4096 for UFS. GPT geometry must be
    /// written in these units or the kernel won't recognise the table.
    pub logical_block_size: u64,
}

impl StorageDevice {
    /// Human-friendly size, e.g. `29.7 GiB`.
    pub fn human_size(&self) -> String {
        human_bytes(self.size_bytes)
    }

    /// Whether the RK3576 boot ROM can boot from this device.
    pub fn boot_rom_capable(&self) -> bool {
        self.kind.boot_rom_capable()
    }

    pub fn summary(&self) -> String {
        format!(
            "{} [{}] {} — {}",
            self.path,
            self.kind.as_str(),
            self.human_size(),
            self.model
        )
    }
}

/// A selectable U-Boot build (one entry from the image server's `/u-boot`
/// listing). The flashable image is `<board>/u-boot-rockchip.bin` inside it.
/// One entry of a build manifest's `sourcestamps`: a source repository and the
/// exact revision that went into the build.
#[derive(Clone, Debug, Deserialize)]
pub struct SourceStamp {
    #[serde(default)]
    pub codebase: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub revision: String,
}

/// The `build` section of a build manifest.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BuildMeta {
    #[serde(default)]
    pub builder: String,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub timestamp: String,
}

/// A build manifest's `build` + `sourcestamps` sections, shown in the details
/// popup. Deserializes directly from the manifest (other fields are ignored).
#[derive(Clone, Debug, Deserialize)]
pub struct BuildDetails {
    #[serde(default)]
    pub build: BuildMeta,
    #[serde(default)]
    pub sourcestamps: Vec<SourceStamp>,
}

#[derive(Clone, Debug)]
pub struct UbootBuild {
    /// Stable id: the build's directory segment on the server.
    pub id: String,
    /// Short human label (revision + date).
    pub label: String,
    /// Modification time (ISO-8601), used for newest-first sorting.
    pub mtime: String,
    /// URL (server) or path (media) of the flashable `u-boot-rockchip.bin`.
    pub image_location: String,
    /// Location of this build's `manifest.json`, for lazily loading details.
    pub manifest_location: String,
    pub source: Source,
    /// Size in bytes if known, else 0.
    pub size_bytes: u64,
    /// Build + source details from the manifest; `None` until fetched.
    pub details: Option<BuildDetails>,
}

impl UbootBuild {
    /// Build number from the (lazily fetched) manifest, if known.
    pub fn build_number(&self) -> Option<u64> {
        self.details.as_ref().and_then(|d| d.build.number)
    }

    /// Identifier shown in the lists: the manifest build number once fetched,
    /// otherwise the human label as a placeholder/fallback.
    pub fn display_name(&self) -> String {
        match self.build_number() {
            Some(n) => format!("#{n}"),
            None => self.label.clone(),
        }
    }

    pub fn summary(&self) -> String {
        format!("{} ({}, {})", self.display_name(), human_time(&self.mtime), self.source.label())
    }

    /// Multi-line details for the info popup, or a loading placeholder.
    pub fn details_text(&self) -> String {
        format_details(self.details.as_ref())
    }
}

/// One downloadable pack file (a zstd-compressed `btrfs send` stream).
#[derive(Clone, Debug)]
pub struct PackFile {
    /// URL (server) or path (media) of the `.zst` pack.
    pub location: String,
    pub source: Source,
    pub size_bytes: u64,
}

/// A profile within a snapshot build, with its full and/or incremental packs.
#[derive(Clone, Debug)]
pub struct ProfilePack {
    /// Profile name as used on the server, e.g. `Minimal`, `Desktop`.
    pub name: String,
    /// Build number as it appears in the pack filenames, e.g. `694`.
    pub build: String,
    /// Full stock pack (`<name>_<build>_stock_pack.zst`).
    pub full: Option<PackFile>,
    /// Incremental pack vs. Minimal (`<name>_<build>_stock_inc_pack.zst`).
    pub incremental: Option<PackFile>,
}

impl ProfilePack {
    /// Whether this profile is the mandatory Minimal base.
    pub fn is_minimal(&self) -> bool {
        self.name.eq_ignore_ascii_case("minimal")
    }

    /// Received (RO) stock subvolume name, matching the pack filename without
    /// the `_pack.zst` suffix, e.g. `@Minimal_694_stock`.
    pub fn stock_subvol(&self) -> String {
        format!("@{}_{}_stock", self.name, self.build)
    }

    /// Deployed (writable) root subvolume name, e.g. `@Minimal`.
    pub fn root_subvol(&self) -> String {
        format!("@{}", self.name)
    }

    pub fn summary(&self) -> String {
        let size = self
            .incremental
            .as_ref()
            .or(self.full.as_ref())
            .map(|p| human_bytes(p.size_bytes))
            .unwrap_or_else(|| "?".to_string());
        format!("{} ({size})", self.name)
    }
}

/// A selectable snapshot (rootfs) build. Its per-profile packs are loaded lazily
/// from the build's own manifest once the build is selected.
#[derive(Clone, Debug)]
pub struct SnapshotBuild {
    /// Stable id: the build's directory segment on the server.
    pub id: String,
    /// Short human label.
    pub label: String,
    /// Modification time (ISO-8601), used for newest-first sorting.
    pub mtime: String,
    pub source: Source,
    /// Base URL/path of the build directory (ends with `/`).
    pub base_location: String,
    /// Build number, if known (filled when profiles are loaded).
    pub build_number: Option<u64>,
    /// Profiles available in this build (empty until loaded).
    pub profiles: Vec<ProfilePack>,
    /// Whether [`Self::profiles`] has been fetched.
    pub loaded: bool,
    /// Build + source details from the manifest; `None` until fetched.
    pub details: Option<BuildDetails>,
}

impl SnapshotBuild {
    /// Build number, from the profile listing (`build_number`) or the lazily
    /// fetched manifest details, if known.
    pub fn resolved_build_number(&self) -> Option<u64> {
        self.build_number
            .or_else(|| self.details.as_ref().and_then(|d| d.build.number))
    }

    /// Identifier shown in the lists: the manifest build number once fetched,
    /// otherwise the human label as a placeholder/fallback.
    pub fn display_name(&self) -> String {
        match self.resolved_build_number() {
            Some(n) => format!("#{n}"),
            None => self.label.clone(),
        }
    }

    pub fn summary(&self) -> String {
        format!("{} ({}, {})", self.display_name(), human_time(&self.mtime), self.source.label())
    }

    /// Multi-line details for the info popup, or a loading placeholder.
    pub fn details_text(&self) -> String {
        format_details(self.details.as_ref())
    }

    /// The mandatory Minimal base profile, if present.
    pub fn minimal(&self) -> Option<&ProfilePack> {
        self.profiles.iter().find(|p| p.is_minimal())
    }

    /// Non-Minimal profiles the user may opt into.
    pub fn extra_profiles(&self) -> impl Iterator<Item = &ProfilePack> {
        self.profiles.iter().filter(|p| !p.is_minimal())
    }
}

/// Detected board / SoC identity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BoardInfo {
    /// Device-tree `compatible` strings, most-specific first.
    pub compatible: Vec<String>,
    /// Human model string from the device tree.
    pub model: String,
    /// SoC identifier, e.g. `rockchip,rk3576`.
    pub soc: String,
    /// Canonical board id used to query the image server, e.g. `flipper-one`.
    pub board_id: String,
}

/// The user's current selections in the installer.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Target whole-disk device path.
    pub target_device: Option<String>,
    /// Selected U-Boot build id.
    pub uboot: Option<String>,
    /// Selected snapshot build id.
    pub snapshot_build: Option<String>,
    /// Extra profile names to deploy (Minimal is always deployed implicitly).
    pub profiles: Vec<String>,
}

/// High-level state machine of the installer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Probing hardware and sources.
    Discovering,
    /// Idle, waiting for the operator to make selections.
    Ready,
    /// Installation in progress.
    Installing,
    /// Installation finished successfully.
    Done,
    /// Installation aborted with an error.
    Failed(String),
}

impl Default for Phase {
    fn default() -> Self {
        Phase::Discovering
    }
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Discovering => "discovering…".to_string(),
            Phase::Ready => "ready".to_string(),
            Phase::Installing => "installing…".to_string(),
            Phase::Done => "done".to_string(),
            Phase::Failed(e) => format!("failed: {e}"),
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Phase::Discovering | Phase::Installing)
    }
}

/// The full application state. A clone of this is the "snapshot" broadcast to
/// every subscribed frontend whenever anything changes.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub board: BoardInfo,
    pub devices: Vec<StorageDevice>,
    /// Available U-Boot builds, newest first.
    pub uboot_builds: Vec<UbootBuild>,
    /// Available snapshot builds, newest first.
    pub snapshot_builds: Vec<SnapshotBuild>,
    pub selection: Selection,
    pub phase: Phase,
    /// Installation progress in the range `0.0..=1.0`.
    pub progress: f32,
    /// Rolling activity log shared by both frontends.
    pub log: Vec<String>,
    /// Device types the image server can install onto, taken from the latest
    /// U-Boot build manifest. Empty until the catalog has been fetched.
    pub supported_device_types: Vec<String>,
}

impl AppState {
    /// Whether the current selection is complete enough to start flashing.
    /// Minimal is always deployed, so no extra profile selection is required.
    pub fn can_install(&self) -> bool {
        !self.phase.is_busy()
            && self.selection.target_device.is_some()
            && self.selection.uboot.is_some()
            && self.selected_build().and_then(|b| b.minimal()).is_some()
    }

    /// The currently selected target device, if any.
    pub fn target(&self) -> Option<&StorageDevice> {
        let path = self.selection.target_device.as_deref()?;
        self.devices.iter().find(|d| d.path == path)
    }

    /// The currently selected U-Boot build, if any.
    pub fn selected_uboot(&self) -> Option<&UbootBuild> {
        let id = self.selection.uboot.as_deref()?;
        self.uboot_builds.iter().find(|b| b.id == id)
    }

    /// The currently selected snapshot build, if any.
    pub fn selected_build(&self) -> Option<&SnapshotBuild> {
        let id = self.selection.snapshot_build.as_deref()?;
        self.snapshot_builds.iter().find(|b| b.id == id)
    }
}

/// Format an ISO-8601 mtime (`2026-07-16T08:43:24.511000+00:00`) as a short
/// local-ish `YYYY-MM-DD HH:MM`. Falls back to the raw string on anything odd.
pub fn human_time(mtime: &str) -> String {
    match mtime.split_once('T') {
        Some((date, rest)) => match rest.get(0..5) {
            Some(hm) if hm.len() == 5 => format!("{date} {hm}"),
            _ => date.to_string(),
        },
        None => mtime.to_string(),
    }
}

/// Format a build's `build` + `sourcestamps` sections for the details popup.
/// `None` means the manifest hasn't been fetched yet.
pub fn format_details(details: Option<&BuildDetails>) -> String {
    let d = match details {
        None => return "Loading\u{2026}".to_string(),
        Some(d) => d,
    };

    let b = &d.build;
    let mut out = match (b.number, b.builder.is_empty()) {
        (Some(n), false) => format!("Build #{n} ({})", b.builder),
        (Some(n), true) => format!("Build #{n}"),
        (None, false) => format!("Build ({})", b.builder),
        (None, true) => "Build".to_string(),
    };
    if !b.timestamp.is_empty() {
        out.push_str(&format!("\n  {}", human_time(&b.timestamp)));
    }
    if !b.url.is_empty() {
        out.push_str(&format!("\n  {}", b.url));
    }
    for s in &d.sourcestamps {
        out.push_str(&format!(
            "\n\n{}\n  branch: {}\n  rev: {}\n  {}",
            s.codebase, s.branch, s.revision, s.repository
        ));
    }
    out
}

/// Format a byte count using binary (GiB/MiB) units.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
