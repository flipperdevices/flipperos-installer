//! Image catalog: reads the two-level build listings published by the image
//! server (and any removable-media mirror of the same layout).
//!
//! Layout:
//!   `<base>/u-boot/manifest.json`            -> list of U-Boot build dirs
//!   `<base>/u-boot/<dir>/manifest.json`      -> files incl. `<board>/u-boot-rockchip.bin`
//!                                               and `<board>/bootmenu-falcon.itb`
//!   `<base>/rootfs/manifest.json`            -> list of rootfs build dirs
//!   `<base>/rootfs/<dir>/manifest.json`      -> `<Profile>_<build>_stock[_inc]_pack.zst`
//!
//! `<base>` is an HTTP(S) URL for the server or a filesystem path for media.

use serde::Deserialize;

use crate::core::fetch;
use crate::core::model::{
    BootMenu, BuildDetails, PackFile, ProfilePack, SnapshotBuild, Source, UbootBuild,
};

/// The flashable bootloader inside a U-Boot build's per-board directory.
const UBOOT_IMAGE: &str = "u-boot-rockchip.bin";
/// The Falcon-mode boot menu FIT, published beside the bootloader it belongs to.
const BOOT_MENU_IMAGE: &str = "bootmenu-falcon.itb";

/// A place to read the catalog from.
#[derive(Clone, Debug)]
pub enum Origin {
    /// The remote image server, rooted at an HTTP(S) base URL.
    Server { base: String },
    /// A mounted removable-media mirror, rooted at a directory.
    Media { device: String, root: String },
}

impl Origin {
    fn base(&self) -> &str {
        match self {
            Origin::Server { base } => base,
            Origin::Media { root, .. } => root,
        }
    }

    fn source(&self) -> Source {
        match self {
            Origin::Server { .. } => Source::Server,
            Origin::Media { device, root } => Source::Removable {
                device: device.clone(),
                mountpoint: root.clone(),
            },
        }
    }

    /// Join a relative path onto the base, preserving a trailing slash in `rel`.
    fn join(&self, rel: &str) -> String {
        let base = self.base().trim_end_matches('/');
        format!("{base}/{}", rel.trim_start_matches('/'))
    }
}

/// Map a detected board id to the server's per-board U-Boot directory.
pub fn board_dir(board_id: &str) -> &'static str {
    match board_id {
        "flipper-one" => "flipper-one",
        _ => "generic",
    }
}

/// The device types (image-server board ids) offered by the latest U-Boot build.
///
/// The newest U-Boot build directory's manifest lists a
/// `<device-type>/u-boot-rockchip.bin` for every supported board, so the set of
/// those `<device-type>` path segments is exactly the list of devices we can
/// install onto. Returns an empty vector if the server is unreachable.
pub fn supported_device_types(origin: &Origin) -> Vec<String> {
    let list: ListManifest = match fetch_json(&origin.join("u-boot/manifest.json")) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    let Some(newest) = newest_first(list.directories, 1).into_iter().next() else {
        return Vec::new();
    };
    // Directory names already carry a trailing slash; normalise so we don't emit
    // a double slash that the object store would reject.
    let manifest_loc = origin.join(&format!(
        "u-boot/{}/manifest.json",
        newest.name.trim_end_matches('/')
    ));
    let bm: BuildManifest = match fetch_json(&manifest_loc) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let mut types: Vec<String> = Vec::new();
    for f in &bm.files {
        if let Some(dir) = f.path.strip_suffix(&format!("/{UBOOT_IMAGE}")) {
            let id = dir.rsplit('/').next().unwrap_or(dir).to_string();
            if !id.is_empty() && !types.contains(&id) {
                types.push(id);
            }
        }
    }
    types.sort();
    types
}

/// List the available U-Boot builds for `board_dir`, newest first (capped). The
/// image size and digest are loaded lazily via [`load_uboot_contents`].
pub fn uboot_builds(origin: &Origin, board_dir: &str, limit: usize) -> Vec<UbootBuild> {
    let list: ListManifest = match fetch_json(&origin.join("u-boot/manifest.json")) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    newest_first(list.directories, limit)
        .into_iter()
        .map(|d| {
            let base_location = origin.join(&format!("u-boot/{}", d.name));
            let image_location = format!("{base_location}{board_dir}/{UBOOT_IMAGE}");
            UbootBuild {
                id: d.name.clone(),
                label: uboot_label(&d.name),
                mtime: d.mtime.unwrap_or_default(),
                image_location,
                manifest_location: format!("{base_location}manifest.json"),
                source: origin.source(),
                size_bytes: 0,
                sha256: None,
                details: None,
                boot_menu: None,
                loaded: false,
            }
        })
        .collect()
}

