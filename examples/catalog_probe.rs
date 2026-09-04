//! Ad-hoc probe: fetch and print the newest U-Boot and snapshot builds from the
//! real image server, and the profiles of the newest snapshot build.
//!
//! Run with: `cargo run --example catalog_probe --no-default-features`

use flipperos_installer::core::catalog::{self, Origin};

fn main() {
    let origin = Origin::Server {
        base: "https://dl-linux-images.flipp.dev".to_string(),
    };

    let types = catalog::supported_device_types(&origin);
    println!("== Supported device types (latest U-Boot manifest): {} ==", types.len());
    println!("  {}", types.join(", "));

    let uboot = catalog::uboot_builds(&origin, "flipper-one", 5);
    println!("== U-Boot builds (newest first): {} ==", uboot.len());
    for b in &uboot {
        println!("  {}  [{}]", b.summary(), b.mtime);
        println!("      -> {}", b.image_location);
        // The image size, digest and boot menu only arrive with the manifest.
        // The locations share this build's (very long) directory, so print only
        // what distinguishes them.
        match catalog::load_uboot_contents(b, "flipper-one") {
            Ok(c) => match c.boot_menu {
                Some(m) => println!(
                    "      image {} B, menu {} B ({})",
                    c.size,
                    m.size_bytes,
                    m.location.rsplit('/').next().unwrap_or(&m.location)
                ),
                None => println!("      image {} B, menu (none)", c.size),
            },
            Err(e) => println!("      manifest error: {e}"),
        }
    }

    let snaps = catalog::snapshot_builds(&origin, 5);
    println!("\n== Snapshot builds (newest first): {} ==", snaps.len());
    for b in &snaps {
        println!("  {}  [{}]", b.summary(), b.mtime);
    }

    if let Some(first) = snaps.first() {
        println!("\n== Profiles for newest build: {} ==", first.label);
        match catalog::load_profiles(first) {
            Ok((number, profiles, home_pack)) => {
                println!("  build #{number:?}, {} profile(s)", profiles.len());
                for p in &profiles {
                    println!(
                        "    {:<14} full={:<5} inc={}",
                        p.name,
                        p.full.is_some(),
                        p.incremental.is_some()
                    );
                }
                println!("  home seed: {}", home_pack.is_some());
            }
            Err(e) => println!("  load_profiles error: {e}"),
        }
    }
}
