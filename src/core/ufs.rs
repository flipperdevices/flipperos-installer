//! UFS device descriptors over the kernel's SCSI BSG endpoint.
//!
//! This module is the transport and the wire format only — no policy about what
//! a *good* configuration looks like (that is [`crate::core::provision`]).
//!
//! The kernel exposes one BSG node per UFS host controller
//! (`/dev/bsg/ufs-bsg<host>`, `CONFIG_SCSI_UFS_BSG`) which passes a whole UPIU
//! through to the device. We use it for QUERY REQUEST UPIUs: reading and writing
//! descriptors and attributes, which is how a UFS device is provisioned. See
//! `include/uapi/scsi/scsi_bsg_ufs.h` and `drivers/ufs/core/ufs_bsg.c`, and
//! JESD220C-2.2 §10.7.8 (query UPIU) and §14.1.4 (the descriptors).
//!
//! Every multi-byte descriptor and UPIU field is big-endian, so the structs
//! below are byte arrays addressed by explicit offset rather than typed fields:
//! it keeps the wire layout visible next to the spec offsets and sidesteps
//! `#[repr]` questions entirely.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::{fs, thread, time::Duration};

pub type Result<T> = std::result::Result<T, String>;

// --- descriptor / attribute identifiers (JESD220C-2.2 Table 14-1, 14-2) -----

/// Device Descriptor.
pub const IDN_DEVICE: u8 = 0x00;
/// Configuration Descriptor — the only writable one, and what provisioning uses.
pub const IDN_CONFIGURATION: u8 = 0x01;
/// Unit Descriptor, indexed by LUN.
pub const IDN_UNIT: u8 = 0x02;
/// Geometry Descriptor.
pub const IDN_GEOMETRY: u8 = 0x07;

/// `bBootLunEn`: which boot LU the well-known BOOT LU resolves to (0 = disabled,
/// 1 = Boot LU A, 2 = Boot LU B).
pub const ATTR_BOOT_LUN_EN: u8 = 0x00;
/// `bConfigDescrLock`: 1 means the Configuration Descriptor can never be
/// written again.
pub const ATTR_CONFIG_DESCR_LOCK: u8 = 0x0B;

/// `fDeviceInit`: setting it starts the device's own initialisation; the device
/// clears it when done. This is what makes a freshly written Configuration
/// Descriptor take effect, and it is the step the mask ROM loader performs (via
/// `ufshcd_complete_dev_init` inside `_ufs_start`) instead of a power cycle.
pub const FLAG_DEVICE_INIT: u8 = 0x01;
/// `fPermanentWPEn`: with it set, an LU marked permanently write-protected can
/// never be written again.
pub const FLAG_PERMANENT_WP_EN: u8 = 0x02;
/// `fPowerOnWPEn`: LUs marked write-protected are read-only until the next power
/// cycle.
pub const FLAG_POWER_ON_WP_EN: u8 = 0x03;

/// `bMemoryType` values (JESD220C-2.2 Table 14-14). A device only offers the
/// subset its Geometry Descriptor advertises — see
/// [`GeometryDescriptor::supports_memory_type`].
pub const MEMORY_TYPE_NORMAL: u8 = 0x00;
pub const MEMORY_TYPE_SYSTEM_CODE: u8 = 0x01;
pub const MEMORY_TYPE_NON_PERSISTENT: u8 = 0x02;
pub const MEMORY_TYPE_ENHANCED1: u8 = 0x03;
pub const MEMORY_TYPE_ENHANCED2: u8 = 0x04;
pub const MEMORY_TYPE_ENHANCED3: u8 = 0x05;
pub const MEMORY_TYPE_ENHANCED4: u8 = 0x06;

/// Every memory type, with the `wSupportedMemoryTypes` bit that advertises it and
/// its name. One table so a type cannot be half-known: the parser, the capability
/// check and every message all draw on this.
///
/// `wSupportedMemoryTypes` also has an RPMB bit (15), but RPMB is a well-known
/// logical unit rather than something `bMemoryType` can select, so it is absent
/// here on purpose.
const MEMORY_TYPES: [(u8, u16, &str); 7] = [
    (MEMORY_TYPE_NORMAL, 1 << 0, "normal"),
    (MEMORY_TYPE_SYSTEM_CODE, 1 << 1, "system code"),
    (MEMORY_TYPE_NON_PERSISTENT, 1 << 2, "non-persistent"),
    (MEMORY_TYPE_ENHANCED1, 1 << 3, "enhanced1"),
    (MEMORY_TYPE_ENHANCED2, 1 << 4, "enhanced2"),
    (MEMORY_TYPE_ENHANCED3, 1 << 5, "enhanced3"),
    (MEMORY_TYPE_ENHANCED4, 1 << 6, "enhanced4"),
];

/// Name a `bMemoryType` value.
pub fn memory_type_name(code: u8) -> &'static str {
    MEMORY_TYPES
        .iter()
        .find(|(t, _, _)| *t == code)
        .map(|(_, _, name)| *name)
        .unwrap_or("unknown")
}

/// The `bMemoryType` a name stands for, for reading a configuration file.
pub fn memory_type_from_name(name: &str) -> Option<u8> {
    MEMORY_TYPES
        .iter()
        .find(|(_, _, n)| n.eq_ignore_ascii_case(name))
        .map(|(code, _, _)| *code)
}

/// Every memory type's name, for listing what a configuration file may say.
pub fn memory_type_names() -> impl Iterator<Item = &'static str> {
    MEMORY_TYPES.iter().map(|(_, _, name)| *name)
}

/// `bBootLunID` values.
pub const BOOT_LUN_NONE: u8 = 0x00;
pub const BOOT_LUN_A: u8 = 0x01;
pub const BOOT_LUN_B: u8 = 0x02;

/// `bProvisioningType`: thin provisioning enabled, TPRZ = 0.
pub const PROVISIONING_THIN: u8 = 0x02;

/// Name a `bBootLunID` / `bBootLunEn` value: which of the boot pair it is, or
/// that there is none.
pub fn boot_lu_name(id: u8) -> &'static str {
    match id {
        BOOT_LUN_A => "A",
        BOOT_LUN_B => "B",
        _ => "none",
    }
}

/// A Configuration Descriptor always carries eight logical-unit blocks; a device
/// supporting 32 LUs spreads LU 8..31 over descriptor indexes 1..3.
pub const LUS_PER_CONFIG_DESC: usize = 8;

/// `bMaxNumberLU` value meaning the device supports 32 logical units (`0x00` is
/// eight), i.e. Configuration Descriptor indexes 1..3 exist as well.
pub const MAX_NUMBER_LU_32: u8 = 0x01;

/// Where the kernel maps well-known logical units (`SCSI_W_LUN_BASE` in
/// `include/scsi/scsi.h`); everything below it is a data LU.
const SCSI_W_LUN_BASE: u32 = 0xc100;

/// Whether a SCSI LUN is one of the device's well-known logical units (BOOT,
/// RPMB, UFS DEVICE, REPORT LUNS) rather than a data LU.
fn is_well_known_lun(lun: u32) -> bool {
    lun & 0xff00 == SCSI_W_LUN_BASE
}

// --- BSG / SG_IO plumbing ---------------------------------------------------

/// `SG_IO` (`include/scsi/sg.h`).
const SG_IO: u64 = 0x2285;

/// `sg_io_v4.guard` — `'Q'`, distinguishing it from the v3 header.
const BSG_GUARD: i32 = b'Q' as i32;
const BSG_PROTOCOL_SCSI: u32 = 0;
const BSG_SUB_PROTOCOL_SCSI_TRANSPORT: u32 = 2;

/// UPIU transaction code for a QUERY REQUEST, and the `msgcode` the BSG driver
/// dispatches on.
const UPIU_TRANSACTION_QUERY_REQ: u32 = 0x16;

/// Query function values (JESD220C-2.2 Table 10-29).
const QUERY_FUNC_STANDARD_READ: u8 = 0x01;
const QUERY_FUNC_STANDARD_WRITE: u8 = 0x81;