/// Parsed contents of a U-Boot build manifest: the flashable image's size and
/// digest, the build metadata for the details popup, and the boot menu the build
/// ships beside the image.
#[derive(Debug)]
pub struct UbootContents {
    pub size: u64,
    pub sha256: Option<String>,
    pub mtime: String,
    pub details: BuildDetails,
    /// `None` for a build predating the boot menu.
    pub boot_menu: Option<BootMenu>,
}

/// Fetch a U-Boot build's manifest and extract the metadata for its
/// `<board_dir>/u-boot-rockchip.bin`, plus the `<board_dir>/bootmenu-falcon.itb`
/// published beside it.
///
/// The sibling of [`load_profiles`]: both kinds of build carry a manifest, and
/// both are read exactly once through this pair, so the details popup, the
/// verification pass and the install path all see the same fields.
pub fn load_uboot_contents(build: &UbootBuild, board_dir: &str) -> Result<UbootContents, String> {
    let bm: BuildManifest = fetch_json(&build.manifest_location)?;
    uboot_contents(&bm, build, board_dir)
}

/// The decision [`load_uboot_contents`] makes, separated from fetching the
/// manifest so it can be exercised directly.
fn uboot_contents(
    bm: &BuildManifest,
    build: &UbootBuild,
    board_dir: &str,
) -> Result<UbootContents, String> {
    let details = bm.details();
    // A manifest may list its files either bare or under a leading directory, so
    // match the tail rather than the whole path.
    let find = |name: &str| {
        let wanted = format!("{board_dir}/{name}");
        bm.files
            .iter()
            .find(move |f| f.path == wanted || f.path.ends_with(&format!("/{wanted}")))
    };

    let Some(image) = find(UBOOT_IMAGE) else {
        // The build exists but ships nothing for this board. Report it rather
        // than silently flashing whatever the URL happens to return.
        return Err(format!(
            "{} lists no {board_dir}/{UBOOT_IMAGE}",
            build.manifest_location
        ));
    };
    // The boot menu is published in the same per-board directory as the image, so
    // its location is that image's sibling. A build that ships none is still
    // installable: the menu is only ever written on UFS.
    let boot_menu = find(BOOT_MENU_IMAGE).map(|f| BootMenu {
        location: sibling_of(&build.image_location, BOOT_MENU_IMAGE),
        source: build.source.clone(),
        size_bytes: f.size,
        sha256: f.digest(),
    });

    Ok(UbootContents {
        size: image.size,
        sha256: image.digest(),
        mtime: image.mtime.clone(),
        details,
        boot_menu,
    })
}

/// Swap the last segment of `location` for `name`, naming a file in the same
/// directory.
fn sibling_of(location: &str, name: &str) -> String {
    match location.rfind('/') {
        Some(cut) => format!("{}{name}", &location[..=cut]),
        None => name.to_string(),
    }
}

/// List the available snapshot (rootfs) builds, newest first (capped). Profile
/// packs are loaded lazily via [`load_profiles`].
pub fn snapshot_builds(origin: &Origin, limit: usize) -> Vec<SnapshotBuild> {
    let list: ListManifest = match fetch_json(&origin.join("rootfs/manifest.json")) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };
    newest_first(list.directories, limit)
        .into_iter()
        .map(|d| SnapshotBuild {
            id: d.name.clone(),
            label: snapshot_label(&d.name),
            mtime: d.mtime.unwrap_or_default(),
            source: origin.source(),
            base_location: origin.join(&format!("rootfs/{}", d.name)),
            build_number: None,
            profiles: Vec::new(),
            home_pack: None,
            loaded: false,
            details: None,
        })
        .collect()
}

/// Fetch a build's `manifest.json` and return its `build` + `sourcestamps`
/// sections (the build metadata and the source revisions that went into it).
pub fn load_details(manifest_location: &str) -> Result<BuildDetails, String> {
    fetch_json(manifest_location)
}

/// Parsed contents of a build manifest: build number, per-profile packs, and the
/// optional shared `/home` seed pack.
pub type BuildContents = (Option<u64>, Vec<ProfilePack>, Option<PackFile>);

