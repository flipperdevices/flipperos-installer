//! UFS provisioning: the Flipper logical-unit scheme, and bringing a device to it.
//!
//! A UFS device ships divided into logical units by its manufacturer, and that
//! division changes only when the Configuration Descriptor is written —
//! destructively, since the device then rebuilds its whole mapping. The scheme we
//! want lives in `config/flipperos-ufs.toml`; this module compares a device
//! against it and rewrites the descriptor when asked to.
//!
//! The wire format and the transport are [`crate::core::ufs`]; everything here is
//! policy. The reference for that policy is Rockchip's downstream USB-plug
//! loader (`drivers/ufs/ufs-rockchip-usbplug.c` in `rockchip-linux/u-boot`),
//! which provisions the same devices from the mask ROM: it builds the descriptor
//! from zero, writes it, re-runs device initialisation without a power cycle and
//! then sets `bBootLunEn`. We do the same, with larger boot LUs and a recovery
//! LU, so a device provisioned by either agrees with the other.

use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

use crate::core::controller::Config;
use crate::core::model::human_bytes;
use crate::core::storage;
use crate::core::ufs::{self, ConfigDescriptor, DeviceDescriptor, GeometryDescriptor, LuConfig};
use crate::core::Controller;

pub type Result<T> = std::result::Result<T, String>;

/// The scheme compiled into the installer.
const DEFAULT_SCHEME: &str = include_str!("../../config/flipperos-ufs.toml");

// --- the scheme -------------------------------------------------------------

/// A complete provisioning scheme: the device-wide settings plus one entry per
/// logical unit that should exist.
#[derive(Clone, Debug, Deserialize)]
pub struct Scheme {
    /// `bBootEnable`. Without it the boot ROM cannot read the bootloader from a
    /// boot LU at all.
    #[serde(default = "default_true")]
    pub boot_enable: bool,
    /// Which boot LU `bBootLunEn` selects once the device has been provisioned:
    /// `A`, `B` or `none`. Only the starting point — the installer moves it
    /// between the pair on every bootloader update, so the comparison accepts
    /// either of the LUs the scheme flags as bootable.
    #[serde(default = "default_active_boot_lu")]
    pub active_boot_lu: String,
    #[serde(default)]
    pub writebooster: WriteBoosterScheme,
    #[serde(default, rename = "lu")]
    pub lus: Vec<SchemeLu>,
}

/// WriteBooster settings. The buffer costs no configurable capacity on any part
/// we have (they all support "preserve user space" only), so the default asks for
/// the device maximum.
#[derive(Clone, Debug, Deserialize)]
pub struct WriteBoosterScheme {
    #[serde(default = "default_wb_size")]
    pub size: WbSize,
    #[serde(default = "default_wb_type")]
    pub r#type: String,
}

impl Default for WriteBoosterScheme {
    fn default() -> Self {
        Self {
            size: default_wb_size(),
            r#type: default_wb_type(),
        }
    }
}

/// `size = "max"` or `size = <MiB>`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WbSize {
    Mib(u64),
    Keyword(String),
}

/// One logical unit of the scheme.
#[derive(Clone, Debug, Deserialize)]
pub struct SchemeLu {
    pub id: u8,
    #[serde(default)]
    pub name: String,
    /// `normal` or `enhanced1`.
    pub memory_type: String,
    /// Usable size. Omitted on exactly one LU, which takes what is left.
    #[serde(default)]
    pub size_mib: Option<u64>,
    /// `A` or `B` when this LU is one of the boot pair.
    #[serde(default)]
    pub boot_lun: Option<String>,
    #[serde(default = "default_true")]
    pub data_reliability: bool,
    /// `thick`, `thin` or `thin-read-zeros`.
    #[serde(default = "default_provisioning")]
    pub provisioning: String,
    #[serde(default = "default_logical_block_size")]
    pub logical_block_size: u32,
}

fn default_true() -> bool {
    true
}

fn default_active_boot_lu() -> String {
    "A".to_string()
}

fn default_wb_size() -> WbSize {
    WbSize::Keyword("max".to_string())
}

fn default_wb_type() -> String {
    "shared".to_string()
}

fn default_provisioning() -> String {
    "thin".to_string()
}

fn default_logical_block_size() -> u32 {
    4096
}

impl Scheme {
    pub fn parse(text: &str) -> Result<Scheme> {
        let scheme: Scheme =
            toml::from_str(text).map_err(|e| format!("parsing ufs scheme: {e}"))?;
        scheme.validate()?;
        Ok(scheme)
    }

    /// The scheme compiled into the installer.
    pub fn embedded_default() -> Scheme {
        Self::parse(DEFAULT_SCHEME).expect("built-in ufs scheme must be valid")
    }

    /// Resolve the scheme to use, and a short description of where it came from.
    /// A scheme file that cannot be read or parsed falls back to the built-in one
    /// with the reason folded into the description, which is what gets logged.
    pub fn resolve(cfg: &Config) -> (Scheme, String) {
        let fallback = || Self::embedded_default();
        let Some(path) = &cfg.ufs_scheme else {
            return (fallback(), "built-in default".to_string());
        };
        match fs::read_to_string(path) {
            Ok(text) => match Self::parse(&text) {
                Ok(scheme) => (scheme, format!("file {path}")),
                Err(e) => (fallback(), format!("built-in default ({e})")),
            },
            Err(e) => (
                fallback(),
                format!("built-in default (reading {path}: {e})"),
            ),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.lus.is_empty() {
            return Err("ufs scheme lists no logical units".to_string());
        }
        let mut seen = Vec::new();
        for lu in &self.lus {
            if lu.id as usize >= ufs::LUS_PER_CONFIG_DESC {
                return Err(format!(
                    "ufs scheme LU {} is out of range (0..{})",
                    lu.id,
                    ufs::LUS_PER_CONFIG_DESC - 1
                ));
            }
            if seen.contains(&lu.id) {
                return Err(format!("ufs scheme lists LU {} twice", lu.id));
            }
            seen.push(lu.id);
            lu.memory_type_code()?;
            lu.provisioning_code()?;
            lu.boot_lun_code()?;
            lu.logical_block_size_exponent()?;
        }
        let unsized_count = self.lus.iter().filter(|l| l.size_mib.is_none()).count();
        if unsized_count != 1 {
            return Err(format!(
                "ufs scheme must have exactly one LU without `size_mib` (the one that \
                 takes the remaining capacity), found {unsized_count}"
            ));
        }
        let want = self.boot_lun_en()?;
        if want != ufs::BOOT_LUN_NONE
            && !self
                .lus
                .iter()
                .any(|l| l.boot_lun_code().unwrap_or(ufs::BOOT_LUN_NONE) == want)
        {
            return Err(format!(
                "ufs scheme selects boot LU {} but no LU is flagged as it",
                self.active_boot_lu
            ));
        }
        Ok(())
    }

    /// The `bBootLunEn` value the scheme asks for.
    pub fn boot_lun_en(&self) -> Result<u8> {
        match self.active_boot_lu.to_ascii_uppercase().as_str() {
            "A" => Ok(ufs::BOOT_LUN_A),
            "B" => Ok(ufs::BOOT_LUN_B),
            "NONE" | "OFF" | "" => Ok(ufs::BOOT_LUN_NONE),
            other => Err(format!("unknown active boot LU {other:?}")),
        }
    }
}

impl SchemeLu {
    fn memory_type_code(&self) -> Result<u8> {
        ufs::memory_type_from_name(&self.memory_type).ok_or_else(|| {
            format!(
                "LU {}: unknown memory type {:?} (one of {})",
                self.id,
                self.memory_type,
                ufs::memory_type_names().collect::<Vec<_>>().join(", ")
            )
        })
    }

    fn provisioning_code(&self) -> Result<u8> {
        match self.provisioning.to_ascii_lowercase().as_str() {
            "thick" => Ok(0x00),
            "thin" => Ok(ufs::PROVISIONING_THIN),
            "thin-read-zeros" => Ok(0x03),
            other => Err(format!("LU {}: unknown provisioning {other:?}", self.id)),
        }
    }

    fn boot_lun_code(&self) -> Result<u8> {
        match self.boot_lun.as_deref().map(|s| s.to_ascii_uppercase()) {
            None => Ok(ufs::BOOT_LUN_NONE),
            Some(s) if s == "A" => Ok(ufs::BOOT_LUN_A),
            Some(s) if s == "B" => Ok(ufs::BOOT_LUN_B),
            Some(other) => Err(format!("LU {}: unknown boot LU {other:?}", self.id)),
        }
    }

