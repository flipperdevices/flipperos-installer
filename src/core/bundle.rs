//! Update bundles: one artifact set that pins a U-Boot build and a rootfs build
//! together, described by a `manifest.json` with a digest for every file.
//!
//! Layout in the bucket:
//! ```text
//! <prefix>/<channel>/<build>/manifest.json      channel = release | testing | nightly
//! <prefix>/dev/<user>/<branch>/<build>/…        same per-build contents
//! ```
//! and inside a build directory:
//! ```text
//! u-boot/<board>/u-boot-rockchip.bin
//! boot-menu/<board>/bootmenu-falcon.itb        UFS targets only
//! profile-packs/<Profile>_<build>_stock[_inc]_pack.zst
//! profile-packs/home_<build>_pack.zst
//! mcu/…                                        not installed by this tool
//! <name>-<version>-<channel>-<n>.tar.zst        the same tree as one archive
//! ```
//!
//! Browsing costs one request per level: the object store lists sub-prefixes, and
//! a build directory is recognised by holding a `manifest.json`. Nothing here
//! parses a build number out of a directory name — the name is the label, and the
//! real build metadata comes from the manifest once a bundle is selected.
//!
//! A bundle resolves to exactly one [`UbootBuild`] and one fully-loaded
//! [`SnapshotBuild`], so the install pipeline consumes it the same way it
//! consumes a hand-picked pair.

use serde::Deserialize;

use crate::core::model::{
    BootMenu, BuildDetails, BuildMeta, BundleLocation, BundleRef, PackFile, ProfilePack,
    SelectedBundle, SnapshotBuild, Source, SourceStamp, UbootBuild,
};
use crate::core::{archive, catalog, fetch};

pub type Result<T> = std::result::Result<T, String>;

/// The flashable bootloader inside a bundle's per-board U-Boot directory.
const UBOOT_IMAGE: &str = "u-boot-rockchip.bin";
/// Directory inside a bundle that holds the per-board boot menu images.
const BOOT_MENU_DIR: &str = "boot-menu";
/// The Falcon-mode boot menu FIT inside a bundle's per-board boot-menu directory.
const BOOT_MENU_IMAGE: &str = "bootmenu-falcon.itb";
/// Directory inside a bundle that holds the profile packs and the `/home` seed.
const PACKS_DIR: &str = "profile-packs";
/// Channel that holds per-developer topic branches rather than build dirs.
pub const DEV_CHANNEL: &str = "dev";
/// How deep the `dev/<user>/<branch>` tree goes before its children are builds.
const DEV_DEPTH: usize = 3;

/// Where bundles are listed and downloaded from.
#[derive(Clone, Debug)]
pub struct Repo {
    /// Object-listing endpoint, e.g.
    /// `https://storage.googleapis.com/storage/v1/b/<bucket>/o`. The public
    /// object host serves files but cannot list directories, which is why this is
    /// separate from [`Self::base_url`].
    pub list_url: String,
    /// Base URL objects are served from, without a trailing slash.
    pub base_url: String,
    /// Prefix inside the bucket holding the channels, without slashes.
    pub prefix: String,
}

impl Repo {
    /// Full object prefix of a path below the bundle root (no trailing slash).
    fn prefix_of(&self, path: &str) -> String {
        let path = path.trim_matches('/');
        if path.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{path}", self.prefix)
        }
    }

    /// URL a path below the bundle root is served from.
    fn url_of(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, self.prefix_of(path))
    }
}

// --- listing ---------------------------------------------------------------

