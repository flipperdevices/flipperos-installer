//! Ad-hoc probe: fetch and print the newest U-Boot, boot menu and snapshot
//! builds from the real image server, and the profiles of the newest snapshot
//! build.
//!
//! Run with: `cargo run --example catalog_probe --no-default-features`

use flipperos_installer::core::catalog::{self, Origin};

fn main() {
    let origin = Origin::Server {
        base: "https://dl-linux-images.flipp.dev".to_string(),
    };

    let types = catalog::supported_device_types(&origin);
    println!(
        "== Supported device types (latest U-Boot manifest): {} ==",
        types.len()
    );
    println!("  {}", types.join(", "));

    let uboot = catalog::uboot_builds(&origin, "flipper-one", 5);
    println!("== U-Boot builds (newest first): {} ==", uboot.len());
    for b in &uboot {
        println!("  {}  [{}]", b.summary(), b.mtime);
        println!("      -> {}", b.image_location);
        // The image size and digest only arrive with the manifest. So does the
        // boot menu, for a build old enough to carry one of its own.
        match catalog::load_uboot_contents(b, "flipper-one") {
            Ok(c) => println!(
                "      image {} B, in-tree menu {}",
                c.size,
                match c.boot_menu {
                    Some(m) => format!("{} B", m.size_bytes),
                    None => "(none)".to_string(),
                }
            ),
            Err(e) => println!("      manifest error: {e}"),
        }
    }

    let menus = catalog::boot_menu_builds(&origin, "flipper-one", 5);
    println!("\n== Boot menu builds (newest first): {} ==", menus.len());
    for b in &menus {
        println!("  {}  [{}]", b.summary(), b.mtime);
        println!("      -> {}", b.image_location);
        match catalog::load_boot_menu_contents(b, "flipper-one") {
            Ok(c) => println!("      image {} B", c.size),
            Err(e) => println!("      manifest error: {e}"),
        }
    }

    let recoveries = catalog::recovery_builds(&origin, "flipper-one", 5);
    println!(
        "\n== Recovery builds (newest first): {} ==",
        recoveries.len()
    );
    for b in &recoveries {
        println!("  {}  [{}]", b.summary(), b.mtime);
        println!("      -> {}", b.image_location);
        match catalog::load_recovery_contents(b, "flipper-one") {
            Ok(c) => println!("      image {} B", c.size),
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
