//! Verifying artifacts before the target is touched.
//!
//! With [`FetchMode::VerifyFirst`] every artifact the run needs is fetched into
//! the scratch directory and checked against the digest its manifest publishes
//! *before* the first destructive command. The install then reads the local
//! copies, so a truncated download or a corrupted pack cannot leave the operator
//! with a wiped device and half an installation.
//!
//! Only what the run actually needs is staged: the U-Boot image for the detected
//! board, the boot menu when the target is one that takes it, the Minimal full
//! pack, the incremental pack of each selected profile, and the `/home` seed. For
//! a bundle that means well under the size of the published archive.
//!
//! [`FetchMode::VerifyFirst`]: crate::core::model::FetchMode::VerifyFirst

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::core::controller::{Config, Controller};
use crate::core::fetch;
use crate::core::install::{apply_verdict, receive_progress, transfer_weight, OnMismatch, Ticker};
use crate::core::model::{
    human_bytes, BundleRef, ImageFile, PackFile, ProfilePack, SnapshotBuild, Source, UbootBuild,
};

pub type Result<T> = std::result::Result<T, String>;

/// Headroom left free on the scratch filesystem. It is usually a tmpfs sharing
/// RAM with everything else running from the initramfs, so filling it to the
/// last byte would take the rest of the system down with it.
const FREE_SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// One artifact the install will read.
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Human label used in progress lines and digest verdicts.
    pub label: String,
    /// Where the artifact is read from now.
    pub location: String,
    pub source: Source,
    /// Digest published for it, if any.
    pub sha256: Option<String>,
    /// Published size, or 0 when unknown.
    pub size: u64,
    /// Path under the scratch dir to stage it to, relative to that dir.
    pub rel: String,
}

/// The images a run writes as they are, outside the filesystem. A named struct
/// rather than several arguments of the same type, which nothing would stop a
/// caller transposing.
#[derive(Clone, Copy, Default)]
pub struct RawImages<'a> {
    pub boot_menu: Option<&'a ImageFile>,
    pub recovery: Option<&'a ImageFile>,
    /// Uploaded to the MCU rather than written to the target, and last.
    pub mcu_firmware: Option<&'a ImageFile>,
}

/// Everything the run will read, in install order.
///
/// An image passed here is staged. Deciding whether it is wanted at all is
/// [`crate::core::install::run`]'s job: it passes `None` for a target that has
/// nowhere to put it, so nothing here has to know what kind of device is being
/// installed onto.
pub fn plan(
    uboot: &UbootBuild,
    images: RawImages,
    build: &SnapshotBuild,
    extras: &[ProfilePack],
) -> Vec<Artifact> {
    let mut out = Vec::new();
    out.push(Artifact {
        label: "u-boot image".to_string(),
        location: uboot.image_location.clone(),
        source: uboot.source.clone(),
        sha256: uboot.sha256.clone(),
        size: uboot.size_bytes,
        rel: "u-boot-rockchip.bin".to_string(),
    });
    if let Some(menu) = images.boot_menu {
        out.push(image_artifact("boot menu image", menu));
    }
    if let Some(recovery) = images.recovery {
        out.push(image_artifact("recovery image", recovery));
    }
    if let Some(home) = &build.home_pack {
        out.push(pack_artifact("/home seed", home));
    }
    if let Some(full) = build.minimal().and_then(|m| m.full.as_ref()) {
        out.push(pack_artifact("Minimal (full)", full));
    }
    for p in extras {
        if let Some(inc) = &p.incremental {
            out.push(pack_artifact(&format!("{} (incremental)", p.name), inc));
        }
    }
    if let Some(mcu) = images.mcu_firmware {
        out.push(image_artifact("MCU firmware", mcu));
    }
    out
}

fn image_artifact(label: &str, image: &ImageFile) -> Artifact {
    Artifact {
        label: label.to_string(),
        location: image.location.clone(),
        source: image.source.clone(),
        sha256: image.sha256.clone(),
        size: image.size_bytes,
        rel: file_name(&image.location),
    }
}

fn pack_artifact(label: &str, pack: &PackFile) -> Artifact {
    Artifact {
        label: label.to_string(),
        location: pack.location.clone(),
        source: pack.source.clone(),
        sha256: pack.sha256.clone(),
        size: pack.size_bytes,
        // Keep the published filename so the staged tree reads like the source.
        rel: file_name(&pack.location),
    }
}