/// A page of an object listing. A prefix with no children yields `{}` — neither
/// key present — so both must default.
#[derive(Debug, Default, Deserialize)]
struct ListPage {
    #[serde(default)]
    prefixes: Vec<String>,
    #[serde(default)]
    items: Vec<ListItem>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListItem {
    name: String,
}

/// What a listing found directly under one prefix.
#[derive(Debug, Default)]
struct Children {
    /// Names of the immediate sub-directories.
    dirs: Vec<String>,
    /// Names of the immediate files.
    files: Vec<String>,
}

impl Children {
    /// A directory holding a `manifest.json` is a bundle build directory.
    fn is_build_dir(&self) -> bool {
        self.files.iter().any(|f| f == "manifest.json")
    }
}

/// Percent-encode the characters that matter in a query parameter.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// List the immediate children of a path below the bundle root.
fn children(repo: &Repo, path: &str) -> Result<Children> {
    let prefix = format!("{}/", repo.prefix_of(path));
    let mut out = Children::default();
    let mut token: Option<String> = None;

    loop {
        let mut url = format!(
            "{}?prefix={}&delimiter=%2F&fields=prefixes,items(name),nextPageToken",
            repo.list_url,
            encode(&prefix)
        );
        if let Some(t) = &token {
            url.push_str(&format!("&pageToken={}", encode(t)));
        }
        let page: ListPage = fetch::json(&url)?;

        for p in &page.prefixes {
            if let Some(name) = p
                .strip_prefix(&prefix)
                .map(|s| s.trim_end_matches('/'))
                .filter(|s| !s.is_empty())
            {
                out.dirs.push(name.to_string());
            }
        }
        for item in &page.items {
            if let Some(name) = item
                .name
                .strip_prefix(&prefix)
                .filter(|s| !s.is_empty() && !s.contains('/'))
            {
                out.files.push(name.to_string());
            }
        }

        match page.next_page_token {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(out)
}

/// The channels published in the bucket, in listing order.
pub fn channels(repo: &Repo) -> Result<Vec<String>> {
    Ok(children(repo, "")?.dirs)
}

/// Sub-directory names of an intermediate level of the `dev/` tree (usernames
/// under `dev`, branch names under `dev/<user>`), newest-looking first.
pub fn dirs(repo: &Repo, path: &str) -> Result<Vec<String>> {
    let mut names = children(repo, path)?.dirs;
    names.sort_by(|a, b| b.cmp(a));
    Ok(names)
}

/// The bundle builds under a channel path (`nightly`, `dev/alchark/topic`),
/// newest first.
///
/// Ordering is by directory name, descending. The trailing build number in a
/// directory name is deliberately *not* parsed: it is not part of the published
/// contract, so nothing here depends on it.
pub fn builds(repo: &Repo, path: &str) -> Result<Vec<BundleRef>> {
    let listing = children(repo, path)?;
    let mut out: Vec<BundleRef> = listing
        .dirs
        .iter()
        .map(|dir| remote_ref(repo, path, dir))
        .collect();
    out.sort_by(|a, b| b.dir.cmp(&a.dir));
    Ok(out)
}

fn remote_ref(repo: &Repo, channel: &str, dir: &str) -> BundleRef {
    let channel = channel.trim_matches('/').to_string();
    BundleRef {
        id: format!("{channel}/{dir}"),
        location: BundleLocation::Remote {
            base: format!("{}/", repo.url_of(&format!("{channel}/{dir}"))),
        },
        channel,
        dir: dir.to_string(),
        source: Source::Server,
        archive: None,
    }
}

/// Whether a path inside the `dev/` tree is deep enough that its children are
/// build directories rather than another level of names.
///
/// A structural shortcut that keeps browsing to one request per level; use
/// [`level_holds_builds`] to ask the bucket instead when the layout is unknown.
pub fn is_dev_leaf(path: &str) -> bool {
    path.trim_matches('/').split('/').count() >= DEV_DEPTH
}

/// Ask the bucket whether the children of `path` are build directories.
///
/// Costs one extra listing, and is only used when drilling in, so browsing never
/// walks the whole `dev/` tree.
pub fn level_holds_builds(repo: &Repo, path: &str) -> Result<bool> {
    let listing = children(repo, path)?;
    match listing.dirs.first() {
        None => Ok(true),
        Some(first) => Ok(children(repo, &format!("{path}/{first}"))?.is_build_dir()),
    }
}

// --- manifest --------------------------------------------------------------

/// A bundle's `manifest.json`.
#[derive(Debug, Default, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub bundle: Meta,
    #[serde(default)]
    pub sourcestamps: Vec<SourceStamp>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// The manifest's `bundle` section.
#[derive(Debug, Default, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub description: String,
    /// Filename of the bundle's own archive.
    #[serde(default)]
    pub archive: String,
    /// Build that produced the bundle. Note this is nested *here* rather than at
    /// the manifest's top level, which is why [`BuildDetails`] must never be
    /// deserialized straight from a bundle manifest: all of its fields default,
    /// so it would silently yield an empty build instead of an error.
    #[serde(default)]
    pub build: BuildMeta,
}

/// One file in a bundle, with the digest of its stored (compressed) bytes.
#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: String,
    #[serde(default)]
    pub sha256: Option<String>,
    /// Which bundle item the file came from (`u-boot`, `profile-packs`, …).
    #[serde(default)]
    pub item: String,
}

impl FileEntry {
    fn digest(&self) -> Option<String> {
        self.sha256
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

impl Manifest {
    /// The build metadata and source revisions, for the details popup. Built
    /// explicitly from the nested `bundle.build` — see [`Meta::build`].
    pub fn details(&self) -> BuildDetails {
        BuildDetails {
            build: self.bundle.build.clone(),
            sourcestamps: self.sourcestamps.clone(),
        }
    }

    /// Board ids this bundle ships a flashable U-Boot image for, sorted.
    pub fn device_types(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in &self.files {
            let Some(dir) = f.path.strip_suffix(&format!("/{UBOOT_IMAGE}")) else {
                continue;
            };
            let Some(board) = dir.strip_prefix("u-boot/") else {
                continue;
            };
            if !board.is_empty() && !board.contains('/') && !out.iter().any(|b| b == board) {
                out.push(board.to_string());
            }
        }
        out.sort();
        out
    }

    /// The per-board U-Boot directory to install from: the detected board when
    /// this bundle ships it, else the generic build.
    pub fn board_dir(&self, board_id: &str) -> String {
        if self.device_types().iter().any(|t| t == board_id) {
            board_id.to_string()
        } else {
            catalog::board_dir(board_id).to_string()
        }
    }
}

/// Read a bundle's manifest, from the update server or a local directory.
pub fn read_manifest(location: &BundleLocation) -> Result<Manifest> {
    fetch::json(&location.manifest())
}

/// Resolve a bundle into the pinned U-Boot image and the fully-loaded snapshot
/// build the install pipeline consumes.
pub fn resolve(
    reference: &BundleRef,
    location: &BundleLocation,
    manifest: &Manifest,
    board_id: &str,
) -> Result<SelectedBundle> {
    let board_dir = manifest.board_dir(board_id);
    let details = manifest.details();
    let source = match location {
        BundleLocation::Remote { .. } => Source::Server,
        BundleLocation::Dir { root } => Source::Local { root: root.clone() },
    };

    // U-Boot: exactly one image, for the board we resolved.
    let image_rel = format!("u-boot/{board_dir}/{UBOOT_IMAGE}");
    let image = manifest
        .files
        .iter()
        .find(|f| f.path == image_rel)
        .ok_or_else(|| {
            format!(
                "bundle {} ships no {image_rel} (it has: {})",
                reference.dir,
                manifest.device_types().join(", ")
            )
        })?;
    // The Falcon boot menu for the same board, when the bundle ships one. Bundles
    // predating it simply have no such file, and installing from them still works
    // — the boot menu is only ever written on UFS, where its absence costs a
    // graphical menu rather than a boot.
    let menu_rel = format!("{BOOT_MENU_DIR}/{board_dir}/{BOOT_MENU_IMAGE}");
    let boot_menu = manifest
        .files
        .iter()
        .find(|f| f.path == menu_rel)
        .map(|f| BootMenu {
            location: location.join(&menu_rel),
            source: source.clone(),
            size_bytes: f.size,
            sha256: f.digest(),
        });

    let uboot = UbootBuild {
        id: reference.id.clone(),
        label: format!("u-boot {}", manifest.bundle.version),
        mtime: image.mtime.clone(),
        image_location: location.join(&image_rel),
        manifest_location: location.manifest(),
        source: source.clone(),
        size_bytes: image.size,
        sha256: image.digest(),
        details: Some(details.clone()),
        boot_menu,
        loaded: true,
    };

    // Profile packs and the shared /home seed. Unlike a rootfs build's manifest,
    // the paths here are nested, so each location keeps its full relative path.
    let mut profiles: Vec<ProfilePack> = Vec::new();
    let mut home_pack: Option<PackFile> = None;
    for f in &manifest.files {
        let Some(fname) = f.path.strip_prefix(&format!("{PACKS_DIR}/")) else {
            continue;
        };
        if fname.contains('/') {
            continue;
        }
        let pack = PackFile {
            location: location.join(&f.path),
            source: source.clone(),
            size_bytes: f.size,
            sha256: f.digest(),
        };
        if catalog::parse_home_pack(fname).is_some() {
            home_pack = Some(pack);
            continue;
        }
        let Some((name, build_num, is_inc)) = catalog::parse_pack(fname) else {
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
    // Minimal first, then the rest alphabetically — the order the profile list
    // uses for a rootfs build.
    profiles.sort_by(|a, b| {
        b.is_minimal()
            .cmp(&a.is_minimal())
            .then_with(|| a.name.cmp(&b.name))
    });
    if !profiles.iter().any(|p| p.is_minimal()) {
        return Err(format!(
            "bundle {} ships no Minimal profile pack",
            reference.dir
        ));
    }

    let build = SnapshotBuild {
        id: reference.id.clone(),
        label: format!("rootfs {}", manifest.bundle.version),
        mtime: manifest.bundle.build.timestamp.clone(),
        source,
        base_location: location.join(""),
        build_number: manifest.bundle.build.number,
        profiles,
        home_pack,
        loaded: true,
        details: Some(details),
    };

    Ok(SelectedBundle {
        reference: reference.clone(),
        version: manifest.bundle.version.clone(),
        channel: if manifest.bundle.channel.is_empty() {
            reference.channel.clone()
        } else {
            manifest.bundle.channel.clone()
        },
        description: manifest.bundle.description.clone(),
        archive: manifest.bundle.archive.clone(),
        board_dir,
        device_types: manifest.device_types(),
        uboot,
        build,
    })
}

/// Read and resolve a bundle in one step.
pub fn load(reference: &BundleRef, board_id: &str) -> Result<SelectedBundle> {
    let manifest = match &reference.archive {
        Some(path) => {
            let bytes = archive::read_manifest(path)?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse manifest in {path}: {e}"))?
        }
        None => read_manifest(&reference.location)?,
    };
    if manifest.schema != 1 {
        return Err(format!(
            "bundle {} declares manifest schema {}, which this installer does not understand",
            reference.dir, manifest.schema
        ));
    }
    resolve(reference, &reference.location, &manifest, board_id)
}

/// Where an archive-backed bundle is unpacked to: a directory named after the
/// archive, inside the scratch dir.
///
/// Chosen when the archive is first listed, so a bundle resolved from it already
/// describes its files at the paths they will occupy once unpacked.
pub fn unpack_dest(cache_dir: &str, archive_path: &str) -> String {
    let stem = archive_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(archive_path)
        .trim_end_matches(archive::ARCHIVE_SUFFIX);
    format!("{}/{stem}", cache_dir.trim_end_matches('/'))
}

/// Unpack an archive-backed bundle into the directory its reference already
/// points at. A no-op for a bundle that is not an archive.
///
/// Returns the number of bytes written.
pub fn unpack(reference: &BundleRef, on_progress: &mut dyn FnMut(u64)) -> Result<u64> {
    let Some(path) = &reference.archive else {
        return Ok(0);
    };
    let BundleLocation::Dir { root } = &reference.location else {
        return Err(format!(
            "{path}: an archive bundle must unpack to a directory"
        ));
    };
    let written = archive::unpack(path, root, on_progress)?;
    Ok(written)
}

// --- local discovery -------------------------------------------------------

/// Find bundles on mounted removable media and at explicitly given paths.
///
/// Probed per root, in order: an unpacked bundle at the root itself, unpacked
/// bundles laid out like the bucket (`bundles/<channel>/<build>/`), and any
/// `*.tar.zst`. Validating an archive is cheap: a bundle archive stores its
/// manifest as the first member, so only the head has to be decompressed.
pub fn discover_local(
    paths: &[String],
    media: &[(String, String)],
    cache_dir: &str,
) -> Vec<BundleRef> {
    let mut out: Vec<BundleRef> = Vec::new();

    for path in paths {
        if path.ends_with(archive::ARCHIVE_SUFFIX) {
            if let Some(r) = archive_ref(path, cache_dir) {
                out.push(r);
            }
        } else if let Some(r) = dir_ref(path) {
            out.push(r);
        }
    }

    for (device, root) in media {
        let label = format!("media {device}");
        if let Some(r) = dir_ref(root) {
            out.push(with_channel(r, &label));
        }
        // The bucket layout, mirrored onto a card.
        let bundles_root = format!("{}/bundles", root.trim_end_matches('/'));
        for channel in read_dirs(&bundles_root) {
            for build in read_dirs(&format!("{bundles_root}/{channel}")) {
                if let Some(r) = dir_ref(&format!("{bundles_root}/{channel}/{build}")) {
                    out.push(with_channel(r, &label));
                }
            }
        }
        for file in read_files(root) {
            if file.ends_with(archive::ARCHIVE_SUFFIX) {
                let path = format!("{}/{file}", root.trim_end_matches('/'));
                if let Some(r) = archive_ref(&path, cache_dir) {
                    out.push(with_channel(r, &label));
                }
            }
        }
    }

    out.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

fn with_channel(mut r: BundleRef, channel: &str) -> BundleRef {
    r.channel = channel.to_string();
    r
}

/// A reference to an unpacked bundle directory, if it holds a bundle manifest.
fn dir_ref(root: &str) -> Option<BundleRef> {
    let root = root.trim_end_matches('/').to_string();
    let location = BundleLocation::Dir { root: root.clone() };
    let manifest: Manifest = fetch::json(&location.manifest()).ok()?;
    if manifest.bundle.name.is_empty() {
        return None;
    }
    Some(BundleRef {
        id: root.clone(),
        channel: "local".to_string(),
        dir: local_label(&manifest, &root),
        location,
        source: Source::Local { root },
        archive: None,
    })
}

/// A reference to a `*.tar.zst` bundle, if its head holds a bundle manifest.
///
/// The location already points at the scratch directory the archive will be
/// unpacked into, so the bundle resolved from it describes its files where they
/// will actually be. `archive` being set is what records that the unpacking is
/// still owed.
fn archive_ref(path: &str, cache_dir: &str) -> Option<BundleRef> {
    let bytes = archive::read_manifest(path).ok()?;
    let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
    if manifest.bundle.name.is_empty() {
        return None;
    }
    let root = unpack_dest(cache_dir, path);
    Some(BundleRef {
        id: path.to_string(),
        channel: "local".to_string(),
        dir: local_label(&manifest, path),
        location: BundleLocation::Dir { root: root.clone() },
        source: Source::Local { root },
        archive: Some(path.to_string()),
    })
}

/// Label a local bundle by what its manifest says rather than where it sits — a
/// copied bundle can live anywhere.
fn local_label(manifest: &Manifest, fallback: &str) -> String {
    let version = &manifest.bundle.version;
    if version.is_empty() {
        return fallback
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(fallback)
            .to_string();
    }
    match manifest.bundle.build.number {
        Some(n) => format!("{version}-{n}"),
        None => version.clone(),
    }
}

fn read_dirs(path: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn read_files(path: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `manifest.json` from the nightly channel, trimmed to four boards
    /// and three sourcestamps. Sizes and digests are the published ones.
    const MANIFEST: &str = include_str!("testdata/bundle-manifest.json");

    fn repo() -> Repo {
        Repo {
            list_url: "https://storage.invalid/storage/v1/b/bkt/o".to_string(),
            base_url: "https://update.invalid".to_string(),
            prefix: "bundles".to_string(),
        }
    }

    fn parse(json: &str) -> Manifest {
        serde_json::from_str(json).expect("manifest parses")
    }

    #[test]
    fn empty_prefix_listing_has_no_keys() {
        // A prefix with no children really does come back as `{}`.
        let page: ListPage = serde_json::from_str("{}").unwrap();
        assert!(page.prefixes.is_empty());
        assert!(page.items.is_empty());
        assert!(page.next_page_token.is_none());
    }

    #[test]
    fn parses_a_listing_page() {
        let page: ListPage = serde_json::from_str(
            r#"{"kind":"storage#objects",
                "prefixes":["bundles/nightly/20260812-83ddb68-15/"],
                "items":[{"name":"bundles/nightly/manifest.json","size":"1"}],
                "nextPageToken":"tok"}"#,
        )
        .unwrap();
        assert_eq!(page.prefixes.len(), 1);
        assert_eq!(page.items[0].name, "bundles/nightly/manifest.json");
        assert_eq!(page.next_page_token.as_deref(), Some("tok"));
    }

    #[test]
    fn recognises_a_build_directory_by_its_manifest() {
        let build = Children {
            dirs: vec!["u-boot".into(), "profile-packs".into()],
            files: vec!["manifest.json".into(), "SHA256SUMS".into()],
        };
        assert!(build.is_build_dir());
        let level = Children {
            dirs: vec!["alchark".into()],
            files: vec![],
        };
        assert!(!level.is_build_dir());
    }

    #[test]
    fn sorts_builds_by_name_descending() {
        let repo = repo();
        let mut refs: Vec<BundleRef> = ["20260807-83ddb68-8", "20260812-83ddb68-15", "83ddb68-7"]
            .iter()
            .map(|d| remote_ref(&repo, "nightly", d))
            .collect();
        refs.sort_by(|a, b| b.dir.cmp(&a.dir));
        let names: Vec<&str> = refs.iter().map(|r| r.dir.as_str()).collect();
        // Plain name ordering, so no build number is parsed out of the name. An
        // undated directory therefore sorts above the dated ones.
        assert_eq!(
            names,
            ["83ddb68-7", "20260812-83ddb68-15", "20260807-83ddb68-8"]
        );
    }

    #[test]
    fn builds_urls_from_the_bucket_prefix() {
        let repo = repo();
        let r = remote_ref(&repo, "nightly", "20260812-83ddb68-15");
        assert_eq!(r.id, "nightly/20260812-83ddb68-15");
        assert_eq!(
            r.location.manifest(),
            "https://update.invalid/bundles/nightly/20260812-83ddb68-15/manifest.json"
        );
        // A dev build sits deeper, but is addressed the same way.
        let dev = remote_ref(&repo, "dev/alchark/topic", "20260812-abc1234-3");
        assert_eq!(dev.id, "dev/alchark/topic/20260812-abc1234-3");
        assert_eq!(
            dev.location.join("profile-packs/x.zst"),
            "https://update.invalid/bundles/dev/alchark/topic/20260812-abc1234-3/profile-packs/x.zst"
        );
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("bundles/dev/"), "bundles%2Fdev%2F");
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn resolves_a_manifest_into_a_pinned_pair() {
        let manifest = parse(MANIFEST);
        let repo = repo();
        let reference = remote_ref(&repo, "nightly", "20260812-83ddb68-15");
        let bundle = resolve(&reference, &reference.location, &manifest, "flipper-one").unwrap();

        // U-Boot: the right board's image, with its published size and digest.
        assert_eq!(bundle.board_dir, "flipper-one");
        assert!(bundle
            .uboot
            .image_location
            .ends_with("/u-boot/flipper-one/u-boot-rockchip.bin"));
        assert_eq!(bundle.uboot.size_bytes, 9944064);
        assert_eq!(
            bundle.uboot.sha256.as_deref(),
            Some("4c383d5b2c89245de80d7f3f1c84f0db7ee79995eebebf12e6479016f71255b5")
        );
        // The boot menu comes from the same board's boot-menu directory.
        let menu = bundle.uboot.boot_menu.as_ref().expect("boot menu");
        assert!(
            menu.location
                .ends_with("/boot-menu/flipper-one/bootmenu-falcon.itb"),
            "{}",
            menu.location
        );
        assert_eq!(menu.size_bytes, 40765440);
        assert_eq!(
            menu.sha256.as_deref(),
            Some("495f4fa9ee07deed190d353910671816b359e649c332917383eafacbeef83ead")
        );
        // Nothing further to fetch for either build.
        assert!(bundle.uboot.loaded);
        assert!(bundle.build.loaded);

        // Profiles: Minimal first, then alphabetical.
        let names: Vec<&str> = bundle
            .build
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["Minimal", "Desktop", "No-Graphics", "Router", "TV-Media-Box"]
        );

        // The profile build number comes from the pack filename (the rootfs
        // build), not from the bundle's own build number.
        let minimal = bundle.build.minimal().unwrap();
        assert_eq!(minimal.build, "917");
        assert_eq!(minimal.stock_subvol(), "@Minimal_917_stock");
        assert_eq!(bundle.build.build_number, Some(15));

        let full = minimal.full.as_ref().unwrap();
        assert_eq!(full.size_bytes, 885256880);
        assert_eq!(
            full.sha256.as_deref(),
            Some("8b7dfd2b6ae80d616f339ba8f9f4b80b01a640aed16858aa0d9cdad898b0019d")
        );
        // Locations keep the nested `profile-packs/` segment.
        assert!(
            full.location
                .ends_with("/profile-packs/Minimal_917_stock_pack.zst"),
            "{}",
            full.location
        );
        // Every pack carries a digest, and the extras are incrementals.
        let desktop = bundle
            .build
            .profiles
            .iter()
            .find(|p| p.name == "Desktop")
            .unwrap();
        assert!(desktop.full.is_none());
        assert!(desktop.incremental.as_ref().unwrap().sha256.is_some());

        // The /home seed is a seed, not a profile called "home".
        assert!(!names.contains(&"home"));
        let home = bundle.build.home_pack.as_ref().unwrap();
        assert!(home.location.ends_with("/profile-packs/home_917_pack.zst"));

        assert_eq!(bundle.channel, "nightly");
        assert_eq!(bundle.version, "20260812-83ddb68");
        assert!(bundle.archive.ends_with(".tar.zst"));
    }

    #[test]
    fn takes_build_details_from_the_nested_bundle_section() {
        let manifest = parse(MANIFEST);
        let details = manifest.details();
        assert_eq!(details.build.number, Some(15));
        assert_eq!(details.build.builder, "update-bundle");
        assert_eq!(details.sourcestamps.len(), 3);

        // Deserializing `BuildDetails` straight from a bundle manifest does not
        // fail — every field defaults — it silently loses the build. Which is
        // exactly why `details()` exists.
        let naive: BuildDetails = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(naive.build.number, None);
    }

    #[test]
    fn lists_device_types_and_falls_back_to_generic() {
        let manifest = parse(MANIFEST);
        assert_eq!(
            manifest.device_types(),
            ["evb", "flipper-one", "generic", "nanopi-m5"]
        );
        assert_eq!(manifest.board_dir("nanopi-m5"), "nanopi-m5");
        // An unknown board gets the generic bootloader.
        assert_eq!(manifest.board_dir("some-new-board"), "generic");
    }

    #[test]
    fn missing_board_image_is_an_error() {
        let mut manifest = parse(MANIFEST);
        manifest
            .files
            .retain(|f| !f.path.starts_with("u-boot/generic/"));
        let repo = repo();
        let reference = remote_ref(&repo, "nightly", "x");
        let err =
            resolve(&reference, &reference.location, &manifest, "unknown-board").unwrap_err();
        assert!(err.contains("ships no u-boot/generic/"), "{err}");
    }

    #[test]
    fn a_bundle_without_a_boot_menu_still_resolves() {
        // Bundles predating the Falcon boot menu ship no `boot-menu/` tree. They
        // are still installable — the menu is optional, unlike the bootloader.
        let mut manifest = parse(MANIFEST);
        manifest.files.retain(|f| !f.path.starts_with("boot-menu/"));
        let repo = repo();
        let reference = remote_ref(&repo, "nightly", "x");
        let bundle = resolve(&reference, &reference.location, &manifest, "flipper-one").unwrap();
        assert!(bundle.uboot.boot_menu.is_none());
    }

    #[test]
    fn a_board_without_a_boot_menu_takes_none_of_anothers() {
        // The lookup is pinned to the resolved board directory, so a board that
        // ships a bootloader but no menu must not pick up a neighbour's.
        let mut manifest = parse(MANIFEST);
        manifest
            .files
            .retain(|f| !f.path.starts_with("boot-menu/flipper-one/"));
        let repo = repo();
        let reference = remote_ref(&repo, "nightly", "x");
        let bundle = resolve(&reference, &reference.location, &manifest, "flipper-one").unwrap();
        assert!(bundle.uboot.boot_menu.is_none());
    }

    #[test]
    fn missing_minimal_pack_is_an_error() {
        let mut manifest = parse(MANIFEST);
        manifest.files.retain(|f| !f.path.contains("Minimal"));
        let repo = repo();
        let reference = remote_ref(&repo, "nightly", "x");
        let err = resolve(&reference, &reference.location, &manifest, "flipper-one").unwrap_err();
        assert!(err.contains("no Minimal profile pack"), "{err}");
    }

    #[test]
    fn resolves_from_a_local_directory() {
        let manifest = parse(MANIFEST);
        let location = BundleLocation::Dir {
            root: "/mnt/sd/bundle".to_string(),
        };
        let reference = BundleRef {
            id: "/mnt/sd/bundle".into(),
            channel: "local".into(),
            dir: "20260812-83ddb68-15".into(),
            location: location.clone(),
            source: Source::Local {
                root: "/mnt/sd/bundle".into(),
            },
            archive: None,
        };
        let bundle = resolve(&reference, &location, &manifest, "flipper-one").unwrap();
        assert_eq!(
            bundle.uboot.image_location,
            "/mnt/sd/bundle/u-boot/flipper-one/u-boot-rockchip.bin"
        );
        assert_eq!(
            bundle.uboot.source,
            Source::Local {
                root: "/mnt/sd/bundle".into()
            }
        );
    }

    #[test]
    fn joins_locations_without_doubling_slashes() {
        let remote = BundleLocation::Remote {
            base: "https://update.invalid/bundles/nightly/b/".to_string(),
        };
        assert_eq!(
            remote.join("profile-packs/x.zst"),
            "https://update.invalid/bundles/nightly/b/profile-packs/x.zst"
        );
        assert_eq!(
            remote.manifest(),
            "https://update.invalid/bundles/nightly/b/manifest.json"
        );
        let dir = BundleLocation::Dir {
            root: "/mnt/sd/bundle".to_string(),
        };
        assert_eq!(dir.join("/manifest.json"), "/mnt/sd/bundle/manifest.json");
    }

    #[test]
    fn bounds_the_dev_walk() {
        assert!(!is_dev_leaf("dev"));
        assert!(!is_dev_leaf("dev/alchark"));
        assert!(is_dev_leaf("dev/alchark/topic"));
    }

    #[test]
    fn labels_a_local_bundle_from_its_manifest() {
        let manifest = parse(MANIFEST);
        assert_eq!(local_label(&manifest, "/mnt/sd/x"), "20260812-83ddb68-15");
        // No manifest version to go on: fall back to the path's last segment.
        let empty = Manifest::default();
        assert_eq!(local_label(&empty, "/mnt/sd/thing/"), "thing");
    }
}
