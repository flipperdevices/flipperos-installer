//! Image catalog: reads the two-level build listings published by the image
//! server (and any removable-media mirror of the same layout).
//!
//! Layout:
//!   `<base>/u-boot/manifest.json`            -> list of U-Boot build dirs
//!   `<base>/u-boot/<dir>/manifest.json`      -> files incl. `<board>/u-boot-rockchip.bin`
//!   `<base>/rootfs/manifest.json`            -> list of rootfs build dirs
//!   `<base>/rootfs/<dir>/manifest.json`      -> `<Profile>_<build>_stock[_inc]_pack.zst`
//!
//! `<base>` is an HTTP(S) URL for the server or a filesystem path for media.

use serde::Deserialize;

use crate::core::fetch;
use crate::core::model::{BuildDetails, PackFile, ProfilePack, SnapshotBuild, Source, UbootBuild};

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
        if let Some(dir) = f.path.strip_suffix("/u-boot-rockchip.bin") {
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
            let image_location = format!("{base_location}{board_dir}/u-boot-rockchip.bin");
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
                loaded: false,
            }
        })
        .collect()
}

/// Parsed contents of a U-Boot build manifest: the flashable image's size and
/// digest, plus the build metadata for the details popup.
pub type UbootContents = (u64, Option<String>, String, BuildDetails);

/// Fetch a U-Boot build's manifest and extract the metadata for its
/// `<board_dir>/u-boot-rockchip.bin`.
///
/// The sibling of [`load_profiles`]: both kinds of build carry a manifest, and
/// both are read exactly once through this pair, so the details popup, the
/// verification pass and the install path all see the same fields.
pub fn load_uboot_contents(build: &UbootBuild, board_dir: &str) -> Result<UbootContents, String> {
    let bm: BuildManifest = fetch_json(&build.manifest_location)?;
    let details = bm.details();
    let wanted = format!("{board_dir}/u-boot-rockchip.bin");
    let entry = bm
        .files
        .iter()
        .find(|f| f.path == wanted || f.path.ends_with(&format!("/{wanted}")));
    match entry {
        Some(f) => Ok((f.size, f.digest(), f.mtime.clone(), details)),
        // The build exists but ships nothing for this board. Report it rather
        // than silently flashing whatever the URL happens to return.
        None => Err(format!(
            "{} lists no {wanted}",
            build.manifest_location
        )),
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
    use super::{parse_home_pack, parse_pack};

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
