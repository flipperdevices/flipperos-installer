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

use std::fs;
use std::path::Path;

use crate::core::model::McuInfo;

/// The firmware upload class device the driver registers.
pub const FW_CLASS_DIR: &str = "/sys/class/firmware/flipper-one-mcu";

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

/// A sysfs attribute's value without its trailing newline, or `None` when it is
/// absent, unreadable or empty.
fn read_attr(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
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
            fs::write(dir.join("loading"), "0\n").unwrap();
            for (attr, value) in attrs {
                fs::write(dir.join("device").join(attr), value).unwrap();
            }
            Self(dir)
        }
    }

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
}
