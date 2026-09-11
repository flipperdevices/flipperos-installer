//! FlipperOS installer entry point.
//!
//! Parses configuration, kicks off hardware/source discovery on a worker
//! thread, then launches the selected frontend(s). When both the TUI and the
//! GUI are enabled they run concurrently against one shared [`Controller`], so
//! the operator can drive the installer from the serial console and the
//! on-device buttons interchangeably.

use std::process::ExitCode;
use std::sync::Arc;

use flipperos_installer::core::model::{FetchMode, InstallMode};
use flipperos_installer::core::power;
use flipperos_installer::core::{Config, Controller};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrontendChoice {
    /// Pick based on compiled-in features.
    Auto,
    Tui,
    Gui,
    Both,
}

struct Args {
    config: Config,
    frontend: FrontendChoice,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            return ExitCode::FAILURE;
        }
    };

    let controller = Controller::new(args.config);

    // Discover hardware and sources off the UI thread; results stream into both
    // frontends via the controller's subscribers.
    {
        let ctrl = Arc::clone(&controller);
        std::thread::spawn(move || {
            ctrl.discover();
            ctrl.refresh_sources();
        });
    }

    let result = run_frontends(Arc::clone(&controller), args.frontend);

    // Every frontend's event loop has returned by now, so the TUI has left the
    // alternate screen and taken the terminal out of raw mode. Only here is it
    // safe to bring the machine down: rebooting from the UI callback that asked
    // for it would skip that teardown and leave the serial console unusable.
    //
    // A frontend that failed never reboots — we don't know what the operator is
    // looking at, or whether the install even ran.
    match result {
        Ok(()) => {
            if controller.reboot_requested() {
                if let Err(e) = power::reboot(controller.config()) {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_frontends(ctrl: Arc<Controller>, choice: FrontendChoice) -> Result<(), String> {
    let has_tui = cfg!(feature = "tui");
    let has_gui = cfg!(feature = "gui");

    // What the operator asked for, before any availability checks. `Auto` means
    // "every frontend this binary was built with".
    let (mut want_tui, mut want_gui) = match choice {
        FrontendChoice::Auto => (has_tui, has_gui),
        FrontendChoice::Tui => (true, false),
        FrontendChoice::Gui => (false, true),
        FrontendChoice::Both => (true, true),
    };

    // Reject requests for frontends this binary doesn't contain.
    if want_tui && !has_tui {
        return Err("this build has no TUI frontend".to_string());
    }
    if want_gui && !has_gui {
        return Err("this build has no GUI frontend".to_string());
    }
    if !want_tui && !want_gui {
        return Err("built without any frontend".to_string());
    }

    // Gracefully drop frontends whose device isn't present, so a missing screen
    // or a non-interactive terminal degrades to whatever else works instead of
    // aborting the whole tool.
    if want_gui && !gui_available() {
        eprintln!("warning: no DRM/KMS display found; skipping GUI frontend");
        // Into the log too, for the same reason run_both records its failure:
        // the TUI's alternate screen hides whatever stderr said beforehand.
        ctrl.log("GUI skipped: no DRM/KMS display found");
        want_gui = false;
    }
    if want_tui && !tui_available() {
        eprintln!("warning: standard input/output is not a terminal; skipping TUI frontend");
        want_tui = false;
    }

    match (want_tui, want_gui) {
        (true, true) => run_both(ctrl),
        (true, false) => run_tui(ctrl),
        (false, true) => run_gui(ctrl),
        (false, false) => {
            Err("no usable frontend: no display device and no interactive terminal".to_string())
        }
    }
}

/// Whether the TUI has an interactive terminal to run on.
fn tui_available() -> bool {
    #[cfg(feature = "tui")]
    {
        flipperos_installer::tui::terminal_available()
    }
    #[cfg(not(feature = "tui"))]
    {
        false
    }
}

/// Whether the GUI has a display device to render on.
fn gui_available() -> bool {
    #[cfg(feature = "gui")]
    {
        flipperos_installer::gui::display_available()
    }
    #[cfg(not(feature = "gui"))]
    {
        false
    }
}

#[cfg(feature = "tui")]
fn run_tui(ctrl: Arc<Controller>) -> Result<(), String> {
    flipperos_installer::tui::run(ctrl);
    Ok(())
}

#[cfg(not(feature = "tui"))]
fn run_tui(_ctrl: Arc<Controller>) -> Result<(), String> {
    Err("TUI frontend not compiled in".to_string())
}

#[cfg(feature = "gui")]
fn run_gui(ctrl: Arc<Controller>) -> Result<(), String> {
    flipperos_installer::gui::run(ctrl).map_err(|e| format!("gui: {e}"))
}

#[cfg(not(feature = "gui"))]
fn run_gui(_ctrl: Arc<Controller>) -> Result<(), String> {
    Err("GUI frontend not compiled in".to_string())
}

#[cfg(all(feature = "tui", feature = "gui"))]
fn run_both(ctrl: Arc<Controller>) -> Result<(), String> {
    // The GUI owns the main thread — its window and everything Slint hangs off
    // it must stay on one thread — so the TUI runs on a worker. Both share the
    // same controller.
    //
    // Bring the GUI up *before* the TUI touches the terminal: building it opens
    // the DRM/KMS panel and the button devices, and whatever it has to say about
    // a failure should reach a console that is still in cooked mode.
    let gui = match flipperos_installer::gui::build(Arc::clone(&ctrl)) {
        Ok(gui) => gui,
        // A board whose panel is missing or busy still has a serial console, and
        // an operator watching it would rather drive the install from there than
        // be told the run is over.
        //
        // The reason goes into the shared log, not just stderr: the TUI is about
        // to take the alternate screen, which wipes anything printed before it,
        // and "the screen stayed dark and nothing said why" is a miserable way
        // to debug a board.
        Err(e) => {
            eprintln!("warning: GUI frontend unavailable ({e}); continuing with the TUI only");
            ctrl.log(format!("GUI unavailable: {e}"));
            return run_tui(ctrl);
        }
    };

    let tui_ctrl = Arc::clone(&ctrl);
    let tui = std::thread::spawn(move || flipperos_installer::tui::run(tui_ctrl));
    let gui_result = flipperos_installer::gui::run_window(gui).map_err(|e| format!("gui: {e}"));
    let _ = tui.join();
    gui_result
}

#[cfg(not(all(feature = "tui", feature = "gui")))]
fn run_both(_ctrl: Arc<Controller>) -> Result<(), String> {
    Err("this build cannot run both frontends".to_string())
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut config = Config::default();
    let mut frontend = FrontendChoice::Auto;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            "--tui" => frontend = FrontendChoice::Tui,
            "--gui" => frontend = FrontendChoice::Gui,
            "--both" => frontend = FrontendChoice::Both,
            "--dry-run" => config.dry_run = true,
            "--no-dry-run" => config.dry_run = false,
            "--server" => {
                config.server_url = iter
                    .next()
                    .ok_or("--server requires a URL argument")?;
            }
            "--bundle-bucket" => {
                config.bundle_bucket = iter
                    .next()
                    .ok_or("--bundle-bucket requires a bucket name")?;
            }
            "--bundle-url" => {
                config.bundle_base_url = iter
                    .next()
                    .ok_or("--bundle-url requires a URL argument")?;
            }
            "--bundle-list-url" => {
                config.bundle_list_url =
                    Some(iter.next().ok_or("--bundle-list-url requires a URL")?);
            }
            "--bundle-prefix" => {
                config.bundle_prefix = iter
                    .next()
                    .ok_or("--bundle-prefix requires a prefix argument")?;
            }
            // Repeatable: several local bundles can be offered at once.
            "--bundle" => {
                config
                    .bundle_paths
                    .push(iter.next().ok_or("--bundle requires a path argument")?);
            }
            "--custom" => config.mode = InstallMode::Custom,
            "--fetch" => {
                let value = iter
                    .next()
                    .ok_or("--fetch requires 'verify-first' or 'stream'")?;
                config.fetch = match value.as_str() {
                    "verify-first" | "verify" => FetchMode::VerifyFirst,
                    "stream" => FetchMode::Stream,
                    other => {
                        return Err(format!(
                            "unknown fetch mode '{other}' (expected 'verify-first' or 'stream')"
                        ))
                    }
                };
            }
            "--cache-dir" => {
                config.cache_dir = iter
                    .next()
                    .ok_or("--cache-dir requires a path argument")?;
            }
            "--keep-cache" => config.keep_cache = true,
            "--no-automount" => config.automount = false,
            "--kms-device" => {
                config.kms_device = iter
                    .next()
                    .ok_or("--kms-device requires a path argument")?;
            }
            "--debug-keys" => config.debug_keys = true,
            "--no-ufs-check" => config.ufs_check = false,
            "--reprovision-ufs" => config.ufs_reprovision = true,
            "--ufs-scheme" => {
                config.ufs_scheme =
                    Some(iter.next().ok_or("--ufs-scheme requires a path argument")?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Some(Args { config, frontend }))
}

fn print_usage() {
    println!(
        "flipperos-installer — on-device installer for RK3576 FlipperOS boards\n\
\n\
USAGE:\n\
    flipperos-installer [OPTIONS]\n\
\n\
FRONTEND (default: all compiled-in frontends, concurrently):\n\
    --tui              Run only the Cursive/Crossterm serial-console UI\n\
    --gui              Run only the Slint DRM/KMS on-device UI\n\
    --both             Run both frontends against one shared installer state\n\
\n\
UPDATE BUNDLES (the default source):\n\
    --bundle-bucket <N>   Bucket the bundles are published in\n\
    --bundle-url <URL>    Base URL bundle files are served from\n\
    --bundle-list-url <U> Object-listing endpoint (derived from the bucket)\n\
    --bundle-prefix <P>   Prefix inside the bucket holding the channels\n\
    --bundle <PATH>       Install from a local bundle directory or *.tar.zst\n\
                          (repeatable; also auto-discovered on removable media)\n\
    --fetch <MODE>        'verify-first' (default) checks every artifact against\n\
                          the manifest before the target is touched; 'stream'\n\
                          writes as it downloads and checks on the way through\n\
    --cache-dir <DIR>     Scratch dir for verified artifacts and unpacked archives\n\
    --keep-cache          Keep the scratch files after the run\n\
    --no-automount        Do not mount removable media read-only during discovery\n\
    --custom              Start on the custom development build flow instead\n\
\n\
UFS PROVISIONING (only for UFS targets):\n\
    --no-ufs-check        Do not check the target's logical units against the\n\
                          Flipper provisioning scheme\n\
    --reprovision-ufs     Reprovision a mismatched target without asking first.\n\
                          DESTRUCTIVE: everything on the device is lost\n\
    --ufs-scheme <PATH>   Read the scheme from this TOML file instead of the\n\
                          one compiled into the installer\n\
\n\
OPTIONS:\n\
    --server <URL>     Image server base URL (custom development builds)\n\
    --kms-device <P>   DRM/KMS device node for the Flipper One screen\n\
                       (autodetected by driver name when not given)\n\
    --dry-run          Log destructive steps without executing them (default)\n\
    --no-dry-run       Actually perform destructive operations\n\
    --debug-keys       Log each GUI keypress to stderr (input debugging)\n\
    -h, --help         Show this help\n"
    );
}