    /// `bLogicalBlockSize` is the base-2 logarithm of the block size, and the
    /// spec floors it at 4 KiB (both datasheets say 4 KiB is the only value their
    /// parts accept).
    fn logical_block_size_exponent(&self) -> Result<u8> {
        let size = self.logical_block_size;
        if !size.is_power_of_two() || size < 4096 {
            return Err(format!(
                "LU {}: logical block size {size} must be a power of two of at least 4096",
                self.id
            ));
        }
        Ok(size.trailing_zeros() as u8)
    }
}

// --- the plan ---------------------------------------------------------------

/// What must be written to bring a device to the scheme.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The Configuration Descriptor to write at index 0.
    pub descriptor: ConfigDescriptor,
    /// Descriptors for indexes 1..3 that must be cleared first, because they
    /// still hold enabled logical units (32-LU devices only). Filled in by
    /// [`probe`], which is what can see them.
    pub clear: Vec<(u8, ConfigDescriptor)>,
    /// The `bBootLunEn` attribute value the scheme asks for.
    pub boot_lun_en: u8,
    /// Human-readable description of the layout, for the log and the UI.
    pub summary: Vec<String>,
}

impl Plan {
    /// The logical units the plan configures.
    pub fn lus(&self) -> Vec<LuConfig> {
        self.descriptor.lus()
    }
}

/// Allocation units a usable size costs, given a memory type's capacity
/// adjustment factor. Worked in 512-byte sectors, the geometry descriptor's unit.
fn alloc_units_for(size_bytes: u64, cap_adj_fac: u64, alloc_unit_sectors: u64) -> u64 {
    let sectors = size_bytes.div_ceil(512);
    (sectors * cap_adj_fac).div_ceil(alloc_unit_sectors)
}

/// Build the descriptor that puts `scheme` onto a device with this geometry.
///
/// Built from zero, not by editing the running descriptor, so the result depends
/// only on the scheme and the device: no stale configuration survives, the
/// comparison is exact, and the bytes match what Rockchip's loader writes — which
/// is what these devices are known to accept. Note this zeroes the UFS 3.x RPMB
/// region fields at 0x0B..0x0F; they are zero on every device we have dumped, and
/// the RPMB write-protection work still to come has to set them here deliberately.
pub fn plan(scheme: &Scheme, dev: &DeviceDescriptor, geo: &GeometryDescriptor) -> Result<Plan> {
    let au_sectors = geo.alloc_unit_sectors();
    let au_bytes = geo.alloc_unit_bytes();
    let total = geo.total_alloc_units();
    let mut summary = vec![
        format!(
            "allocation unit {}, {total} units total ({})",
            human_bytes(au_bytes),
            human_bytes(total * au_bytes)
        ),
        // The inputs behind that total. A device that refuses the descriptor says
        // only "invalid value", so the arithmetic has to be visible in the log to
        // be arguable at all.
        format!(
            "  qTotalRawDeviceCapacity {} x 512 B, dSegmentSize {}, bAllocationUnitSize {}",
            geo.total_raw_capacity, geo.segment_size, geo.allocation_unit_size
        ),
    ];

    // WriteBooster first: in the (so far unseen) user-space-reduction mode its
    // buffer eats configurable capacity, so LU sizing depends on it.
    let wb = plan_writebooster(scheme, dev, geo, &mut summary)?;

    // Size every LU: the fixed ones from the scheme, then whatever is left for
    // the single LU that carries no size.
    let mut units: Vec<(&SchemeLu, u8, u64)> = Vec::new();
    let mut used = wb.capacity_cost_alloc_units;
    for lu in &scheme.lus {
        let memory_type = lu.memory_type_code()?;
        if !geo.supports_memory_type(memory_type) {
            return Err(format!(
                "LU {}: the device does not support the {} memory type",
                lu.id, lu.memory_type
            ));
        }
        let count = match lu.size_mib {
            None => 0,
            Some(mib) => {
                let count =
                    alloc_units_for(mib * 1024 * 1024, geo.cap_adj_fac(memory_type)?, au_sectors);
                used += count;
                count
            }
        };
        units.push((lu, memory_type, count));
    }
    if used >= total {
        return Err(format!(
            "the scheme's fixed logical units need {used} allocation units but the device \
             only has {total}"
        ));
    }
    let rest = total - used;
    for entry in units.iter_mut() {
        if entry.0.size_mib.is_none() {
            entry.2 = rest;
        }
    }

    // Every memory type but Normal publishes its own ceiling, which can be well
    // under the whole device (the Foresee 64 GB caps Enhanced1 at about half).
    for (memory_type, ceiling) in memory_type_ceilings(&units, geo) {
        let wanted: u64 = units
            .iter()
            .filter(|(_, mt, _)| *mt == memory_type)
            .map(|(_, _, count)| *count)
            .sum();
        if wanted > u64::from(ceiling) {
            return Err(format!(
                "the scheme needs {wanted} {} allocation units but the device allows at \
                 most {ceiling}",
                ufs::memory_type_name(memory_type)
            ));
        }
    }

    let mut descriptor = ConfigDescriptor::empty(dev);
    // These header values are the ones Rockchip's loader writes: descriptor
    // access disabled, active after init, no high-priority LU, physical secure
    // removal, lowest ICC level, no periodic RTC update.
    descriptor.set_header(
        u8::from(scheme.boot_enable),
        0x00,
        0x01,
        0x7F,
        0x00,
        0x00,
        0x0000,
    );
    match wb.placement {
        WbPlacement::None => {}
        WbPlacement::Shared => {
            descriptor.set_shared_writebooster(wb.preserve_user_space, wb.alloc_units)
        }
        WbPlacement::Dedicated => descriptor.set_lu_dedicated_writebooster(wb.preserve_user_space),
    }

    for (lu, memory_type, count) in &units {
        let count = u32::try_from(*count)
            .map_err(|_| format!("LU {}: allocation unit count overflows", lu.id))?;
        let boot_lun_id = lu.boot_lun_code()?;
        // A dedicated WriteBooster buffer may only sit on a normal-memory,
        // non-boot LU — the spec enforces that by failing the query — and only
        // one LU may have one. The LU taking the remaining capacity is the main
        // one, so it is the natural home.
        let wb_units = if matches!(wb.placement, WbPlacement::Dedicated)
            && lu.size_mib.is_none()
            && *memory_type == ufs::MEMORY_TYPE_NORMAL
            && boot_lun_id == ufs::BOOT_LUN_NONE
        {
            wb.alloc_units
        } else {
            0
        };
        let cfg = LuConfig {
            enable: 1,
            boot_lun_id,
            // Left unprotected on purpose: LU 1-3 are to be protected through
            // RPMB later, and setting bLUWriteProtect now would make them
            // read-only after a power-on reset, breaking the U-Boot write.
            write_protect: 0,
            memory_type: *memory_type,
            num_alloc_units: count,
            data_reliability: u8::from(lu.data_reliability),
            logical_block_size: lu.logical_block_size_exponent()?,
            provisioning_type: lu.provisioning_code()?,
            context_capabilities: 0,
            writebooster_alloc_units: wb_units,
        };
        // The LU that takes the remainder is the install target, so it has to be
        // big enough to hold one.
        if lu.size_mib.is_none() && cfg.size_bytes(geo) < storage::MIN_TARGET_SIZE_BYTES {
            return Err(format!(
                "LU {} would be left with only {}, less than the {} an install needs",
                lu.id,
                human_bytes(cfg.size_bytes(geo)),
                human_bytes(storage::MIN_TARGET_SIZE_BYTES)
            ));
        }
        descriptor.set_lu(lu.id as usize, &cfg);
        summary.push(format!(
            "LU {} {:<9} {:>9}  {} units, {}{}",
            lu.id,
            lu.name,
            human_bytes(cfg.size_bytes(geo)),
            count,
            lu.memory_type,
            match boot_lun_id {
                ufs::BOOT_LUN_A => ", boot LU A",
                ufs::BOOT_LUN_B => ", boot LU B",
                _ => "",
            }
        ));
    }

    // Logical units the scheme does not claim keep the zeroes they started with,
    // which is `bLUEnable = 0`: gone.

    // The accounting, per memory type and overall. If a device ever refuses the
    // descriptor with "invalid value", one of these numbers is why.
    let allocated: u64 = descriptor
        .lus()
        .iter()
        .map(|lu| u64::from(lu.num_alloc_units))
        .sum();
    summary.push(format!(
        "allocating {allocated} of {total} units{}",
        match wb.capacity_cost_alloc_units {
            0 => String::new(),
            cost => format!(", {cost} of them to the WriteBooster buffer"),
        }
    ));
    for (memory_type, ceiling) in memory_type_ceilings(&units, geo) {
        let wanted: u64 = units
            .iter()
            .filter(|(_, mt, _)| *mt == memory_type)
            .map(|(_, _, count)| *count)
            .sum();
        summary.push(format!(
            "  {} {wanted} of {ceiling} units the device allows",
            ufs::memory_type_name(memory_type)
        ));
    }

    Ok(Plan {
        descriptor,
        clear: Vec::new(),
        boot_lun_en: scheme.boot_lun_en()?,
        summary,
    })
}

/// Each memory type the plan uses that publishes an allocation ceiling, once —
/// several logical units of one type share it, and it is the *total* that has to
/// fit.
fn memory_type_ceilings(
    units: &[(&SchemeLu, u8, u64)],
    geo: &GeometryDescriptor,
) -> Vec<(u8, u32)> {
    let mut out: Vec<(u8, u32)> = Vec::new();
    for (_, memory_type, _) in units {
        if out.iter().any(|(t, _)| t == memory_type) {
            continue;
        }
        if let Some(ceiling) = geo.max_alloc_units(*memory_type) {
            out.push((*memory_type, ceiling));
        }
    }
    out
}

