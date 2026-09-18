//! Data model shared by both frontends and the installer engine.
//!
//! Everything here is plain data: it is [`Clone`] so the [`Controller`] can hand
//! immutable snapshots to each frontend, and (de)serializable so it can be read
//! from the image server API and from on-disk snapshot manifests.
//!
//! [`Controller`]: crate::core::Controller

use std::collections::HashMap;
use std::sync::Arc;

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
    /// A plain local directory: a bundle unpacked into the scratch dir, one
    /// passed with `--bundle`, or staged artifacts verified before the install.
    Local {
        /// Directory the files live under.
        root: String,
    },
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Server => "server".to_string(),
            Source::Removable { device, .. } => format!("media {device}"),
            Source::Local { .. } => "local".to_string(),
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

    /// Whether the RK3576 boot ROM can boot from this class of device.
    pub fn boot_rom_capable(&self) -> bool {
        matches!(
            self,
            StorageKind::Ufs | StorageKind::Emmc | StorageKind::SdCard
        )
    }

    /// Whether this is media an operator can pull out — which is what makes a
    /// device a *source* to read bundles from rather than something to
    /// auto-select as an install target.
    ///
    /// Answered from the class, not from sysfs: `/sys/block/<disk>/removable`
    /// is wrong in both directions. The mmc block driver never sets it, so an
    /// SD card reads 0, and a USB disk commonly reads 0 as well.
    pub fn is_removable(&self) -> bool {
        matches!(self, StorageKind::SdCard | StorageKind::Usb)
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
    /// Removable *media*, derived from [`StorageKind::is_removable`] — not the
    /// kernel's `removable` flag, which does not answer this. Do not re-wire it.
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
    /// Location of this build's `manifest.json`, for lazily loading contents.
    pub manifest_location: String,
    pub source: Source,
    /// Size of the image in bytes; 0 until the manifest has been read.
    pub size_bytes: u64,
    /// SHA-256 the manifest publishes for the image, if it publishes one.
    /// `None` after loading means the build shipped no digest.
    pub sha256: Option<String>,
    /// Build + source details from the manifest; `None` until fetched.
    pub details: Option<BuildDetails>,
    /// The Falcon boot menu this build carries in its own directory, if it
    /// carries one. `None` until the manifest has been read, and after that for
    /// every build that predates the boot menu or postdates the split that moved
    /// it into a listing of its own — see [`FalconBuild`]. Only a fallback now,
    /// for an origin that publishes no such listing.
    pub boot_menu: Option<FalconImage>,
    /// Whether this build's manifest has been read (filling [`Self::size_bytes`],
    /// [`Self::sha256`], [`Self::details`] and [`Self::boot_menu`]). Mirrors
    /// [`SnapshotBuild::loaded`]: both kinds of build carry a manifest and are
    /// loaded through the same path.
    pub loaded: bool,
}

/// A Falcon-mode FIT image — a kernel plus an initramfs that U-Boot starts
/// without going through a full boot — written raw to a fixed place on the
/// target.
///
/// Two of them exist: the boot menu (`bootmenu-falcon.itb`), which goes to the
/// start of the loader partition, and the recovery system
/// (`recovery-falcon.itb`), which goes to its own logical unit. Both are UFS-only
/// for the same reason: the boot ROM reads U-Boot from a boot LU there, leaving
/// the loader partition free, and only a UFS device has the spare logical unit.
#[derive(Clone, Debug)]
pub struct FalconImage {
    /// URL (server) or path (media) of the `.itb`.
    pub location: String,
    pub source: Source,
    pub size_bytes: u64,
    /// SHA-256 the manifest publishes for the image, if it publishes one.
    pub sha256: Option<String>,
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
        format!(
            "{} ({}, {})",
            self.display_name(),
            human_time(&self.mtime),
            self.source.label()
        )
    }

    /// Multi-line details for the info popup, or a loading placeholder.
    pub fn details_text(&self) -> String {
        format_details(self.details.as_ref())
    }
}

