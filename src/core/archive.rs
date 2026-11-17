//! Reading `*.tar.zst` update bundles handed to the installer whole.
//!
//! Installs from the update server fetch the individual files a manifest lists,
//! and never peek inside the published archive. An archive is therefore either a
//! local file (`--bundle`, removable media) or a stream uploaded to the
//! installer, and either way it is handled by unpacking it into the scratch
//! directory once, after which it is an ordinary unpacked bundle directory. That
//! keeps member ordering irrelevant — in particular it avoids any dependency on
//! an incremental pack appearing after the Minimal pack it is received on top of.
//!
//! A local file is unpacked whole by [`unpack`]; a stream, which cannot be read
//! twice, is unpacked selectively and verified on the way by [`unpack_stream`].

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use crate::core::fetch;

pub type Result<T> = std::result::Result<T, String>;

/// Suffix that marks a bundle archive.
pub const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// The bundle's manifest, by its path inside the archive's top-level directory.
const MANIFEST: &str = "manifest.json";

/// Largest manifest read into memory. Real ones are tens of kilobytes; the cap
/// only stops a hostile archive from claiming all of RAM with one member.
const MANIFEST_MAX: u64 = 32 * 1024 * 1024;

/// Open a `tar.zst` stream for sequential reading.
fn entries<R: Read>(
    reader: R,
    what: &str,
) -> Result<tar::Archive<zstd::stream::read::Decoder<'static, io::BufReader<R>>>> {
    let decoder =
        zstd::stream::read::Decoder::new(reader).map_err(|e| format!("zstd {what}: {e}"))?;
    Ok(tar::Archive::new(decoder))
}

/// Open an archive file for sequential reading.
fn file_entries(
    archive: &str,
) -> Result<tar::Archive<zstd::stream::read::Decoder<'static, io::BufReader<fs::File>>>> {
    let file = fs::File::open(archive).map_err(|e| format!("open {archive}: {e}"))?;
    entries(file, archive)
}

