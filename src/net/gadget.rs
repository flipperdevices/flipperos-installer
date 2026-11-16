// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The USB NCM gadget, set up through configfs.
//!
//! The RK3576 has a single USB device controller, the DWC3 behind the
//! dual-role Type-C port, so the gadget binds to whichever UDC the kernel
//! offers unless told otherwise. The port's data role follows the Type-C
//! partner: a host on the other end makes the board a device, and the gadget
//! comes alive; a USB stick makes it a host again, and the gadget just waits.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, String>;

/// Where configfs is mounted.
const CONFIGFS: &str = "/sys/kernel/config";
/// The gadget's directory under [`CONFIGFS`].
const GADGET: &str = "usb_gadget/flipperos";
/// The NCM function instance, and the configuration it is linked into.
const FUNCTION: &str = "functions/ncm.usb0";
const CONFIG: &str = "configs/c.1";
/// US English, the only language the strings are given in.
const LANG: &str = "strings/0x409";

/// Flipper Devices' USB vendor ID.
const VENDOR_ID: u16 = 0x37c1;

/// How the device describes itself, together with the product ID registered
/// for that description — the two must change together.
#[derive(Clone, Copy, Debug)]
struct Identity {
    class: u8,
    subclass: u8,
    protocol: u8,
    product: u16,
}

impl Identity {
    /// A single-function CDC device, the way the kernel's own `g_ncm` presents
    /// itself.
    const CDC_NCM: Identity = Identity {
        class: 0x02,
        subclass: 0x00,
        protocol: 0x00,
        product: 0xf121,
    };

    /// A composite device, its interfaces grouped by an IAD
    /// (`0xEF/0x02/0x01`). For a host that will not bind NCM otherwise.
    #[allow(dead_code)]
    const COMPOSITE: Identity = Identity {
        class: 0xef,
        subclass: 0x02,
        protocol: 0x01,
        product: 0xf12f,
    };
}

const IDENTITY: Identity = Identity::CDC_NCM;

/// How long the NCM function may take to name its network interface once
/// bound to the controller.
const IFNAME_TIMEOUT: Duration = Duration::from_secs(3);

/// A bound gadget.
#[derive(Clone, Debug)]
pub struct Gadget {
    /// The device controller it is bound to, e.g. `23000000.usb`.
    pub udc: String,
    /// The board's network interface on the link, e.g. `usb0`.
    pub ifname: String,
}

/// Create the NCM gadget and bind it to `udc`, or to the only controller there
/// is. Replaces a gadget of ours left by a previous run of the installer.
pub fn setup(udc: Option<&str>) -> Result<Gadget> {
    mount_configfs()?;
    let root = Path::new(CONFIGFS).join("usb_gadget");
    if !root.is_dir() {
        return Err("the kernel has no USB gadget support in configfs \
             (CONFIG_USB_CONFIGFS, CONFIG_USB_CONFIGFS_NCM)"
            .to_string());
    }
    let udc = match udc {
        Some(name) => name.to_string(),
        None => only_udc()?,
    };

    let g = Path::new(CONFIGFS).join(GADGET);
    if g.exists() {
        teardown(&g)?;
    }
    create(&g).inspect_err(|_| {
        let _ = teardown(&g);
    })?;
    write(&g.join("UDC"), &udc)?;

    let ifname = wait_for_ifname(&g.join(FUNCTION))?;
    Ok(Gadget { udc, ifname })
}