/// One build directory of a Falcon image listing (`falcon-bootmenu/`,
/// `falcon-recovery/`), holding one image per board.
///
/// The image server builds these separately from the bootloader, so they are
/// picked separately too — the counterpart of [`UbootBuild`] for everything that
/// is not the bootloader itself.
#[derive(Clone, Debug)]
pub struct FalconBuild {
    /// Stable id: the build's directory segment on the server.
    pub id: String,
    /// Short human label (revision + date).
    pub label: String,
    /// Modification time (ISO-8601), used for newest-first sorting.
    pub mtime: String,
    /// URL (server) or path (media) of this board's `.itb`.
    pub image_location: String,
    /// Location of this build's `manifest.json`, for lazily loading contents.
    pub manifest_location: String,
    pub source: Source,
    /// Size of the image in bytes; 0 until the manifest has been read.
    pub size_bytes: u64,
    /// SHA-256 the manifest publishes for the image, if it publishes one.
    pub sha256: Option<String>,
    /// Build + source details from the manifest; `None` until fetched.
    pub details: Option<BuildDetails>,
    /// Whether this build's manifest has been read.
    pub loaded: bool,
}

impl FalconBuild {
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
        format!(
            "{} ({}, {})",
            self.display_name(),
            human_time(&self.mtime),
            self.source.label()
        )
    }

    /// Multi-line details for the info popup, or a loading placeholder.
    pub fn details_text(&self) -> String {
        format_details(self.details.as_ref())
    }

    /// The image this build offers for the board it was listed for. `None` until
    /// the manifest has been read, since neither the size nor the digest the
    /// install verifies against is known before that.
    pub fn image(&self) -> Option<FalconImage> {
        if !self.loaded {
            return None;
        }
        Some(FalconImage {
            location: self.image_location.clone(),
            source: self.source.clone(),
            size_bytes: self.size_bytes,
            sha256: self.sha256.clone(),
        })
    }
}

/// One downloadable pack file (a zstd-compressed `btrfs send` stream).
#[derive(Clone, Debug)]
pub struct PackFile {
    /// URL (server) or path (media) of the `.zst` pack.
    pub location: String,
    pub source: Source,
    pub size_bytes: u64,
    /// SHA-256 the manifest publishes for this pack, if any. Covers the pack as
    /// stored, i.e. the *compressed* bytes.
    pub sha256: Option<String>,
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
    /// Shared, version-independent `/home` seed pack (a full `btrfs send` of
    /// `@home`), if the build ships one. Filled when profiles are loaded.
    pub home_pack: Option<PackFile>,
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
        format!(
            "{} ({}, {})",
            self.display_name(),
            human_time(&self.mtime),
            self.source.label()
        )
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

// --- update bundles --------------------------------------------------------

/// Where a bundle's files live. A bundle is either served from the update
/// server or sits unpacked in a local directory; a `*.tar.zst` bundle becomes a
/// [`BundleLocation::Dir`] once it has been unpacked into the scratch dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleLocation {
    /// Remote build directory, ending in `/`.
    Remote { base: String },
    /// Local directory holding `manifest.json`, ending in `/`.
    Dir { root: String },
}

impl BundleLocation {
    /// Join a path relative to the bundle root.
    pub fn join(&self, rel: &str) -> String {
        let base = match self {
            BundleLocation::Remote { base } => base,
            BundleLocation::Dir { root } => root,
        };
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            rel.trim_start_matches('/')
        )
    }

    /// Location of the bundle's `manifest.json`.
    pub fn manifest(&self) -> String {
        self.join("manifest.json")
    }
}

/// A listed bundle build directory, before its manifest has been read. Cheap:
/// everything here comes from the listing, so browsing costs one request per
/// level rather than one per build.
#[derive(Clone, Debug)]
pub struct BundleRef {
    /// Stable id used by the UI and by [`Selection::bundle`], e.g.
    /// `nightly/20260812-83ddb68-15` or the path of a local bundle.
    pub id: String,
    /// Channel path this build was listed under: `nightly`, `dev/alchark/topic`,
    /// or `local` for a bundle found on media.
    pub channel: String,
    /// Build directory name, which is also how the build is labelled.
    pub dir: String,
    pub location: BundleLocation,
    pub source: Source,
    /// Set when this bundle came from a `*.tar.zst` that must be unpacked
    /// before it can be installed from.
    pub archive: Option<String>,
}