/// Where the WriteBooster buffer goes, and what it costs.
struct WbPlan {
    placement: WbPlacement,
    /// Buffer size in allocation units.
    alloc_units: u32,
    preserve_user_space: u8,
    /// Configurable capacity the buffer consumes — zero whenever the device can
    /// preserve user space, which is every part we have seen.
    capacity_cost_alloc_units: u64,
}

enum WbPlacement {
    None,
    Shared,
    Dedicated,
}

fn plan_writebooster(
    scheme: &Scheme,
    dev: &DeviceDescriptor,
    geo: &GeometryDescriptor,
    summary: &mut Vec<String>,
) -> Result<WbPlan> {
    let none = WbPlan {
        placement: WbPlacement::None,
        alloc_units: 0,
        preserve_user_space: 0,
        capacity_cost_alloc_units: 0,
    };
    let max = geo.writebooster_max_alloc_units;
    if !dev.writebooster_supported() || max == 0 {
        summary.push("WriteBooster: not supported by this device".to_string());
        return Ok(none);
    }
    let requested = match &scheme.writebooster.size {
        WbSize::Keyword(k) if k.eq_ignore_ascii_case("max") => max,
        WbSize::Keyword(other) => {
            return Err(format!(
                "unknown writebooster size {other:?} (\"max\" or a number of MiB)"
            ))
        }
        WbSize::Mib(0) => {
            summary.push("WriteBooster: disabled by the scheme".to_string());
            return Ok(none);
        }
        WbSize::Mib(mib) => {
            let want = (mib * 1024 * 1024).div_ceil(geo.alloc_unit_bytes());
            u32::try_from(want.min(u64::from(max))).unwrap_or(max)
        }
    };

    // "Preserve user space" keeps the buffer out of the configurable capacity;
    // the alternative charges bWriteBoosterBufferCapAdjFac raw units per buffer
    // unit against it (JESD220C-2.2 §13.4.14.2).
    let preserve = u8::from(geo.writebooster_can_preserve_user_space());
    let cost = if preserve == 1 {
        0
    } else {
        u64::from(requested) * u64::from(geo.writebooster_cap_adj_fac.max(1))
    };

    let want_shared = !scheme.writebooster.r#type.eq_ignore_ascii_case("lu");
    let placement = if want_shared && geo.writebooster_supports_shared() {
        WbPlacement::Shared
    } else if geo.writebooster_buffer_types != 0x01 && dev.lu_block_has_writebooster() {
        // Either the scheme asked for a dedicated buffer, or the device offers
        // nothing else.
        WbPlacement::Dedicated
    } else if geo.writebooster_supports_shared() {
        WbPlacement::Shared
    } else {
        summary.push("WriteBooster: no supported buffer type, disabled".to_string());
        return Ok(none);
    };

    summary.push(format!(
        "WriteBooster: {} buffer of {} ({requested} units{})",
        match placement {
            WbPlacement::Shared => "shared",
            WbPlacement::Dedicated => "dedicated",
            WbPlacement::None => "no",
        },
        human_bytes(u64::from(requested) * geo.alloc_unit_bytes()),
        if cost == 0 {
            ", user space preserved"
        } else {
            ", charged against user capacity"
        }
    ));
    Ok(WbPlan {
        placement,
        alloc_units: requested,
        preserve_user_space: preserve,
        capacity_cost_alloc_units: cost,
    })
}

// --- comparison -------------------------------------------------------------

/// One way a device's configuration differs from the scheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub what: String,
    /// Whether this difference means the device must be reprovisioned. Anything
    /// else is reported but tolerated.
    pub critical: bool,
    /// Whether fixing it needs a Configuration Descriptor write — the
    /// destructive part. `bBootLunEn` is a device attribute, so it does not.
    pub descriptor_write: bool,
}

/// Everything a probe learned about one device.
#[derive(Clone, Debug)]
pub struct Status {
    /// The device this describes, and how to reach it.
    pub target: Target,
    /// Where the scheme came from.
    pub scheme_origin: String,
    /// The device's current logical units.
    pub current: Vec<LuConfig>,
    /// What the scheme asks for.
    pub plan: Plan,
    /// `bBootEnable` as the device is configured now.
    pub boot_enable: u8,
    pub boot_lun_en: u8,
    pub config_locked: bool,
    /// `fPowerOnWPEn` / `fPermanentWPEn`. A write-protected logical unit cannot be
    /// rewritten, so these are the first thing to look at when a device refuses to
    /// be reconfigured.
    pub power_on_wp: bool,
    pub permanent_wp: bool,
    /// The device's geometry, which is what turns an allocation unit count into a
    /// size — differently for each memory type.
    pub geometry: GeometryDescriptor,
    pub mismatches: Vec<Mismatch>,
}

impl Status {
    /// Whether the device already matches the scheme closely enough to install
    /// onto without reprovisioning.
    pub fn is_provisioned(&self) -> bool {
        !self.mismatches.iter().any(|m| m.critical)
    }

    pub fn critical(&self) -> impl Iterator<Item = &Mismatch> {
        self.mismatches.iter().filter(|m| m.critical)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Mismatch> {
        self.mismatches.iter().filter(|m| !m.critical)
    }

    /// Whether fixing this device means rewriting its Configuration Descriptor,
    /// which is what loses the data on it.
    pub fn needs_descriptor_write(&self) -> bool {
        self.critical().any(|m| m.descriptor_write)
    }

    /// Whether the device has no logical units at all — factory-blank.
    ///
    /// Such a device presents no block device, so it cannot be an install target
    /// until it is provisioned, and provisioning it destroys nothing.
    pub fn is_blank(&self) -> bool {
        !self.current.iter().any(|lu| lu.enabled())
    }

    /// A logical unit's usable size in bytes.
    fn lu_size(&self, lu: &LuConfig) -> u64 {
        lu.size_bytes(&self.geometry)
    }

    /// A logical unit in a few words, for the side-by-side prompt list.
    fn lu_summary(&self, lu: &LuConfig) -> String {
        if !lu.enabled() {
            return "absent".to_string();
        }
        let mut out = human_bytes(self.lu_size(lu));
        match lu.boot_lun_id {
            ufs::BOOT_LUN_A => out.push_str(" boot A"),
            ufs::BOOT_LUN_B => out.push_str(" boot B"),
            _ => {}
        }
        out
    }

    /// One-line summary for the target-device row.
    pub fn short_label(&self) -> &'static str {
        if self.config_locked && !self.is_provisioned() {
            "locked"
        } else if self.is_provisioned() {
            "ok"
        } else {
            "unprovisioned"
        }
    }

    /// Lines describing the current layout, for the log and the details popup.
    pub fn report(&self) -> Vec<String> {
        let mut out = vec![format!(
            "UFS {} — scheme: {}",
            self.target.label(),
            self.scheme_origin
        )];
        out.push(format!(
            "allocation unit {}, {} units total",
            human_bytes(self.geometry.alloc_unit_bytes()),
            self.geometry.total_alloc_units()
        ));
        if self.is_blank() {
            out.push("no logical units configured".to_string());
        }
        for (i, lu) in self.current.iter().enumerate() {
            if !lu.enabled() {
                continue;
            }
            out.push(format!(
                "LU {i}: {:>9}  {}{}{}",
                human_bytes(self.lu_size(lu)),
                ufs::memory_type_name(lu.memory_type),
                match lu.boot_lun_id {
                    ufs::BOOT_LUN_A => ", boot LU A",
                    ufs::BOOT_LUN_B => ", boot LU B",
                    _ => "",
                },
                if lu.write_protect != 0 {
                    ", write-protected"
                } else {
                    ""
                }
            ));
        }
        out.push(format!(
            "active boot LU: {}",
            ufs::boot_lu_name(self.boot_lun_en)
        ));
        if self.config_locked {
            out.push("configuration descriptor is LOCKED".to_string());
        }
        if self.power_on_wp {
            out.push("power-on write protection is enabled".to_string());
        }
        if self.permanent_wp {
            out.push("PERMANENT write protection is enabled".to_string());
        }
        if self.is_provisioned() {
            out.push("matches the Flipper scheme".to_string());
        }
        for m in &self.mismatches {
            out.push(format!(
                "{} {}",
                if m.critical { "mismatch:" } else { "note:" },
                m.what
            ));
        }
        out
    }

    /// The prompt text offering to reprovision.
    ///
    /// The consequence comes first and the detail after: only the first handful
    /// of lines fit the 256x144 panel without scrolling, and what the operator
    /// must not miss is what happens to the device. The full technical report
    /// stays one keypress away in the details popup.
    pub fn prompt_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.is_blank() {
            // Nothing to lose: a device with no logical units holds no data, and
            // claiming otherwise would be crying wolf.
            out.push(format!(
                "{} has no logical units yet. Provisioning creates them:",
                self.target.display_name()
            ));
        } else if self.needs_descriptor_write() {
            out.push(format!(
                "ALL DATA ON {} WILL BE PERMANENTLY LOST.",
                self.target.display_name()
            ));
        } else {
            out.push(format!(
                "{} only needs its active boot LU switched. No data is erased.",
                self.target.display_name()
            ));
        }
        out.push(String::new());

