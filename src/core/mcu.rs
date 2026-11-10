// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The Flipper One MCU and its firmware, as the kernel's `flipper-one-mcu-update`
//! driver exposes them.
//!
//! The driver registers with the firmware upload class, so an image is uploaded
//! through `/sys/class/firmware/flipper-one-mcu/` (see the kernel's
//! `sysfs-class-firmware` ABI), and adds two attributes of its own to the
//! platform device behind it:
//!
//! - `target`: the MCU firmware build target matching this board revision
//!   (`f1`, `f2`), which names the directory its images are published under.
//!   The driver cannot tell from an image which target it was built for, so this
//!   is the only thing that picks one. Absent on a revision the driver does not
//!   know.
//! - `firmware_version`: what the MCU runs now, as `major.minor.patch`.
//!
//! Discovery starts from the class device and reaches the platform device
//! through its `device` link, rather than listing the driver's directory under
//! `/sys/bus/platform/drivers/`, which also holds entries that are not devices.
//!
//! An upload is `1 > loading`, the image into `data`, then `0 > loading`, after
//! which the driver works through `status` — `preparing` while the MCU erases its
//! spare slot, `transferring`, `programming` while it checks the image — and back
//! to `idle`, leaving `error` empty on success. The MCU keeps running the image
//! it booted until it restarts, and keeps it as the fallback should the new one
//! not verify, so a failed upload leaves it exactly as it was.
//!
//! Two rules follow from the hardware. The image is checked against its
//! manifest digest *before* `0 > loading`, and a bad one is dropped with
//! `-1 > loading`, which discards the data before the driver ever sees it. And
//! the upload is never cancelled through `cancel`: an abort that reaches the MCU
//! during the erase has been seen to wedge the CPU–MCU I2C link until the MCU
//! loses power.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::core::controller::{Config, Controller};
use crate::core::fetch;
use crate::core::model::{human_bytes, ImageFile, McuInfo};

type Result<T> = std::result::Result<T, String>;

/// The firmware upload class device the driver registers.
pub const FW_CLASS_DIR: &str = "/sys/class/firmware/flipper-one-mcu";

/// The image both sources publish in each per-target directory. The driver also
/// takes a raw binary, but the UF2 carries what it needs to check the image
/// before the MCU erases anything.
pub const IMAGE_NAME: &str = "flipperone-mcu-firmware.uf2";

/// What the log calls the image.
const WHAT: &str = "MCU firmware";

/// How often `status` is read back while the driver works.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long the driver is given to finish. A backstop only: it times out the
/// erase, every block and the final check on its own, and an update takes about
/// 25 s. Running into this is reported, not cancelled — see the module docs.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(180);

/// Find the board's MCU, if the running kernel can update its firmware. `None`
/// means it cannot: no driver, or a board without this MCU.
pub fn detect() -> Option<McuInfo> {
    detect_at(Path::new(FW_CLASS_DIR))
}