impl BundleRef {
    /// Row label in the build lists.
    pub fn label(&self) -> String {
        self.dir.clone()
    }

    pub fn summary(&self) -> String {
        format!("{} ({}, {})", self.dir, self.channel, self.source.label())
    }
}

/// A bundle whose manifest has been read: one pinned U-Boot image and one
/// fully-loaded snapshot build, so the install pipeline consumes it exactly as
/// it consumes a hand-picked pair.
#[derive(Clone, Debug)]
pub struct SelectedBundle {
    pub reference: BundleRef,
    /// `bundle.version` from the manifest, e.g. `20260812-83ddb68`.
    pub version: String,
    /// `bundle.channel` from the manifest.
    pub channel: String,
    pub description: String,
    /// Name of the bundle's own `*.tar.zst`, as published. Informational: the
    /// remote install path fetches individual files instead.
    pub archive: String,
    /// Per-board U-Boot directory the images were taken from.
    pub board_dir: String,
    /// Board ids this bundle ships U-Boot images for.
    pub device_types: Vec<String>,
    pub uboot: UbootBuild,
    /// The Falcon boot menu for [`Self::board_dir`], if the bundle ships one.
    pub boot_menu: Option<FalconImage>,
    /// The Falcon recovery image for [`Self::board_dir`], if the bundle ships
    /// one.
    pub recovery: Option<FalconImage>,
    pub build: SnapshotBuild,
}

impl SelectedBundle {
    /// Short label for the summary row. Kept brief: the GUI's value chip is
    /// 158 px wide and elides.
    pub fn label(&self) -> String {
        match self.build.resolved_build_number() {
            Some(n) => format!("{} #{n}", self.channel),
            None => format!("{} {}", self.channel, self.reference.dir),
        }
    }
}

/// Progress of a lazily fetched listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoadState {
    /// Not requested yet.
    #[default]
    Idle,
    /// A worker is fetching it.
    Loading,
    Loaded,
    Failed(String),
}

/// One lazily fetched level of the bundle hierarchy.
#[derive(Clone, Debug)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub state: LoadState,
}

// Hand-written so an empty listing does not require `T: Default`.
impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            state: LoadState::default(),
        }
    }
}

impl<T> Listing<T> {
    pub fn is_pending(&self) -> bool {
        matches!(self.state, LoadState::Idle | LoadState::Loading)
    }
}

/// The lazily listed bundle hierarchy. Held behind an `Arc` in [`AppState`] so
/// the snapshot broadcast on every mutation stays a refcount bump rather than a
/// deep copy of every level.
#[derive(Clone, Debug, Default)]
pub struct CatalogCache {
    /// Channel names offered by the bucket, in listing order.
    pub channels: Listing<String>,
    /// Builds per channel path (`nightly`, `dev/alchark/topic`).
    pub builds: HashMap<String, Listing<BundleRef>>,
    /// Intermediate levels of the `dev/` tree, keyed by their prefix path
    /// (`dev` → usernames, `dev/alchark` → branch names).
    pub dirs: HashMap<String, Listing<String>>,
    /// Bundles found on removable media or passed with `--bundle`.
    pub local: Listing<BundleRef>,
}

/// Which kind of source the operator is installing from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstallMode {
    /// An update bundle: a pinned U-Boot + rootfs pair. The default.
    #[default]
    Bundle,
    /// A hand-picked U-Boot build and rootfs build from the image server.
    Custom,
}

impl InstallMode {
    pub fn label(&self) -> &'static str {
        match self {
            InstallMode::Bundle => "bundle",
            InstallMode::Custom => "custom",
        }
    }
}

/// Whether artifacts are checked before the target is touched, or as they are
/// written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FetchMode {
    /// Download every needed artifact into the scratch dir and verify it
    /// against the manifest *before* anything destructive happens, then install
    /// from the local copies.
    #[default]
    VerifyFirst,
    /// Stream straight into the install. Artifacts are still hashed on the way
    /// through, but a mismatch can only be reported after the bytes have landed,
    /// so it is a warning rather than a failure.
    Stream,
}