/// Strip the archive's single top-level directory from a member path, so
/// `flipperone-update-…-15/profile-packs/x.zst` becomes `profile-packs/x.zst`.
///
/// Returns `None` for the top-level directory entry itself, and for anything not
/// made purely of ordinary path components — an absolute path or one containing
/// `..` is refused outright rather than sanitised, since a bundle never contains
/// either and we are unpacking as root.
fn inner_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    // The first component is the archive's own directory; it must be an ordinary
    // name, so a leading `/` or `..` is rejected here rather than skipped over.
    match components.next()? {
        Component::Normal(_) => {}
        _ => return None,
    }
    let mut out = PathBuf::new();
    for comp in components {
        match comp {
            Component::Normal(part) => out.push(part),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Read the bundle's `manifest.json` out of an archive.
///
/// The manifest is written first, so this only decompresses the head of the
/// archive (tens of kilobytes) and stops — cheap enough to run while merely
/// listing candidate bundles found on removable media.
pub fn read_manifest(archive: &str) -> Result<Vec<u8>> {
    let mut ar = file_entries(archive)?;
    let iter = ar.entries().map_err(|e| format!("read {archive}: {e}"))?;
    for entry in iter {
        let mut entry = entry.map_err(|e| format!("read {archive}: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("read {archive}: {e}"))?
            .to_path_buf();
        if inner_path(&path).as_deref() == Some(Path::new("manifest.json")) {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| format!("read manifest.json from {archive}: {e}"))?;
            return Ok(buf);
        }
    }
    Err(format!("{archive}: no manifest.json in the archive"))
}

/// Unpack every file in `archive` into `dest`, stripping the archive's top-level
/// directory, and return `dest` for use as a bundle directory.
///
/// `on_progress` is called with the running total of *uncompressed* bytes
/// written. Only regular files and the directories holding them are created:
/// symlinks, devices and hard links are skipped, since a bundle ships plain
/// artifacts and we are writing into a scratch dir as root.
pub fn unpack(archive: &str, dest: &str, on_progress: &mut dyn FnMut(u64)) -> Result<u64> {
    fs::create_dir_all(dest).map_err(|e| format!("create {dest}: {e}"))?;
    let mut ar = file_entries(archive)?;
    let iter = ar.entries().map_err(|e| format!("read {archive}: {e}"))?;
    let mut files = 0usize;
    let mut total: u64 = 0;

    for entry in iter {
        let mut entry = entry.map_err(|e| format!("read {archive}: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| format!("read {archive}: {e}"))?
            .to_path_buf();
        let Some(rel) = inner_path(&path) else {
            continue;
        };
        let out_path = Path::new(dest).join(&rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut out = fs::File::create(&out_path)
            .map_err(|e| format!("create {}: {e}", out_path.display()))?;
        // Copy in chunks so progress moves within a large member, not just
        // between members (the Minimal pack alone is most of the archive).
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read {} from {archive}: {e}", rel.display())),
            };
            io::Write::write_all(&mut out, &buf[..n])
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
            total += n as u64;
            on_progress(total);
        }
        files += 1;
    }

    if files == 0 {
        return Err(format!("{archive}: archive contained no files"));
    }
    Ok(total)
}

/// Total uncompressed size of the files in an archive, for a free-space check.
/// Requires a full decompression pass of the header stream, so it is only worth
/// calling when the sizes are needed up front.
pub fn unpacked_size(archive: &str) -> Result<u64> {
    let mut ar = file_entries(archive)?;
    let iter = ar.entries().map_err(|e| format!("read {archive}: {e}"))?;
    let mut total = 0u64;
    for entry in iter {
        let entry = entry.map_err(|e| format!("read {archive}: {e}"))?;
        if entry.header().entry_type().is_file() {
            total += entry.header().size().unwrap_or(0);
        }
    }
    Ok(total)
}

/// What to keep of an archive, decided from its manifest by the caller of
/// [`unpack_stream`].
pub struct Keep {
    /// Whether the member at this path (relative to the bundle root) is
    /// extracted. Members it rejects are skipped, or deleted if they arrived
    /// before the manifest did.
    pub filter: Box<dyn Fn(&Path) -> bool>,
    /// Every file the manifest lists that `filter` keeps, with the digest it
    /// publishes for it, if any. Each must turn up in the archive, and must match.
    pub expected: HashMap<PathBuf, Option<String>>,
}

/// What [`unpack_stream`] wrote.
#[derive(Debug)]
pub struct Unpacked {
    /// Uncompressed bytes written.
    pub bytes: u64,
    /// The SHA-256 of every file kept, by its path relative to `dest`, taken
    /// as it was written — so whoever verifies them next need not read them
    /// back to know it.
    pub sha256: HashMap<PathBuf, String>,
}

/// Unpack a `tar.zst` *stream* into `dest`, keeping only what the manifest calls
/// for, and verifying every kept file against the manifest on the way.
///
/// This is for an archive nobody has a copy of — one arriving over the network
/// — so it is read exactly once and never stored. Member order does not matter:
///
/// - until `manifest.json` turns up, every file is extracted;
/// - when it does, `on_manifest` is handed its bytes and either rejects the
///   bundle (the error ends the unpack) or returns the [`Keep`] that applies to
///   every later member, and files already extracted that it rejects are
///   deleted again.
///
/// A bundle lists the manifest first, so normally nothing unwanted is ever
/// written; a manifest at the end costs a transient full unpack, not a failure.
///
/// Every kept file is hashed as it is written, and checked against the manifest
/// as soon as both are in: at once, behind the manifest, or when the manifest
/// arrives, ahead of it. So a corrupt file fails the unpack before the rest of
/// the stream is read, not at its end. The unpack also fails if there was no
/// manifest, or if a file the manifest lists and the filter keeps is missing.
/// On any failure `dest` is removed, so nothing is left behind on what is
/// usually a tmpfs.
///
/// `on_progress` is called with the running total of uncompressed bytes written.
pub fn unpack_stream<R: Read>(
    reader: R,
    dest: &Path,
    on_manifest: &mut dyn FnMut(&[u8]) -> Result<Keep>,
    on_progress: &mut dyn FnMut(u64),
) -> Result<Unpacked> {
    let result = unpack_stream_inner(reader, dest, on_manifest, on_progress);
    if result.is_err() {
        let _ = fs::remove_dir_all(dest);
    }
    result
}

fn unpack_stream_inner<R: Read>(
    reader: R,
    dest: &Path,
    on_manifest: &mut dyn FnMut(&[u8]) -> Result<Keep>,
    on_progress: &mut dyn FnMut(u64),
) -> Result<Unpacked> {
    let what = "the bundle";
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut ar = entries(reader, what)?;
    let iter = ar.entries().map_err(|e| format!("read {what}: {e}"))?;

    let mut keep: Option<Keep> = None;
    // The SHA-256 of every file written so far, by bundle-relative path.
    let mut written: HashMap<PathBuf, String> = HashMap::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];

    for entry in iter {
        let mut entry = entry.map_err(|e| format!("read {what}: {e}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| format!("read {what}: {e}"))?
            .to_path_buf();
        let Some(rel) = inner_path(&path) else {
            continue;
        };

        if rel == Path::new(MANIFEST) && keep.is_none() {
            let mut bytes = Vec::new();
            (&mut entry)
                .take(MANIFEST_MAX + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("read {MANIFEST} from {what}: {e}"))?;
            if bytes.len() as u64 > MANIFEST_MAX {
                return Err(format!("{MANIFEST} is larger than {MANIFEST_MAX} bytes"));
            }
            let k = on_manifest(&bytes)?;
            // Drop what arrived ahead of the manifest and is not wanted, and
            // check what is.
            written.retain(|rel, _| {
                let wanted = (k.filter)(rel);
                if !wanted {
                    remove_with_empty_parents(dest, rel);
                }
                wanted
            });
            let mut early: Vec<_> = written.iter().collect();
            early.sort();
            for (rel, got) in early {
                check_digest(&k, rel, got)?;
            }
            let out_path = dest.join(&rel);
            fs::write(&out_path, &bytes)
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
            total += bytes.len() as u64;
            on_progress(total);
            keep = Some(k);
            continue;
        }

        if let Some(k) = &keep {
            if !(k.filter)(&rel) {
                continue;
            }
        }

        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut out = fs::File::create(&out_path)
            .map_err(|e| format!("create {}: {e}", out_path.display()))?;
        let mut sha = fetch::Sha256::new();
        loop {
            let n = match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read {} from {what}: {e}", rel.display())),
            };
            sha.update(&buf[..n]);
            io::Write::write_all(&mut out, &buf[..n])
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
            total += n as u64;
            on_progress(total);
        }
        let got = sha.finish_hex();
        // With the manifest in, a corrupt file ends the upload now rather than
        // after the gigabytes behind it.
        if let Some(k) = &keep {
            check_digest(k, &rel, &got)?;
        }
        written.insert(rel, got);
    }

    let Some(keep) = keep else {
        return Err(format!("no {MANIFEST} in {what}"));
    };
    // Every file written was checked as it came; what is left is whether
    // every file wanted came at all.
    let mut expected: Vec<_> = keep.expected.keys().collect();
    expected.sort();
    for rel in expected {
        if !written.contains_key(rel) {
            return Err(format!(
                "{} is listed in the manifest but missing from the archive",
                rel.display()
            ));
        }
    }
    Ok(Unpacked {
        bytes: total,
        sha256: written,
    })
}

/// Check a file's SHA-256 against the one the manifest publishes for it. A file
/// the manifest does not list, or lists with no digest, passes.
fn check_digest(keep: &Keep, rel: &Path, got: &str) -> Result<()> {
    match keep.expected.get(rel) {
        Some(Some(want)) if !want.eq_ignore_ascii_case(got) => Err(format!(
            "{} is corrupt: SHA-256 {got}, the manifest says {}",
            rel.display(),
            want.to_ascii_lowercase()
        )),
        _ => Ok(()),
    }
}

/// Delete `dest/rel`, then each directory above it that this leaves empty, up to
/// but not including `dest` itself.
fn remove_with_empty_parents(dest: &Path, rel: &Path) {
    let _ = fs::remove_file(dest.join(rel));
    let mut dir = rel.parent();
    while let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        // Fails, and so stops, at the first directory still holding something.
        if fs::remove_dir(dest.join(d)).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `tar.zst` in memory whose members are deliberately in an
    /// inconvenient order (an incremental pack before the Minimal one it depends
    /// on), to pin down that unpacking does not care.
    fn sample_archive(dir: &Path) -> String {
        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let members: [(&str, &[u8]); 4] = [
                ("bundle-1/manifest.json", br#"{"schema":1}"#),
                (
                    "bundle-1/profile-packs/Desktop_9_stock_inc_pack.zst",
                    b"inc",
                ),
                ("bundle-1/profile-packs/Minimal_9_stock_pack.zst", b"full"),
                ("bundle-1/u-boot/flipper-one/u-boot-rockchip.bin", b"uboot"),
            ];
            for (name, body) in members {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, body).unwrap();
            }
            builder.finish().unwrap();
        }
        let compressed = zstd::stream::encode_all(&tar_bytes[..], 1).unwrap();
        let path = dir.join("bundle-1.tar.zst");
        fs::write(&path, compressed).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flipperos-archive-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_manifest_without_unpacking() {
        let dir = scratch("manifest");
        let archive = sample_archive(&dir);
        let bytes = read_manifest(&archive).unwrap();
        assert_eq!(bytes, br#"{"schema":1}"#);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpacks_every_member_regardless_of_order() {
        let dir = scratch("unpack");
        let archive = sample_archive(&dir);
        let dest = dir.join("out");
        let mut seen = 0u64;
        let written = unpack(&archive, dest.to_str().unwrap(), &mut |n| seen = n).unwrap();

        assert_eq!(written, (br#"{"schema":1}"#.len() + 3 + 4 + 5) as u64);
        // The archive's top-level directory is stripped.
        assert!(dest.join("manifest.json").is_file());
        assert_eq!(
            fs::read(dest.join("profile-packs/Minimal_9_stock_pack.zst")).unwrap(),
            b"full"
        );
        assert_eq!(
            fs::read(dest.join("profile-packs/Desktop_9_stock_inc_pack.zst")).unwrap(),
            b"inc"
        );
        assert!(dest
            .join("u-boot/flipper-one/u-boot-rockchip.bin")
            .is_file());
        // Progress reported the uncompressed total.
        assert_eq!(seen, (br#"{"schema":1}"#.len() + 3 + 4 + 5) as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_unpacked_size() {
        let dir = scratch("size");
        let archive = sample_archive(&dir);
        assert_eq!(
            unpacked_size(&archive).unwrap(),
            (br#"{"schema":1}"#.len() + 3 + 4 + 5) as u64
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_manifest_is_a_clear_error() {
        let dir = scratch("empty");
        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "x/other.txt", &b"z"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let path = dir.join("bad.tar.zst");
        fs::write(&path, zstd::stream::encode_all(&tar_bytes[..], 1).unwrap()).unwrap();
        let err = read_manifest(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no manifest.json"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn tar_zst(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, body) in members {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        zstd::stream::encode_all(&tar_bytes[..], 1).unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        let mut s = fetch::Sha256::new();
        s.update(bytes);
        s.finish_hex()
    }

    /// Keep everything but other boards' U-Boot, and expect the two files a
    /// flipper-one install needs, with their real digests.
    fn keep_flipper_one(_manifest: &[u8]) -> Result<Keep> {
        Ok(Keep {
            filter: Box::new(|p: &Path| !p.starts_with("u-boot/evb")),
            expected: HashMap::from([
                (
                    PathBuf::from("u-boot/flipper-one/u-boot-rockchip.bin"),
                    Some(sha(b"uboot")),
                ),
                (
                    PathBuf::from("profile-packs/Minimal_9_stock_pack.zst"),
                    Some(sha(b"full")),
                ),
            ]),
        })
    }

    const MEMBERS: [(&str, &[u8]); 4] = [
        ("b/u-boot/evb/u-boot-rockchip.bin", b"evb"),
        ("b/u-boot/flipper-one/u-boot-rockchip.bin", b"uboot"),
        ("b/profile-packs/Minimal_9_stock_pack.zst", b"full"),
        ("b/notes.txt", b"kept, though nobody asked"),
    ];

    fn stream_into(
        name: &str,
        members: &[(&str, &[u8])],
        on_manifest: &mut dyn FnMut(&[u8]) -> Result<Keep>,
    ) -> (PathBuf, Result<Unpacked>) {
        let dir = scratch(name);
        let dest = dir.join("out");
        let bytes = tar_zst(members);
        let r = unpack_stream(&bytes[..], &dest, on_manifest, &mut |_| {});
        (dest, r)
    }

    #[test]
    fn streams_only_what_the_manifest_keeps() {
        let mut members = vec![("b/manifest.json", &br#"{"schema":1}"#[..])];
        members.extend(MEMBERS);
        let mut seen = None;
        let (dest, r) = stream_into("stream-first", &members, &mut |m| {
            seen = Some(m.to_vec());
            keep_flipper_one(m)
        });
        let unpacked = r.unwrap();
        // The digest of every file kept, and only of those.
        assert_eq!(
            unpacked.sha256,
            HashMap::from([
                (
                    PathBuf::from("u-boot/flipper-one/u-boot-rockchip.bin"),
                    sha(b"uboot")
                ),
                (
                    PathBuf::from("profile-packs/Minimal_9_stock_pack.zst"),
                    sha(b"full")
                ),
                (
                    PathBuf::from("notes.txt"),
                    sha(b"kept, though nobody asked")
                ),
            ])
        );
        assert_eq!(seen.as_deref(), Some(&br#"{"schema":1}"#[..]));
        assert!(dest.join("manifest.json").is_file());
        assert_eq!(
            fs::read(dest.join("u-boot/flipper-one/u-boot-rockchip.bin")).unwrap(),
            b"uboot"
        );
        assert!(dest.join("notes.txt").is_file());
        // Another board's image is never written.
        assert!(!dest.join("u-boot/evb").exists());
    }

    #[test]
    fn a_late_manifest_prunes_what_arrived_before_it() {
        let mut members = MEMBERS.to_vec();
        members.push(("b/manifest.json", br#"{"schema":1}"#));
        let (dest, r) = stream_into("stream-last", &members, &mut keep_flipper_one);
        let unpacked = r.unwrap();
        assert!(!unpacked
            .sha256
            .contains_key(Path::new("u-boot/evb/u-boot-rockchip.bin")));
        assert!(dest.join("manifest.json").is_file());
        assert!(dest
            .join("profile-packs/Minimal_9_stock_pack.zst")
            .is_file());
        // Extracted while the filter was unknown, then deleted with its
        // directory, which it left empty.
        assert!(!dest.join("u-boot/evb").exists());
        assert!(dest.join("u-boot/flipper-one").is_dir());
    }

    #[test]
    fn a_corrupt_member_fails_and_leaves_nothing() {
        let members: [(&str, &[u8]); 3] = [
            ("b/manifest.json", br#"{"schema":1}"#),
            ("b/u-boot/flipper-one/u-boot-rockchip.bin", b"uboot"),
            ("b/profile-packs/Minimal_9_stock_pack.zst", b"fu11"),
        ];
        let (dest, r) = stream_into("stream-corrupt", &members, &mut keep_flipper_one);
        let err = r.unwrap_err();
        assert!(err.contains("Minimal_9_stock_pack.zst is corrupt"), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn a_corrupt_member_ends_the_unpack_at_once() {
        let tail = vec![0u8; 1 << 20];
        // Corrupt behind the manifest, and corrupt ahead of it: either way the
        // megabyte that follows is never read.
        for manifest_first in [true, false] {
            let mut members: Vec<(&str, &[u8])> = vec![
                ("b/u-boot/flipper-one/u-boot-rockchip.bin", b"uboot"),
                ("b/profile-packs/Minimal_9_stock_pack.zst", b"fu11"),
            ];
            let manifest = ("b/manifest.json", &br#"{"schema":1}"#[..]);
            if manifest_first {
                members.insert(0, manifest);
            } else {
                members.push(manifest);
            }
            members.push(("b/profile-packs/Desktop_9_stock_inc_pack.zst", &tail));
            let dest = scratch("stream-corrupt-early").join("out");
            let mut furthest = 0;
            let r = unpack_stream(
                &tar_zst(&members)[..],
                &dest,
                &mut keep_flipper_one,
                &mut |n| furthest = n,
            );
            let err = r.unwrap_err();
            assert!(err.contains("Minimal_9_stock_pack.zst is corrupt"), "{err}");
            assert!(furthest < tail.len() as u64, "read {furthest} bytes");
        }
    }

    #[test]
    fn a_missing_member_fails() {
        let members: [(&str, &[u8]); 2] = [
            ("b/manifest.json", br#"{"schema":1}"#),
            ("b/u-boot/flipper-one/u-boot-rockchip.bin", b"uboot"),
        ];
        let (dest, r) = stream_into("stream-missing", &members, &mut keep_flipper_one);
        let err = r.unwrap_err();
        assert!(err.contains("missing from the archive"), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn a_rejected_manifest_stops_the_unpack() {
        let mut members = vec![("b/manifest.json", &br#"{"schema":7}"#[..])];
        members.extend(MEMBERS);
        let mut calls = 0;
        let (dest, r) = stream_into("stream-rejected", &members, &mut |_| {
            calls += 1;
            Err("unsupported schema 7".to_string())
        });
        assert_eq!(r.unwrap_err(), "unsupported schema 7");
        assert_eq!(calls, 1);
        assert!(!dest.exists());
    }

    #[test]
    fn a_stream_without_a_manifest_fails() {
        let (dest, r) = stream_into("stream-none", &MEMBERS, &mut keep_flipper_one);
        let err = r.unwrap_err();
        assert!(err.contains("no manifest.json"), "{err}");
        assert!(!dest.exists());
    }

    #[test]
    fn refuses_paths_that_escape() {
        assert_eq!(
            inner_path(Path::new("b/a.txt")).unwrap(),
            Path::new("a.txt")
        );
        assert_eq!(inner_path(Path::new("b/")), None);
        assert_eq!(inner_path(Path::new("b/../../etc/passwd")), None);
        assert_eq!(inner_path(Path::new("/etc/passwd")), None);
    }
}
