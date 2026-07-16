//! FlipperOS installer entry point.
//!
//! Parses configuration, kicks off hardware/source discovery on a worker
//! thread, then launches the selected frontend(s). When both the TUI and the
//! GUI are enabled they run concurrently against one shared [`Controller`], so
//! the operator can drive the installer from the serial console and the
//! on-device buttons interchangeably.

use std::process::ExitCode;
use std::sync::Arc;

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

    match run_frontends(controller, args.frontend) {
        Ok(()) => ExitCode::SUCCESS,
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
        eprintln!("warning: no DRM/KMS or framebuffer display found; skipping GUI frontend");
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
    // The GUI (LinuxKMS) event loop must own the main thread; run the TUI on a
    // worker thread. Both share the same controller.
    //
    // Bring the GUI up *before* the TUI touches the terminal. Building the window
    // opens the DRM/KMS panel, and on hardware without a display Slint aborts the
    // process at that point. If the TUI had already switched the terminal into
    // raw mode, that abort would leave the operator's serial console corrupted,
    // so we initialize the GUI here, on the main thread, and only spawn the TUI
    // once the screen is known to be up.
    let window = flipperos_installer::gui::build(Arc::clone(&ctrl)).map_err(|e| format!("gui: {e}"))?;

    let tui_ctrl = Arc::clone(&ctrl);
    let tui = std::thread::spawn(move || flipperos_installer::tui::run(tui_ctrl));
    let gui_result = flipperos_installer::gui::run_window(window).map_err(|e| format!("gui: {e}"));
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
            "--kms-device" => {
                config.kms_device = iter
                    .next()
                    .ok_or("--kms-device requires a path argument")?;
            }
            "--debug-keys" => config.debug_keys = true,
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
    --gui              Run only the Slint LinuxKMS on-device UI\n\
    --both             Run both frontends against one shared installer state\n\
\n\
OPTIONS:\n\
    --server <URL>     Image server base URL\n\
    --kms-device <P>   DRM/KMS device node for the Flipper One screen\n\
    --dry-run          Log destructive steps without executing them (default)\n\
    --no-dry-run       Actually perform destructive operations\n\
    --debug-keys       Log each GUI keypress to stderr (input debugging)\n\
    -h, --help         Show this help\n"
    );
}