/// Query opcodes (JESD220C-2.2 Table 10-31).
const OPCODE_READ_DESC: u8 = 0x01;
const OPCODE_WRITE_DESC: u8 = 0x02;
const OPCODE_READ_ATTR: u8 = 0x03;
const OPCODE_WRITE_ATTR: u8 = 0x04;
const OPCODE_READ_FLAG: u8 = 0x05;
const OPCODE_SET_FLAG: u8 = 0x06;
const OPCODE_CLEAR_FLAG: u8 = 0x07;

/// Largest descriptor the kernel will move in one query (`QUERY_DESC_MAX_SIZE`).
const QUERY_DESC_MAX_SIZE: u16 = 255;
/// `bLength` + `bDescriptorIDN`: enough to learn a descriptor's real length
/// before asking for all of it, which is how the kernel does it too.
const QUERY_DESC_HDR_SIZE: u16 = 2;

const QUERY_TIMEOUT_MS: u32 = 30_000;

/// `struct sg_io_v4` (`include/uapi/linux/bsg.h`). Plain `#[repr(C)]` reproduces
/// the kernel layout exactly on every target we build for.
#[repr(C)]
#[derive(Default)]
struct SgIoV4 {
    guard: i32,
    protocol: u32,
    subprotocol: u32,
    request_len: u32,
    request: u64,
    request_tag: u64,
    request_attr: u32,
    request_priority: u32,
    request_extra: u32,
    max_response_len: u32,
    response: u64,
    dout_iovec_count: u32,
    dout_xfer_len: u32,
    din_iovec_count: u32,
    din_xfer_len: u32,
    dout_xferp: u64,
    din_xferp: u64,
    timeout: u32,
    flags: u32,
    usr_ptr: u64,
    spare_in: u32,
    driver_status: u32,
    transport_status: u32,
    device_status: u32,
    retry_delay: u32,
    info: u32,
    duration: u32,
    response_len: u32,
    din_resid: i32,
    dout_resid: i32,
    generated_tag: u64,
    spare_out: u32,
    padding: u32,
}

/// `struct ufs_bsg_request`: a message code plus a whole `utp_upiu_req` (a
/// 12-byte UPIU header followed by a 20-byte transaction-specific area).
#[repr(C)]
struct BsgRequest {
    msgcode: u32,
    upiu: [u8; 32],
}

/// `struct ufs_bsg_reply`.
#[repr(C)]
struct BsgReply {
    /// Negative for an `-Exxx`, else the SCSI result word.
    result: i32,
    reply_payload_rcv_len: u32,
    upiu: [u8; 32],
}

// Offsets inside `utp_upiu_req` (JESD220C-2.2 §10.7.8, §10.7.9).
const UPIU_TRANSACTION_CODE: usize = 0;
const UPIU_QUERY_FUNCTION: usize = 5;
/// Query response code in a QUERY RESPONSE UPIU; non-zero means the device
/// refused the request.
const UPIU_RESPONSE: usize = 6;
/// Length of the UPIU's data segment — the descriptor itself, on a write.
const UPIU_DATA_SEGMENT_LENGTH: usize = 10;
const UPIU_QR_OPCODE: usize = 12;
const UPIU_QR_IDN: usize = 13;
const UPIU_QR_INDEX: usize = 14;
const UPIU_QR_SELECTOR: usize = 15;
const UPIU_QR_LENGTH: usize = 18;
const UPIU_QR_VALUE: usize = 20;

/// An open BSG endpoint for one UFS host controller.
pub struct Bsg {
    file: File,
    /// Kept for error messages.
    node: PathBuf,
}

impl Bsg {
    /// Open the BSG endpoint belonging to whole-disk `disk` (e.g. `/dev/sda`).
    pub fn open_for_disk(disk: &str) -> Result<Bsg> {
        let node = bsg_node(disk)?;
        Self::open(&node)
    }

    pub fn open(node: &Path) -> Result<Bsg> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| format!("open {}: {e}", node.display()))?;
        Ok(Bsg {
            file,
            node: node.to_path_buf(),
        })
    }

    /// Read a descriptor whole. Its real length comes from its own `bLength`,
    /// read first in a two-byte query, so we never ask a device for more than it
    /// has.
    pub fn read_descriptor(&self, idn: u8, index: u8) -> Result<Vec<u8>> {
        let mut header = [0u8; QUERY_DESC_HDR_SIZE as usize];
        self.query(
            OPCODE_READ_DESC,
            idn,
            index,
            QUERY_DESC_HDR_SIZE,
            Dir::In(&mut header),
            0,
        )?;
        let len = header[0];
        if len < QUERY_DESC_HDR_SIZE as u8 {
            return Err(format!(
                "descriptor {idn:#04x}[{index}] reports an impossible length {len}"
            ));
        }
        let len = u16::from(len).min(QUERY_DESC_MAX_SIZE);
        let mut buf = vec![0u8; len as usize];
        self.query(OPCODE_READ_DESC, idn, index, len, Dir::In(&mut buf), 0)?;
        Ok(buf)
    }

    /// Write a descriptor. Only the Configuration Descriptor and the OEM_ID
    /// string are writable, and only while `bConfigDescrLock` is clear.
    pub fn write_descriptor(&self, idn: u8, index: u8, bytes: &[u8]) -> Result<()> {
        let len = u16::try_from(bytes.len())
            .ok()
            .filter(|l| *l > 0 && *l <= QUERY_DESC_MAX_SIZE)
            .ok_or_else(|| {
                format!(
                    "descriptor payload of {} bytes is out of range",
                    bytes.len()
                )
            })?;
        self.query(OPCODE_WRITE_DESC, idn, index, len, Dir::Out(bytes), 0)?;
        Ok(())
    }

    /// Read a device attribute. Attributes have no data segment; the value comes
    /// back in the response UPIU.
    pub fn read_attr(&self, idn: u8) -> Result<u32> {
        let reply = self.query(OPCODE_READ_ATTR, idn, 0, 0, Dir::None, 0)?;
        Ok(u32::from_be_bytes(
            reply.upiu[UPIU_QR_VALUE..UPIU_QR_VALUE + 4]
                .try_into()
                .expect("4 bytes"),
        ))
    }

    pub fn write_attr(&self, idn: u8, value: u32) -> Result<()> {
        self.query(OPCODE_WRITE_ATTR, idn, 0, 0, Dir::None, value)?;
        Ok(())
    }

    /// Set or clear a device flag. Flags carry no value: the opcode says which way.
    pub fn write_flag(&self, idn: u8, on: bool) -> Result<()> {
        let opcode = if on {
            OPCODE_SET_FLAG
        } else {
            OPCODE_CLEAR_FLAG
        };
        self.query(opcode, idn, 0, 0, Dir::None, 0)?;
        Ok(())
    }

    /// Read a device flag. Like an attribute, the answer rides in the response
    /// UPIU rather than a data segment.
    pub fn read_flag(&self, idn: u8) -> Result<bool> {
        let reply = self.query(OPCODE_READ_FLAG, idn, 0, 0, Dir::None, 0)?;
        // A flag is the least significant byte of the returned value.
        Ok(reply.upiu[UPIU_QR_VALUE + 3] != 0)
    }

    /// Issue one QUERY REQUEST UPIU.
    fn query(
        &self,
        opcode: u8,
        idn: u8,
        index: u8,
        length: u16,
        dir: Dir<'_>,
        value: u32,
    ) -> Result<BsgReply> {
        let write = matches!(
            opcode,
            OPCODE_WRITE_DESC | OPCODE_WRITE_ATTR | OPCODE_SET_FLAG | OPCODE_CLEAR_FLAG
        );
        let mut request = BsgRequest {
            msgcode: UPIU_TRANSACTION_QUERY_REQ,
            upiu: [0u8; 32],
        };
        request.upiu[UPIU_TRANSACTION_CODE] = UPIU_TRANSACTION_QUERY_REQ as u8;
        request.upiu[UPIU_QUERY_FUNCTION] = if write {
            QUERY_FUNC_STANDARD_WRITE
        } else {
            QUERY_FUNC_STANDARD_READ
        };
        request.upiu[UPIU_QR_OPCODE] = opcode;
        request.upiu[UPIU_QR_IDN] = idn;
        request.upiu[UPIU_QR_INDEX] = index;
        request.upiu[UPIU_QR_SELECTOR] = 0;
        request.upiu[UPIU_QR_LENGTH..UPIU_QR_LENGTH + 2].copy_from_slice(&length.to_be_bytes());
        request.upiu[UPIU_QR_VALUE..UPIU_QR_VALUE + 4].copy_from_slice(&value.to_be_bytes());
        // A WRITE DESCRIPTOR carries the descriptor in the UPIU's data segment, so
        // the header has to say how long that segment is. The kernel sets it on
        // its *own* query path (`ufshcd_prepare_utp_query_req_upiu`) but not on the
        // raw BSG one (`ufshcd_issue_devman_upiu_cmd` copies our UPIU verbatim and
        // only appends the payload), so here it is ours to fill in — as ufs-utils
        // does. Leave it zero and the device receives a descriptor of no length and
        // answers "invalid value", having never seen the bytes.
        if matches!(dir, Dir::Out(_)) {
            request.upiu[UPIU_DATA_SEGMENT_LENGTH..UPIU_DATA_SEGMENT_LENGTH + 2]
                .copy_from_slice(&length.to_be_bytes());
        }

        let mut reply = BsgReply {
            result: 0,
            reply_payload_rcv_len: 0,
            upiu: [0u8; 32],
        };

        let mut hdr = SgIoV4 {
            guard: BSG_GUARD,
            protocol: BSG_PROTOCOL_SCSI,
            subprotocol: BSG_SUB_PROTOCOL_SCSI_TRANSPORT,
            request_len: std::mem::size_of::<BsgRequest>() as u32,
            request: &request as *const BsgRequest as u64,
            max_response_len: std::mem::size_of::<BsgReply>() as u32,
            response: &mut reply as *mut BsgReply as u64,
            timeout: QUERY_TIMEOUT_MS,
            ..SgIoV4::default()
        };
        // `bsg_transport_fill_hdr` maps whichever of dout/din is set into the
        // single `job->request_payload` that `ufs_bsg.c` uses for descriptor
        // traffic in *both* directions — so a read must go through `din_*`.
        match dir {
            Dir::None => {}
            Dir::In(ref buf) => {
                hdr.din_xfer_len = buf.len() as u32;
                hdr.din_xferp = buf.as_ptr() as u64;
            }
            Dir::Out(buf) => {
                hdr.dout_xfer_len = buf.len() as u32;
                hdr.dout_xferp = buf.as_ptr() as u64;
            }
        }

        // SAFETY: `hdr` is a correctly sized `sg_io_v4` whose embedded pointers
        // all refer to live locals that outlive the call, and the fd is open for
        // read+write on a BSG node.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            return Err(format!(
                "{}: query {opcode:#04x} idn {idn:#04x}[{index}]: {e}",
                self.node.display()
            ));
        }
        if reply.result != 0 {
            return Err(format!(
                "{}: query {opcode:#04x} idn {idn:#04x}[{index}] failed, result {}",
                self.node.display(),
                reply.result
            ));
        }
        let response = reply.upiu[UPIU_RESPONSE];
        if response != 0 {
            return Err(format!(
                "{}: query {opcode:#04x} idn {idn:#04x}[{index}] rejected by the device: \
                 {} ({response:#04x})",
                self.node.display(),
                query_response_name(response)
            ));
        }
        Ok(reply)
    }
}