/// Last path segment of a URL or filesystem path.
fn file_name(location: &str) -> String {
    location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(location)
        .to_string()
}

/// Staged copies of the artifacts, and the scratch files backing them.
///
/// Dropping this removes the files it created — and only those, so a directory
/// or archive the operator passed with `--bundle` is never touched.
pub struct Staged {
    dir: PathBuf,
    /// `(original location, staged path)` for each artifact.
    map: Vec<(String, String)>,
    created: Vec<PathBuf>,
    keep: bool,
}

impl Staged {
    fn staged_path(&self, location: &str) -> Option<&str> {
        self.map
            .iter()
            .find(|(orig, _)| orig == location)
            .map(|(_, staged)| staged.as_str())
    }

    fn local_source(&self) -> Source {
        Source::Local {
            root: self.dir.to_string_lossy().into_owned(),
        }
    }

    /// Point a U-Boot build at its staged image.
    pub fn localise_uboot(&self, uboot: &mut UbootBuild) {
        if let Some(path) = self.staged_path(&uboot.image_location) {
            uboot.image_location = path.to_string();
            uboot.source = self.local_source();
        }
    }

    /// Point an image at its staged copy.
    pub fn localise_image(&self, image: &mut ImageFile) {
        if let Some(path) = self.staged_path(&image.location) {
            image.location = path.to_string();
            image.source = self.local_source();
        }
    }

    /// Point every pack of a snapshot build at its staged copy.
    pub fn localise_build(&self, build: &mut SnapshotBuild) {
        let source = self.local_source();
        let fix = |pack: &mut PackFile| {
            if let Some(path) = self.staged_path(&pack.location) {
                pack.location = path.to_string();
                pack.source = source.clone();
            }
        };
        if let Some(home) = build.home_pack.as_mut() {
            fix(home);
        }
        for p in build.profiles.iter_mut() {
            if let Some(full) = p.full.as_mut() {
                fix(full);
            }
            if let Some(inc) = p.incremental.as_mut() {
                fix(inc);
            }
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        for path in &self.created {
            let _ = fs::remove_file(path);
        }
        // Only succeeds once the directory is empty, which is what we want: a
        // scratch dir holding anything else is left alone.
        let _ = fs::remove_dir(&self.dir);
    }
}

/// Refuse to install artifacts that live on the device about to be wiped.
///
/// Reading a pack off the very disk being partitioned would fail partway through
/// and destroy the source at the same time, so this is a hard error rather than a
/// warning.
pub fn guard_not_on_target(
    uboot: &UbootBuild,
    images: RawImages,
    build: &SnapshotBuild,
    extras: &[ProfilePack],
    disk: &str,
) -> Result<()> {
    let on_target = crate::core::removable::mounts_on(disk);
    let all = crate::core::removable::mountpoints();
    check_not_on_mounts(&plan(uboot, images, build, extras), &on_target, &all, disk)
}

/// The decision [`guard_not_on_target`] makes, separated from reading
/// `/proc/mounts` so it can be exercised directly.
///
/// What holds a file is the deepest mount above it, not every mount above it:
/// a target mounted at `/` contains every path by prefix, yet a file in a
/// `tmpfs` on `/run` is nowhere near it.
fn check_not_on_mounts(
    plan: &[Artifact],
    on_target: &[String],
    all: &[String],
    disk: &str,
) -> Result<()> {
    for a in plan {
        // A remote artifact is unaffected by what the target disk holds.
        if fetch::is_url(&a.location) {
            continue;
        }
        let holder = all
            .iter()
            .chain(on_target)
            .filter(|mp| under(&a.location, mp))
            .max_by_key(|mp| mp.trim_end_matches('/').len());
        if let Some(mp) = holder.filter(|mp| on_target.contains(mp)) {
            return Err(format!(
                "{} is read from {mp}, which lives on the target {disk}; \
                 copy the bundle elsewhere or pick another target",
                a.label
            ));
        }
    }
    Ok(())
}

/// Whether `path` is inside `mountpoint`, comparing whole path segments so
/// `/mnt/sdcard` is not treated as living under `/mnt/sd`.
fn under(path: &str, mountpoint: &str) -> bool {
    let mp = mountpoint.trim_end_matches('/');
    if mp.is_empty() {
        // The root filesystem contains everything.
        return path.starts_with('/');
    }
    match path.strip_prefix(mp) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Free bytes on the filesystem holding `dir`.
fn free_space(dir: &Path) -> Result<u64> {
    // `dir` may not exist yet; walk up to the nearest existing ancestor, which
    // is on the same filesystem it will be created on.
    let mut probe = dir.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(p) if p != probe => probe = p.to_path_buf(),
            _ => break,
        }
    }
    let c_path = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("bad path {}: {e}", probe.display()))?;
    // SAFETY: `stat` is only read after a successful call, and `c_path` is a
    // valid NUL-terminated string that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(format!(
            "statvfs {}: {}",
            probe.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Refuse to start if the scratch filesystem cannot hold `needed` bytes plus