/// [`detect`] against a class device at `class_dir`, so it can be exercised on
/// a fake sysfs tree.
fn detect_at(class_dir: &Path) -> Option<McuInfo> {
    // `loading` is what an upload starts with, so it is what decides whether
    // there is anything here to update through.
    if !class_dir.join("loading").exists() {
        return None;
    }
    let device = class_dir.join("device");
    Some(McuInfo {
        target: read_attr(&device.join("target")),
        version: read_attr(&device.join("firmware_version"))
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Upload `image` to the MCU and wait for the driver to finish with it.
///
/// `on_progress` sees the share of the update done so far, from 0 to 1, while
/// the image is transferred to the MCU. On success the image is staged in the
/// MCU's spare slot and runs once the MCU restarts; restarting it is not this
/// function's business.
pub fn upload(
    cfg: &Config,
    ctrl: &Controller,
    image: &ImageFile,
    on_progress: &mut dyn FnMut(f32),
) -> Result<()> {
    if cfg.dry_run {
        ctrl.log(format!(
            "[dry-run] would upload {} ({}) to {FW_CLASS_DIR}",
            image.location,
            human_bytes(image.size_bytes)
        ));
        return Ok(());
    }
    ctrl.log(format!(
        "uploading {} ({}) to {FW_CLASS_DIR}",
        image.location,
        human_bytes(image.size_bytes)
    ));
    let reader = fetch::open(&image.location, &image.source)?;
    upload_at(
        Path::new(FW_CLASS_DIR),
        ctrl,
        reader,
        image.sha256.as_deref(),
        Pacing {
            poll: POLL_INTERVAL,
            deadline: UPLOAD_DEADLINE,
        },
        on_progress,
    )
}

/// How [`upload_at`] waits for the driver: real time on a device, none at all
/// against a fake sysfs tree.
struct Pacing {
    poll: Duration,
    deadline: Duration,
}

/// [`upload`] through a class device at `class_dir`, from an already opened
/// image, so it can be exercised on a fake sysfs tree.
fn upload_at(
    class_dir: &Path,
    ctrl: &Controller,
    image: impl Read,
    expect: Option<&str>,
    pacing: Pacing,
    on_progress: &mut dyn FnMut(f32),
) -> Result<()> {
    // The driver refuses a second upload while one runs, but only once the whole
    // image has been written; asking first gives a clearer answer, sooner.
    let status = read_attr(&class_dir.join("status")).unwrap_or_default();
    if status != "idle" {
        return Err(format!(
            "another MCU firmware update is already under way ({status})"
        ));
    }

    write_attr(class_dir, "loading", "1")?;
    let mut sha = fetch::Sha256::new();
    let sent = match send(class_dir, image, &mut sha) {
        Ok(sent) => sent,
        Err(e) => {
            discard(class_dir, ctrl);
            return Err(e);
        }
    };

    // Checked before the driver is handed anything: unlike an image written to
    // the target, a bad one can still be dropped here, whatever the fetch mode.
    let verdict = fetch::verify(sha, expect);
    match verdict.message(WHAT) {
        Some(msg) => ctrl.log(msg.clone()),
        None => ctrl.log(format!("{WHAT}: sha256 verified")),
    }
    if let fetch::Verdict::Mismatch(..) = verdict {
        discard(class_dir, ctrl);
        return Err(verdict.message(WHAT).unwrap_or_default());
    }

    // From here the driver owns the update. This write fails outright if it
    // could not start one, and otherwise moves `status` off `idle` before it
    // returns, so waiting for `idle` cannot mistake the start for the end.
    write_attr(class_dir, "loading", "0")?;
    wait_until_idle(class_dir, ctrl, sent, &pacing, on_progress)?;
    match read_attr(&class_dir.join("error")) {
        None => Ok(()),
        // `<status the update had reached>:<what went wrong>`.
        Some(e) => Err(match e.split_once(':') {
            Some((stage, code)) => format!("the update failed while {stage}: {code}"),
            None => format!("the update failed: {e}"),
        }),
    }
}

/// Write the image into `data`, hashing it on the way, and return its size.
fn send(class_dir: &Path, image: impl Read, sha: &mut fetch::Sha256) -> Result<u64> {
    let path = class_dir.join("data");
    let mut data = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut reader = fetch::Digesting { inner: image, sha };
    io::copy(&mut reader, &mut data).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Drop whatever was written to `data` without starting an update. Best
/// effort: it runs on the way out of a failure that is already being reported.
fn discard(class_dir: &Path, ctrl: &Controller) {
    if let Err(e) = write_attr(class_dir, "loading", "-1") {
        ctrl.log(format!("warning: {e}"));
    }
}

/// Follow `status` until the driver is done, logging each stage and reporting
/// the transfer's progress from `remaining_size`.
fn wait_until_idle(
    class_dir: &Path,
    ctrl: &Controller,
    size: u64,
    pacing: &Pacing,
    on_progress: &mut dyn FnMut(f32),
) -> Result<()> {
    let started = Instant::now();
    let mut last = String::new();
    loop {
        std::thread::sleep(pacing.poll);
        let status = read_attr(&class_dir.join("status")).unwrap_or_default();
        if status == "idle" {
            on_progress(1.0);
            return Ok(());
        }
        if status != last {
            if let Some(what) = describe(&status) {
                ctrl.log(format!("{WHAT}: {what}"));
            }
            last = status.clone();
        }
        if status == "transferring" && size > 0 {
            if let Some(remaining) =
                read_attr(&class_dir.join("remaining_size")).and_then(|r| r.parse::<u64>().ok())
            {
                on_progress(1.0 - remaining.min(size) as f32 / size as f32);
            }
        }
        if started.elapsed() > pacing.deadline {
            return Err(format!(
                "the update is still {status} after {} s; it is left to finish rather than \
                 cancelled, since cancelling can wedge the MCU's I2C link",
                pacing.deadline.as_secs()
            ));
        }
    }
}

/// What a firmware upload `status` means for this MCU, for the log.
fn describe(status: &str) -> Option<String> {
    Some(match status {
        // Over in an instant, while the driver takes the image in.
        "receiving" => return None,
        "preparing" => "the MCU is erasing its spare slot".to_string(),
        "transferring" => "sending the image".to_string(),
        "programming" => "the MCU is checking the image".to_string(),
        other => other.to_string(),
    })
}

/// A sysfs attribute's value without its trailing newline, or `None` when it is
/// absent, unreadable or empty.
fn read_attr(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Write `value` to the attribute `name` of the class device.
fn write_attr(class_dir: &Path, name: &str, value: &str) -> Result<()> {
    let path = class_dir.join(name);
    fs::write(&path, value).map_err(|e| format!("write {value} to {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fake class device that removes itself.
    struct FakeClass(PathBuf);

    impl FakeClass {
        /// A class device whose platform device carries `attrs`.
        fn new(name: &str, attrs: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("flipperos-mcu-{name}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("device")).unwrap();
            for (attr, value) in [
                ("loading", "0\n"),
                ("data", ""),
                ("status", "idle\n"),
                ("remaining_size", "0\n"),
                ("error", ""),
            ] {
                fs::write(dir.join(attr), value).unwrap();
            }
            for (attr, value) in attrs {
                fs::write(dir.join("device").join(attr), value).unwrap();
            }
            Self(dir)
        }

        fn read(&self, attr: &str) -> String {
            fs::read_to_string(self.0.join(attr)).unwrap()
        }

        fn set(&self, attr: &str, value: &str) {
            fs::write(self.0.join(attr), value).unwrap();
        }

        /// Upload `image` against this tree, expecting `digest`, with no waiting.
        fn upload(&self, image: &[u8], digest: Option<&str>) -> (Result<()>, Vec<f32>) {
            let ctrl = Controller::new(Config {
                automount: false,
                ..Config::default()
            });
            let mut seen = Vec::new();
            let result = upload_at(
                &self.0,
                &ctrl,
                image,
                digest,
                Pacing {
                    poll: Duration::ZERO,
                    deadline: Duration::ZERO,
                },
                &mut |p| seen.push(p),
            );
            (result, seen)
        }
    }

    /// SHA-256 of `bytes`, as a manifest would publish it.
    fn digest_of(bytes: &[u8]) -> String {
        let mut sha = fetch::Sha256::new();
        sha.update(bytes);
        sha.finish_hex()
    }

    const IMAGE: &[u8] = b"UF2\nnot really an image, but the driver is not here to mind";

    impl Drop for FakeClass {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_the_target_and_version_off_the_platform_device() {
        let class = FakeClass::new(
            "known",
            &[("target", "f1\n"), ("firmware_version", "0.1.0\n")],
        );
        assert_eq!(
            detect_at(&class.0),
            Some(McuInfo {
                target: Some("f1".into()),
                version: "0.1.0".into(),
            })
        );
    }

    #[test]
    fn an_unknown_board_revision_has_no_target() {
        // The driver leaves `target` out on a revision it does not know. The MCU
        // is still there, it just has nothing to pick an image by.
        let class = FakeClass::new("unknown", &[("firmware_version", "0.0.0\n")]);
        let mcu = detect_at(&class.0).expect("an MCU");
        assert_eq!(mcu.target, None);
        assert_eq!(mcu.version, "0.0.0");
    }

    #[test]
    fn no_upload_interface_means_no_mcu() {
        let class = FakeClass::new("absent", &[("target", "f2\n")]);
        fs::remove_file(class.0.join("loading")).unwrap();
        assert_eq!(detect_at(&class.0), None);
        assert_eq!(detect_at(&class.0.join("nonexistent")), None);
    }

    #[test]
    fn a_verified_image_is_handed_to_the_driver() {
        let class = FakeClass::new("upload", &[]);
        let (result, seen) = class.upload(IMAGE, Some(&digest_of(IMAGE)));
        result.unwrap();
        assert_eq!(class.read("data").as_bytes(), IMAGE);
        // `0 > loading` is what starts the update.
        assert_eq!(class.read("loading"), "0");
        assert_eq!(seen.last(), Some(&1.0));
        // Nothing ever cancels an update, not even a successful one.
        assert!(!class.0.join("cancel").exists());
    }

    #[test]
    fn an_image_that_does_not_match_its_digest_never_reaches_the_driver() {
        let class = FakeClass::new("mismatch", &[]);
        let (result, _) = class.upload(IMAGE, Some(&"0".repeat(64)));
        let err = result.unwrap_err();
        assert!(err.contains("sha256 mismatch for MCU firmware"), "{err}");
        // `-1 > loading` discards the data instead of starting an update.
        assert_eq!(class.read("loading"), "-1");
        assert!(!class.0.join("cancel").exists());
    }

    #[test]
    fn an_unpublished_digest_does_not_stop_the_update() {
        let class = FakeClass::new("unpublished", &[]);
        let (result, _) = class.upload(IMAGE, None);
        result.unwrap();
        assert_eq!(class.read("loading"), "0");
    }

    #[test]
    fn an_update_already_under_way_is_left_alone() {
        let class = FakeClass::new("busy", &[]);
        class.set("status", "transferring\n");
        let (result, _) = class.upload(IMAGE, Some(&digest_of(IMAGE)));
        let err = result.unwrap_err();
        assert!(err.contains("already under way (transferring)"), "{err}");
        // Neither started over nor discarded.
        assert_eq!(class.read("loading"), "0\n");
        assert_eq!(class.read("data"), "");
    }

    #[test]
    fn the_drivers_verdict_is_reported() {
        let class = FakeClass::new("refused", &[]);
        class.set("error", "preparing:firmware-invalid\n");
        let (result, _) = class.upload(IMAGE, Some(&digest_of(IMAGE)));
        assert_eq!(
            result.unwrap_err(),
            "the update failed while preparing: firmware-invalid"
        );
    }
}
