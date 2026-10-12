//! What the metadata partition records about an installation.
//!
//! The partition belongs to the profiles' `flipmeta` tool, which stores one
//! CBOR blob in it and edits it a dotted key at a time. The installer adds a
//! single key, [`KEY`], holding the `build` + `sourcestamps` records of what it
//! wrote, so a booted system can say where it came from:
//!
//! ```text
//! install:
//!   mode: "bundle" | "custom"
//!   bundle:     {name, version, channel, build, sourcestamps}   # bundle installs
//!   components: {u-boot, rootfs, boot-menu, recovery}           # custom installs
//! ```
//!
//! A bundle is one build with one set of sourcestamps covering everything in
//! it, so it is recorded once. A custom install is put together from builds of
//! their own, so each component that was written is recorded with its own
//! manifest's records.
//!
//! Writing it is [`crate::core::install`]'s business; this module only decides
//! what goes in, from the selection alone, so it can be tested without a disk.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::core::model::{AppState, BuildDetails, InstallMode};

/// The top-level key the record is stored under.
pub const KEY: &str = "install";

/// The installation record stored under [`KEY`].
#[derive(Debug, Serialize)]
pub struct InstallRecord {
    /// [`InstallMode::label`]: which of the other two fields to expect.
    pub mode: &'static str,
    /// The bundle installed from. Bundle installs only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BundleRecord>,
    /// The builds installed from, by component, for those whose records are
    /// known. Custom installs only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub components: BTreeMap<&'static str, BuildDetails>,
}

/// A bundle's identity and its manifest's records.
#[derive(Debug, Serialize)]
pub struct BundleRecord {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub channel: String,
    /// `build` and `sourcestamps`, side by side with the fields above.
    #[serde(flatten)]
    pub details: BuildDetails,
}

impl InstallRecord {
    /// The record as the compact JSON `flipmeta set --json` takes.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| format!("encode the install record: {e}"))
    }
}