impl FetchMode {
    pub fn label(&self) -> &'static str {
        match self {
            FetchMode::VerifyFirst => "verify first",
            FetchMode::Stream => "stream",
        }
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
    /// Selected U-Boot build id. [`InstallMode::Custom`] only.
    pub uboot: Option<String>,
    /// Selected Falcon boot menu build id. [`InstallMode::Custom`] only.
    pub boot_menu: Option<String>,
    /// Selected Falcon recovery build id. [`InstallMode::Custom`] only.
    pub recovery: Option<String>,
    /// Selected snapshot build id. [`InstallMode::Custom`] only.
    pub snapshot_build: Option<String>,
    /// Extra profile names to deploy (Minimal is always deployed implicitly).
    pub profiles: Vec<String>,
    /// Whether we install from a bundle or from a hand-picked pair.
    pub mode: InstallMode,
    /// Selected bundle id ([`BundleRef::id`]). [`InstallMode::Bundle`] only.
    pub bundle: Option<String>,
    /// Whether artifacts are verified before the target is touched.
    pub fetch: FetchMode,
}

/// High-level state machine of the installer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Probing hardware and sources.
    #[default]
    Discovering,
    /// Idle, waiting for the operator to make selections.
    Ready,
    /// Rewriting a UFS target's logical units before an install can start.
    Provisioning,
    /// Installation in progress.
    Installing,
    /// Installation finished successfully.
    Done,
    /// Installation aborted with an error.
    Failed(String),
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Discovering => "discovering…".to_string(),
            Phase::Ready => "ready".to_string(),
            Phase::Provisioning => "provisioning…".to_string(),
            Phase::Installing => "installing…".to_string(),
            Phase::Done => "done".to_string(),
            Phase::Failed(e) => format!("failed: {e}"),
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Phase::Discovering | Phase::Provisioning | Phase::Installing
        )
    }
}

/// A modal question the core needs answered before it can go on.
///
/// It lives in [`AppState`] rather than in a frontend because the core is what
/// raises it: the navigation stack is per-frontend and the core cannot push a
/// level onto it. Both frontends therefore show the same prompt at the same time,
/// and whichever one answers it clears it for the other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    /// The body, one paragraph per entry; the frontends wrap it themselves.
    pub lines: Vec<String>,
    /// Caption for the confirming action, e.g. `Reprovision`.
    pub confirm: String,
    pub cancel: String,
}

/// What confirming a [`Prompt`] sets in motion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// Rewrite a UFS target's logical units to the Flipper scheme. Carries the
    /// target it described, so confirming cannot act on a different device than
    /// the operator was shown.
    ReprovisionUfs {
        target: crate::core::provision::Target,
    },
}

/// The full application state. A clone of this is the "snapshot" broadcast to
/// every subscribed frontend whenever anything changes.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub board: BoardInfo,
    pub devices: Vec<StorageDevice>,
    /// Available U-Boot builds, newest first.
    pub uboot_builds: Vec<UbootBuild>,
    /// Available Falcon boot menu builds, newest first.
    pub boot_menu_builds: Vec<FalconBuild>,
    /// Available Falcon recovery builds, newest first.
    pub recovery_builds: Vec<FalconBuild>,
    /// Available snapshot builds, newest first.
    pub snapshot_builds: Vec<SnapshotBuild>,
    pub selection: Selection,
    pub phase: Phase,
    /// Installation progress in the range `0.0..=1.0`.
    pub progress: f32,
    /// Rolling activity log shared by both frontends.
    pub log: Vec<String>,
    /// Device types we can install onto: in bundle mode the boards the selected
    /// bundle ships U-Boot for, otherwise those from the image server's latest
    /// U-Boot build manifest. Empty until a catalog has been fetched.
    pub supported_device_types: Vec<String>,
    /// The lazily listed bundle hierarchy.
    pub catalog: Arc<CatalogCache>,
    /// The selected bundle with its manifest read; `None` until it resolves.
    pub bundle: Option<SelectedBundle>,
    /// Why the selected bundle could not be resolved, if it could not be.
    pub bundle_error: Option<String>,
    /// Whether this run only logs destructive steps (`--dry-run`). Copied from
    /// the config at construction and never changed; it lives here rather than
    /// being read from the config because the frontends render from a snapshot
    /// alone, and they use it to hide affordances that would be a lie in a dry
    /// run.
    pub dry_run: bool,
    /// How the selected UFS target's logical units compare to the Flipper
    /// provisioning scheme. `None` for a non-UFS target, or before the probe has
    /// finished.
    pub ufs: Option<crate::core::provision::Status>,
    /// The modal question waiting for an answer, if any.
    pub prompt: Option<Prompt>,
}