/// Fetch a build's manifest and extract its per-profile packs plus the optional
/// shared `/home` seed pack.
pub fn load_profiles(build: &SnapshotBuild) -> Result<BuildContents, String> {
    let manifest_loc = format!("{}manifest.json", build.base_location);
    let bm: BuildManifest = fetch_json(&manifest_loc)?;

    let mut profiles: Vec<ProfilePack> = Vec::new();
    let mut home_pack: Option<PackFile> = None;
    for f in &bm.files {
        let fname = f.path.rsplit('/').next().unwrap_or(&f.path);
        let pack = PackFile {
            location: format!("{}{}", build.base_location, fname),
            source: build.source.clone(),
            size_bytes: f.size,
            sha256: f.digest(),
        };
        // The shared /home seed (`home_<build>_pack.zst`) is not a profile.
        if parse_home_pack(fname).is_some() {
            home_pack = Some(pack);
            continue;
        }
        let Some((name, build_num, is_inc)) = parse_pack(fname) else {
            continue;
        };
        let entry = match profiles.iter_mut().find(|p| p.name == name) {
            Some(p) => p,
            None => {
                profiles.push(ProfilePack {
                    name: name.clone(),
                    build: build_num.clone(),
                    full: None,
                    incremental: None,
                });
                profiles.last_mut().unwrap()
            }
        };
        if is_inc {
            entry.incremental = Some(pack);
        } else {
            entry.full = Some(pack);
        }
    }

    // Minimal first, then the rest alphabetically.
    profiles.sort_by(|a, b| {
        b.is_minimal()
            .cmp(&a.is_minimal())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok((bm.build.number, profiles, home_pack))
}

// --- manifest schema -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListManifest {
    #[serde(default)]
    directories: Vec<DirEntry>,
}

#[derive(Debug, Deserialize)]
struct DirEntry {
    name: String,
    #[serde(default)]
    mtime: Option<String>,
}

/// A build's `manifest.json`. `build` and `sourcestamps` are the same sections
/// [`BuildDetails`] exposes to the details popup, so one fetch serves both.
#[derive(Debug, Deserialize)]
struct BuildManifest {
    #[serde(default)]
    build: crate::core::model::BuildMeta,
    #[serde(default)]
    sourcestamps: Vec<crate::core::model::SourceStamp>,
    #[serde(default)]
    files: Vec<FileEntry>,
}

impl BuildManifest {
    fn details(&self) -> BuildDetails {
        BuildDetails {
            build: self.build.clone(),
            sourcestamps: self.sourcestamps.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    path: String,
    #[serde(default)]
    size: u64,
    /// SHA-256 of the file as stored. Current builds publish one; older builds
    /// predate the field, hence `Option`.
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    mtime: String,
}

impl FileEntry {
    /// The digest, treating an empty string the same as an absent field.
    fn digest(&self) -> Option<String> {
        self.sha256
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

// --- helpers ---------------------------------------------------------------

/// Sort directory entries by mtime descending and cap the count.
fn newest_first(mut dirs: Vec<DirEntry>, limit: usize) -> Vec<DirEntry> {
    dirs.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    dirs.truncate(limit);
    dirs
}

fn uboot_label(dir: &str) -> String {
    let first = dir.split('/').next().unwrap_or(dir);
    let hash = first.strip_prefix("u=").unwrap_or(first);
    format!("u-boot {}", short(hash))
}

fn snapshot_label(dir: &str) -> String {
    for seg in dir.trim_end_matches('/').split("__") {
        if let Some(h) = seg.strip_prefix("linux-mainline=") {
            return format!("rootfs ml:{}", short(h));
        }
    }
    "rootfs".to_string()
}

fn short(hash: &str) -> String {
    hash.chars().take(7).collect()
}

/// Parse a pack filename into `(profile, build, is_incremental)`.
/// Accepts `<Profile>_<build>_stock_pack.zst` and `..._stock_inc_pack.zst`.
///
/// Shared with [`crate::core::bundle`], which walks a bundle manifest's file
/// list rather than a rootfs build's.
pub(crate) fn parse_pack(fname: &str) -> Option<(String, String, bool)> {
    let stem = fname.strip_suffix(".zst")?;
    let (rest, is_inc) = if let Some(r) = stem.strip_suffix("_stock_inc_pack") {
        (r, true)
    } else if let Some(r) = stem.strip_suffix("_stock_pack") {
        (r, false)
    } else {
        return None;
    };
    // `rest` is `<Profile>_<build>`; strip the trailing `_<digits>`.
    let idx = rest.rfind('_')?;
    let (name, num) = rest.split_at(idx);
    let num = &num[1..];
    if name.is_empty() || num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((name.to_string(), num.to_string(), is_inc))
}

/// Parse a shared `/home` seed filename `home_<build>_pack.zst`, returning the
/// build number. This is a full `btrfs send` of `@home` (version-independent,
/// no incremental), distinct from the per-profile `_stock` packs.
pub(crate) fn parse_home_pack(fname: &str) -> Option<String> {
    let num = fname
        .strip_suffix(".zst")?
        .strip_prefix("home_")?
        .strip_suffix("_pack")?;
    if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
        Some(num.to_string())
    } else {
        None
    }
}

/// Read a (small) manifest from an HTTP URL or a local path. Thin alias for
/// [`fetch::json`], kept so the call sites here read as before.
fn fetch_json<T: serde::de::DeserializeOwned>(location: &str) -> Result<T, String> {
    fetch::json(location)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A U-Boot build directory as the image server publishes one: per-board
    /// subdirectories holding the bootloader and the boot menu beside it.
    const BUILD_MANIFEST: &str = r#"{
      "build": { "builder": "uboot", "number": 570 },
      "files": [
        { "path": "flipper-one/idbloader.img", "size": 241664, "sha256": "aa" },
        { "path": "flipper-one/u-boot-rockchip.bin", "size": 9961984,
          "mtime": "2026-09-04T10:29:01Z", "sha256": "bb" },
        { "path": "flipper-one/bootmenu-falcon.itb", "size": 40766464,
          "mtime": "2026-09-04T10:29:03Z", "sha256": "cc" },
        { "path": "generic/u-boot-rockchip.bin", "size": 9441280, "sha256": "dd" }
      ]
    }"#;

    fn uboot(board_dir: &str) -> UbootBuild {
        let base = "https://images.invalid/u-boot/u=abc/";
        UbootBuild {
            id: "u=abc/".into(),
            label: "abc".into(),
            mtime: String::new(),
            image_location: format!("{base}{board_dir}/u-boot-rockchip.bin"),
            manifest_location: format!("{base}manifest.json"),
            source: Source::Server,
            size_bytes: 0,
            sha256: None,
            details: None,
            boot_menu: None,
            loaded: false,
        }
    }

    fn manifest() -> BuildManifest {
        serde_json::from_str(BUILD_MANIFEST).expect("valid build manifest")
    }

    #[test]
    fn reads_the_boot_menu_beside_the_bootloader() {
        let build = uboot("flipper-one");
        let c = uboot_contents(&manifest(), &build, "flipper-one").unwrap();
        assert_eq!(c.size, 9961984);
        assert_eq!(c.sha256.as_deref(), Some("bb"));

        let menu = c.boot_menu.expect("boot menu");
        assert_eq!(
            menu.location,
            "https://images.invalid/u-boot/u=abc/flipper-one/bootmenu-falcon.itb"
        );
        assert_eq!(menu.size_bytes, 40766464);
        assert_eq!(menu.sha256.as_deref(), Some("cc"));
    }

    #[test]
    fn a_build_without_a_boot_menu_still_loads() {
        // Builds predating the boot menu ship only the bootloader, and stay
        // installable: the menu is optional in a way the bootloader is not.
        let build = uboot("generic");
        let c = uboot_contents(&manifest(), &build, "generic").unwrap();
        assert_eq!(c.size, 9441280);
        assert!(c.boot_menu.is_none());
    }

    #[test]
    fn a_build_without_this_board_is_an_error() {
        let build = uboot("nanopi-m5");
        let err = uboot_contents(&manifest(), &build, "nanopi-m5").unwrap_err();
        assert!(err.contains("lists no nanopi-m5/u-boot-rockchip.bin"), "{err}");
    }

    #[test]
    fn names_a_file_beside_another() {
        assert_eq!(sibling_of("https://x.invalid/a/b/one.bin", "two.itb"), "https://x.invalid/a/b/two.itb");
        assert_eq!(sibling_of("/mnt/sd/one.bin", "two.itb"), "/mnt/sd/two.itb");
        assert_eq!(sibling_of("one.bin", "two.itb"), "two.itb");
    }

    #[test]
    fn parses_pack_names() {
        assert_eq!(parse_pack("Minimal_688_stock_pack.zst"), Some(("Minimal".to_string(), "688".to_string(), false)));
        assert_eq!(parse_pack("Desktop_688_stock_inc_pack.zst"), Some(("Desktop".to_string(), "688".to_string(), true)));
        assert_eq!(parse_pack("TV-Media-Box_688_stock_inc_pack.zst"), Some(("TV-Media-Box".to_string(), "688".to_string(), true)));
        assert_eq!(parse_pack("No-Graphics_688_stock_pack.zst"), Some(("No-Graphics".to_string(), "688".to_string(), false)));
        assert_eq!(parse_pack("debian-rootfs.img.zst"), None);
        assert_eq!(parse_pack("debian-rootfs.img.bmap"), None);
    }

    #[test]
    fn parses_home_pack_names() {
        assert_eq!(parse_home_pack("home_688_pack.zst"), Some("688".to_string()));
        // Not a home seed: profile packs, missing build number, wrong suffix.
        assert_eq!(parse_home_pack("home_pack.zst"), None);
        assert_eq!(parse_home_pack("home_688_stock_pack.zst"), None);
        assert_eq!(parse_home_pack("Minimal_688_stock_pack.zst"), None);
        // A profile literally named "home" still parses as a profile, not a seed.
        assert_eq!(parse_pack("home_688_stock_pack.zst"), Some(("home".to_string(), "688".to_string(), false)));
    }
}