/// Name a QUERY RESPONSE code, so a refusal says what the device objected to
/// rather than only that it did (JESD220C-2.2 §10.7.9.2; the same list the kernel
/// carries as `QUERY_RESULT_*`).
fn query_response_name(response: u8) -> &'static str {
    match response {
        0xF6 => "parameter not readable",
        0xF7 => "parameter not writeable",
        0xF8 => "parameter already written",
        0xF9 => "invalid length",
        0xFA => "invalid value",
        0xFB => "invalid selector",
        0xFC => "invalid index",
        0xFD => "invalid IDN",
        0xFE => "invalid opcode",
        0xFF => "general failure",
        _ => "unknown failure",
    }
}

/// Which way a query's data segment travels, if it has one.
enum Dir<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

/// The BSG node for the UFS host controller behind whole-disk `disk`.
///
/// The kernel names it `ufs-bsg<host_no>` under the `bsg` class, and every
/// logical unit of one device hangs off that host, so the SCSI host number in
/// the sysfs path is all we need. Falls back to a lone `/dev/bsg/ufs-bsg*` when
/// the path cannot be walked.
pub fn bsg_node(disk: &str) -> Result<PathBuf> {
    if let Some(host) = scsi_host_number(disk) {
        let node = PathBuf::from(format!("/dev/bsg/ufs-bsg{host}"));
        if node.exists() {
            return Ok(node);
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/bsg") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("ufs-bsg") {
                found.push(entry.path());
            }
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!(
            "no UFS BSG endpoint for {disk} (is CONFIG_SCSI_UFS_BSG enabled?)"
        )),
        n => Err(format!(
            "{n} UFS BSG endpoints present but none matches {disk}"
        )),
    }
}

/// The SCSI host number of whole-disk `disk`, from the `hostN` component of its
/// canonical sysfs path.
pub fn scsi_host_number(disk: &str) -> Option<u32> {
    let name = disk.trim_start_matches("/dev/");
    let real = fs::canonicalize(format!("/sys/block/{name}/device")).ok()?;
    real.components()
        .filter_map(|c| c.as_os_str().to_str())
        .find_map(|c| c.strip_prefix("host")?.parse().ok())
}

/// Make the device digest a freshly written Configuration Descriptor, and the
/// kernel notice the logical units it produced.
///
/// Setting `fDeviceInit` runs the device's own initialisation — the same step the
/// mask ROM loader performs after provisioning (`ufshcd_complete_dev_init` inside
/// `_ufs_start`) — and the device clears the flag when it has finished. The
/// logical units then have their new sizes, but the kernel is still holding the
/// capacities it read at boot, so they are dropped and rescanned.
///
/// Deliberately no `SG_SCSI_RESET`: on RK3576 a host reset goes through the error
/// handler and has been observed to leave the controller in `eh_fatal` with
/// `ufs_eh_wq` stuck, which would take the rest of an install down with it.
pub fn apply_configuration(disk: &str, bsg: &Bsg) -> Result<()> {
    bsg.write_flag(FLAG_DEVICE_INIT, true)?;
    // The device clears it when its internal configuration is complete.
    let deadline = std::time::Instant::now() + DEVICE_INIT_TIMEOUT;
    loop {
        match bsg.read_flag(FLAG_DEVICE_INIT) {
            Ok(false) => break,
            Ok(true) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50))
            }
            Ok(true) => return Err("device did not finish initialising".to_string()),
            Err(e) => return Err(format!("reading fDeviceInit: {e}")),
        }
    }
    rescan_logical_units(disk)?;
    if !wait_for_path(disk, RESCAN_TIMEOUT) {
        return Err(format!(
            "{disk} did not come back after the rescan; its logical units may have been              renamed"
        ));
    }
    Ok(())
}

/// How long to wait for the logical units to reappear after a rescan.
const RESCAN_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the device to clear `fDeviceInit`.
const DEVICE_INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Drop every logical unit of `disk` and let the SCSI host find them again, so
/// their capacities are re-read.
pub fn rescan_logical_units(disk: &str) -> Result<()> {
    let host =
        scsi_host_number(disk).ok_or_else(|| format!("cannot find the SCSI host of {disk}"))?;
    for lu in data_lu_dirs(disk) {
        let _ = fs::write(lu.join("delete"), "1\n");
    }
    fs::write(format!("/sys/class/scsi_host/host{host}/scan"), "- - -\n")
        .map_err(|e| format!("rescan host{host}: {e}"))?;
    Ok(())
}

