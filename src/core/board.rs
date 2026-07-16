//! Board / SoC discovery from the device tree.
//!
//! On a booted RK3576 system the kernel exposes the flattened device tree under
//! `/proc/device-tree` (a.k.a. `/sys/firmware/devicetree/base`). We read the
//! root `compatible` and `model` properties to work out which board we are on
//! and derive a canonical `board_id` used to query the image server.

use std::fs;
use std::path::Path;

use crate::core::model::BoardInfo;

const DT_ROOT: &str = "/proc/device-tree";

/// Detect the running board. Never fails: on non-DT hosts (e.g. an x86 dev box)
/// it falls back to a generic identity so the rest of the tool still runs.
pub fn detect() -> BoardInfo {
    let compatible = read_string_list(&format!("{DT_ROOT}/compatible"));
    let model = read_string(&format!("{DT_ROOT}/model")).unwrap_or_default();

    let soc = compatible
        .iter()
        .find(|c| c.contains("rk35") || c.contains("rockchip"))
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());

    let board_id = derive_board_id(&compatible);

    BoardInfo {
        compatible,
        model: if model.is_empty() {
            "unknown board".to_string()
        } else {
            model
        },
        soc,
        board_id,
    }
}

/// Device-tree `compatible` string → image-server device-type id.
///
/// The id on the right must match a `<device-type>/u-boot-rockchip.bin` directory
/// published in the U-Boot build manifest on the image server (see
/// [`crate::core::catalog::supported_device_types`]; at time of writing that set
/// is `evb, flipper-one, generic, nanopi-m5, nanopi-r76s, omni3576, roc-pc,
/// rock-4d, sige5`). This is the authoritative map used to identify which
/// supported device we are running on; add a row here when a board's exact
/// device-tree `compatible` string is known. Boards not listed here are never
/// guessed — they fall back to the `generic` U-Boot build.
pub const COMPATIBLE_DEVICE_TYPES: &[(&str, &str)] = &[
    ("flipper,one-rev-f0b0c1", "flipper-one"),
    ("flipper,one-rev-f0b1c2", "flipper-one"),
    ("friendlyarm,nanopi-m5", "nanopi-m5"),
    ("friendlyarm,nanopi-r76s", "nanopi-r76s"),
    ("luckfox,omni3576", "omni3576"),
    ("firefly,roc-rk3576-pc", "roc-pc"),
    ("radxa,rock-4d", "rock-4d"),
    ("armsom,sige5", "sige5"),
    ("rockchip,rk3576-evb1-v10", "evb"),
];

/// Map the device-tree `compatible` list to an image-server device-type id,
/// trying the most specific compatible string first.
pub fn device_type_for(compatible: &[String]) -> Option<&'static str> {
    compatible.iter().find_map(|c| {
        COMPATIBLE_DEVICE_TYPES
            .iter()
            .find(|(compat, _)| c == compat)
            .map(|(_, id)| *id)
    })
}

/// Map device-tree identity to the image-server device-type id.
///
/// Only the explicit [`COMPATIBLE_DEVICE_TYPES`] dictionary is trusted: an
/// unrecognised board is never guessed from loose string matching — it falls
/// back to the `generic` U-Boot build, which every supported SoC can boot.
fn derive_board_id(compatible: &[String]) -> String {
    device_type_for(compatible)
        .map(str::to_string)
        .unwrap_or_else(|| "generic".to_string())
}

/// Read a NUL-terminated device-tree string property.
fn read_string(path: &str) -> Option<String> {
    let bytes = fs::read(Path::new(path)).ok()?;
    Some(bytes_to_string(&bytes))
}

/// Read a device-tree "stringlist" property (NUL-separated strings).
fn read_string_list(path: &str) -> Vec<String> {
    match fs::read(Path::new(path)) {
        Ok(bytes) => bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn bytes_to_string(bytes: &[u8]) -> String {
    let trimmed: &[u8] = match bytes.iter().position(|b| *b == 0) {
        Some(n) => &bytes[..n],
        None => bytes,
    };
    String::from_utf8_lossy(trimmed).into_owned()
}
