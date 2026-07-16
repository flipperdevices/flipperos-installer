//! Btrfs layout configuration.
//!
//! The shared, top-level subvolume skeleton created on every install is driven
//! by a small TOML file (see `config/flipperos-btrfs.toml`). At install time the
//! installer prefers a layout shipped alongside the image manifest and falls back
//! to the copy compiled into the binary.

use serde::Deserialize;
use std::fs;

use crate::core::controller::Config;
use crate::core::model::Source;

/// The layout compiled into the installer, used when no image-shipped one exists.
const DEFAULT_LAYOUT: &str = include_str!("../../config/flipperos-btrfs.toml");

/// A complete Btrfs layout: filesystem label, mount options and the shared
/// top-level subvolumes to create.
#[derive(Debug, Clone, Deserialize)]
pub struct Layout {
    #[serde(default = "default_label")]
    pub label: String,
    #[serde(default = "default_options")]
    pub options: String,
    #[serde(default, rename = "subvolume")]
    pub subvolumes: Vec<Subvolume>,
}

/// One shared top-level subvolume and its Btrfs properties.
#[derive(Debug, Clone, Deserialize)]
pub struct Subvolume {
    /// Subvolume name at the Btrfs top level, e.g. `boot` or `@home`.
    pub name: String,
    /// Where this subvolume is mounted on the running system (informational; the
    /// received profile roots carry their own `/etc/fstab`).
    #[serde(default)]
    pub mountpoint: Option<String>,
    /// Btrfs compression property to set on the subvolume, e.g. `none`, `zstd`.
    #[serde(default)]
    pub compression: Option<String>,
    /// Directories (relative to the subvolume root) to create with NODATACOW.
    #[serde(default)]
    pub nodatacow: Vec<String>,
}

fn default_label() -> String {
    "flipperos".to_string()
}

fn default_options() -> String {
    "compress=zstd,noatime,ssd,discard=async".to_string()
}

impl Layout {
    /// Parse a layout from TOML text.
    pub fn parse(text: &str) -> Result<Layout, String> {
        toml::from_str(text).map_err(|e| format!("parsing btrfs layout: {e}"))
    }

    /// The layout compiled into the installer.
    pub fn embedded_default() -> Layout {
        Self::parse(DEFAULT_LAYOUT).expect("built-in btrfs layout must be valid")
    }

    /// Name of the shared `/boot` subvolume (the one mounted at `/boot`),
    /// defaulting to `boot`.
    pub fn boot_subvol(&self) -> &str {
        self.subvolumes
            .iter()
            .find(|s| s.mountpoint.as_deref() == Some("/boot"))
            .map(|s| s.name.as_str())
            .unwrap_or("boot")
    }
}

/// Resolve the layout to use for an install.
///
/// Prefers a `btrfs-layout.toml` shipped with the images (next to the manifest),
/// scanning the sources of the selected profiles; falls back to the built-in
/// default. Returns the layout and a short description of where it came from.
pub fn resolve(cfg: &Config, board_id: &str, sources: &[&Source]) -> (Layout, String) {
    let mut tried: Vec<String> = Vec::new();
    for src in sources {
        let key = src.label();
        if tried.contains(&key) {
            continue;
        }
        tried.push(key);
        if let Some(found) = try_load(cfg, board_id, src) {
            return found;
        }
    }
    (Layout::embedded_default(), "built-in default".to_string())
}

/// Attempt to load an image-shipped layout from a single source.
fn try_load(cfg: &Config, board_id: &str, src: &Source) -> Option<(Layout, String)> {
    match src {
        Source::Removable { mountpoint, .. } => {
            let path = format!(
                "{}/flipperos/{}/btrfs-layout.toml",
                mountpoint.trim_end_matches('/'),
                board_id
            );
            let text = fs::read_to_string(&path).ok()?;
            let layout = Layout::parse(&text).ok()?;
            Some((layout, format!("media {path}")))
        }
        Source::Server => {
            let base = cfg.server_url.trim_end_matches('/');
            let url = format!("{base}/boards/{board_id}/btrfs-layout.toml");
            let resp = ureq::get(&url).call().ok()?;
            let text = resp.into_string().ok()?;
            let layout = Layout::parse(&text).ok()?;
            Some((layout, format!("server {url}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_default_parses() {
        let layout = Layout::embedded_default();
        assert_eq!(layout.label, "flipperos");
        // The shared /boot subvolume must be present and uncompressed.
        assert_eq!(layout.boot_subvol(), "boot");
        let boot = layout
            .subvolumes
            .iter()
            .find(|s| s.name == "boot")
            .expect("boot subvolume");
        assert_eq!(boot.compression.as_deref(), Some("none"));
        // journald journal dir is NODATACOW under @var-log.
        let varlog = layout
            .subvolumes
            .iter()
            .find(|s| s.name == "@var-log")
            .expect("@var-log subvolume");
        assert_eq!(varlog.nodatacow, vec!["journal".to_string()]);
    }
}