/// The sysfs SCSI-device directories of the *data* logical units of `disk` — its
/// own and its siblings under the shared SCSI target.
///
/// Every data LU is included, not just the eight one Configuration Descriptor
/// covers: a device reporting `bMaxNumberLU = 0x01` can have LUs 8..31 as well,
/// and a rescan that skipped them would leave stale block devices behind.
///
/// What is excluded is the *well-known* logical units. They sit under the same
/// target (`0:0:0:49488` and friends) and look like any other SCSI device, but the
/// driver holds pointers to them — `hba->ufs_device_wlun` above all — and deleting
/// one makes the driver dereference freed memory: on RK3576 that produced a NULL
/// dereference in `rpm_drop_usage_count` from `ufshcd_err_handler`, killing the
/// error-handler worker. The kernel puts them at `SCSI_W_LUN_BASE` and above
/// (`include/scsi/scsi.h`), which is what separates them from a data LU.
fn data_lu_dirs(disk: &str) -> Vec<PathBuf> {
    let name = disk.trim_start_matches("/dev/");
    let Ok(scsi_dev) = fs::canonicalize(format!("/sys/block/{name}/device")) else {
        return Vec::new();
    };
    let Some(target) = scsi_dev.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(target) {
        for entry in entries.flatten() {
            // An LU directory is named `host:channel:target:lun` and carries a
            // `scsi_device` link; siblings like `power/` do not.
            if !entry.path().join("scsi_device").exists() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let lun: u32 = match name.rsplit(':').next().and_then(|l| l.parse().ok()) {
                Some(lun) => lun,
                // An unparseable name is not something to delete on a guess.
                None => continue,
            };
            if !is_well_known_lun(lun) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Wait until `path` shows up, polling for at most `timeout`.
pub fn wait_for_path(path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if Path::new(path).exists() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Hexdump `bytes` as `offset: xx xx …` lines, for logging a descriptor.
pub fn hexdump(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            format!("{:04x}: {}", i * 16, hex.join(" "))
        })
        .collect()
}

// --- descriptor accessors ---------------------------------------------------

fn u8_at(buf: &[u8], off: usize) -> u8 {
    buf.get(off).copied().unwrap_or(0)
}

fn u16_at(buf: &[u8], off: usize) -> u16 {
    match buf.get(off..off + 2) {
        Some(s) => u16::from_be_bytes([s[0], s[1]]),
        None => 0,
    }
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    match buf.get(off..off + 4) {
        Some(s) => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

fn u64_at(buf: &[u8], off: usize) -> u64 {
    match buf.get(off..off + 8) {
        Some(s) => u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]),
        None => 0,
    }
}

/// The fields of the Device Descriptor that provisioning needs
/// (JESD220C-2.2 Table 14-4).
#[derive(Clone, Debug)]
pub struct DeviceDescriptor {
    /// Number of configured (enabled) logical units.
    pub number_lu: u8,
    /// `bBootEnable` as the device currently reports it.
    pub boot_enable: u8,
    /// UFS version, BCD (`0x0310` = UFS 3.1).
    pub spec_version: u16,
    /// Offset of the LU 0 block inside a Configuration Descriptor.
    pub ud0_base_offset: u8,
    /// Size of one LU block inside a Configuration Descriptor.
    pub ud_config_p_length: u8,
    pub extended_features: u32,
}

impl DeviceDescriptor {
    pub fn parse(buf: &[u8]) -> Result<DeviceDescriptor> {
        if buf.len() < 0x1C {
            return Err(format!("device descriptor is only {} bytes", buf.len()));
        }
        let d = DeviceDescriptor {
            number_lu: u8_at(buf, 0x06),
            boot_enable: u8_at(buf, 0x08),
            spec_version: u16_at(buf, 0x10),
            ud0_base_offset: u8_at(buf, 0x1A),
            ud_config_p_length: u8_at(buf, 0x1B),
            extended_features: u32_at(buf, 0x4F),
        };
        if d.ud0_base_offset < 0x10 || d.ud_config_p_length < 0x10 {
            return Err(format!(
                "device reports an unusable configuration-descriptor layout \
                 (bUD0BaseOffset {:#04x}, bUDConfigPLength {:#04x})",
                d.ud0_base_offset, d.ud_config_p_length
            ));
        }
        Ok(d)
    }

    /// Whether the device supports WriteBooster (bit 8 of
    /// `dExtendedUFSFeaturesSupport`). Writing a non-zero WriteBooster parameter
    /// to a device without it is rejected outright.
    pub fn writebooster_supported(&self) -> bool {
        self.extended_features & (1 << 8) != 0
    }

    /// Total length of a Configuration Descriptor on this device.
    pub fn config_desc_len(&self) -> usize {
        self.ud0_base_offset as usize + LUS_PER_CONFIG_DESC * self.ud_config_p_length as usize
    }

    /// Whether an LU block is long enough to carry a per-LU WriteBooster buffer
    /// size (the field sits at `+0x16`, past the end of the short UFS 2.2 block).
    pub fn lu_block_has_writebooster(&self) -> bool {
        self.ud_config_p_length as usize >= CFG_LU_WB_ALLOC_UNITS + 4
    }
}

/// The fields of the Geometry Descriptor that provisioning needs
/// (JESD220C-2.2 Table 14-13).
#[derive(Clone, Debug)]
pub struct GeometryDescriptor {
    /// Total configurable capacity, in 512-byte units.
    pub total_raw_capacity: u64,
    /// `0x00` = 8 logical units, `0x01` = 32.
    pub max_number_lu: u8,
    /// Segment size in 512-byte units.
    pub segment_size: u32,
    /// Allocation unit size, in segments.
    pub allocation_unit_size: u8,
    /// Bit mask of the memory types the device can allocate.
    pub supported_memory_types: u16,
    /// Per memory type, how many allocation units may be given to it and what
    /// each costs in raw ones (times 256), indexed by `bMemoryType`. Normal memory
    /// publishes neither: it is 1:1 and bounded only by the device's capacity.
    max_alloc_units: [u32; MEMORY_TYPES.len()],
    cap_adj_fac: [u16; MEMORY_TYPES.len()],
    pub writebooster_max_alloc_units: u32,
    pub max_writebooster_lus: u8,
    pub writebooster_cap_adj_fac: u8,
    /// `0x00` reduction only, `0x01` preserve only, `0x02` either.
    pub writebooster_user_space_types: u8,
    /// `0x00` LU-dedicated only, `0x01` shared only, `0x02` both.
    pub writebooster_buffer_types: u8,
}

/// Where each memory type's `d…MaxNAllocU` / `w…CapAdjFac` pair sits in the
/// Geometry Descriptor, in `bMemoryType` order. Normal memory has no such pair —
/// hence the `None` — and the rest follow at a regular 6-byte stride from 0x20
/// (JESD220C-2.2 Table 14-13).
const MEMORY_TYPE_GEOMETRY_OFFSETS: [Option<usize>; MEMORY_TYPES.len()] = [
    None,       // normal
    Some(0x20), // system code
    Some(0x26), // non-persistent
    Some(0x2C), // enhanced1
    Some(0x32), // enhanced2
    Some(0x38), // enhanced3
    Some(0x3E), // enhanced4
];

impl GeometryDescriptor {
    pub fn parse(buf: &[u8]) -> Result<GeometryDescriptor> {
        if buf.len() < 0x21 {
            return Err(format!("geometry descriptor is only {} bytes", buf.len()));
        }
        let mut max_alloc_units = [0u32; MEMORY_TYPES.len()];
        let mut cap_adj_fac = [0u16; MEMORY_TYPES.len()];
        for (i, offset) in MEMORY_TYPE_GEOMETRY_OFFSETS.iter().enumerate() {
            if let Some(at) = offset {
                max_alloc_units[i] = u32_at(buf, *at);
                cap_adj_fac[i] = u16_at(buf, at + 4);
            }
        }
        let g = GeometryDescriptor {
            total_raw_capacity: u64_at(buf, 0x04),
            max_number_lu: u8_at(buf, 0x0C),
            segment_size: u32_at(buf, 0x0D),
            allocation_unit_size: u8_at(buf, 0x11),
            supported_memory_types: u16_at(buf, 0x1E),
            max_alloc_units,
            cap_adj_fac,
            writebooster_max_alloc_units: u32_at(buf, 0x4F),
            max_writebooster_lus: u8_at(buf, 0x53),
            writebooster_cap_adj_fac: u8_at(buf, 0x54),
            writebooster_user_space_types: u8_at(buf, 0x55),
            writebooster_buffer_types: u8_at(buf, 0x56),
        };
        if g.segment_size == 0 || g.allocation_unit_size == 0 {
            return Err("device reports a zero allocation unit size".to_string());
        }
        Ok(g)
    }

    /// Allocation unit size in 512-byte units — the denominator of every
    /// capacity calculation.
    pub fn alloc_unit_sectors(&self) -> u64 {
        self.segment_size as u64 * self.allocation_unit_size as u64
    }

    pub fn alloc_unit_bytes(&self) -> u64 {
        self.alloc_unit_sectors() * 512
    }

    /// Total allocation units the host may hand out across all logical units.
    pub fn total_alloc_units(&self) -> u64 {
        self.total_raw_capacity / self.alloc_unit_sectors()
    }

    /// Index of `memory_type` in the per-type tables, or `None` for a value the
    /// spec does not define.
    fn memory_type_index(memory_type: u8) -> Option<usize> {
        MEMORY_TYPES.iter().position(|(t, _, _)| *t == memory_type)
    }

    /// Raw allocation units consumed per usable allocation unit of `memory_type`.
    /// Normal memory is 1:1; the others cost their published capacity adjustment
    /// factor (3 for Enhanced1 on every device we have seen).
    pub fn cap_adj_fac(&self, memory_type: u8) -> Result<u64> {
        let index = Self::memory_type_index(memory_type)
            .ok_or_else(|| format!("unknown memory type {memory_type:#04x}"))?;
        if memory_type == MEMORY_TYPE_NORMAL {
            return Ok(1);
        }
        match self.cap_adj_fac[index] / 256 {
            0 => Err(format!(
                "device reports a zero {} capacity adjustment factor",
                memory_type_name(memory_type)
            )),
            f => Ok(u64::from(f)),
        }
    }

    /// How many allocation units may be given to `memory_type`, or `None` when the
    /// device publishes no ceiling for it (Normal memory, bounded only by
    /// [`Self::total_alloc_units`]). Some parts allow far less than the whole
    /// device: the Foresee 64 GB caps Enhanced1 at about half of it.
    pub fn max_alloc_units(&self, memory_type: u8) -> Option<u32> {
        let index = Self::memory_type_index(memory_type)?;
        MEMORY_TYPE_GEOMETRY_OFFSETS[index].map(|_| self.max_alloc_units[index])
    }

    pub fn supports_memory_type(&self, memory_type: u8) -> bool {
        MEMORY_TYPES
            .iter()
            .find(|(t, _, _)| *t == memory_type)
            .map(|(_, bit, _)| self.supported_memory_types & bit != 0)
            .unwrap_or(false)
    }

    /// Whether the device can keep the WriteBooster buffer out of the
    /// configurable user space (`0x01` preserve only, `0x02` either).
    pub fn writebooster_can_preserve_user_space(&self) -> bool {
        matches!(self.writebooster_user_space_types, 0x01 | 0x02)
    }

    /// Whether a single shared WriteBooster buffer is available (`0x01` shared
    /// only, `0x02` either).
    pub fn writebooster_supports_shared(&self) -> bool {
        matches!(self.writebooster_buffer_types, 0x01 | 0x02)
    }
}

// Offsets inside a Configuration Descriptor header (JESD220C-2.2 Table 14-10).
const CFG_LENGTH: usize = 0x00;
const CFG_DESCRIPTOR_IDN: usize = 0x01;
const CFG_CONF_DESC_CONTINUE: usize = 0x02;
const CFG_BOOT_ENABLE: usize = 0x03;
const CFG_DESCR_ACCESS_EN: usize = 0x04;
const CFG_INIT_POWER_MODE: usize = 0x05;
const CFG_HIGH_PRIORITY_LUN: usize = 0x06;
const CFG_SECURE_REMOVAL_TYPE: usize = 0x07;
const CFG_INIT_ACTIVE_ICC_LEVEL: usize = 0x08;
const CFG_PERIODIC_RTC_UPDATE: usize = 0x09;
const CFG_WB_PRESERVE_USER_SPACE: usize = 0x10;
const CFG_WB_BUFFER_TYPE: usize = 0x11;
const CFG_WB_SHARED_ALLOC_UNITS: usize = 0x12;

// Offsets inside one LU block (JESD220C-2.2 Table 14-12).
const CFG_LU_ENABLE: usize = 0x00;
const CFG_LU_BOOT_LUN_ID: usize = 0x01;
const CFG_LU_WRITE_PROTECT: usize = 0x02;
const CFG_LU_MEMORY_TYPE: usize = 0x03;
const CFG_LU_NUM_ALLOC_UNITS: usize = 0x04;
const CFG_LU_DATA_RELIABILITY: usize = 0x08;
const CFG_LU_LOGICAL_BLOCK_SIZE: usize = 0x09;
const CFG_LU_PROVISIONING_TYPE: usize = 0x0A;
const CFG_LU_CONTEXT_CAPABILITIES: usize = 0x0B;
const CFG_LU_WB_ALLOC_UNITS: usize = 0x16;

/// One logical unit's user-configurable parameters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LuConfig {
    pub enable: u8,
    pub boot_lun_id: u8,
    pub write_protect: u8,
    pub memory_type: u8,
    pub num_alloc_units: u32,
    pub data_reliability: u8,
    pub logical_block_size: u8,
    pub provisioning_type: u8,
    pub context_capabilities: u16,
    pub writebooster_alloc_units: u32,
}

impl LuConfig {
    pub fn enabled(&self) -> bool {
        self.enable != 0
    }

    /// Usable capacity in bytes, given the device geometry.
    pub fn size_bytes(&self, geo: &GeometryDescriptor) -> u64 {
        let adj = geo.cap_adj_fac(self.memory_type).unwrap_or(1);
        self.num_alloc_units as u64 * geo.alloc_unit_bytes() / adj
    }
}

/// A Configuration Descriptor: the raw bytes plus typed access to the header and
/// the eight LU blocks. This is both what we compare against the wanted scheme
/// and what we write back, so it deliberately owns its buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDescriptor {
    bytes: Vec<u8>,
    base: usize,
    stride: usize,
}