/// headroom. Checked up front so a long transfer or unpack is not wasted, and so
/// the operator can still choose to stream instead.
pub fn ensure_space(dir: &str, needed: u64, what: &str) -> Result<()> {
    ensure_room(dir, needed, what).map_err(|e| {
        format!("{e} — free up space, pass --cache-dir elsewhere, or switch Fetch to 'stream'")
    })
}

/// [`ensure_space`] without its advice, for a caller with other options to
/// offer.
pub fn ensure_room(dir: &str, needed: u64, what: &str) -> Result<()> {
    let path = Path::new(dir);
    let available = free_space(path)?;
    if available >= needed.saturating_add(FREE_SPACE_MARGIN) {
        return Ok(());
    }
    Err(format!(
        "not enough space in {dir} to {what}: need {} plus {} headroom, {} available",
        human_bytes(needed),
        human_bytes(FREE_SPACE_MARGIN),
        human_bytes(available),
    ))
}

/// Size of the archive behind a bundle, or 0 when it has none or it cannot be
/// read.
///
/// The bundle's payload is already-compressed packs, so this is also a close
/// estimate of the unpacked total — close enough for a space check and for the
/// progress bar, and far cheaper than a decompression pass just to add up the
/// headers.
pub(crate) fn archive_size(reference: &BundleRef) -> u64 {
    reference
        .archive
        .as_ref()
        .and_then(|path| fs::metadata(path).ok())
        .map_or(0, |m| m.len())
}

/// Unpack an archive-backed bundle into the scratch directory it was listed
/// against, so the install reads plain local files.
///
/// A remote install never opens an archive — it fetches the individual files the
/// manifest lists — so this only ever runs for a locally supplied `*.tar.zst`.
pub(crate) fn unpack_bundle(
    cfg: &Config,
    ctrl: &Controller,
    reference: &BundleRef,
    ticker: &mut Ticker,
) -> Result<()> {
    let Some(path) = &reference.archive else {
        return Ok(());
    };
    let estimate = archive_size(reference);
    ticker.begin(ctrl, &format!("unpacking {path}"), estimate);

    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] would unpack {path} (~{}) into {}",
            human_bytes(estimate),
            reference.location.join("")
        ));
        return Ok(());
    }
    ensure_space(&cfg.cache_dir, estimate, "unpack the bundle archive")?;

    let (base, span) = ticker.step_span();
    let mut on_progress = receive_progress(ctrl, format!("unpacking {path}"), base, span, estimate);
    let written = crate::core::bundle::unpack(reference, &mut on_progress)?;
    ctrl.log(format!(
        "unpacked {} into {}",
        human_bytes(written),
        reference.location.join("")
    ));
    Ok(())
}

/// Whether the artifact at `location` is verified where it lies rather than
/// copied to `dest`.
///
/// Anything already inside the scratch directory is ours — an unpacked bundle,
/// or a previous run's copy kept with `--keep-cache` — so copying it again
/// would only double its footprint on a filesystem that is usually RAM. Files
/// anywhere else (removable media, a directory passed with `--bundle`) are
/// still copied, so the install reads bytes nobody can change under it.
fn verify_in_place(location: &str, dir: &Path, dest: &Path) -> bool {
    if fetch::is_url(location) {
        return false;
    }
    let path = Path::new(location);
    path == dest || path.starts_with(dir)
}