impl AppState {
    /// Whether the current selection is complete enough to start flashing.
    /// Minimal is always deployed, so no extra profile selection is required.
    pub fn can_install(&self) -> bool {
        !self.phase.is_busy()
            && self.selection.target_device.is_some()
            && self.selected_uboot().is_some()
            && self.selected_build().and_then(|b| b.minimal()).is_some()
    }

    /// Whether the machine can be rebooted from the UI.
    ///
    /// Only after a completed install: by then the target has been flushed and
    /// unmounted ([`crate::core::install`] does that on every exit path), so
    /// going down is safe. Never in a dry run — nothing was written, so offering
    /// it would imply the machine is ready to boot the new system.
    pub fn can_reboot(&self) -> bool {
        matches!(self.phase, Phase::Done) && !self.dry_run
    }

    /// Short label for the "Install" summary row shared by both frontends:
    /// "in progress" during a healthy run, "ready" when a run can be started,
    /// else "incomplete" (a selection is still missing).
    pub fn install_status_label(&self) -> &'static str {
        match self.phase {
            Phase::Installing => "in progress",
            _ if self.can_install() => "ready",
            _ => "incomplete",
        }
    }

    /// The currently selected target device, if any.
    pub fn target(&self) -> Option<&StorageDevice> {
        let path = self.selection.target_device.as_deref()?;
        self.devices.iter().find(|d| d.path == path)
    }

    /// The currently selected U-Boot build: the one a bundle pins, or the one
    /// hand-picked from the image server.
    pub fn selected_uboot(&self) -> Option<&UbootBuild> {
        match self.selection.mode {
            InstallMode::Bundle => self.bundle.as_ref().map(|b| &b.uboot),
            InstallMode::Custom => {
                let id = self.selection.uboot.as_deref()?;
                self.uboot_builds.iter().find(|b| b.id == id)
            }
        }
    }

    /// The Falcon boot menu image this run would write: the one a bundle pins, or
    /// the one hand-picked from the image server.
    ///
    /// Falls back to the menu the selected U-Boot build carries in its own
    /// directory, which is how an origin publishing no `falcon-bootmenu/` listing
    /// — an older removable-media mirror, or a U-Boot build from before the split
    /// — still yields a menu.
    pub fn selected_boot_menu(&self) -> Option<FalconImage> {
        match self.selection.mode {
            InstallMode::Bundle => self.bundle.as_ref()?.boot_menu.clone(),
            InstallMode::Custom => match self.selection.boot_menu.as_deref() {
                Some(id) => self
                    .boot_menu_builds
                    .iter()
                    .find(|b| b.id == id)
                    .and_then(|b| b.image()),
                None => self.selected_uboot()?.boot_menu.clone(),
            },
        }
    }

    /// Value for the boot menu status line: the hand-picked build if there is
    /// one, else what the image a bundle or a U-Boot build supplied says about
    /// itself — there being no build to name in that case.
    pub fn boot_menu_summary(&self) -> Option<String> {
        if self.selection.mode == InstallMode::Custom {
            if let Some(id) = self.selection.boot_menu.as_deref() {
                return self
                    .boot_menu_builds
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| b.summary());
            }
        }
        let image = self.selected_boot_menu()?;
        Some(format!(
            "{} ({})",
            human_bytes(image.size_bytes),
            image.source.label()
        ))
    }

    /// The Falcon recovery image this run would write: the one a bundle pins, or
    /// the one hand-picked from the image server.
    ///
    /// No fallback to the U-Boot build, unlike [`Self::selected_boot_menu`]: the
    /// builds that carry a recovery image of their own were never installed from,
    /// so there is no behaviour to preserve.
    pub fn selected_recovery(&self) -> Option<FalconImage> {
        match self.selection.mode {
            InstallMode::Bundle => self.bundle.as_ref()?.recovery.clone(),
            InstallMode::Custom => self
                .recovery_builds
                .iter()
                .find(|b| Some(b.id.as_str()) == self.selection.recovery.as_deref())
                .and_then(|b| b.image()),
        }
    }

    /// Value for the recovery status line, the counterpart of
    /// [`Self::boot_menu_summary`].
    pub fn recovery_summary(&self) -> Option<String> {
        if self.selection.mode == InstallMode::Custom {
            if let Some(id) = self.selection.recovery.as_deref() {
                return self
                    .recovery_builds
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| b.summary());
            }
        }
        let image = self.selected_recovery()?;
        Some(format!(
            "{} ({})",
            human_bytes(image.size_bytes),
            image.source.label()
        ))
    }

    /// The currently selected snapshot build: the one a bundle pins, or the one
    /// hand-picked from the image server.
    pub fn selected_build(&self) -> Option<&SnapshotBuild> {
        match self.selection.mode {
            InstallMode::Bundle => self.bundle.as_ref().map(|b| &b.build),
            InstallMode::Custom => {
                let id = self.selection.snapshot_build.as_deref()?;
                self.snapshot_builds.iter().find(|b| b.id == id)
            }
        }
    }

    /// The selected bundle's [`BundleRef`], if the selection names one.
    pub fn selected_bundle_ref(&self) -> Option<&BundleRef> {
        self.bundle_ref_by_id(self.selection.bundle.as_deref()?)
    }

    /// Find a listed bundle by id, across every level that has been listed
    /// (including the local ones).
    pub fn bundle_ref_by_id(&self, id: &str) -> Option<&BundleRef> {
        self.catalog
            .builds
            .values()
            .chain(std::iter::once(&self.catalog.local))
            .flat_map(|l| l.items.iter())
            .find(|b| b.id == id)
    }

    /// Value for the "Source" summary row.
    pub fn source_label(&self) -> String {
        match self.selection.mode {
            InstallMode::Custom => "custom".to_string(),
            InstallMode::Bundle => match (&self.bundle, &self.bundle_error) {
                (Some(b), _) => b.label(),
                (None, Some(_)) => "unavailable".to_string(),
                (None, None) => match self.selected_bundle_ref() {
                    Some(r) => format!("{}\u{2026}", r.dir),
                    None => "(select)".to_string(),
                },
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reboot_is_offered_only_after_a_real_install_completed() {
        let done = AppState {
            phase: Phase::Done,
            ..AppState::default()
        };
        assert!(done.can_reboot());

        // A dry run wrote nothing, so there is nothing to boot into.
        assert!(!AppState {
            dry_run: true,
            ..done.clone()
        }
        .can_reboot());

        for phase in [
            Phase::Discovering,
            Phase::Ready,
            Phase::Installing,
            Phase::Failed("boom".to_string()),
        ] {
            let state = AppState {
                phase,
                ..AppState::default()
            };
            assert!(!state.can_reboot(), "{}", state.phase.label());
        }
    }

    fn falcon_image(location: &str, size: u64) -> FalconImage {
        FalconImage {
            location: location.to_string(),
            source: Source::Server,
            size_bytes: size,
            sha256: None,
        }
    }

    fn uboot_carrying(menu: Option<FalconImage>) -> UbootBuild {
        UbootBuild {
            id: "u".into(),
            label: "u".into(),
            mtime: String::new(),
            image_location: "https://i.invalid/u/flipper-one/u-boot-rockchip.bin".into(),
            manifest_location: "https://i.invalid/u/manifest.json".into(),
            source: Source::Server,
            size_bytes: 10,
            sha256: None,
            details: None,
            boot_menu: menu,
            loaded: true,
        }
    }

    #[test]
    fn the_boot_menu_listing_wins_over_the_one_inside_a_uboot_build() {
        let in_tree = falcon_image("https://i.invalid/u/flipper-one/bootmenu-falcon.itb", 40);
        let listed = FalconBuild {
            id: "m".into(),
            label: "m".into(),
            mtime: String::new(),
            image_location: "https://i.invalid/falcon-bootmenu/m/flipper-one/bootmenu-falcon.itb"
                .into(),
            manifest_location: "https://i.invalid/falcon-bootmenu/m/manifest.json".into(),
            source: Source::Server,
            size_bytes: 19,
            sha256: None,
            details: None,
            loaded: true,
        };
        let mut state = AppState {
            uboot_builds: vec![uboot_carrying(Some(in_tree))],
            boot_menu_builds: vec![listed.clone()],
            selection: Selection {
                mode: InstallMode::Custom,
                uboot: Some("u".to_string()),
                boot_menu: Some("m".to_string()),
                ..Selection::default()
            },
            ..AppState::default()
        };
        let picked = state.selected_boot_menu().expect("a menu");
        assert_eq!(picked.location, listed.image_location);
        assert_eq!(picked.size_bytes, 19);

        // With no listing to pick from, the menu the U-Boot build carries is what
        // an origin publishing none still yields.
        state.boot_menu_builds.clear();
        state.selection.boot_menu = None;
        assert_eq!(state.selected_boot_menu().expect("a menu").size_bytes, 40);

        // A U-Boot build carrying none either leaves the loader partition empty.
        state.uboot_builds = vec![uboot_carrying(None)];
        assert!(state.selected_boot_menu().is_none());
    }

    #[test]
    fn the_recovery_image_comes_only_from_its_own_listing() {
        // No fallback to the U-Boot build, unlike the boot menu: a build carrying
        // `recovery-falcon.itb` of its own was never installed from, so picking it
        // up now would start writing an image this installer has never written.
        let listed = FalconBuild {
            id: "r".into(),
            label: "r".into(),
            mtime: String::new(),
            image_location: "https://i.invalid/falcon-recovery/r/flipper-one/recovery-falcon.itb"
                .into(),
            manifest_location: "https://i.invalid/falcon-recovery/r/manifest.json".into(),
            source: Source::Server,
            size_bytes: 45,
            sha256: None,
            details: None,
            loaded: true,
        };
        let mut state = AppState {
            uboot_builds: vec![uboot_carrying(Some(falcon_image("u/menu.itb", 40)))],
            recovery_builds: vec![listed.clone()],
            selection: Selection {
                mode: InstallMode::Custom,
                uboot: Some("u".to_string()),
                recovery: Some("r".to_string()),
                ..Selection::default()
            },
            ..AppState::default()
        };
        let picked = state.selected_recovery().expect("a recovery image");
        assert_eq!(picked.location, listed.image_location);
        assert_eq!(picked.size_bytes, 45);

        state.recovery_builds.clear();
        state.selection.recovery = None;
        assert!(state.selected_recovery().is_none());
    }

    #[test]
    fn a_boot_menu_build_offers_nothing_until_its_manifest_is_read() {
        // The size and digest the install verifies against arrive with the
        // manifest, so an unloaded build must not be installed from.
        let build = FalconBuild {
            id: "m".into(),
            label: "m".into(),
            mtime: String::new(),
            image_location: "https://i.invalid/m/flipper-one/bootmenu-falcon.itb".into(),
            manifest_location: "https://i.invalid/m/manifest.json".into(),
            source: Source::Server,
            size_bytes: 0,
            sha256: None,
            details: None,
            loaded: false,
        };
        assert!(build.image().is_none());
        assert!(FalconBuild {
            loaded: true,
            ..build
        }
        .image()
        .is_some());
    }

    #[test]
    fn removable_media_is_what_an_operator_can_pull_out() {
        // An SD card is removable *and* a legal target — it just should not be
        // the one picked automatically.
        assert!(StorageKind::SdCard.is_removable());
        assert!(StorageKind::SdCard.boot_rom_capable());
        assert!(StorageKind::Usb.is_removable());

        for kind in [StorageKind::Emmc, StorageKind::Ufs, StorageKind::Other] {
            assert!(!kind.is_removable(), "{}", kind.as_str());
        }
    }
}