/// Mount configfs where the kernel expects it, unless something already is.
fn mount_configfs() -> Result<()> {
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    if mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(CONFIGFS))
    {
        return Ok(());
    }
    let c = |s: &str| std::ffi::CString::new(s).unwrap();
    let (src, target, fstype) = (c("configfs"), c(CONFIGFS), c("configfs"));
    // SAFETY: plain C strings that outlive the call, and no mount data.
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "mount configfs on {CONFIGFS}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// The name of the one USB device controller, or why there is not exactly one.
fn only_udc() -> Result<String> {
    let mut names: Vec<String> = fs::read_dir("/sys/class/udc")
        .map_err(|e| format!("list /sys/class/udc: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    match names.len() {
        0 => Err("no USB device controller (is the port's controller in OTG mode?)".to_string()),
        1 => Ok(names.remove(0)),
        _ => Err(format!(
            "several USB device controllers ({}); pick one with --usb-udc",
            names.join(", ")
        )),
    }
}

fn create(g: &Path) -> Result<()> {
    mkdir(g)?;
    write(&g.join("idVendor"), &format!("0x{VENDOR_ID:04x}"))?;
    write(&g.join("idProduct"), &format!("0x{:04x}", IDENTITY.product))?;
    write(&g.join("bcdDevice"), "0x0100")?;
    write(&g.join("bcdUSB"), "0x0200")?;
    write(
        &g.join("bDeviceClass"),
        &format!("0x{:02x}", IDENTITY.class),
    )?;
    write(
        &g.join("bDeviceSubClass"),
        &format!("0x{:02x}", IDENTITY.subclass),
    )?;
    write(
        &g.join("bDeviceProtocol"),
        &format!("0x{:02x}", IDENTITY.protocol),
    )?;

    let serial = board_serial();
    let strings = g.join(LANG);
    mkdir(&strings)?;
    write(&strings.join("manufacturer"), "Flipper Devices")?;
    write(&strings.join("product"), "FlipperOS Installer")?;
    write(&strings.join("serialnumber"), &serial)?;

    let function = g.join(FUNCTION);
    mkdir(&function)?;
    let (dev, host) = link_macs(&serial);
    write(&function.join("dev_addr"), &dev)?;
    write(&function.join("host_addr"), &host)?;

    let config = g.join(CONFIG);
    mkdir(&config)?;
    write(&config.join("MaxPower"), "100")?;
    let config_strings = config.join(LANG);
    mkdir(&config_strings)?;
    write(&config_strings.join("configuration"), "NCM")?;
    let link = config.join("ncm.usb0");
    symlink(&function, &link).map_err(|e| format!("link {}: {e}", link.display()))?;
    Ok(())
}

/// Unbind and dismantle a gadget, innermost first: configfs refuses to remove
/// a directory something still points into.
fn teardown(g: &Path) -> Result<()> {
    // Unbinding a gadget that is not bound fails harmlessly.
    let _ = fs::write(g.join("UDC"), "\n");
    let config = g.join(CONFIG);
    if let Ok(entries) = fs::read_dir(&config) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    let functions: Vec<PathBuf> = fs::read_dir(g.join("functions"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    let configs: Vec<PathBuf> = fs::read_dir(g.join("configs"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    for c in &configs {
        let _ = fs::remove_dir(c.join(LANG));
        let _ = fs::remove_dir(c);
    }
    for f in &functions {
        let _ = fs::remove_dir(f);
    }
    let _ = fs::remove_dir(g.join(LANG));
    fs::remove_dir(g).map_err(|e| format!("remove the old gadget {}: {e}", g.display()))
}

/// Wait for the NCM function to register its network interface, which it
/// names only once bound (`usb%d` until then).
fn wait_for_ifname(function: &Path) -> Result<String> {
    let deadline = Instant::now() + IFNAME_TIMEOUT;
    loop {
        let name = fs::read_to_string(function.join("ifname"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !name.is_empty()
            && !name.contains('%')
            && Path::new("/sys/class/net").join(&name).exists()
        {
            return Ok(name);
        }
        if Instant::now() > deadline {
            return Err("the NCM function never named its network interface".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Where the RK3576 keeps its CPU ID: 16 bytes at offset 0x0a of the OTP.
const OTP: &str = "/sys/bus/nvmem/devices/rockchip-otp0/nvmem";
const CPUID_OFFSET: usize = 0x0a;
const CPUID_LEN: usize = 16;

/// The board's serial number, for the USB serial string and the MAC addresses.
///
/// U-Boot puts it in the device tree, but not every boot path goes through
/// U-Boot's: a kernel started from the Falcon boot menu sees no
/// `serial-number`. The serial is derived from the CPU ID in the OTP, though,
/// so it is derived again here the same way, and the board shows up under the
/// same serial (and in its hostname) whichever way it booted.
fn board_serial() -> String {
    let raw = fs::read("/proc/device-tree/serial-number").unwrap_or_default();
    let s = String::from_utf8_lossy(&raw)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    if !s.is_empty() {
        return s;
    }
    fs::read(OTP)
        .ok()
        .and_then(|otp| {
            otp.get(CPUID_OFFSET..CPUID_OFFSET + CPUID_LEN)
                .map(serial_from_cpuid)
        })
        .unwrap_or_else(|| "0".to_string())
}

/// U-Boot's `serial#` from a Rockchip CPU ID (`rockchip_cpuid_set()`): a CRC32
/// over the odd bytes, chained into one over the even bytes, as 16 hex digits,
/// high half first.
fn serial_from_cpuid(cpuid: &[u8]) -> String {
    let odd: Vec<u8> = cpuid.iter().skip(1).step_by(2).copied().collect();
    let even: Vec<u8> = cpuid.iter().step_by(2).copied().collect();
    let lo = crc32_no_comp(0, &odd);
    let hi = crc32_no_comp(lo, &even);
    format!("{hi:08x}{lo:08x}")
}

/// U-Boot's `crc32_no_comp`: reflected CRC-32 without the initial and final
/// complement.
fn crc32_no_comp(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Stable MAC addresses for the two ends of the link, derived from the board's
/// serial number: the same board shows up as the same network adapter on every
/// boot, so the host keeps whatever settings it gave it.
///
/// Both are locally administered unicast addresses, distinct from each other.
fn link_macs(serial: &str) -> (String, String) {
    let digest = ring::digest::digest(&ring::digest::SHA256, serial.as_bytes());
    let h = digest.as_ref();
    let mac = |bytes: &[u8]| {
        let mut m = [0u8; 6];
        m.copy_from_slice(bytes);
        // Locally administered, unicast.
        m[0] = (m[0] & 0xfc) | 0x02;
        m.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    let dev = mac(&h[0..6]);
    let mut host = mac(&h[6..12]);
    if host == dev {
        host = mac(&h[12..18]);
    }
    (dev, host)
}

fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(|e| format!("create {}: {e}", path.display()))
}

fn write(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_macs_are_stable_local_and_distinct() {
        let (dev, host) = link_macs("8e2a5b00c1d2e3f4");
        assert_eq!(link_macs("8e2a5b00c1d2e3f4"), (dev.clone(), host.clone()));
        assert_ne!(dev, host);
        for mac in [&dev, &host] {
            assert_eq!(mac.len(), 17);
            let first = u8::from_str_radix(&mac[..2], 16).unwrap();
            assert_eq!(first & 0x03, 0x02, "{mac} must be local unicast");
        }
        assert_ne!(link_macs("another board").0, dev);
    }

    #[test]
    fn derives_the_serial_from_the_cpu_id_as_u_boot_does() {
        // flipperone-ae8b42: the CPU ID in its OTP, and the serial U-Boot (and
        // the image's own rk3576_cpu_serial.sh) computes from it.
        let cpuid = [
            0x4e, 0x59, 0x35, 0x37, 0x56, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c,
            0x28, 0x24,
        ];
        assert_eq!(serial_from_cpuid(&cpuid), "ae8b426ab71bd8ec");
    }

    #[test]
    fn the_product_id_goes_with_the_device_class() {
        // A composite description needs the composite product ID, and the
        // single-function one its own.
        assert_eq!(Identity::CDC_NCM.class, 0x02);
        assert_eq!(Identity::CDC_NCM.product, 0xf121);
        assert_eq!(Identity::COMPOSITE.class, 0xef);
        assert_eq!(Identity::COMPOSITE.product, 0xf12f);
    }
}