        let differing: Vec<String> = self
            .plan
            .lus()
            .iter()
            .zip(self.current.iter())
            .enumerate()
            // A logical unit that is absent and stays absent is not a change. The
            // two can still differ byte for byte — a factory descriptor leaves
            // `bLogicalBlockSize` set in blocks it disables — and listing
            // "absent → absent" would be noise in the one prompt that must read
            // clearly.
            .filter(|(_, (want, have))| want.enabled() || have.enabled())
            .filter(|(_, (want, have))| want != have)
            .map(|(i, (want, have))| {
                format!(
                    "LU {i}: {} → {}",
                    self.lu_summary(have),
                    self.lu_summary(want)
                )
            })
            .collect();
        if !differing.is_empty() {
            out.push("Logical units, now and wanted:".to_string());
            out.extend(differing);
        }
        if self.plan.descriptor.boot_enable() != self.boot_enable {
            out.push("The boot feature is off, so the board cannot boot.".to_string());
        }
        // The attribute-only mismatch is the `bBootLunEn` one, and a device
        // legitimately booting either side of the pair does not produce it.
        if self.critical().any(|m| !m.descriptor_write) {
            out.push(format!(
                "Active boot LU: {} → {}",
                ufs::boot_lu_name(self.boot_lun_en),
                ufs::boot_lu_name(self.plan.boot_lun_en)
            ));
        }
        out
    }
}

fn boot_lun_name(code: u8) -> &'static str {
    match code {
        ufs::BOOT_LUN_A => "boot LU A",
        ufs::BOOT_LUN_B => "boot LU B",
        _ => "not bootable",
    }
}

/// Compare a device's configuration against the plan.
///
/// Critical differences are the ones that change what the device *is*: which
/// logical units exist, how big they are, what memory they use, which one the
/// boot ROM boots from, and whether booting is enabled at all. Everything else is
/// reported and tolerated — a reprovision normalises it anyway, since the whole
/// descriptor is rewritten.
pub fn compare(
    plan: &Plan,
    current: &ConfigDescriptor,
    boot_lun_en: u8,
    extra_enabled_indexes: &[u8],
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let want = &plan.descriptor;

    diff(
        &mut out,
        true,
        "the boot feature (bBootEnable)",
        want.boot_enable(),
        current.boot_enable(),
    );
    // `bBootLunEn` says which boot LU the boot ROM reads, and the installer moves
    // it between the pair on every bootloader update (writing the spare LU, then
    // switching once the image verifies). So any LU the scheme flags as bootable
    // is an acceptable value here — what would be wrong is booting an LU the
    // scheme does not flag, or none at all.
    let bootable: Vec<u8> = plan
        .lus()
        .iter()
        .filter(|lu| lu.enabled() && lu.boot_lun_id != ufs::BOOT_LUN_NONE)
        .map(|lu| lu.boot_lun_id)
        .collect();
    let acceptable = if bootable.is_empty() {
        vec![plan.boot_lun_en]
    } else {
        bootable
    };
    if !acceptable.contains(&boot_lun_en) {
        let want: Vec<&str> = acceptable.iter().map(|id| ufs::boot_lu_name(*id)).collect();
        out.push(Mismatch {
            what: format!(
                "the active boot LU (bBootLunEn) is {}, want {}",
                ufs::boot_lu_name(boot_lun_en),
                want.join(" or ")
            ),
            critical: true,
            // A device attribute, not part of the configuration: switching it
            // erases nothing.
            descriptor_write: false,
        });
    }

    for index in extra_enabled_indexes {
        out.push(Mismatch {
            what: format!("logical units above LU 7 are enabled (descriptor index {index})"),
            critical: true,
            descriptor_write: true,
        });
    }

    for lu in 0..ufs::LUS_PER_CONFIG_DESC {
        let w = want.lu(lu);
        let c = current.lu(lu);
        if w.enable != c.enable {
            out.push(Mismatch {
                what: format!(
                    "LU {lu} is {}, want it {}",
                    if c.enabled() { "enabled" } else { "absent" },
                    if w.enabled() { "enabled" } else { "absent" }
                ),
                critical: true,
                descriptor_write: true,
            });
            // Nothing else about a differently-existing LU is worth reporting.
            continue;
        }
        if !w.enabled() {
            continue;
        }
        diff(
            &mut out,
            true,
            &format!("LU {lu} memory type"),
            ufs::memory_type_name(w.memory_type),
            ufs::memory_type_name(c.memory_type),
        );
        diff(
            &mut out,
            true,
            &format!("LU {lu} boot flag"),
            boot_lun_name(w.boot_lun_id),
            boot_lun_name(c.boot_lun_id),
        );
        diff(
            &mut out,
            true,
            &format!("LU {lu} size in allocation units"),
            w.num_alloc_units,
            c.num_alloc_units,
        );
        diff(
            &mut out,
            true,
            &format!("LU {lu} logical block size"),
            1u32 << w.logical_block_size,
            1u32 << c.logical_block_size,
        );
        diff(
            &mut out,
            false,
            &format!("LU {lu} write protection"),
            w.write_protect,
            c.write_protect,
        );
        diff(
            &mut out,
            false,
            &format!("LU {lu} data reliability"),
            w.data_reliability,
            c.data_reliability,
        );
        diff(
            &mut out,
            false,
            &format!("LU {lu} provisioning type"),
            w.provisioning_type,
            c.provisioning_type,
        );
        diff(
            &mut out,
            false,
            &format!("LU {lu} WriteBooster units"),
            w.writebooster_alloc_units,
            c.writebooster_alloc_units,
        );
    }

    diff(
        &mut out,
        false,
        "the WriteBooster buffer type",
        want.writebooster_buffer_type(),
        current.writebooster_buffer_type(),
    );
    diff(
        &mut out,
        false,
        "the shared WriteBooster size in allocation units",
        want.writebooster_shared_alloc_units(),
        current.writebooster_shared_alloc_units(),
    );
    diff(
        &mut out,
        false,
        "WriteBooster user-space preservation",
        want.writebooster_preserve_user_space(),
        current.writebooster_preserve_user_space(),
    );
    diff(
        &mut out,
        false,
        "descriptor access after boot",
        want.descr_access_en(),
        current.descr_access_en(),
    );
    diff(
        &mut out,
        false,
        "the initial power mode",
        want.init_power_mode(),
        current.init_power_mode(),
    );
    diff(
        &mut out,
        false,
        "the high priority LUN",
        want.high_priority_lun(),
        current.high_priority_lun(),
    );
    diff(
        &mut out,
        false,
        "the secure removal type",
        want.secure_removal_type(),
        current.secure_removal_type(),
    );
    diff(
        &mut out,
        false,
        "the initial active ICC level",
        want.init_active_icc_level(),
        current.init_active_icc_level(),
    );
    diff(
        &mut out,
        false,
        "the periodic RTC update",
        want.periodic_rtc_update(),
        current.periodic_rtc_update(),
    );
    out
}

fn diff<T: PartialEq + std::fmt::Display>(
    out: &mut Vec<Mismatch>,
    critical: bool,
    what: &str,
    want: T,
    have: T,
) {
    if want != have {
        out.push(Mismatch {
            what: format!("{what} is {have}, want {want}"),
            critical,
            descriptor_write: true,
        });
    }
}

// --- probing and applying ---------------------------------------------------

/// What provisioning acts on: a UFS host controller, plus the whole-disk node of
/// its main logical unit when it has one.
///
/// Keyed on the controller rather than on a disk because a factory-blank device
/// has no disk at all — no logical units means nothing in `/sys/block` — and that
/// is precisely the device that needs provisioning most. Everything provisioning
/// does goes through the BSG endpoint, which exists either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub bsg: PathBuf,
    pub host: u32,
    /// Whole-disk node of the main logical unit, when the device has one.
    pub disk: Option<String>,
    /// How the device names itself (SCSI vendor and model), for the UI and log.
    pub name: String,
}

impl Target {
    /// The target behind a whole-disk UFS node.
    pub fn for_disk(disk: &str) -> Result<Target> {
        let host = ufs::scsi_host_number(disk)
            .ok_or_else(|| format!("cannot find the SCSI host of {disk}"))?;
        Ok(Target {
            bsg: ufs::bsg_node(disk)?,
            host,
            disk: Some(disk.to_string()),
            name: ufs::identity(host).unwrap_or_default(),
        })
    }

    /// The target behind a UFS host controller, whether or not it has any logical
    /// units yet.
    pub fn for_endpoint(endpoint: &ufs::Endpoint) -> Target {
        Target {
            bsg: endpoint.bsg.clone(),
            host: endpoint.host,
            disk: main_lu_node(endpoint.host),
            name: ufs::identity(endpoint.host).unwrap_or_default(),
        }
    }