/// Build the record for the installation `state` describes.
///
/// `rootfs` is the rootfs build's records for a custom install. They are passed
/// in rather than read from the selection because the selection only has them
/// once the details popup asked for them. `boot_menu` and `recovery` say
/// whether the run writes those images at all: a target that takes neither
/// should not claim either.
pub fn record(
    state: &AppState,
    rootfs: Option<BuildDetails>,
    boot_menu: bool,
    recovery: bool,
) -> InstallRecord {
    let mode = state.selection.mode;
    let mut record = InstallRecord {
        mode: mode.label(),
        bundle: None,
        components: BTreeMap::new(),
    };
    match mode {
        InstallMode::Bundle => {
            // `resolve` hands the bundle's own records to the builds it pins,
            // having read them from the nested `bundle.build` correctly.
            record.bundle = state.bundle.as_ref().map(|b| BundleRecord {
                name: b.name.clone(),
                version: b.version.clone(),
                channel: b.channel.clone(),
                details: b.build.details.clone().unwrap_or_default(),
            });
        }
        InstallMode::Custom => {
            let components = [
                (
                    "u-boot",
                    state.selected_uboot().and_then(|b| b.details.clone()),
                ),
                ("rootfs", rootfs),
                (
                    "boot-menu",
                    state
                        .selected_boot_menu_build()
                        .and_then(|b| b.details.clone())
                        .filter(|_| boot_menu),
                ),
                (
                    "recovery",
                    state
                        .selected_recovery_build()
                        .and_then(|b| b.details.clone())
                        .filter(|_| recovery),
                ),
            ];
            record.components = components
                .into_iter()
                .filter_map(|(name, details)| Some((name, details?)))
                .collect();
        }
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bundle;
    use crate::core::model::{
        BundleLocation, BundleRef, FalconBuild, Selection, Source, UbootBuild,
    };
    use serde_json::Value;

    fn details(json: &str) -> BuildDetails {
        serde_json::from_str(json).expect("details parse")
    }

    fn uboot(details: Option<BuildDetails>) -> UbootBuild {
        UbootBuild {
            id: "u".into(),
            label: "u".into(),
            mtime: String::new(),
            image_location: "https://i.invalid/u/flipper-one/u-boot-rockchip.bin".into(),
            manifest_location: "https://i.invalid/u/manifest.json".into(),
            source: Source::Server,
            size_bytes: 10,
            sha256: None,
            details,
            boot_menu: None,
            loaded: true,
        }
    }

    fn falcon(id: &str, details: Option<BuildDetails>) -> FalconBuild {
        FalconBuild {
            id: id.into(),
            label: id.into(),
            mtime: String::new(),
            image_location: format!("https://i.invalid/{id}/flipper-one/x.itb"),
            manifest_location: format!("https://i.invalid/{id}/manifest.json"),
            source: Source::Server,
            size_bytes: 1,
            sha256: None,
            details,
            loaded: true,
        }
    }

    fn json(record: &InstallRecord) -> Value {
        serde_json::from_str(&record.to_json().unwrap()).unwrap()
    }

    #[test]
    fn a_bundle_is_recorded_once_with_its_own_records() {
        let manifest: bundle::Manifest =
            serde_json::from_str(include_str!("testdata/bundle-manifest.json")).unwrap();
        let reference = BundleRef {
            id: "local".into(),
            channel: "local".into(),
            dir: "b".into(),
            location: BundleLocation::Dir {
                root: "/m/b/".into(),
            },
            source: Source::Local { root: "/m".into() },
            archive: None,
        };
        let selected =
            bundle::resolve(&reference, &reference.location, &manifest, "flipper-one").unwrap();
        let state = AppState {
            bundle: Some(selected),
            selection: Selection {
                mode: InstallMode::Bundle,
                ..Selection::default()
            },
            ..AppState::default()
        };

        // What the operator passes for the rootfs is ignored: the bundle speaks
        // for everything in it.
        let v = json(&record(&state, Some(details("{}")), true, true));
        assert_eq!(v["mode"], "bundle");
        assert!(v.get("components").is_none());
        let b = &v["bundle"];
        assert_eq!(b["name"], "flipperone-update");
        assert_eq!(b["version"], "20260812-83ddb68");
        assert_eq!(b["channel"], "nightly");
        assert_eq!(b["build"]["builder"], "update-bundle");
        assert_eq!(b["build"]["number"], 15);
        assert_eq!(b["sourcestamps"].as_array().unwrap().len(), 3);
        assert_eq!(b["sourcestamps"][2]["codebase"], "u-boot");
    }

    #[test]
    fn a_custom_install_records_each_component_it_wrote() {
        let state = AppState {
            uboot_builds: vec![uboot(Some(details(
                r#"{"build":{"builder":"uboot","number":570},
                    "sourcestamps":[{"codebase":"u-boot","revision":"fddd7e0"}]}"#,
            )))],
            boot_menu_builds: vec![falcon(
                "m",
                Some(details(
                    r#"{"build":{"builder":"falcon-bootmenu","number":1}}"#,
                )),
            )],
            recovery_builds: vec![falcon(
                "r",
                Some(details(
                    r#"{"build":{"builder":"falcon-recovery","number":2}}"#,
                )),
            )],
            selection: Selection {
                mode: InstallMode::Custom,
                uboot: Some("u".into()),
                boot_menu: Some("m".into()),
                recovery: Some("r".into()),
                ..Selection::default()
            },
            ..AppState::default()
        };
        let rootfs = details(r#"{"build":{"builder":"rootfs"}}"#);

        let v = json(&record(&state, Some(rootfs.clone()), true, true));
        assert_eq!(v["mode"], "custom");
        assert!(v.get("bundle").is_none());
        let c = &v["components"];
        assert_eq!(c["u-boot"]["build"]["number"], 570);
        assert_eq!(c["u-boot"]["sourcestamps"][0]["revision"], "fddd7e0");
        assert_eq!(c["boot-menu"]["build"]["number"], 1);
        assert_eq!(c["recovery"]["build"]["number"], 2);
        // A field the manifest did not have is not stored as null or "".
        assert_eq!(
            c["rootfs"]["build"],
            serde_json::json!({ "builder": "rootfs" })
        );

        // A target that takes neither Falcon image does not claim them, and a
        // rootfs whose manifest could not be read is left out.
        let v = json(&record(&state, None, false, false));
        let names: Vec<&str> = v["components"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, ["u-boot"]);
    }
}