/// Fetch and verify every artifact in `plan` into the scratch directory.
pub(crate) fn run(
    cfg: &Config,
    ctrl: &Controller,
    plan: &[Artifact],
    ticker: &mut Ticker,
) -> Result<Staged> {
    let dir = PathBuf::from(&cfg.cache_dir);
    let total: u64 = plan.iter().map(|a| a.size).sum();
    // Only what is copied takes space; what is verified in place has it.
    let needed: u64 = plan
        .iter()
        .filter(|a| !verify_in_place(&a.location, &dir, &dir.join(&a.rel)))
        .map(|a| a.size)
        .sum();

    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] would verify {} artifact(s), {} total, into {}",
            plan.len(),
            human_bytes(total),
            dir.display()
        ));
        for a in plan {
            ticker.begin(
                ctrl,
                &format!("[dry-run] verifying {}", a.label),
                transfer_weight(a.size),
            );
        }
        return Ok(Staged {
            dir,
            map: Vec::new(),
            created: Vec::new(),
            keep: true,
        });
    }

    // Check the space up front: running out halfway would waste the whole
    // transfer, and the operator can still switch to streaming instead.
    ensure_space(&cfg.cache_dir, needed, "verify before installing")?;
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    ctrl.log(format!(
        "verifying {} artifact(s), {} total, in {}",
        plan.len(),
        human_bytes(total),
        dir.display(),
    ));

    let mut staged = Staged {
        dir: dir.clone(),
        map: Vec::new(),
        created: Vec::new(),
        keep: cfg.keep_cache,
    };

    for a in plan {
        ticker.begin(
            ctrl,
            &format!("verifying {}", a.label),
            transfer_weight(a.size),
        );
        let (base, span) = ticker.step_span();
        let mut on_progress =
            receive_progress(ctrl, format!("verifying {}", a.label), base, span, a.size);

        let dest = dir.join(&a.rel);
        let in_place = verify_in_place(&a.location, &dir, &dest);
        // Hashed already as it was unpacked here, and unchanged since.
        let noted = in_place
            .then(|| ctrl.unpacked_sha256(Path::new(&a.location)))
            .flatten();
        if let Some(got) = noted {
            on_progress(a.size);
            match fetch::verdict(got, a.sha256.as_deref()) {
                fetch::Verdict::Match => {
                    ctrl.log(format!("{}: sha256 verified as it arrived", a.label))
                }
                verdict => apply_verdict(ctrl, &a.label, verdict, OnMismatch::Fail)?,
            }
            staged.map.push((a.location.clone(), a.location.clone()));
            continue;
        }
        let mut sha = fetch::Sha256::new();
        {
            let mut reader = fetch::Digesting {
                inner: fetch::open(&a.location, &a.source)?,
                sha: &mut sha,
            };
            if in_place {
                drain(&mut reader, &mut on_progress)
                    .map_err(|e| format!("read {}: {e}", a.location))?;
            } else {
                let mut out = fs::File::create(&dest)
                    .map_err(|e| format!("create {}: {e}", dest.display()))?;
                staged.created.push(dest.clone());
                copy(&mut reader, &mut out, &mut on_progress)
                    .map_err(|e| format!("stage {} to {}: {e}", a.location, dest.display()))?;
                out.sync_all()
                    .map_err(|e| format!("sync {}: {e}", dest.display()))?;
            }
        }
        // A mismatch here is fatal: the operator asked to verify before we touch
        // the disk, and nothing destructive has run yet.
        apply_verdict(
            ctrl,
            &a.label,
            fetch::verify(sha, a.sha256.as_deref()),
            OnMismatch::Fail,
        )?;
        let at = if in_place {
            a.location.clone()
        } else {
            dest.to_string_lossy().into_owned()
        };
        staged.map.push((a.location.clone(), at));
    }

    ctrl.log("all artifacts verified; starting the install".to_string());
    Ok(staged)
}

