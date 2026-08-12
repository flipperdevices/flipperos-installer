//! Ad-hoc probe: drive a whole installation headlessly, in dry-run mode.
//!
//! Walks the same path a frontend would — discover, refresh, pick a source
//! through the menu tree, pick a target, start the install — and prints the
//! activity log, so the flow can be exercised without a terminal or a screen.
//! Nothing destructive runs: `dry_run` stays on, so every command is logged
//! instead of executed.
//!
//! Run with: `cargo run --example dry_run --no-default-features`
//!
//! Optional arguments:
//!   `--bundle <path>`  add a local bundle directory or `*.tar.zst`
//!   `--custom`         install a hand-picked U-Boot + rootfs pair instead
//!   `--stream`         stream and verify while writing, rather than up front
//!   `--profile <name>` add an extra profile (repeatable)

use flipperos_installer::core::menu::{self, MenuKey, Nav};
use flipperos_installer::core::model::{FetchMode, InstallMode, Phase};
use flipperos_installer::core::{Config, Controller};

fn main() {
    let mut config = Config {
        // The point of this probe: log every destructive step, run none of it.
        dry_run: true,
        // Nothing here should mount anything on a development host.
        automount: false,
        ..Config::default()
    };
    let mut profiles: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bundle" => config.bundle_paths.push(args.next().expect("--bundle <path>")),
            "--custom" => config.mode = InstallMode::Custom,
            "--stream" => config.fetch = FetchMode::Stream,
            "--profile" => profiles.push(args.next().expect("--profile <name>")),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let ctrl = Controller::new(config);
    ctrl.discover();
    ctrl.refresh_sources();

    // Walk the menu the way an operator would, so the printed levels are exactly
    // what a frontend would draw.
    let nav = Nav::new();
    let state = ctrl.snapshot();
    for key in [MenuKey::Root, MenuKey::Source] {
        let level = menu::level(&clone_at(&nav, key.clone()), &state);
        println!("== {} ==", level.crumb);
        for item in &level.items {
            println!(
                "  {:<28} {:<24} {}",
                item.text,
                item.detail,
                if item.drill { "\u{203a}" } else { "" }
            );
        }
    }

    for name in &profiles {
        ctrl.toggle_profile(name, true);
    }

    let state = ctrl.snapshot();
    println!("\n== Selection ==");
    println!("  source  : {}", state.source_label());
    println!("  fetch   : {}", state.selection.fetch.label());
    println!(
        "  target  : {}",
        state
            .target()
            .map(|d| d.summary())
            .unwrap_or_else(|| "(none)".to_string())
    );
    println!(
        "  u-boot  : {}",
        state
            .selected_uboot()
            .map(|b| format!("{} ({})", b.label, b.image_location))
            .unwrap_or_else(|| "(none)".to_string())
    );
    println!(
        "  rootfs  : {}",
        state
            .selected_build()
            .map(|b| b.summary())
            .unwrap_or_else(|| "(none)".to_string())
    );
    println!("  profiles: Minimal + {:?}", state.selection.profiles);
    println!("  ready   : {}", state.can_install());
    if let Some(e) = &state.bundle_error {
        println!("  bundle error: {e}");
    }

    if !state.can_install() {
        println!("\nnothing to install (no target device on this host?)");
        return;
    }

    println!("\n== Dry-run install ==");
    let from = state.log.len();
    ctrl.start_install();
    // The install runs on a worker thread; wait for it to settle.
    loop {
        let s = ctrl.snapshot();
        if !matches!(s.phase, Phase::Installing | Phase::Ready | Phase::Discovering) {
            for line in s.log.iter().skip(from) {
                println!("  {line}");
            }
            println!("\nphase: {}", s.phase.label());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A navigation position pointing at `key`, for printing one level.
fn clone_at(nav: &Nav, key: MenuKey) -> Nav {
    let mut out = nav.clone();
    if key != MenuKey::Root {
        out.push(key);
    }
    out
}