impl ConfigDescriptor {
    /// Wrap raw descriptor bytes, using the layout the Device Descriptor
    /// reports. The 22-byte header with 26-byte LU blocks is what UFS 3.x parts
    /// use; UFS 2.2 parts have 16 bytes of each, so nothing may be hardcoded.
    pub fn new(bytes: Vec<u8>, dev: &DeviceDescriptor) -> Result<ConfigDescriptor> {
        let want = dev.config_desc_len();
        if bytes.len() < want {
            return Err(format!(
                "configuration descriptor is {} bytes, expected {want}",
                bytes.len()
            ));
        }
        Ok(ConfigDescriptor {
            bytes,
            base: dev.ud0_base_offset as usize,
            stride: dev.ud_config_p_length as usize,
        })
    }

    /// A zeroed descriptor of the right shape for this device, carrying only
    /// `bLength` and `bDescriptorIDN`.
    pub fn empty(dev: &DeviceDescriptor) -> ConfigDescriptor {
        let len = dev.config_desc_len();
        let mut bytes = vec![0u8; len];
        bytes[CFG_LENGTH] = len as u8;
        bytes[CFG_DESCRIPTOR_IDN] = IDN_CONFIGURATION;
        ConfigDescriptor {
            bytes,
            base: dev.ud0_base_offset as usize,
            stride: dev.ud_config_p_length as usize,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn lu_offset(&self, lu: usize) -> usize {
        self.base + lu * self.stride
    }

    /// Whether this layout carries the per-LU WriteBooster buffer size field.
    fn has_lu_writebooster(&self) -> bool {
        self.stride >= CFG_LU_WB_ALLOC_UNITS + 4
    }

    // Header getters/setters.

    pub fn boot_enable(&self) -> u8 {
        self.bytes[CFG_BOOT_ENABLE]
    }

    pub fn conf_desc_continue(&self) -> u8 {
        self.bytes[CFG_CONF_DESC_CONTINUE]
    }

    pub fn set_conf_desc_continue(&mut self, value: u8) {
        self.bytes[CFG_CONF_DESC_CONTINUE] = value;
    }

    pub fn descr_access_en(&self) -> u8 {
        self.bytes[CFG_DESCR_ACCESS_EN]
    }

    pub fn init_power_mode(&self) -> u8 {
        self.bytes[CFG_INIT_POWER_MODE]
    }

    pub fn high_priority_lun(&self) -> u8 {
        self.bytes[CFG_HIGH_PRIORITY_LUN]
    }

    pub fn secure_removal_type(&self) -> u8 {
        self.bytes[CFG_SECURE_REMOVAL_TYPE]
    }

    pub fn init_active_icc_level(&self) -> u8 {
        self.bytes[CFG_INIT_ACTIVE_ICC_LEVEL]
    }

    pub fn periodic_rtc_update(&self) -> u16 {
        u16_at(&self.bytes, CFG_PERIODIC_RTC_UPDATE)
    }

    pub fn writebooster_preserve_user_space(&self) -> u8 {
        self.bytes[CFG_WB_PRESERVE_USER_SPACE]
    }

    pub fn writebooster_buffer_type(&self) -> u8 {
        self.bytes[CFG_WB_BUFFER_TYPE]
    }

    pub fn writebooster_shared_alloc_units(&self) -> u32 {
        u32_at(&self.bytes, CFG_WB_SHARED_ALLOC_UNITS)
    }

    /// Set the header fields the host owns. The values are the ones Rockchip's
    /// loader writes, so a device provisioned by either agrees with the other.
    #[allow(clippy::too_many_arguments)]
    pub fn set_header(
        &mut self,
        boot_enable: u8,
        descr_access_en: u8,
        init_power_mode: u8,
        high_priority_lun: u8,
        secure_removal_type: u8,
        init_active_icc_level: u8,
        periodic_rtc_update: u16,
    ) {
        self.bytes[CFG_BOOT_ENABLE] = boot_enable;
        self.bytes[CFG_DESCR_ACCESS_EN] = descr_access_en;
        self.bytes[CFG_INIT_POWER_MODE] = init_power_mode;
        self.bytes[CFG_HIGH_PRIORITY_LUN] = high_priority_lun;
        self.bytes[CFG_SECURE_REMOVAL_TYPE] = secure_removal_type;
        self.bytes[CFG_INIT_ACTIVE_ICC_LEVEL] = init_active_icc_level;
        self.bytes[CFG_PERIODIC_RTC_UPDATE..CFG_PERIODIC_RTC_UPDATE + 2]
            .copy_from_slice(&periodic_rtc_update.to_be_bytes());
    }

    pub fn set_shared_writebooster(&mut self, preserve_user_space: u8, alloc_units: u32) {
        self.bytes[CFG_WB_PRESERVE_USER_SPACE] = preserve_user_space;
        // `bWriteBoosterBufferType` 0x01 selects the single shared buffer.
        self.bytes[CFG_WB_BUFFER_TYPE] = 0x01;
        self.bytes[CFG_WB_SHARED_ALLOC_UNITS..CFG_WB_SHARED_ALLOC_UNITS + 4]
            .copy_from_slice(&alloc_units.to_be_bytes());
    }

    pub fn set_lu_dedicated_writebooster(&mut self, preserve_user_space: u8) {
        self.bytes[CFG_WB_PRESERVE_USER_SPACE] = preserve_user_space;
        self.bytes[CFG_WB_BUFFER_TYPE] = 0x00;
    }

    // LU getters/setters.

    pub fn lu(&self, lu: usize) -> LuConfig {
        let at = self.lu_offset(lu);
        LuConfig {
            enable: u8_at(&self.bytes, at + CFG_LU_ENABLE),
            boot_lun_id: u8_at(&self.bytes, at + CFG_LU_BOOT_LUN_ID),
            write_protect: u8_at(&self.bytes, at + CFG_LU_WRITE_PROTECT),
            memory_type: u8_at(&self.bytes, at + CFG_LU_MEMORY_TYPE),
            num_alloc_units: u32_at(&self.bytes, at + CFG_LU_NUM_ALLOC_UNITS),
            data_reliability: u8_at(&self.bytes, at + CFG_LU_DATA_RELIABILITY),
            logical_block_size: u8_at(&self.bytes, at + CFG_LU_LOGICAL_BLOCK_SIZE),
            provisioning_type: u8_at(&self.bytes, at + CFG_LU_PROVISIONING_TYPE),
            context_capabilities: u16_at(&self.bytes, at + CFG_LU_CONTEXT_CAPABILITIES),
            writebooster_alloc_units: if self.has_lu_writebooster() {
                u32_at(&self.bytes, at + CFG_LU_WB_ALLOC_UNITS)
            } else {
                0
            },
        }
    }

    pub fn set_lu(&mut self, lu: usize, cfg: &LuConfig) {
        let at = self.lu_offset(lu);
        self.bytes[at + CFG_LU_ENABLE] = cfg.enable;
        self.bytes[at + CFG_LU_BOOT_LUN_ID] = cfg.boot_lun_id;
        self.bytes[at + CFG_LU_WRITE_PROTECT] = cfg.write_protect;
        self.bytes[at + CFG_LU_MEMORY_TYPE] = cfg.memory_type;
        self.bytes[at + CFG_LU_NUM_ALLOC_UNITS..at + CFG_LU_NUM_ALLOC_UNITS + 4]
            .copy_from_slice(&cfg.num_alloc_units.to_be_bytes());
        self.bytes[at + CFG_LU_DATA_RELIABILITY] = cfg.data_reliability;
        self.bytes[at + CFG_LU_LOGICAL_BLOCK_SIZE] = cfg.logical_block_size;
        self.bytes[at + CFG_LU_PROVISIONING_TYPE] = cfg.provisioning_type;
        self.bytes[at + CFG_LU_CONTEXT_CAPABILITIES..at + CFG_LU_CONTEXT_CAPABILITIES + 2]
            .copy_from_slice(&cfg.context_capabilities.to_be_bytes());
        if self.has_lu_writebooster() {
            self.bytes[at + CFG_LU_WB_ALLOC_UNITS..at + CFG_LU_WB_ALLOC_UNITS + 4]
                .copy_from_slice(&cfg.writebooster_alloc_units.to_be_bytes());
        }
    }

    /// Every LU block, in order.
    pub fn lus(&self) -> Vec<LuConfig> {
        (0..LUS_PER_CONFIG_DESC).map(|i| self.lu(i)).collect()
    }
}

/// The fields of a Unit Descriptor worth reporting (JESD220C-2.2 Table 14-14).
/// Read back only for logging: the Configuration Descriptor is the authority on
/// what was asked for, this is what the device actually built.
#[derive(Clone, Copy, Debug)]
pub struct UnitDescriptor {
    pub lun: u8,
    pub boot_lun_id: u8,
    pub memory_type: u8,
    pub logical_block_size: u8,
    pub logical_block_count: u64,
}

impl UnitDescriptor {
    pub fn parse(buf: &[u8]) -> Result<UnitDescriptor> {
        if buf.len() < 0x13 {
            return Err(format!("unit descriptor is only {} bytes", buf.len()));
        }
        Ok(UnitDescriptor {
            lun: u8_at(buf, 0x02),
            boot_lun_id: u8_at(buf, 0x04),
            memory_type: u8_at(buf, 0x08),
            logical_block_size: u8_at(buf, 0x0A),
            logical_block_count: u64_at(buf, 0x0B),
        })
    }

    pub fn size_bytes(&self) -> u64 {
        self.logical_block_count << self.logical_block_size
    }
}

/// Read `disk`'s size in bytes from sysfs. Used to check a boot LU is big
/// enough before anything is written to it.
pub fn block_size_bytes(node: &str) -> Option<u64> {
    let name = node.trim_start_matches("/dev/");
    let text = fs::read_to_string(format!("/sys/block/{name}/size")).ok()?;
    Some(text.trim().parse::<u64>().ok()? * 512)
}

/// Write `value` to a sysfs attribute, ignoring a missing file.
pub fn write_sysfs(path: &Path, value: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(value.as_bytes())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Descriptor dumps from real parts, as hex. Biwin 256 GB (UFS 3.1), Biwin
    /// 64 GB and Foresee 64 GB (both UFS 2.2). Kept as text so no binary blobs
    /// enter the tree.
    pub(crate) const BIWIN256_DEVICE: &str = "59000000000004040100017f0001080003100525000102030dab161a040000bf0a20001020013dc2aa1204010000000000000000000000000000000000000000020000000000000000000000000000000001bf010100001800";
    pub(crate) const BIWIN256_GEOMETRY: &str = "57070000000000001dcbc00001000020000108008040402000000500000980090000000000000000000000000000ee5e0300000000000000000000000000000000000000000000000f200f3b9700000000180001030101";
    pub(crate) const BIWIN256_CONFIG: &str = "e601000100017f000000000000000000010100001800010000000000ee52010c020000000000000000000000000000000101000300000003010c020000000000000000000000000000000102000300000003010c020000000000000000000000000000000100000300000006010c020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    pub(crate) const BIWIN64_DEVICE: &str = "59000000000004040100017f0001050002201904000102030dab161a020000070a20275220003b9a001204000000000000000000000000000000000000000000000000000000000000000000000000000021070101000008e8";
    pub(crate) const BIWIN64_GEOMETRY: &str = "570700000000000007734000010000200001080080404020000005000001800900000000000000000000000000003b9a03000000000000000000000000000000000000000000000000000000000000000008e801030102";
    pub(crate) const FORESEE64_DEVICE: &str = "59000000000001040000017f0001050002200625000102030bd6161a020000830320000020003b9c001104000000000000000000000000000000000000000000020000000000000000000000000000c0020983000000000000";
    pub(crate) const FORESEE64_GEOMETRY: &str = "570700000000000007738000010000200001088080404040000005000009800900000000000000000000000000001d3e0300000000000000000000000000000000000000000000000f200f1000000000000ee701030102";
    pub(crate) const FORESEE64_CONFIG: &str = "e601000000017f0000000000000000000000000000000100000000003b9c000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c00000000000000000000000000000000";
    /// The same Foresee part after being provisioned to the Flipper scheme by
    /// hand: LU1/LU2 at 12 allocation units and LU3 at 96, which is exactly what
    /// [`crate::core::provision`] must compute.
    pub(crate) const FORESEE64_CONFIG_PROVISIONED: &str = "e601000100017f0000000000000000000101000005dc0100000000003548010c02000000000000000000000000000000010100030000000c010c02000000000000000000000000000000010200030000000c010c020000000000000000000000000000000100000300000060010c020000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000c00000000000000000000000000000000";

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
        assert!(
            s.len().is_multiple_of(2),
            "hex string must have an even length"
        );
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    #[test]
    fn sg_io_v4_matches_the_kernel_layout() {
        // `struct sg_io_v4` is 160 bytes on every LP64 target; a mismatch here
        // means the ioctl would be rejected or misread.
        assert_eq!(std::mem::size_of::<SgIoV4>(), 160);
        assert_eq!(std::mem::size_of::<BsgRequest>(), 36);
        assert_eq!(std::mem::size_of::<BsgReply>(), 40);
    }

    #[test]
    fn device_descriptors_parse() {
        for (name, hex, spec, lus) in [
            ("biwin 256G", BIWIN256_DEVICE, 0x0310u16, 4u8),
            ("biwin 64G", BIWIN64_DEVICE, 0x0220, 4),
            ("foresee 64G", FORESEE64_DEVICE, 0x0220, 1),
        ] {
            let dev = DeviceDescriptor::parse(&unhex(hex)).expect(name);
            assert_eq!(dev.spec_version, spec, "{name}");
            assert_eq!(dev.number_lu, lus, "{name}");
            // Every part we have uses the long configuration-descriptor layout:
            // a 22-byte header with 26-byte LU blocks, 230 bytes in total.
            assert_eq!(dev.ud0_base_offset, 0x16, "{name}");
            assert_eq!(dev.ud_config_p_length, 0x1A, "{name}");
            assert_eq!(dev.config_desc_len(), 230, "{name}");
            assert!(dev.lu_block_has_writebooster(), "{name}");
            assert!(dev.writebooster_supported(), "{name}");
        }
        // The Foresee ships with the boot feature disabled, which would leave a
        // board unbootable, so provisioning has to turn it on.
        let foresee = DeviceDescriptor::parse(&unhex(FORESEE64_DEVICE)).unwrap();
        assert_eq!(foresee.boot_enable, 0);
        let biwin = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        assert_eq!(biwin.boot_enable, 1);
    }

    #[test]
    fn geometry_descriptors_parse() {
        // (name, hex, total allocation units, WriteBooster maximum, buffer types)
        for (name, hex, total, wb_max, wb_types) in [
            ("biwin 256G", BIWIN256_GEOMETRY, 61022u64, 6144u32, 0x01u8),
            ("biwin 64G", BIWIN64_GEOMETRY, 15258, 2280, 0x02),
            ("foresee 64G", FORESEE64_GEOMETRY, 15260, 3815, 0x02),
        ] {
            let geo = GeometryDescriptor::parse(&unhex(hex)).expect(name);
            // 4 MiB allocation units everywhere: 8192 sectors of 512 bytes.
            assert_eq!(geo.alloc_unit_sectors(), 8192, "{name}");
            assert_eq!(geo.alloc_unit_bytes(), 4 * 1024 * 1024, "{name}");
            assert_eq!(geo.total_alloc_units(), total, "{name}");
            // The capacity is an exact multiple of the allocation unit, which is
            // what lets LU 0's size be computed rather than guessed.
            assert_eq!(
                geo.total_raw_capacity % geo.alloc_unit_sectors(),
                0,
                "{name}"
            );
            assert_eq!(geo.max_number_lu, MAX_NUMBER_LU_32, "{name}");
            assert!(geo.supports_memory_type(MEMORY_TYPE_NORMAL), "{name}");
            assert!(geo.supports_memory_type(MEMORY_TYPE_ENHANCED1), "{name}");
            assert_eq!(geo.cap_adj_fac(MEMORY_TYPE_NORMAL).unwrap(), 1, "{name}");
            assert_eq!(geo.cap_adj_fac(MEMORY_TYPE_ENHANCED1).unwrap(), 3, "{name}");
            assert_eq!(geo.writebooster_max_alloc_units, wb_max, "{name}");
            assert_eq!(geo.writebooster_buffer_types, wb_types, "{name}");
            // Both datasheets state the parts support "preserve user space"
            // only, so the buffer never costs configurable capacity.
            assert_eq!(geo.writebooster_user_space_types, 0x01, "{name}");
            assert!(geo.writebooster_can_preserve_user_space(), "{name}");
            assert!(geo.writebooster_supports_shared(), "{name}");
            assert_eq!(geo.max_writebooster_lus, 1, "{name}");
        }
    }

    #[test]
    fn every_memory_type_is_named_and_parsed() {
        // One table drives the name, the capability bit and the geometry pair, so
        // no type can be half-known.
        for (code, name) in [
            (MEMORY_TYPE_NORMAL, "normal"),
            (MEMORY_TYPE_SYSTEM_CODE, "system code"),
            (MEMORY_TYPE_NON_PERSISTENT, "non-persistent"),
            (MEMORY_TYPE_ENHANCED1, "enhanced1"),
            (MEMORY_TYPE_ENHANCED2, "enhanced2"),
            (MEMORY_TYPE_ENHANCED3, "enhanced3"),
            (MEMORY_TYPE_ENHANCED4, "enhanced4"),
        ] {
            assert_eq!(memory_type_name(code), name);
            assert_eq!(memory_type_from_name(name), Some(code));
            // A configuration file may spell it in any case.
            assert_eq!(memory_type_from_name(&name.to_uppercase()), Some(code));
        }
        assert_eq!(memory_type_names().count(), 7);
        assert_eq!(memory_type_name(0x07), "unknown");
        assert_eq!(memory_type_from_name("enhanced5"), None);

        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();
        // Normal memory publishes no ceiling of its own: it is bounded by the
        // device's capacity, which `total_alloc_units` covers.
        assert_eq!(geo.max_alloc_units(MEMORY_TYPE_NORMAL), None);
        assert_eq!(geo.cap_adj_fac(MEMORY_TYPE_NORMAL).unwrap(), 1);
        // Enhanced1 is the only other type these parts offer.
        assert_eq!(geo.max_alloc_units(MEMORY_TYPE_ENHANCED1), Some(61022));
        assert_eq!(geo.cap_adj_fac(MEMORY_TYPE_ENHANCED1).unwrap(), 3);
        for code in [
            MEMORY_TYPE_SYSTEM_CODE,
            MEMORY_TYPE_NON_PERSISTENT,
            MEMORY_TYPE_ENHANCED2,
            MEMORY_TYPE_ENHANCED3,
            MEMORY_TYPE_ENHANCED4,
        ] {
            assert!(
                !geo.supports_memory_type(code),
                "{}",
                memory_type_name(code)
            );
            assert_eq!(geo.max_alloc_units(code), Some(0));
            // An unsupported type publishes no factor, and asking for one says
            // which type is at fault rather than a bare number.
            let err = geo.cap_adj_fac(code).expect_err("no published factor");
            assert!(err.contains(memory_type_name(code)), "{err}");
        }
        // A value the spec does not define is not silently treated as normal.
        assert!(geo.cap_adj_fac(0x07).is_err());
        assert_eq!(geo.max_alloc_units(0x07), None);
    }

    #[test]
    fn factory_configuration_descriptor_parses() {
        let dev = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();
        let cfg = ConfigDescriptor::new(unhex(BIWIN256_CONFIG), &dev).unwrap();

        assert_eq!(cfg.boot_enable(), 1);
        assert_eq!(cfg.high_priority_lun(), 0x7F);
        assert_eq!(cfg.init_power_mode(), 1);
        assert_eq!(cfg.writebooster_preserve_user_space(), 1);
        assert_eq!(cfg.writebooster_buffer_type(), 1);
        assert_eq!(cfg.writebooster_shared_alloc_units(), 6144);

        // Rockchip's stock scheme: a big Normal LU 0, 4 MiB boot LUs A and B and
        // an 8 MiB spare, all Enhanced1.
        let lus = cfg.lus();
        assert_eq!(lus[0].memory_type, MEMORY_TYPE_NORMAL);
        assert_eq!(lus[0].num_alloc_units, 61010);
        assert_eq!(lus[1].boot_lun_id, BOOT_LUN_A);
        assert_eq!(lus[1].memory_type, MEMORY_TYPE_ENHANCED1);
        assert_eq!(lus[1].num_alloc_units, 3);
        assert_eq!(lus[1].size_bytes(&geo), 4 * 1024 * 1024);
        assert_eq!(lus[2].boot_lun_id, BOOT_LUN_B);
        assert_eq!(lus[2].num_alloc_units, 3);
        assert_eq!(lus[3].boot_lun_id, BOOT_LUN_NONE);
        assert_eq!(lus[3].num_alloc_units, 6);
        assert_eq!(lus[3].size_bytes(&geo), 8 * 1024 * 1024);
        for lu in &lus[0..4] {
            assert!(lu.enabled());
            assert_eq!(lu.logical_block_size, 0x0C, "4 KiB blocks");
            assert_eq!(lu.provisioning_type, PROVISIONING_THIN);
            assert_eq!(lu.data_reliability, 1);
            assert_eq!(lu.write_protect, 0);
        }
        for lu in &lus[4..] {
            assert!(!lu.enabled());
        }
        // The factory allocation accounts for the whole configurable capacity,
        // and the WriteBooster buffer sits outside it.
        let allocated: u64 = lus.iter().map(|l| l.num_alloc_units as u64).sum();
        assert_eq!(allocated, geo.total_alloc_units());
    }

    #[test]
    fn config_descriptor_round_trips_through_setters() {
        for hex in [BIWIN256_DEVICE, BIWIN64_DEVICE, FORESEE64_DEVICE] {
            let dev = DeviceDescriptor::parse(&unhex(hex)).unwrap();
            let mut cfg = ConfigDescriptor::empty(&dev);
            assert_eq!(cfg.as_bytes().len(), dev.config_desc_len());
            assert_eq!(cfg.as_bytes()[CFG_LENGTH] as usize, dev.config_desc_len());
            assert_eq!(cfg.as_bytes()[CFG_DESCRIPTOR_IDN], IDN_CONFIGURATION);

            cfg.set_header(1, 0, 1, 0x7F, 0, 0, 0);
            cfg.set_shared_writebooster(1, 6144);
            let want = LuConfig {
                enable: 1,
                boot_lun_id: BOOT_LUN_B,
                write_protect: 0,
                memory_type: MEMORY_TYPE_ENHANCED1,
                num_alloc_units: 12,
                data_reliability: 1,
                logical_block_size: 0x0C,
                provisioning_type: PROVISIONING_THIN,
                context_capabilities: 0,
                writebooster_alloc_units: 0,
            };
            cfg.set_lu(2, &want);
            assert_eq!(cfg.lu(2), want);
            assert_eq!(cfg.boot_enable(), 1);
            assert_eq!(cfg.high_priority_lun(), 0x7F);
            assert_eq!(cfg.writebooster_shared_alloc_units(), 6144);
            // Neighbours untouched.
            assert!(!cfg.lu(1).enabled());
            assert!(!cfg.lu(3).enabled());
        }
    }

    #[test]
    fn short_unit_blocks_have_no_per_lu_writebooster() {
        // A UFS 2.2 part with a 16-byte header and 16-byte LU blocks: the per-LU
        // WriteBooster field lives at +0x16, past the end of the block.
        let mut device = unhex(BIWIN64_DEVICE);
        device[0x1A] = 0x10;
        device[0x1B] = 0x10;
        let dev = DeviceDescriptor::parse(&device).unwrap();
        assert_eq!(dev.config_desc_len(), 0x90);
        assert!(!dev.lu_block_has_writebooster());

        let mut cfg = ConfigDescriptor::empty(&dev);
        let mut want = LuConfig {
            enable: 1,
            memory_type: MEMORY_TYPE_NORMAL,
            num_alloc_units: 1234,
            logical_block_size: 0x0C,
            writebooster_alloc_units: 99,
            ..LuConfig::default()
        };
        cfg.set_lu(7, &want);
        // Everything but the unrepresentable WriteBooster size survives, and no
        // write ran past the end of the block.
        want.writebooster_alloc_units = 0;
        assert_eq!(cfg.lu(7), want);
        assert_eq!(cfg.as_bytes().len(), 0x90);
    }

    #[test]
    fn only_data_logical_units_may_be_deleted() {
        // The LUNs this Foresee part actually presents: four data LUs and three
        // well-known ones. Deleting a well-known LU takes `hba->ufs_device_wlun`
        // out from under the driver, which cost a kernel NULL dereference in
        // `rpm_drop_usage_count` before this rule existed.
        for lun in [0, 1, 2, 3] {
            assert!(!is_well_known_lun(lun), "LUN {lun} is a data LU");
        }
        for lun in [49456, 49476, 49488] {
            assert!(is_well_known_lun(lun), "LUN {lun} is well-known");
        }
        // A device reporting 32 logical units has data LUs well past the eight one
        // Configuration Descriptor covers, and a rescan has to include them.
        for lun in [8, 15, 16, 23, 24, 31, 0x7F] {
            assert!(!is_well_known_lun(lun), "LUN {lun} is a data LU");
        }
        // The whole 0xc1xx page is well-known, per SCSI_W_LUN_BASE.
        assert!(is_well_known_lun(SCSI_W_LUN_BASE));
        assert!(is_well_known_lun(SCSI_W_LUN_BASE + 0xff));
        assert!(!is_well_known_lun(SCSI_W_LUN_BASE - 1));
        assert!(!is_well_known_lun(SCSI_W_LUN_BASE + 0x100));
    }

    #[test]
    fn hexdump_is_offset_prefixed() {
        let lines = hexdump(&[0u8, 1, 2]);
        assert_eq!(lines, vec!["0000: 00 01 02".to_string()]);
        let lines = hexdump(&[0u8; 17]);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].starts_with("0010: 00"));
    }
}