/// Read a source to its end, reporting progress, without keeping the bytes.
fn drain(reader: &mut dyn Read, on_progress: &mut dyn FnMut(u64)) -> io::Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => {
                total += n as u64;
                on_progress(total);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Copy a source into `out`, reporting progress as it goes.
fn copy(
    reader: &mut dyn Read,
    out: &mut fs::File,
    on_progress: &mut dyn FnMut(u64),
) -> io::Result<u64> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        out.write_all(&buf[..n])?;
        total += n as u64;
        on_progress(total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pack(name: &str, size: u64, sha: Option<&str>) -> PackFile {
        PackFile {
            location: format!("https://example.invalid/build/{name}"),
            source: Source::Server,
            size_bytes: size,
            sha256: sha.map(|s| s.to_string()),
        }
    }

    fn build() -> SnapshotBuild {
        SnapshotBuild {
            id: "b".into(),
            label: "b".into(),
            mtime: String::new(),
            source: Source::Server,
            base_location: "https://example.invalid/build/".into(),
            build_number: Some(9),
            profiles: vec![
                ProfilePack {
                    name: "Minimal".into(),
                    build: "9".into(),
                    full: Some(pack("Minimal_9_stock_pack.zst", 100, Some("aa"))),
                    incremental: None,
                },
                ProfilePack {
                    name: "Desktop".into(),
                    build: "9".into(),
                    full: None,
                    incremental: Some(pack("Desktop_9_stock_inc_pack.zst", 20, Some("bb"))),
                },
                ProfilePack {
                    name: "Router".into(),
                    build: "9".into(),
                    full: None,
                    incremental: Some(pack("Router_9_stock_inc_pack.zst", 5, None)),
                },
            ],
            home_pack: Some(pack("home_9_pack.zst", 7, Some("cc"))),
            loaded: true,
            details: None,
        }
    }

    fn uboot() -> UbootBuild {
        UbootBuild {
            id: "u".into(),
            label: "u".into(),
            mtime: String::new(),
            image_location: "https://example.invalid/u/flipper-one/u-boot-rockchip.bin".into(),
            manifest_location: "https://example.invalid/u/manifest.json".into(),
            source: Source::Server,
            size_bytes: 42,
            sha256: Some("dd".into()),
            details: None,
            boot_menu: None,
            loaded: true,
        }
    }

    /// The boot menu a UFS run would install.
    fn boot_menu() -> ImageFile {
        ImageFile {
            location: "https://example.invalid/m/flipper-one/bootmenu-falcon.itb".into(),
            source: Source::Server,
            size_bytes: 99,
            sha256: Some("ee".into()),
        }
    }

    /// The recovery image a UFS run would install.
    fn recovery() -> ImageFile {
        ImageFile {
            location: "https://example.invalid/r/flipper-one/recovery-falcon.itb".into(),
            source: Source::Server,
            size_bytes: 55,
            sha256: Some("ff".into()),
        }
    }

    #[test]
    fn plans_only_what_is_needed_in_install_order() {
        let b = build();
        let extras = vec![b.profiles[1].clone()];
        let p = plan(&uboot(), RawImages::default(), &b, &extras);

        let labels: Vec<&str> = p.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "u-boot image",
                "/home seed",
                "Minimal (full)",
                "Desktop (incremental)"
            ]
        );
        // Router was not selected, so it is never fetched.
        assert!(!p.iter().any(|a| a.label.starts_with("Router")));
        assert_eq!(p.iter().map(|a| a.size).sum::<u64>(), 42 + 7 + 100 + 20);
        // Staged names keep the published filenames.
        assert_eq!(p[2].rel, "Minimal_9_stock_pack.zst");
        assert_eq!(p[0].rel, "u-boot-rockchip.bin");
        // An unpublished digest is carried through as `None`, not invented.
        let all = plan(&uboot(), RawImages::default(), &b, &b.profiles[1..]);
        assert!(all.iter().any(|a| a.sha256.is_none()));
    }

    #[test]
    fn plans_the_falcon_images_right_after_the_bootloader() {
        let b = build();
        let menu = boot_menu();
        let rec = recovery();
        let p = plan(
            &uboot(),
            RawImages {
                boot_menu: Some(&menu),
                recovery: Some(&rec),
                ..RawImages::default()
            },
            &b,
            &[],
        );

        let labels: Vec<&str> = p.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "u-boot image",
                "boot menu image",
                "recovery image",
                "/home seed",
                "Minimal (full)"
            ]
        );
        assert_eq!(p[1].rel, "bootmenu-falcon.itb");
        assert_eq!(p[1].size, 99);
        assert_eq!(p[1].sha256.as_deref(), Some("ee"));
        // Distinct staged names, so the two images cannot collide in the cache.
        assert_eq!(p[2].rel, "recovery-falcon.itb");
        assert_eq!(p[2].size, 55);
        assert_eq!(p[2].sha256.as_deref(), Some("ff"));
    }

    #[test]
    fn plans_the_mcu_firmware_last() {
        // The upload is the last install step, after the packs, and the plan is
        // in install order.
        let b = build();
        let extras = vec![b.profiles[1].clone()];
        let mcu = ImageFile {
            location: "https://example.invalid/mcu=abc/f1/flipperone-mcu-firmware.uf2".into(),
            source: Source::Server,
            size_bytes: 33,
            sha256: Some("cc".into()),
        };
        let p = plan(
            &uboot(),
            RawImages {
                mcu_firmware: Some(&mcu),
                ..RawImages::default()
            },
            &b,
            &extras,
        );

        let last = p.last().expect("a plan");
        assert_eq!(last.label, "MCU firmware");
        assert_eq!(last.rel, "flipperone-mcu-firmware.uf2");
        assert_eq!(last.size, 33);
        assert_eq!(last.sha256.as_deref(), Some("cc"));
        assert_eq!(p[p.len() - 2].label, "Desktop (incremental)");
    }

    #[test]
    fn refuses_only_what_the_target_actually_holds() {
        let local = |location: &str| Artifact {
            label: "u-boot image".into(),
            location: location.into(),
            source: Source::Local { root: "/".into() },
            sha256: None,
            size: 1,
            rel: "u-boot-rockchip.bin".into(),
        };
        let on_target = ["/".to_string(), "/home".to_string()];
        let all = [
            "/".to_string(),
            "/home".to_string(),
            "/run".to_string(),
            "/mnt/stick".to_string(),
        ];
        let check = |path: &str| check_not_on_mounts(&[local(path)], &on_target, &all, "/dev/sda");

        // Booted from the target: its root and /home hold these…
        assert!(check("/var/tmp/b/u-boot.bin").is_err());
        let err = check("/home/user/b/u-boot.bin").unwrap_err();
        assert!(err.contains("read from /home"), "{err}");
        // …but not a tmpfs or a stick mounted on top of them.
        assert!(check("/run/flipperos-cache/upload/u-boot.bin").is_ok());
        assert!(check("/mnt/stick/b/u-boot.bin").is_ok());
        // Nothing of the target mounted at all: nothing to refuse.
        assert!(check_not_on_mounts(&[local("/home/x")], &[], &all, "/dev/sda").is_ok());
    }

    #[test]
    fn verifies_what_is_already_in_the_scratch_dir_in_place() {
        let dir = Path::new("/run/cache");
        let dest = dir.join("Minimal_9_stock_pack.zst");
        // An unpacked bundle lives in a subdirectory of the scratch dir, never at
        // the staged name itself; copying it again would double its RAM.
        assert!(verify_in_place(
            "/run/cache/upload/profile-packs/Minimal_9_stock_pack.zst",
            dir,
            &dest
        ));
        // A previous run's staged copy.
        assert!(verify_in_place(
            "/run/cache/Minimal_9_stock_pack.zst",
            dir,
            &dest
        ));
        // Removable media and downloads are still copied.
        assert!(!verify_in_place(
            "/run/flipperos-media/sda1/b/profile-packs/Minimal_9_stock_pack.zst",
            dir,
            &dest
        ));
        assert!(!verify_in_place(
            "https://example.invalid/run/cache/x",
            dir,
            &dest
        ));
        // A sibling whose name merely starts with the scratch dir's is not in it.
        assert!(!verify_in_place("/run/cache-other/x", dir, &dest));
    }

    #[test]
    fn localise_rewrites_only_staged_artifacts() {
        let mut b = build();
        let mut u = uboot();
        let mut menu = boot_menu();
        let staged = Staged {
            dir: PathBuf::from("/run/cache"),
            map: vec![
                (
                    u.image_location.clone(),
                    "/run/cache/u-boot-rockchip.bin".to_string(),
                ),
                (
                    menu.location.clone(),
                    "/run/cache/bootmenu-falcon.itb".to_string(),
                ),
                (
                    b.profiles[0].full.as_ref().unwrap().location.clone(),
                    "/run/cache/Minimal_9_stock_pack.zst".to_string(),
                ),
            ],
            created: Vec::new(),
            keep: true,
        };
        staged.localise_uboot(&mut u);
        staged.localise_image(&mut menu);
        staged.localise_build(&mut b);

        assert_eq!(u.image_location, "/run/cache/u-boot-rockchip.bin");
        assert_eq!(
            u.source,
            Source::Local {
                root: "/run/cache".into()
            }
        );
        assert_eq!(menu.location, "/run/cache/bootmenu-falcon.itb");
        assert_eq!(
            menu.source,
            Source::Local {
                root: "/run/cache".into()
            }
        );
        let minimal = b.profiles[0].full.as_ref().unwrap();
        assert_eq!(minimal.location, "/run/cache/Minimal_9_stock_pack.zst");
        assert_eq!(
            minimal.source,
            Source::Local {
                root: "/run/cache".into()
            }
        );
        // Not staged (not in the map): left pointing at the server.
        let desktop = b.profiles[1].incremental.as_ref().unwrap();
        assert!(desktop.location.starts_with("https://"));
        assert_eq!(desktop.source, Source::Server);
    }

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("flipperos-stage-{name}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn str(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// sha256 of the bytes, as the manifest would publish it.
    fn digest(bytes: &[u8]) -> String {
        let mut sha = fetch::Sha256::new();
        sha.update(bytes);
        sha.finish_hex()
    }

    fn local_artifact(dir: &Scratch, name: &str, body: &[u8], sha: Option<String>) -> Artifact {
        let path = dir.path(name);
        fs::write(&path, body).unwrap();
        Artifact {
            label: name.to_string(),
            location: path.to_string_lossy().into_owned(),
            source: Source::Local { root: dir.str() },
            sha256: sha,
            size: body.len() as u64,
            rel: name.to_string(),
        }
    }

    fn controller(cache: &Scratch) -> std::sync::Arc<Controller> {
        Controller::new(Config {
            cache_dir: cache.str(),
            dry_run: false,
            automount: false,
            ..Config::default()
        })
    }

    #[test]
    fn stages_and_verifies_every_artifact_before_returning() {
        let src = Scratch::new("verify-src");
        let cache = Scratch::new("verify-cache");
        let ctrl = controller(&cache);

        let plan = vec![
            local_artifact(
                &src,
                "u-boot-rockchip.bin",
                b"loader bytes",
                Some(digest(b"loader bytes")),
            ),
            local_artifact(
                &src,
                "Minimal_9_stock_pack.zst",
                b"pack bytes",
                Some(digest(b"pack bytes")),
            ),
            // No digest published: staged anyway, with a warning.
            local_artifact(&src, "home_9_pack.zst", b"seed", None),
        ];

        let mut ticker = Ticker::new(plan.iter().map(|a| a.size).sum());
        let staged = run(ctrl.config(), &ctrl, &plan, &mut ticker).expect("staging succeeds");

        // Every artifact was copied into the scratch dir, byte for byte.
        assert_eq!(
            fs::read(cache.path("Minimal_9_stock_pack.zst")).unwrap(),
            b"pack bytes"
        );
        assert_eq!(fs::read(cache.path("home_9_pack.zst")).unwrap(), b"seed");

        // …and the builds can be pointed at the staged copies.
        let mut uboot = uboot();
        uboot.image_location = plan[0].location.clone();
        staged.localise_uboot(&mut uboot);
        assert_eq!(
            uboot.image_location,
            cache.path("u-boot-rockchip.bin").to_string_lossy()
        );

        let log = ctrl.snapshot().log.join("\n");
        assert!(log.contains("sha256 verified"), "{log}");
        assert!(
            log.contains("no sha256 published for home_9_pack.zst"),
            "an unpublished digest must be called out: {log}"
        );
    }

    #[test]
    fn takes_the_digest_of_what_was_unpacked_here() {
        let cache = Scratch::new("unpacked-cache");
        let ctrl = controller(&cache);
        fs::create_dir_all(cache.path("upload")).unwrap();
        let in_upload = |name: &str, body: &[u8], sha: String| {
            let path = cache.path(&format!("upload/{name}"));
            fs::write(&path, body).unwrap();
            Artifact {
                label: name.to_string(),
                location: path.to_string_lossy().into_owned(),
                source: Source::Local { root: cache.str() },
                sha256: Some(sha),
                size: body.len() as u64,
                rel: name.to_string(),
            }
        };
        let plan = vec![
            in_upload("kept.zst", b"pack bytes", digest(b"pack bytes")),
            in_upload("changed.zst", b"old bytes", digest(b"new bytes, longer")),
        ];
        ctrl.note_unpacked(
            &cache.path("upload"),
            HashMap::from([
                (PathBuf::from("kept.zst"), digest(b"pack bytes")),
                (PathBuf::from("changed.zst"), digest(b"old bytes")),
            ]),
        );
        // Rewritten after it was noted: its digest no longer stands for it.
        fs::write(cache.path("upload/changed.zst"), b"new bytes, longer").unwrap();

        let mut ticker = Ticker::new(plan.iter().map(|a| a.size).sum());
        run(ctrl.config(), &ctrl, &plan, &mut ticker).expect("staging succeeds");
        let log = ctrl.snapshot().log.join("\n");
        assert!(
            log.contains("kept.zst: sha256 verified as it arrived"),
            "{log}"
        );
        assert!(log.contains("changed.zst: sha256 verified\n"), "{log}");

        // A noted digest is held to the manifest like a computed one.
        let plan = vec![in_upload("kept.zst", b"pack bytes", digest(b"other"))];
        ctrl.note_unpacked(
            &cache.path("upload"),
            HashMap::from([(PathBuf::from("kept.zst"), digest(b"pack bytes"))]),
        );
        let mut ticker = Ticker::new(plan[0].size);
        let err = run(ctrl.config(), &ctrl, &plan, &mut ticker).err().unwrap();
        assert!(err.contains("sha256 mismatch for kept.zst"), "{err}");
    }

    #[test]
    fn a_bad_digest_fails_before_anything_destructive() {
        let src = Scratch::new("mismatch-src");
        let cache = Scratch::new("mismatch-cache");
        let ctrl = controller(&cache);

        let plan = vec![local_artifact(
            &src,
            "Minimal_9_stock_pack.zst",
            b"pack bytes",
            Some(digest(b"different bytes")),
        )];

        let mut ticker = Ticker::new(plan[0].size);
        let err = match run(ctrl.config(), &ctrl, &plan, &mut ticker) {
            Ok(_) => panic!("a digest mismatch must fail the run"),
            Err(e) => e,
        };
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(err.contains("Minimal_9_stock_pack.zst"), "{err}");
    }

    #[test]
    fn dry_run_stages_nothing() {
        let src = Scratch::new("dry-src");
        let cache = Scratch::new("dry-cache");
        let ctrl = Controller::new(Config {
            cache_dir: cache.str(),
            dry_run: true,
            automount: false,
            ..Config::default()
        });
        let plan = vec![local_artifact(
            &src,
            "pack.zst",
            b"bytes",
            Some(digest(b"bytes")),
        )];

        let mut ticker = Ticker::new(plan[0].size);
        run(ctrl.config(), &ctrl, &plan, &mut ticker).expect("dry run succeeds");
        assert!(
            !cache.path("pack.zst").exists(),
            "a dry run must not transfer"
        );
        assert!(ctrl
            .snapshot()
            .log
            .join("\n")
            .contains("[dry-run] would verify 1 artifact"));
    }

    #[test]
    fn refuses_a_bundle_that_lives_on_the_target() {
        // A pack read from a mount backed by the target disk would be destroyed
        // by the very install that needs it.
        let mut b = build();
        let disk = "/dev/definitely-not-a-real-disk";
        // Nothing is mounted from that disk, so a remote bundle is fine.
        assert!(guard_not_on_target(&uboot(), RawImages::default(), &b, &[], disk).is_ok());
        // A local pack under a mountpoint of the target would be caught; with no
        // such mount present the guard is a no-op, which is what this asserts.
        if let Some(full) = b.profiles[0].full.as_mut() {
            full.location = "/mnt/sd/profile-packs/Minimal_9_stock_pack.zst".into();
            full.source = Source::Local {
                root: "/mnt/sd".into(),
            };
        }
        assert!(guard_not_on_target(&uboot(), RawImages::default(), &b, &[], disk).is_ok());
    }

    #[test]
    fn reports_free_space_for_a_missing_directory() {
        // Walks up to an existing ancestor rather than failing.
        let dir = std::env::temp_dir().join("flipperos-stage-nonexistent/deeper");
        assert!(free_space(&dir).unwrap() > 0);
    }

    #[test]
    fn takes_last_path_segment() {
        assert_eq!(file_name("https://h/a/b/c.zst"), "c.zst");
        assert_eq!(file_name("/mnt/sd/x.zst"), "x.zst");
    }
}
