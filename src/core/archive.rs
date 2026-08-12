//! Reading locally supplied `*.tar.zst` update bundles.
//!
//! An archive is always local: installs from the update server fetch the
//! individual files a manifest lists, and never peek inside the published
//! archive. A local archive is therefore handled by unpacking it whole into the
//! scratch directory once, after which it is an ordinary unpacked bundle
//! directory. That keeps member ordering irrelevant — in particular it avoids
//! any dependency on an incremental pack appearing after the Minimal pack it is
//! received on top of.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub type Result<T> = std::result::Result<T, String>;

/// Suffix that marks a bundle archive.
pub const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Open an archive for sequential reading.
fn entries(archive: &str) -> Result<tar::Archive<zstd::stream::read::Decoder<'static, io::BufReader<fs::File>>>> {
    let file = fs::File::open(archive).map_err(|e| format!("open {archive}: {e}"))?;
    let decoder =
        zstd::stream::read::Decoder::new(file).map_err(|e| format!("zstd {archive}: {e}"))?;
    Ok(tar::Archive::new(decoder))
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
    let mut ar = entries(archive)?;
    let iter = ar
        .entries()
        .map_err(|e| format!("read {archive}: {e}"))?;
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
    let mut ar = entries(archive)?;
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
            fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
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
    let mut ar = entries(archive)?;
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
                ("bundle-1/profile-packs/Desktop_9_stock_inc_pack.zst", b"inc"),
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
        assert!(dest.join("u-boot/flipper-one/u-boot-rockchip.bin").is_file());
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
            builder.append_data(&mut header, "x/other.txt", &b"z"[..]).unwrap();
            builder.finish().unwrap();
        }
        let path = dir.join("bad.tar.zst");
        fs::write(&path, zstd::stream::encode_all(&tar_bytes[..], 1).unwrap()).unwrap();
        let err = read_manifest(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("no manifest.json"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_paths_that_escape() {
        assert_eq!(inner_path(Path::new("b/a.txt")).unwrap(), Path::new("a.txt"));
        assert_eq!(inner_path(Path::new("b/")), None);
        assert_eq!(inner_path(Path::new("b/../../etc/passwd")), None);
        assert_eq!(inner_path(Path::new("/etc/passwd")), None);
    }
}