    /// The same controller, with its logical units looked up again — the block node
    /// of a device that has just been provisioned did not exist before.
    pub fn rediscovered(&self) -> Target {
        Target::for_endpoint(&ufs::Endpoint {
            bsg: self.bsg.clone(),
            host: self.host,
        })
    }

    /// The shortest thing that identifies this device, for prose the operator reads
    /// on a 256x144 panel. Same as [`Self::label`] but without the endpoint, which
    /// is noise once the model is there — the log and the details popup still carry
    /// the full form.
    pub fn display_name(&self) -> String {
        match (&self.disk, self.name.is_empty()) {
            (Some(disk), _) => disk.clone(),
            (None, false) => self.name.clone(),
            (None, true) => self.label(),
        }
    }

    /// What to call this device in messages: its disk when it has one, else its own
    /// name and endpoint — a blank device has no `/dev/sd*` to be known by.
    pub fn label(&self) -> String {
        if let Some(disk) = &self.disk {
            return disk.clone();
        }
        let endpoint = self
            .bsg
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("host{}", self.host));
        if self.name.is_empty() {
            endpoint
        } else {
            format!("{} ({endpoint})", self.name)
        }
    }
}

#[cfg(test)]
impl Target {
    /// A target naming `disk`, built without touching sysfs.
    pub(crate) fn for_test(disk: &str) -> Target {
        Target {
            bsg: PathBuf::from("/dev/bsg/ufs-bsg0"),
            host: 0,
            disk: Some(disk.to_string()),
            name: "TESTDEV 0".to_string(),
        }
    }

    /// A target for a device with no logical units, as a blank one has.
    pub(crate) fn blank_for_test() -> Target {
        Target {
            bsg: PathBuf::from("/dev/bsg/ufs-bsg0"),
            host: 0,
            disk: None,
            name: "BIWIN BWU2A0526B128G".to_string(),
        }
    }
}

/// The block node of the logical unit holding the main filesystem — LU 0 by both
/// our scheme and Rockchip's — falling back to whichever data LU has one.
fn main_lu_node(host: u32) -> Option<String> {
    let dirs = ufs::data_lu_dirs(host);
    let lu_zero = dirs.iter().find(|dir| {
        dir.file_name()
            .map(|n| n.to_string_lossy().ends_with(":0"))
            .unwrap_or(false)
    });
    lu_zero
        .and_then(|dir| ufs::lu_block_node(dir))
        .or_else(|| dirs.iter().find_map(|dir| ufs::lu_block_node(dir)))
}

/// Read a UFS device's configuration and compare it against the scheme.
pub fn probe(target: &Target, scheme: &Scheme, scheme_origin: &str) -> Result<Status> {
    let bsg = ufs::Bsg::open(&target.bsg)?;
    let dev = DeviceDescriptor::parse(&bsg.read_descriptor(ufs::IDN_DEVICE, 0)?)?;
    let geo = GeometryDescriptor::parse(&bsg.read_descriptor(ufs::IDN_GEOMETRY, 0)?)?;
    let current = ConfigDescriptor::new(bsg.read_descriptor(ufs::IDN_CONFIGURATION, 0)?, &dev)?;
    let boot_lun_en = bsg.read_attr(ufs::ATTR_BOOT_LUN_EN)? as u8;
    let config_locked = bsg.read_attr(ufs::ATTR_CONFIG_DESCR_LOCK)? != 0;
    let power_on_wp = bsg.read_flag(ufs::FLAG_POWER_ON_WP_EN).unwrap_or(false);
    let permanent_wp = bsg.read_flag(ufs::FLAG_PERMANENT_WP_EN).unwrap_or(false);

    // A device that reports 32 logical units keeps LU 8..31 in the other three
    // Configuration Descriptors; the scheme wants all of them disabled.
    let mut extra_enabled = Vec::new();
    let mut clear = Vec::new();
    if geo.max_number_lu == ufs::MAX_NUMBER_LU_32 {
        for index in 1u8..4 {
            let Ok(bytes) = bsg.read_descriptor(ufs::IDN_CONFIGURATION, index) else {
                continue;
            };
            let Ok(desc) = ConfigDescriptor::new(bytes, &dev) else {
                continue;
            };
            if desc.lus().iter().any(|l| l.enabled()) {
                extra_enabled.push(index);
                let mut empty = ConfigDescriptor::empty(&dev);
                empty.set_conf_desc_continue(1);
                clear.push((index, empty));
            }
        }
    }

    let mut plan = plan(scheme, &dev, &geo)?;
    plan.clear = clear;
    let mismatches = compare(&plan, &current, boot_lun_en, &extra_enabled);

    Ok(Status {
        target: target.clone(),
        scheme_origin: scheme_origin.to_string(),
        current: current.lus(),
        plan,
        boot_enable: current.boot_enable(),
        boot_lun_en,
        config_locked,
        power_on_wp,
        permanent_wp,
        geometry: geo,
        mismatches,
    })
}

