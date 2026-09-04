//! Ad-hoc probe: browse the real update-bundle bucket and resolve a bundle,
//! printing everything the installer would use.
//!
//! Run with: `cargo run --example bundle_probe --no-default-features`
//!
//! Optional arguments:
//!   `--channel <path>`   list a channel other than the first non-dev one
//!   `--archive <path>`   read a local `*.tar.zst` bundle instead of the bucket
//!   `--board <id>`       resolve for a board other than `flipper-one`

use flipperos_installer::core::bundle::{self, Repo};
use flipperos_installer::core::model::{human_bytes, BundleLocation, BundleRef, Source};

fn main() {
    let mut channel: Option<String> = None;
    let mut archive: Option<String> = None;
    let mut board = "flipper-one".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--channel" => channel = args.next(),
            "--archive" => archive = args.next(),
            "--board" => board = args.next().unwrap_or(board),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let repo = Repo {
        list_url: "https://storage.googleapis.com/storage/v1/b/\
                   flipper-dev-update-server-euw1-dev/o"
            .to_string(),
        base_url: "https://update.flipper.net.flipp.dev".to_string(),
        prefix: "bundles".to_string(),
    };

    if let Some(path) = archive {
        let reference = BundleRef {
            id: path.clone(),
            channel: "local".to_string(),
            dir: path.clone(),
            location: BundleLocation::Dir {
                root: "/tmp/bundle-probe".to_string(),
            },
            source: Source::Local {
                root: "/tmp/bundle-probe".to_string(),
            },
            archive: Some(path.clone()),
        };
        println!("== Local archive: {path} ==");
        match bundle::load(&reference, &board) {
            Ok(b) => print_bundle(&b),
            Err(e) => println!("  error: {e}"),
        }
        return;
    }

    let channels = match bundle::channels(&repo) {
        Ok(c) => c,
        Err(e) => {
            println!("listing channels failed: {e}");
            return;
        }
    };
    println!("== Channels: {} ==", channels.join(", "));

    // The dev channel holds another level of directories, so show how deep it is
    // rather than trying to list builds in it.
    for name in &channels {
        if name == bundle::DEV_CHANNEL {
            match bundle::dirs(&repo, name) {
                Ok(users) if users.is_empty() => println!("  {name}/ — empty"),
                Ok(users) => println!("  {name}/ — {} user(s): {}", users.len(), users.join(", ")),
                Err(e) => println!("  {name}/ — error: {e}"),
            }
            continue;
        }
        match bundle::builds(&repo, name) {
            Ok(builds) => {
                println!("  {name}/ — {} build(s), newest first:", builds.len());
                for b in builds.iter().take(5) {
                    println!("      {}", b.dir);
                }
            }
            Err(e) => println!("  {name}/ — error: {e}"),
        }
    }

    let target = channel.or_else(|| {
        channels
            .iter()
            .find(|c| c.as_str() != bundle::DEV_CHANNEL)
            .cloned()
    });
    let Some(target) = target else {
        println!("no channel to resolve");
        return;
    };
    let Some(newest) = bundle::builds(&repo, &target)
        .ok()
        .and_then(|b| b.into_iter().next())
    else {
        println!("no builds in {target}");
        return;
    };

    println!("\n== Resolving {} (board {board}) ==", newest.id);
    match bundle::load(&newest, &board) {
        Ok(b) => print_bundle(&b),
        Err(e) => println!("  error: {e}"),
    }
}

fn print_bundle(b: &flipperos_installer::core::model::SelectedBundle) {
    println!("  channel : {}", b.channel);
    println!("  version : {}", b.version);
    println!("  build   : {}", b.build.display_name());
    println!("  archive : {}", b.archive);
    println!("  boards  : {}", b.device_types.join(", "));
    println!("  u-boot  : {} ({})", b.uboot.image_location, human_bytes(b.uboot.size_bytes));
    println!("            sha256 {}", b.uboot.sha256.as_deref().unwrap_or("(none)"));
    match &b.uboot.boot_menu {
        Some(m) => {
            println!("  menu    : {} ({})", m.location, human_bytes(m.size_bytes));
            println!("            sha256 {}", m.sha256.as_deref().unwrap_or("(none)"));
        }
        None => println!("  menu    : (none)"),
    }
    println!("  profiles: {}", b.build.profiles.len());
    for p in &b.build.profiles {
        for (kind, pack) in [("full", &p.full), ("inc", &p.incremental)] {
            if let Some(pack) = pack {
                println!(
                    "      {:<14} {:<4} {:>10}  sha256 {}",
                    p.name,
                    kind,
                    human_bytes(pack.size_bytes),
                    pack.sha256.as_deref().unwrap_or("(none)"),
                );
            }
        }
    }
    match &b.build.home_pack {
        Some(h) => println!(
            "  /home   : {} ({})",
            h.location,
            human_bytes(h.size_bytes)
        ),
        None => println!("  /home   : (no seed)"),
    }
}
