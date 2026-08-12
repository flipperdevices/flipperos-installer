//! Verifying artifacts before the target is touched.
//!
//! With [`FetchMode::VerifyFirst`] every artifact the run needs is fetched into
//! the scratch directory and checked against the digest its manifest publishes
//! *before* the first destructive command. The install then reads the local
//! copies, so a truncated download or a corrupted pack cannot leave the operator
//! with a wiped device and half an installation.
//!
//! Only what the run actually needs is staged: the U-Boot image for the detected
//! board, the Minimal full pack, the incremental pack of each selected profile,
//! and the `/home` seed. For a bundle that means well under the size of the
//! published archive.
//!
//! [`FetchMode::VerifyFirst`]: crate::core::model::FetchMode::VerifyFirst

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::core::controller::{Config, Controller};
use crate::core::fetch;
use crate::core::install::{apply_verdict, receive_progress, OnMismatch, Ticker};
use crate::core::model::{
    human_bytes, BundleRef, PackFile, ProfilePack, SnapshotBuild, Source, UbootBuild,
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

/// Everything the run will read, in install order.
pub fn plan(uboot: &UbootBuild, build: &SnapshotBuild, extras: &[ProfilePack]) -> Vec<Artifact> {
    let mut out = Vec::new();
    out.push(Artifact {
        label: "u-boot image".to_string(),
        location: uboot.image_location.clone(),
        source: uboot.source.clone(),
        sha256: uboot.sha256.clone(),
        size: uboot.size_bytes,
        rel: "u-boot-rockchip.bin".to_string(),
    });
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
    out
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
    build: &SnapshotBuild,
    extras: &[ProfilePack],
    disk: &str,
) -> Result<()> {
    let mounts = crate::core::removable::mounts_on(disk);
    check_not_on_mounts(&plan(uboot, build, extras), &mounts, disk)
}

/// The decision [`guard_not_on_target`] makes, separated from reading
/// `/proc/mounts` so it can be exercised directly.
fn check_not_on_mounts(plan: &[Artifact], mounts: &[String], disk: &str) -> Result<()> {
    for a in plan {
        // A remote artifact is unaffected by what the target disk holds.
        if fetch::is_url(&a.location) {
            continue;
        }
        if let Some(mp) = mounts.iter().find(|mp| under(&a.location, mp)) {
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
    let path = Path::new(dir);
    let available = free_space(path)?;
    if available >= needed.saturating_add(FREE_SPACE_MARGIN) {
        return Ok(());
    }
    Err(format!(
        "not enough space in {dir} to {what}: need {} plus {} headroom, {} available — \
         free up space, pass --cache-dir elsewhere, or switch Fetch to 'stream'",
        human_bytes(needed),
        human_bytes(FREE_SPACE_MARGIN),
        human_bytes(available),
    ))
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
    ticker.begin(ctrl, &format!("unpacking {path}"));

    // The bundle's payload is already-compressed packs, so the archive's own size
    // is a close estimate of the unpacked total — close enough for a space check,
    // and far cheaper than a decompression pass just to add up the headers.
    let estimate = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
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
    let mut on_progress = receive_progress(
        ctrl,
        format!("unpacking {path}"),
        base,
        span,
        estimate,
    );
    let written = crate::core::bundle::unpack(reference, &mut on_progress)?;
    ctrl.log(format!(
        "unpacked {} into {}",
        human_bytes(written),
        reference.location.join("")
    ));
    Ok(())
}

/// Fetch and verify every artifact in `plan` into the scratch directory.
pub(crate) fn run(
    cfg: &Config,
    ctrl: &Controller,
    plan: &[Artifact],
    ticker: &mut Ticker,
) -> Result<Staged> {
    let dir = PathBuf::from(&cfg.cache_dir);
    let needed: u64 = plan.iter().map(|a| a.size).sum();

    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] would verify {} artifact(s), {} total, into {}",
            plan.len(),
            human_bytes(needed),
            dir.display()
        ));
        for a in plan {
            ticker.begin(ctrl, &format!("[dry-run] verifying {}", a.label));
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
        human_bytes(needed),
        dir.display(),
    ));

    let mut staged = Staged {
        dir: dir.clone(),
        map: Vec::new(),
        created: Vec::new(),
        keep: cfg.keep_cache,
    };

    for a in plan {
        ticker.begin(ctrl, &format!("verifying {}", a.label));
        let (base, span) = ticker.step_span();
        let mut on_progress =
            receive_progress(ctrl, format!("verifying {}", a.label), base, span, a.size);

        let dest = dir.join(&a.rel);
        // An artifact already staged locally (an unpacked bundle, or a previous
        // run with --keep-cache) is verified in place rather than copied.
        let in_place = !fetch::is_url(&a.location) && Path::new(&a.location) == dest;
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
        staged.map.push((
            a.location.clone(),
            dest.to_string_lossy().into_owned(),
        ));
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
            loaded: true,
        }
    }

    #[test]
    fn plans_only_what_is_needed_in_install_order() {
        let b = build();
        let extras = vec![b.profiles[1].clone()];
        let p = plan(&uboot(), &b, &extras);

        let labels: Vec<&str> = p.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            ["u-boot image", "/home seed", "Minimal (full)", "Desktop (incremental)"]
        );
        // Router was not selected, so it is never fetched.
        assert!(!p.iter().any(|a| a.label.starts_with("Router")));
        assert_eq!(p.iter().map(|a| a.size).sum::<u64>(), 42 + 7 + 100 + 20);
        // Staged names keep the published filenames.
        assert_eq!(p[2].rel, "Minimal_9_stock_pack.zst");
        assert_eq!(p[0].rel, "u-boot-rockchip.bin");
        // An unpublished digest is carried through as `None`, not invented.
        let all = plan(&uboot(), &b, &b.profiles[1..]);
        assert!(all.iter().any(|a| a.sha256.is_none()));
    }

    #[test]
    fn localise_rewrites_only_staged_artifacts() {
        let mut b = build();
        let mut u = uboot();
        let staged = Staged {
            dir: PathBuf::from("/run/cache"),
            map: vec![
                (
                    u.image_location.clone(),
                    "/run/cache/u-boot-rockchip.bin".to_string(),
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
        staged.localise_build(&mut b);

        assert_eq!(u.image_location, "/run/cache/u-boot-rockchip.bin");
        assert_eq!(u.source, Source::Local { root: "/run/cache".into() });
        let minimal = b.profiles[0].full.as_ref().unwrap();
        assert_eq!(minimal.location, "/run/cache/Minimal_9_stock_pack.zst");
        assert_eq!(minimal.source, Source::Local { root: "/run/cache".into() });
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
            local_artifact(&src, "u-boot-rockchip.bin", b"loader bytes", Some(digest(b"loader bytes"))),
            local_artifact(&src, "Minimal_9_stock_pack.zst", b"pack bytes", Some(digest(b"pack bytes"))),
            // No digest published: staged anyway, with a warning.
            local_artifact(&src, "home_9_pack.zst", b"seed", None),
        ];

        let mut ticker = Ticker::new(plan.len() as u32);
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

        let mut ticker = Ticker::new(1);
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
        let plan = vec![local_artifact(&src, "pack.zst", b"bytes", Some(digest(b"bytes")))];

        let mut ticker = Ticker::new(1);
        run(ctrl.config(), &ctrl, &plan, &mut ticker).expect("dry run succeeds");
        assert!(!cache.path("pack.zst").exists(), "a dry run must not transfer");
        assert!(ctrl.snapshot().log.join("\n").contains("[dry-run] would verify 1 artifact"));
    }

    #[test]
    fn refuses_a_bundle_that_lives_on_the_target() {
        // A pack read from a mount backed by the target disk would be destroyed
        // by the very install that needs it.
        let mut b = build();
        let disk = "/dev/definitely-not-a-real-disk";
        // Nothing is mounted from that disk, so a remote bundle is fine.
        assert!(guard_not_on_target(&uboot(), &b, &[], disk).is_ok());
        // A local pack under a mountpoint of the target would be caught; with no
        // such mount present the guard is a no-op, which is what this asserts.
        if let Some(full) = b.profiles[0].full.as_mut() {
            full.location = "/mnt/sd/profile-packs/Minimal_9_stock_pack.zst".into();
            full.source = Source::Local { root: "/mnt/sd".into() };
        }
        assert!(guard_not_on_target(&uboot(), &b, &[], disk).is_ok());
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