/// Bring `target` to the scheme. Destructive on a device that already holds data:
/// rewriting the Configuration Descriptor makes it rebuild its logical units.
pub fn apply(
    cfg: &Config,
    ctrl: &Controller,
    target: &Target,
    scheme: &Scheme,
    origin: &str,
) -> Result<()> {
    let disk = target.label();
    let status = probe(target, scheme, origin)?;
    for line in status.report() {
        ctrl.log(line);
    }
    if status.is_provisioned() {
        ctrl.log(format!("{disk}: already provisioned, nothing to do"));
        return Ok(());
    }
    let needs_write = status.needs_descriptor_write();
    if status.config_locked && needs_write {
        return Err(format!(
            "{disk}: the configuration descriptor is locked (bConfigDescrLock), so the \
             device cannot be reprovisioned"
        ));
    }
    // Only a device that has logical units can be in use; a blank one has nothing
    // to hold open.
    if let Some(node) = &target.disk {
        let in_use = storage::device_in_use(node);
        if !in_use.is_empty() {
            return Err(format!(
                "{node} is in use, refusing to reprovision: {}",
                in_use.join("; ")
            ));
        }
    }

    ctrl.log(format!("{disk}: reprovisioning to the Flipper UFS scheme"));
    for line in &status.plan.summary {
        ctrl.log(format!("  {line}"));
    }

    if cfg.dry_run {
        if needs_write {
            ctrl.log("[dry-run] would write the configuration descriptor:");
            for line in ufs::hexdump(status.plan.descriptor.as_bytes()) {
                ctrl.log(format!("[dry-run]   {line}"));
            }
            for (index, _) in &status.plan.clear {
                ctrl.log(format!(
                    "[dry-run] would first disable the logical units in descriptor index {index}"
                ));
            }
        }
        ctrl.log(format!(
            "[dry-run] would set bBootLunEn = {}",
            status.plan.boot_lun_en
        ));
        return Ok(());
    }

    {
        let bsg = ufs::Bsg::open(&target.bsg)?;
        if needs_write {
            // Any descriptor that still holds LU 8..31 goes first, marked "more
            // to come", so the device only reconfigures once index 0 arrives.
            for (index, desc) in &status.plan.clear {
                ctrl.log(format!(
                    "disabling the logical units in descriptor index {index}"
                ));
                bsg.write_descriptor(ufs::IDN_CONFIGURATION, *index, desc.as_bytes())?;
            }
            if let Err(e) =
                bsg.write_descriptor(ufs::IDN_CONFIGURATION, 0, status.plan.descriptor.as_bytes())
            {
                // The device names no field, so leave both descriptors in the log:
                // the one it is running and the one it turned down.
                ctrl.log("the configuration descriptor the device rejected:");
                for line in ufs::hexdump(status.plan.descriptor.as_bytes()) {
                    ctrl.log(format!("  {line}"));
                }
                match bsg.read_descriptor(ufs::IDN_CONFIGURATION, 0) {
                    Ok(bytes) => {
                        ctrl.log("the configuration descriptor still in effect:");
                        for line in ufs::hexdump(&bytes) {
                            ctrl.log(format!("  {line}"));
                        }
                    }
                    Err(e) => ctrl.log(format!("warning: cannot re-read the descriptor: {e}")),
                }
                return Err(e);
            }
            ctrl.log("configuration descriptor written");
        }
        bsg.write_attr(ufs::ATTR_BOOT_LUN_EN, u32::from(status.plan.boot_lun_en))?;
        ctrl.log(format!(
            "active boot LU set to {}",
            ufs::boot_lu_name(status.plan.boot_lun_en)
        ));
    }

    // The descriptor is written but the logical units still have their old sizes:
    // the device only rebuilds them when it next initialises. Ask it to do that
    // now — the same `fDeviceInit` step the mask ROM loader performs after
    // provisioning — and rescan, so no reboot is needed.
    ctrl.log("applying the new configuration (fDeviceInit) and rescanning");
    {
        let bsg = ufs::Bsg::open(&target.bsg)?;
        ufs::apply_configuration(target.host, target.disk.as_deref(), &bsg)?;
    }

    // Re-resolve the target before verifying: a device that had no logical units
    // has one now, behind a block node that did not exist when this started.
    let after = probe(&target.rediscovered(), scheme, origin)?;
    for line in after.report() {
        ctrl.log(line);
    }
    if !after.is_provisioned() {
        return Err(format!(
            "{disk}: the configuration descriptor was written but does not read back as \
             the scheme; reboot the board and check it before installing"
        ));
    }
    ctrl.log(format!("{disk}: provisioning complete"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ufs::tests::{
        unhex, BIWIN256_CONFIG, BIWIN256_DEVICE, BIWIN256_GEOMETRY, BIWIN64_DEVICE,
        BIWIN64_GEOMETRY, FORESEE64_CONFIG, FORESEE64_CONFIG_PROVISIONED, FORESEE64_DEVICE,
        FORESEE64_GEOMETRY,
    };

    fn parts(
        dev: &str,
        geo: &str,
        cfg: &str,
    ) -> (DeviceDescriptor, GeometryDescriptor, ConfigDescriptor) {
        let dev = DeviceDescriptor::parse(&unhex(dev)).expect("device descriptor");
        let geo = GeometryDescriptor::parse(&unhex(geo)).expect("geometry descriptor");
        let cfg = ConfigDescriptor::new(unhex(cfg), &dev).expect("config descriptor");
        (dev, geo, cfg)
    }

    fn critical_texts(mismatches: &[Mismatch]) -> Vec<&str> {
        mismatches
            .iter()
            .filter(|m| m.critical)
            .map(|m| m.what.as_str())
            .collect()
    }

    /// A `Status` as `probe` would build it, from descriptor dumps rather than a
    /// device.
    fn status(dev: &str, geo: &str, cfg: &str, boot_lun_en: u8) -> Status {
        let (dev, geo, current) = parts(dev, geo, cfg);
        let scheme = Scheme::embedded_default();
        let built = plan(&scheme, &dev, &geo).expect("plan");
        let mismatches = compare(&built, &current, boot_lun_en, &[]);
        Status {
            target: Target::for_test("/dev/sda"),
            scheme_origin: "built-in default".to_string(),
            current: current.lus(),
            plan: built,
            boot_enable: current.boot_enable(),
            boot_lun_en,
            config_locked: false,
            power_on_wp: false,
            permanent_wp: false,
            geometry: geo,
            mismatches,
        }
    }

    /// A `Status` for a factory-blank device: real geometry, but a Configuration
    /// Descriptor with every logical unit disabled, which is what one ships as.
    fn blank_status() -> Status {
        let dev = DeviceDescriptor::parse(&unhex(FORESEE64_DEVICE)).expect("device descriptor");
        let geo = GeometryDescriptor::parse(&unhex(FORESEE64_GEOMETRY)).expect("geometry");
        let empty = ConfigDescriptor::empty(&dev);
        let built = plan(&Scheme::embedded_default(), &dev, &geo).expect("plan");
        let mismatches = compare(&built, &empty, ufs::BOOT_LUN_NONE, &[]);
        Status {
            target: Target::blank_for_test(),
            scheme_origin: "built-in default".to_string(),
            current: empty.lus(),
            plan: built,
            boot_enable: empty.boot_enable(),
            boot_lun_en: ufs::BOOT_LUN_NONE,
            config_locked: false,
            power_on_wp: false,
            permanent_wp: false,
            geometry: geo,
            mismatches,
        }
    }

    #[test]
    fn a_target_names_itself_by_disk_or_by_device() {
        // With a block node the disk is the name everyone already knows it by.
        assert_eq!(Target::for_test("/dev/sda").label(), "/dev/sda");
        // Without one — a blank device — there is no `/dev/sd*` to use, so the
        // device's own name and its endpoint stand in.
        let blank = Target::blank_for_test();
        assert_eq!(blank.label(), "BIWIN BWU2A0526B128G (ufs-bsg0)");
        assert!(blank.disk.is_none());
        // Prose on the device's screen drops the endpoint: the model identifies it,
        // and a 256x144 panel shows six lines.
        assert_eq!(blank.display_name(), "BIWIN BWU2A0526B128G");
        assert_eq!(Target::for_test("/dev/sda").display_name(), "/dev/sda");
    }

    #[test]
    fn a_blank_device_is_recognised_and_reported_as_such() {
        let st = blank_status();
        assert!(st.is_blank());
        assert!(
            !st.is_provisioned(),
            "a blank device is not usable as a target"
        );
        let report = st.report().join("\n");
        assert!(report.contains("no logical units configured"), "{report}");
        // The geometry is still readable, which is what makes planning possible.
        assert!(report.contains("allocation unit 4.0 MiB"), "{report}");

        // And a device that does have logical units is not called blank.
        let populated = status(
            BIWIN256_DEVICE,
            BIWIN256_GEOMETRY,
            BIWIN256_CONFIG,
            ufs::BOOT_LUN_A,
        );
        assert!(!populated.is_blank());
        assert!(!populated.report().join("\n").contains("no logical units"));
    }

    #[test]
    fn the_prompt_does_not_claim_data_loss_on_a_blank_device() {
        // There is nothing on a device with no logical units, and saying otherwise
        // would be crying wolf on the one prompt that must be believed.
        let lines = blank_status().prompt_lines();
        assert!(
            lines[0].contains("has no logical units yet"),
            "{:?}",
            lines[0]
        );
        assert!(
            !lines.iter().any(|l| l.contains("LOST")),
            "nothing to lose: {lines:?}"
        );
        // It still shows what it is about to create.
        assert!(lines.iter().any(|l| l.contains("16.0 MiB")), "{lines:?}");
        // But not the logical units it leaves absent: a factory descriptor keeps
        // `bLogicalBlockSize` set in the blocks it disables, so those compare
        // unequal without being a change worth showing.
        assert!(
            !lines.iter().any(|l| l.contains("absent → absent")),
            "{lines:?}"
        );
        for lu in 4..8 {
            assert!(
                !lines.iter().any(|l| l.starts_with(&format!("LU {lu}:"))),
                "LU {lu} stays absent: {lines:?}"
            );
        }

        // A populated device keeps the warning it needs.
        let lines = status(
            BIWIN256_DEVICE,
            BIWIN256_GEOMETRY,
            BIWIN256_CONFIG,
            ufs::BOOT_LUN_A,
        )
        .prompt_lines();
        assert!(
            lines[0].contains("WILL BE PERMANENTLY LOST"),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn embedded_scheme_parses() {
        let scheme = Scheme::embedded_default();
        assert!(scheme.boot_enable);
        assert_eq!(scheme.boot_lun_en().unwrap(), ufs::BOOT_LUN_A);
        assert_eq!(scheme.lus.len(), 4);
        for (i, lu) in scheme.lus.iter().enumerate() {
            assert_eq!(lu.id as usize, i);
            assert_eq!(lu.logical_block_size, 4096);
            assert!(lu.data_reliability);
        }
        assert!(scheme.lus[0].size_mib.is_none(), "LU 0 takes the remainder");
        assert_eq!(scheme.lus[1].size_mib, Some(16));
        assert_eq!(scheme.lus[1].boot_lun.as_deref(), Some("A"));
        assert_eq!(scheme.lus[2].size_mib, Some(16));
        assert_eq!(scheme.lus[2].boot_lun.as_deref(), Some("B"));
        assert_eq!(scheme.lus[3].size_mib, Some(128));
        assert_eq!(scheme.writebooster.size, WbSize::Keyword("max".to_string()));
    }

    #[test]
    fn scheme_validation_rejects_bad_input() {
        // Two LUs without a size: nothing says which takes the remainder.
        assert!(Scheme::parse(
            r#"
            [[lu]]
            id = 0
            memory_type = "normal"
            [[lu]]
            id = 1
            memory_type = "normal"
            "#
        )
        .is_err());

        // A boot LU that is selected but does not exist.
        assert!(Scheme::parse(
            r#"
            active_boot_lu = "B"
            [[lu]]
            id = 0
            memory_type = "normal"
            "#
        )
        .is_err());

        // Duplicate ids.
        assert!(Scheme::parse(
            r#"
            active_boot_lu = "none"
            [[lu]]
            id = 0
            memory_type = "normal"
            [[lu]]
            id = 0
            memory_type = "enhanced1"
            size_mib = 4
            "#
        )
        .is_err());

        // A 512-byte block size, which no UFS device accepts.
        assert!(Scheme::parse(
            r#"
            active_boot_lu = "none"
            [[lu]]
            id = 0
            memory_type = "normal"
            logical_block_size = 512
            "#
        )
        .is_err());
    }

    #[test]
    fn plan_sizes_match_the_hand_provisioned_foresee() {
        // The same part, provisioned to this scheme by hand: LU1/LU2 at 12
        // allocation units (16 MiB of Enhanced1, which costs 3x) and LU3 at 96
        // (128 MiB). Anything else here would mean the arithmetic is wrong.
        let scheme = Scheme::embedded_default();
        let (dev, geo, _) = parts(FORESEE64_DEVICE, FORESEE64_GEOMETRY, FORESEE64_CONFIG);
        let by_hand = ConfigDescriptor::new(unhex(FORESEE64_CONFIG_PROVISIONED), &dev).unwrap();

        let built = plan(&scheme, &dev, &geo).expect("plan");
        let lus = built.lus();
        assert_eq!(lus[1].num_alloc_units, 12);
        assert_eq!(lus[2].num_alloc_units, 12);
        assert_eq!(lus[3].num_alloc_units, 96);
        assert_eq!(lus[1].size_bytes(&geo), 16 * 1024 * 1024);
        assert_eq!(lus[2].size_bytes(&geo), 16 * 1024 * 1024);
        assert_eq!(lus[3].size_bytes(&geo), 128 * 1024 * 1024);
        assert_eq!(by_hand.lu(1).num_alloc_units, 12);
        assert_eq!(by_hand.lu(2).num_alloc_units, 12);
        assert_eq!(by_hand.lu(3).num_alloc_units, 96);

        // LU 0 takes everything the others leave, and the WriteBooster costs it
        // nothing because the part preserves user space.
        assert_eq!(lus[0].num_alloc_units as u64, geo.total_alloc_units() - 120);
        assert_eq!(lus[0].memory_type, ufs::MEMORY_TYPE_NORMAL);
        assert_eq!(lus[0].boot_lun_id, ufs::BOOT_LUN_NONE);
        for lu in &lus[4..] {
            assert!(!lu.enabled());
        }
        // Boot flags, and the boot feature this part ships without.
        assert_eq!(lus[1].boot_lun_id, ufs::BOOT_LUN_A);
        assert_eq!(lus[2].boot_lun_id, ufs::BOOT_LUN_B);
        assert_eq!(built.descriptor.boot_enable(), 1);
        assert_eq!(built.boot_lun_en, ufs::BOOT_LUN_A);
    }

    #[test]
    fn plan_allocates_every_allocation_unit() {
        let scheme = Scheme::embedded_default();
        for (name, d, g, expect_lu0) in [
            (
                "biwin 256G",
                BIWIN256_DEVICE,
                BIWIN256_GEOMETRY,
                61022 - 120,
            ),
            ("biwin 64G", BIWIN64_DEVICE, BIWIN64_GEOMETRY, 15258 - 120),
            (
                "foresee 64G",
                FORESEE64_DEVICE,
                FORESEE64_GEOMETRY,
                15260 - 120,
            ),
        ] {
            let dev = DeviceDescriptor::parse(&unhex(d)).expect(name);
            let geo = GeometryDescriptor::parse(&unhex(g)).expect(name);
            let built = plan(&scheme, &dev, &geo).expect(name);
            let allocated: u64 = built
                .lus()
                .iter()
                .map(|l| u64::from(l.num_alloc_units))
                .sum();
            assert_eq!(allocated, geo.total_alloc_units(), "{name}");
            assert_eq!(built.lus()[0].num_alloc_units as u64, expect_lu0, "{name}");
            // The main LU must still be a plausible install target.
            assert!(
                built.lus()[0].size_bytes(&geo) > storage::MIN_TARGET_SIZE_BYTES,
                "{name}"
            );
        }
    }

    #[test]
    fn factory_layouts_are_reported_as_mismatched() {
        let scheme = Scheme::embedded_default();

        // Biwin ships Rockchip's layout: 4 MiB boot LUs and an 8 MiB spare, so
        // the four sizes differ but the header fields already agree.
        let (dev, geo, current) = parts(BIWIN256_DEVICE, BIWIN256_GEOMETRY, BIWIN256_CONFIG);
        let built = plan(&scheme, &dev, &geo).unwrap();
        let mismatches = compare(&built, &current, ufs::BOOT_LUN_A, &[]);
        let critical = critical_texts(&mismatches);
        assert_eq!(critical.len(), 4, "LU 0-3 sizes: {critical:?}");
        for lu in 0..4 {
            assert!(
                critical
                    .iter()
                    .any(|w| w.starts_with(&format!("LU {lu} size"))),
                "expected an LU {lu} size mismatch in {critical:?}"
            );
        }
        // bBootEnable is already set on this part, so it must not be flagged.
        assert!(!critical.iter().any(|w| w.contains("bBootEnable")));

        // The Foresee ships with one big LU and the boot feature off.
        let (dev, geo, current) = parts(FORESEE64_DEVICE, FORESEE64_GEOMETRY, FORESEE64_CONFIG);
        let built = plan(&scheme, &dev, &geo).unwrap();
        let mismatches = compare(&built, &current, ufs::BOOT_LUN_NONE, &[]);
        let critical = critical_texts(&mismatches);
        assert!(
            critical.iter().any(|w| w.contains("bBootEnable")),
            "the boot feature is off, which would leave the board unbootable: {critical:?}"
        );
        assert!(critical.iter().any(|w| w.contains("bBootLunEn")));
        for lu in 1..4 {
            assert!(
                critical.iter().any(|w| w.contains(&format!("LU {lu} is"))),
                "LU {lu} does not exist yet: {critical:?}"
            );
        }
    }

    #[test]
    fn a_provisioned_device_does_not_prompt_again() {
        // Idempotence: comparing the plan against the descriptor the plan itself
        // produces must find nothing, or the installer would offer to wipe a
        // freshly provisioned device on every run.
        let scheme = Scheme::embedded_default();
        for (name, d, g) in [
            ("biwin 256G", BIWIN256_DEVICE, BIWIN256_GEOMETRY),
            ("biwin 64G", BIWIN64_DEVICE, BIWIN64_GEOMETRY),
            ("foresee 64G", FORESEE64_DEVICE, FORESEE64_GEOMETRY),
        ] {
            let dev = DeviceDescriptor::parse(&unhex(d)).expect(name);
            let geo = GeometryDescriptor::parse(&unhex(g)).expect(name);
            let built = plan(&scheme, &dev, &geo).expect(name);
            let written = built.descriptor.clone();
            let mismatches = compare(&built, &written, built.boot_lun_en, &[]);
            assert!(
                mismatches.is_empty(),
                "{name} still differs after provisioning: {mismatches:?}"
            );
        }
    }

    #[test]
    fn either_side_of_the_boot_pair_may_be_active() {
        // The installer writes the spare boot LU and switches to it once the image
        // verifies, so a provisioned device boots A or B depending on how many
        // updates it has had. Neither may be reported as a mismatch, or every
        // second install would offer to wipe the device.
        let scheme = Scheme::embedded_default();
        let dev = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();
        let built = plan(&scheme, &dev, &geo).unwrap();
        let written = built.descriptor.clone();

        for active in [ufs::BOOT_LUN_A, ufs::BOOT_LUN_B] {
            let mismatches = compare(&built, &written, active, &[]);
            assert!(
                mismatches.is_empty(),
                "booting {active} should be fine: {mismatches:?}"
            );
        }
    }

    #[test]
    fn booting_nothing_is_flagged_but_erases_nothing() {
        let scheme = Scheme::embedded_default();
        let dev = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();
        let built = plan(&scheme, &dev, &geo).unwrap();
        let written = built.descriptor.clone();

        // A device on the scheme, except that the boot ROM is pointed at no boot
        // LU at all — so it would not boot, but one attribute write fixes it.
        let mismatches = compare(&built, &written, ufs::BOOT_LUN_NONE, &[]);
        let critical: Vec<&Mismatch> = mismatches.iter().filter(|m| m.critical).collect();
        assert_eq!(critical.len(), 1, "{mismatches:?}");
        assert!(critical[0].what.contains("bBootLunEn"));
        assert!(
            critical[0].what.contains("want A or B"),
            "{:?}",
            critical[0]
        );
        assert!(
            !critical[0].descriptor_write,
            "switching an attribute must not be advertised as destructive"
        );
    }

    #[test]
    fn writebooster_follows_what_the_device_supports() {
        let scheme = Scheme::embedded_default();
        let dev = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();

        // The Biwin 256 GB supports only a shared buffer.
        let built = plan(&scheme, &dev, &geo).unwrap();
        assert_eq!(built.descriptor.writebooster_buffer_type(), 0x01);
        assert_eq!(built.descriptor.writebooster_shared_alloc_units(), 6144);
        // User space preserved, so it costs LU 0 nothing.
        assert_eq!(built.descriptor.writebooster_preserve_user_space(), 1);
        assert_eq!(built.lus()[0].writebooster_alloc_units, 0);
        assert_eq!(built.lus()[0].num_alloc_units as u64, 61022 - 120);

        // Asking for a dedicated buffer on a device that only offers a shared
        // one still produces a working configuration.
        let dedicated = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [writebooster]
            size = "max"
            type = "lu"
            [[lu]]
            id = 0
            memory_type = "normal"
            "#,
        )
        .unwrap();
        let built = plan(&dedicated, &dev, &geo).unwrap();
        assert_eq!(built.descriptor.writebooster_buffer_type(), 0x01);

        // A device that offers both honours the request, and charges the buffer
        // to the LU rather than the shared field.
        let dev64 = DeviceDescriptor::parse(&unhex(BIWIN64_DEVICE)).unwrap();
        let geo64 = GeometryDescriptor::parse(&unhex(BIWIN64_GEOMETRY)).unwrap();
        let built = plan(&dedicated, &dev64, &geo64).unwrap();
        assert_eq!(built.descriptor.writebooster_buffer_type(), 0x00);
        assert_eq!(built.descriptor.writebooster_shared_alloc_units(), 0);
        assert_eq!(built.lus()[0].writebooster_alloc_units, 2280);

        // An explicit size in MiB becomes allocation units.
        let sized = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [writebooster]
            size = 2048
            [[lu]]
            id = 0
            memory_type = "normal"
            "#,
        )
        .unwrap();
        let built = plan(&sized, &dev, &geo).unwrap();
        assert_eq!(built.descriptor.writebooster_shared_alloc_units(), 512);

        // Zero disables it.
        let off = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [writebooster]
            size = 0
            [[lu]]
            id = 0
            memory_type = "normal"
            "#,
        )
        .unwrap();
        let built = plan(&off, &dev, &geo).unwrap();
        assert_eq!(built.descriptor.writebooster_shared_alloc_units(), 0);
        assert_eq!(built.descriptor.writebooster_buffer_type(), 0x00);
    }

    #[test]
    fn a_scheme_larger_than_the_device_is_rejected() {
        let scheme = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [[lu]]
            id = 0
            memory_type = "normal"
            [[lu]]
            id = 1
            memory_type = "enhanced1"
            size_mib = 1048576
            "#,
        )
        .unwrap();
        let dev = DeviceDescriptor::parse(&unhex(FORESEE64_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(FORESEE64_GEOMETRY)).unwrap();
        let err = plan(&scheme, &dev, &geo).expect_err("1 TiB cannot fit in 64 GB");
        assert!(err.contains("allocation units"), "{err}");
    }

    #[test]
    fn enhanced_memory_has_its_own_ceiling() {
        // The Foresee only allows about half its capacity as Enhanced1 (7486 of
        // 15260 allocation units), so a 16 GiB Enhanced1 LU fits the device but
        // not that ceiling, and must be refused rather than written.
        let scheme = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [[lu]]
            id = 0
            memory_type = "normal"
            [[lu]]
            id = 1
            memory_type = "enhanced1"
            size_mib = 16384
            "#,
        )
        .unwrap();
        let dev = DeviceDescriptor::parse(&unhex(FORESEE64_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(FORESEE64_GEOMETRY)).unwrap();
        let err = plan(&scheme, &dev, &geo).expect_err("over the enhanced1 ceiling");
        assert!(err.contains("enhanced1"), "{err}");
        assert!(err.contains("7486"), "the device's own ceiling: {err}");
    }

    #[test]
    fn a_main_lu_too_small_to_install_onto_is_rejected() {
        // 60 GiB of a 64 GB device claimed by a fixed LU leaves too little for
        // the Btrfs volume, which must be caught before anything is written.
        let scheme = Scheme::parse(
            r#"
            active_boot_lu = "none"
            [[lu]]
            id = 0
            memory_type = "normal"
            [[lu]]
            id = 1
            memory_type = "normal"
            size_mib = 57344
            "#,
        )
        .unwrap();
        let dev = DeviceDescriptor::parse(&unhex(FORESEE64_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(FORESEE64_GEOMETRY)).unwrap();
        let err = plan(&scheme, &dev, &geo).expect_err("no room for an install");
        assert!(err.contains("an install needs"), "{err}");
    }

    #[test]
    fn the_prompt_leads_with_the_consequence() {
        // Only the first handful of lines fit the 256x144 panel, so the warning
        // that the device gets erased has to be the very first thing on it.
        let st = status(
            BIWIN256_DEVICE,
            BIWIN256_GEOMETRY,
            BIWIN256_CONFIG,
            ufs::BOOT_LUN_A,
        );
        let lines = st.prompt_lines();
        assert_eq!(lines[0], "ALL DATA ON /dev/sda WILL BE PERMANENTLY LOST.");
        assert_eq!(lines[1], "", "a blank line separates it from the detail");
        // Then the layout, in sizes rather than allocation units.
        assert!(lines.contains(&"Logical units, now and wanted:".to_string()));
        assert!(
            lines
                .iter()
                .any(|l| l == "LU 1: 4.0 MiB boot A \u{2192} 16.0 MiB boot A"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "LU 3: 8.0 MiB \u{2192} 128.0 MiB"),
            "{lines:?}"
        );
        assert_eq!(st.short_label(), "unprovisioned");
    }

    #[test]
    fn the_prompt_says_so_when_nothing_is_erased() {
        // A device already on the scheme but pointed at no boot LU at all: fixing
        // it is one attribute write, and the prompt must not claim otherwise.
        let dev = DeviceDescriptor::parse(&unhex(BIWIN256_DEVICE)).unwrap();
        let geo = GeometryDescriptor::parse(&unhex(BIWIN256_GEOMETRY)).unwrap();
        let built = plan(&Scheme::embedded_default(), &dev, &geo).unwrap();
        let provisioned = built.descriptor.clone();
        let mut st = status(
            BIWIN256_DEVICE,
            BIWIN256_GEOMETRY,
            BIWIN256_CONFIG,
            ufs::BOOT_LUN_NONE,
        );
        st.current = provisioned.lus();
        st.boot_enable = provisioned.boot_enable();
        st.mismatches = compare(&built, &provisioned, ufs::BOOT_LUN_NONE, &[]);

        assert!(!st.is_provisioned());
        assert!(!st.needs_descriptor_write());
        let lines = st.prompt_lines();
        assert!(!lines[0].contains("LOST"), "{lines:?}");
        assert!(lines[0].contains("No data is erased"), "{lines:?}");
        assert!(
            lines.iter().any(|l| l == "Active boot LU: none \u{2192} A"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_missing_boot_feature_is_called_out_in_the_prompt() {
        // The Foresee ships with bBootEnable clear, which is the difference that
        // would otherwise leave a freshly flashed board dead.
        let st = status(
            FORESEE64_DEVICE,
            FORESEE64_GEOMETRY,
            FORESEE64_CONFIG,
            ufs::BOOT_LUN_NONE,
        );
        let lines = st.prompt_lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "The boot feature is off, so the board cannot boot."),
            "{lines:?}"
        );
        // LUs 1-3 do not exist yet, and are shown as arriving.
        for lu in 1..4 {
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with(&format!("LU {lu}: absent"))),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn the_report_lists_the_layout_and_every_difference() {
        let st = status(
            BIWIN256_DEVICE,
            BIWIN256_GEOMETRY,
            BIWIN256_CONFIG,
            ufs::BOOT_LUN_A,
        );
        let report = st.report().join("\n");
        assert!(report.contains("scheme: built-in default"), "{report}");
        assert!(report.contains("allocation unit 4.0 MiB"), "{report}");
        assert!(report.contains("boot LU A"), "{report}");
        assert!(report.contains("active boot LU: A"), "{report}");
        assert!(report.contains("mismatch:"), "{report}");
        assert!(!report.contains("matches the Flipper scheme"), "{report}");
        // Disabled LUs are left out of the layout listing.
        assert!(!report.contains("LU 7:"), "{report}");
    }

    #[test]
    fn a_bad_scheme_file_falls_back_and_says_why() {
        let dir = std::env::temp_dir().join("flipperos-ufs-scheme-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.toml");
        std::fs::write(&path, "this is not toml =\n").unwrap();
        let cfg = Config {
            ufs_scheme: Some(path.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let (scheme, origin) = Scheme::resolve(&cfg);
        // The built-in scheme still applies, and the reason travels with it so it
        // reaches the activity log rather than being swallowed.
        assert_eq!(scheme.lus.len(), 4);
        assert!(origin.starts_with("built-in default ("), "{origin}");
        assert!(origin.contains("parsing ufs scheme"), "{origin}");

        // A scheme file that is fine is used, and named.
        let good = dir.join("good.toml");
        std::fs::write(
            &good,
            "active_boot_lu = \"none\"\n[[lu]]\nid = 0\nmemory_type = \"normal\"\n",
        )
        .unwrap();
        let cfg = Config {
            ufs_scheme: Some(good.to_string_lossy().into_owned()),
            ..Config::default()
        };
        let (scheme, origin) = Scheme::resolve(&cfg);
        assert_eq!(scheme.lus.len(), 1);
        assert!(origin.starts_with("file "), "{origin}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alloc_units_round_up() {
        // 4 MiB allocation units, Enhanced1 costing three raw units per usable
        // one: 16 MiB needs 12, and anything over a unit boundary rounds up.
        assert_eq!(alloc_units_for(16 * 1024 * 1024, 3, 8192), 12);
        assert_eq!(alloc_units_for(128 * 1024 * 1024, 3, 8192), 96);
        assert_eq!(alloc_units_for(4 * 1024 * 1024, 3, 8192), 3);
        assert_eq!(alloc_units_for(4 * 1024 * 1024, 1, 8192), 1);
        assert_eq!(alloc_units_for(4 * 1024 * 1024 + 1, 1, 8192), 2);
        assert_eq!(alloc_units_for(1, 1, 8192), 1);
    }
}
