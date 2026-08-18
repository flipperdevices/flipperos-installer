//! Slint GUI frontend for the Flipper One 256x144 DRM/KMS screen.
//!
//! Like the TUI, this is a thin view over the shared [`Controller`]. The Slint
//! window renders the current [`AppState`] snapshot and drives navigation with
//! the on-device buttons (delivered by libinput as key events). All selection
//! changes and the install action call back into the controller, so the GUI and
//! the serial-console TUI stay in lock-step.
//!
//! The `-noseat` LinuxKMS backend is used because inside an initramfs there is
//! no seatd/logind session to broker DRM access; we run as root and open the
//! DRM device directly.

use std::sync::{Arc, Mutex};

use crate::core::menu::{self, DetailsTarget, Level, LevelKind, Nav};
use crate::core::model::{AppState, Phase};
use crate::core::Controller;

// The Slint UI is authored as external `.slint` files under `src/gui/ui/` (the
// path is relative to this source file). We compile them with the `slint!`
// proc-macro rather than a build.rs + `slint-build`: `slint-build` pulls in
// `i-slint-compiler` with its full default feature set, which unifies the fat
// `image` crate (avif/exr/tiff/rayon codecs) into the graph on a same-platform
// build, whereas the proc-macro's `slint-macros` uses a slim compiler that
// keeps `image` at just png/jpeg. Re-exporting from the external file gives the
// same generated `MainWindow` type (and `flipperos_installer::gui::MainWindow`
// export) with no extra dependencies.
slint::slint! {
    export { MainWindow, MenuEntry } from "ui/main.slint";
}

/// Convert one core menu row into the struct the Slint rows render.
fn entry(item: &menu::MenuItem) -> MenuEntry {
    MenuEntry {
        text: item.text.clone().into(),
        detail: item.detail.clone().into(),
        icon: item.icon.as_int(),
        marker: item.marker.as_int(),
        drill: item.drill,
        dim: item.dim,
        action: matches!(item.action, menu::Action::StartInstall),
    }
}

fn entries(level: &Level) -> slint::ModelRc<MenuEntry> {
    let rows: Vec<MenuEntry> = level.items.iter().map(entry).collect();
    slint::ModelRc::new(slint::VecModel::from(rows))
}

/// Build the window and wire it to `ctrl`, without starting the event loop.
///
/// Creating the window is what actually opens the DRM/KMS panel, so this is the
/// step that fails (and, on hardware without a display, aborts) when there is no
/// screen. Callers that run the GUI alongside the TUI use this to bring the GUI
/// up on the main thread *before* the TUI takes over the terminal.
///
/// Must be called on the main thread (the LinuxKMS backend owns it).
pub fn build(ctrl: Arc<Controller>) -> Result<MainWindow, slint::PlatformError> {
    // Select the seat-less LinuxKMS backend and point it at the Flipper One
    // panel. Slint honours these environment variables at backend init. The
    // backend name is just "linuxkms" (the seat-less behaviour comes from the
    // compiled-in `backend-linuxkms-noseat` feature); the suffix names the
    // renderer, so we request the software renderer explicitly.
    std::env::set_var("SLINT_BACKEND", "linuxkms-software");
    std::env::set_var("SLINT_KMS_DEVICE", &ctrl.config().kms_device);

    let win = MainWindow::new()?;

    // Input-debug trace: only wired under `--debug-keys`. Left unwired the
    // callback is a no-op, so keypresses never reach stderr / the shared console.
    if ctrl.config().debug_keys {
        win.on_trace_key(|text, screen, menu, cursor| {
            eprintln!(
                "gui key: text=<{text}> screen={screen} menu={menu} cursor={cursor}"
            );
        });
    }

    // A cache of the latest snapshot, so a callback carrying only a row index can
    // resolve it against exactly the rows that were on screen.
    let cache: Arc<Mutex<AppState>> = Arc::new(Mutex::new(ctrl.snapshot()));

    // Where this frontend is in the menu tree. Kept here rather than in the
    // shared state so arrow keys never take the state lock, and so the TUI can
    // sit on a different level at the same time.
    let nav: Arc<Mutex<Nav>> = Arc::new(Mutex::new(Nav::new()));

    // What the details popup is showing, so every snapshot can refresh it as the
    // sourcestamps arrive.
    let detail_target: Arc<Mutex<Option<DetailsTarget>>> = Arc::new(Mutex::new(None));

    {
        // Activate a row: the core performs the action and says where to go; we
        // apply that to the stack and repaint.
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        let nav = Arc::clone(&nav);
        let weak = win.as_weak();
        win.on_activate(move |idx| {
            let state = cache.lock().unwrap().clone();
            let mut nav_now = nav.lock().unwrap();
            let level = menu::level(&nav_now, &state);
            let index = idx.max(0) as usize;
            nav_now.set_cursor(index);
            let mv = menu::activate(&ctrl, &level, index);
            nav_now.apply(mv);
            // A level that was just entered starts on its committed choice.
            let opened = menu::level(&nav_now, &state);
            if let Some(preferred) = opened.preferred_cursor {
                nav_now.set_cursor(preferred);
            }
            let nav_copy = nav_now.clone();
            drop(nav_now);

            menu::on_open(&ctrl, &opened);
            menu::on_focus(&ctrl, &opened, nav_copy.cursor());
            if let Some(win) = weak.upgrade() {
                apply_nav(&win, &state, &nav_copy);
            }
        });
    }
    {
        // Leave the current level. The stack lives in Rust, so Slint delegates
        // rather than decrementing a screen counter itself.
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        let nav = Arc::clone(&nav);
        let weak = win.as_weak();
        win.on_back(move || {
            let state = cache.lock().unwrap().clone();
            let mut nav_now = nav.lock().unwrap();
            nav_now.pop();
            let nav_copy = nav_now.clone();
            drop(nav_now);
            let level = menu::level(&nav_copy, &state);
            menu::on_open(&ctrl, &level);
            if let Some(win) = weak.upgrade() {
                apply_nav(&win, &state, &nav_copy);
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        win.on_start_install(move || ctrl.start_install());
    }
    {
        // The reboot itself happens in `main`, after this event loop (and the
        // TUI's) has returned; all this does is record it and start the teardown.
        let ctrl = Arc::clone(&ctrl);
        win.on_reboot(move || ctrl.request_reboot());
    }
    {
        // Re-scan sources off the event-loop thread, like the TUI's Refresh.
        let ctrl = Arc::clone(&ctrl);
        win.on_refresh(move || {
            let ctrl = Arc::clone(&ctrl);
            std::thread::spawn(move || ctrl.refresh_sources());
        });
    }
    {
        // Start whatever lazy fetch the rows now on screen need. Deduping happens
        // in the controller, which both frontends share.
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        let nav = Arc::clone(&nav);
        win.on_ensure_visible(move |first, count| {
            let state = cache.lock().unwrap().clone();
            let nav_now = nav.lock().unwrap().clone();
            let level = menu::level(&nav_now, &state);
            menu::on_visible(
                &ctrl,
                &level,
                first.max(0) as usize,
                count.max(0) as usize,
            );
        });
    }
    {
        // Answering the core's prompt only calls the controller: it clears the
        // prompt in the shared state, and the snapshot that follows is what takes
        // the popup down — in both frontends at once.
        let ctrl = Arc::clone(&ctrl);
        win.on_confirm_prompt(move || ctrl.confirm_prompt());
    }
    {
        let ctrl = Arc::clone(&ctrl);
        win.on_dismiss_prompt(move || ctrl.dismiss_prompt());
    }
    {
        let cache = Arc::clone(&cache);
        let nav = Arc::clone(&nav);
        let detail_target = Arc::clone(&detail_target);
        let weak = win.as_weak();
        win.on_show_details(move |idx| {
            let state = cache.lock().unwrap().clone();
            let nav_now = nav.lock().unwrap().clone();
            let level = menu::level(&nav_now, &state);
            if let Some(target) = level.details_at(idx.max(0) as usize) {
                *detail_target.lock().unwrap() = Some(target);
                if let Some(win) = weak.upgrade() {
                    apply_details(&win, &state, &detail_target.lock().unwrap());
                }
            }
        });
    }

    // Close this frontend when either frontend's Reboot action asks the
    // installer to shut down. `invoke_from_event_loop` is callable from any
    // thread, which is what lets the TUI (on its own thread) end the GUI loop
    // that owns the main thread — without it, quitting the TUI would leave the
    // process running and nothing would ever reboot.
    ctrl.on_exit(|| {
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });

    // Subscribe: marshal every snapshot into the Slint event loop.
    let weak = win.as_weak();
    let detail_for_sub = Arc::clone(&detail_target);
    let nav_for_sub = Arc::clone(&nav);
    ctrl.subscribe(move |snapshot| {
        let weak = weak.clone();
        let cache = Arc::clone(&cache);
        let detail_target = Arc::clone(&detail_for_sub);
        let nav = Arc::clone(&nav_for_sub);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(win) = weak.upgrade() {
                *cache.lock().unwrap() = snapshot.clone();
                let mut nav_now = nav.lock().unwrap();
                // An install takes over the screen, so drop back to the summary.
                if matches!(snapshot.phase, Phase::Installing) {
                    nav_now.reset();
                }
                let nav_copy = nav_now.clone();
                drop(nav_now);
                apply(&win, &snapshot);
                apply_nav(&win, &snapshot, &nav_copy);
                apply_details(&win, &snapshot, &detail_target.lock().unwrap());
                // Last, so it owns `screen`: a prompt outranks whatever level
                // `apply_nav` just decided to show.
                apply_prompt(&win, &snapshot, &nav_copy);
            }
        });
    });

    Ok(win)
}

/// Build the window, wire it to `ctrl`, and run the Slint event loop.
///
/// Must be called on the main thread (the LinuxKMS backend owns it).
pub fn run(ctrl: Arc<Controller>) -> Result<(), slint::PlatformError> {
    run_window(build(ctrl)?)
}

/// Run the Slint event loop for an already-built window.
///
/// Split out from [`run`] so a caller can build the window on the main thread
/// (bringing the screen up) before starting other frontends, then hand the
/// window here to block on the event loop.
pub fn run_window(window: MainWindow) -> Result<(), slint::PlatformError> {
    window.run()
}

/// Best-effort check for whether the LinuxKMS backend has a device to draw on.
///
/// The GUI cannot be started safely without one: Slint opens the panel lazily
/// while creating the window and `.unwrap()`s the failure, and our release build
/// is `panic = "abort"`, so a missing screen would abort the whole process
/// instead of returning an error we could recover from. Callers use this to skip
/// the GUI when there is nothing to render on.
///
/// This mirrors the device discovery in Slint's software LinuxKMS display, which
/// tries a DRM/KMS node under `/dev/dri` first and then falls back to a legacy
/// `/dev/fb*` framebuffer (or only the framebuffer when `SLINT_BACKEND_LINUXFB`
/// is set). It is a presence check, not a full modeset probe, so a device that
/// exists but can't be driven (e.g. DRM master held by a compositor) can still
/// fail later — but the common "no display at all" case is caught here.
pub fn display_available() -> bool {
    let has_framebuffer =
        || (0..10).any(|n| std::path::Path::new(&format!("/dev/fb{n}")).exists());

    if std::env::var_os("SLINT_BACKEND_LINUXFB").is_some() {
        return has_framebuffer();
    }

    let has_drm_card = std::fs::read_dir("/dev/dri")
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("card")
            })
        })
        .unwrap_or(false);

    has_drm_card || has_framebuffer()
}

/// Wrap each line of `text` to at most `width` characters, so a popup can scroll
/// by whole lines.
///
/// Breaks at spaces where it can — the confirmation prompts are prose, and
/// chopping mid-word made them hard to read — and falls back to a hard cut for a
/// single token longer than the width (a URL in a build's sourcestamps, say).
///
/// Public so the off-device screenshot harness wraps exactly the way the device
/// does, instead of approximating it.
pub fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut rest: Vec<char> = line.chars().collect();
        if rest.is_empty() {
            out.push(String::new());
            continue;
        }
        while rest.len() > width {
            // The last space that still fits; `width` itself is a valid break
            // point, since the space is dropped rather than carried over.
            let cut = rest[..=width]
                .iter()
                .rposition(|c| *c == ' ')
                .filter(|p| *p > 0)
                .unwrap_or(width)
                .max(1);
            out.push(rest[..cut].iter().collect());
            // Drop the space we broke on, plus any run of them.
            let mut next = cut;
            while rest.get(next) == Some(&' ') {
                next += 1;
            }
            rest = rest[next..].to_vec();
        }
        out.push(rest.iter().collect());
    }
    out
}

/// Fill the details popup (title + wrapped source-stamp lines) for the currently
/// open target, resolving it against the freshest snapshot.
fn apply_details(win: &MainWindow, state: &AppState, target: &Option<DetailsTarget>) {
    use slint::{ModelRc, SharedString, VecModel};

    let (title, text) = match target {
        Some(target) => menu::details_text(state, target),
        None => (String::new(), String::new()),
    };
    win.set_details_title(title.into());
    let lines: Vec<SharedString> = wrap_lines(&text, 40).into_iter().map(SharedString::from).collect();
    win.set_details_model(ModelRc::new(VecModel::from(lines)));
}

/// Raise or drop the core's modal prompt (screen 3).
///
/// The prompt lives in the shared state, so this is the only thing that puts the
/// GUI on that screen and the only thing that takes it off again — pressing a
/// soft button just answers the controller. Called after [`apply_nav`], whose
/// `screen` decision it overrides while a prompt is pending.
fn apply_prompt(win: &MainWindow, state: &AppState, nav: &Nav) {
    use slint::{ModelRc, SharedString, VecModel};

    let Some(prompt) = &state.prompt else {
        // Back to whatever was on screen before, unless a level popup or the
        // summary is already showing.
        if win.get_screen() == PROMPT_SCREEN {
            win.set_screen(if nav.depth() > 0 { 1 } else { 0 });
        }
        return;
    };
    win.set_prompt_title(prompt.title.clone().into());
    win.set_prompt_confirm(prompt.confirm.clone().into());
    win.set_prompt_cancel(prompt.cancel.clone().into());
    let lines: Vec<SharedString> = wrap_lines(&prompt.lines.join("\n"), 40)
        .into_iter()
        .map(SharedString::from)
        .collect();
    win.set_prompt_model(ModelRc::new(VecModel::from(lines)));
    if win.get_screen() != PROMPT_SCREEN {
        win.set_prompt_scroll(0);
        win.set_screen(PROMPT_SCREEN);
    }
}

/// `screen` value of the confirmation prompt; see the property's comment in
/// `ui/main.slint`.
const PROMPT_SCREEN: i32 = 3;

/// Push the menu rows and the navigation position into the Slint properties.
///
/// The summary rows are set unconditionally: the GUI draws them dimmed behind an
/// open level. `screen` follows the stack depth, so leaving the last level is
/// what returns to the summary — except while a prompt is up, which
/// [`apply_prompt`] owns.
fn apply_nav(win: &MainWindow, state: &AppState, nav: &Nav) {
    win.set_summary_items(entries(&menu::root_level(state)));

    let level = menu::level(nav, state);
    win.set_level_items(entries(&level));
    win.set_level_title(level.title.clone().into());
    win.set_level_multi(level.kind == LevelKind::MultiPick);
    win.set_level_can_refresh(level.can_refresh);

    // Keep the cursor inside a level whose contents just changed — a level that
    // was showing a single "(loading…)" row may now have a hundred entries, or
    // none at all.
    let cursor = (nav.cursor() as i32).min(level.items.len().saturating_sub(1) as i32);
    win.set_cursor(cursor.max(0));
    win.set_level_can_details(level.details_at(cursor.max(0) as usize).is_some());

    // A pending prompt owns the screen; `apply_prompt` restores it afterwards.
    if win.get_screen() == PROMPT_SCREEN {
        return;
    }
    if nav.depth() == 0 {
        win.set_menu_index((nav.cursor() as i32).min(level.items.len().saturating_sub(1) as i32));
        // The details popup belongs to a level, so it cannot outlive one.
        if win.get_screen() != 0 {
            win.set_screen(0);
        }
    } else if win.get_screen() == 0 {
        win.set_screen(1);
    }
}

/// Push the parts of a state snapshot that are not menu rows: the header, the
/// progress bar and the status lines.
fn apply(win: &MainWindow, state: &AppState) {
    // Header: "Device type: <model> [<id>]". The model is the device-tree human
    // string; the bracketed id is the device type we mapped it to.
    win.set_device_type_text(
        format!("Device type: {} [{}]", state.board.model, state.board.board_id).into(),
    );
    win.set_progress(state.progress);
    win.set_can_install(state.can_install());
    win.set_can_reboot(state.can_reboot());

    // The progress bar + log line only appear once installation has started;
    // until then the freed line shows a discovery/summary count instead.
    let installing = matches!(
        state.phase,
        Phase::Installing | Phase::Done | Phase::Failed(_)
    );
    win.set_installing(installing);
    // Hide Refresh while discovery / install is in flight.
    win.set_busy(state.phase.is_busy());
    win.set_status_text(state.log.last().cloned().unwrap_or_default().into());
    let info = match state.phase {
        Phase::Discovering => "discovering…".to_string(),
        _ => format!(
            "{} target(s), {} build(s) found",
            state.devices.len(),
            state.uboot_builds.len() + state.snapshot_builds.len()
        ),
    };
    win.set_info_text(info.into());

    // Everything list-shaped — the summary rows and the open level's rows, with
    // their values, markers and chevrons — is built by `core::menu` and pushed by
    // `apply_nav`, which the same subscriber calls.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_breaks_on_spaces() {
        // The reprovisioning caution has to stay readable: no word may be cut in
        // half, and every line has to fit the popup's 40 columns.
        let caution = "ALL DATA ON /dev/sda WILL BE PERMANENTLY LOST.";
        let lines = wrap_lines(caution, 40);
        assert_eq!(
            lines,
            vec![
                "ALL DATA ON /dev/sda WILL BE PERMANENTLY".to_string(),
                "LOST.".to_string(),
            ]
        );
        for line in wrap_lines(caution, 40) {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
    }

    #[test]
    fn wrapping_keeps_short_lines_and_blank_ones() {
        assert_eq!(wrap_lines("short", 40), vec!["short".to_string()]);
        // Blank lines are the paragraph breaks in a prompt, so they survive.
        assert_eq!(
            wrap_lines("a\n\nb", 40),
            vec!["a".to_string(), String::new(), "b".to_string()]
        );
        // Exactly the width is left alone.
        let exact = "x".repeat(40);
        assert_eq!(wrap_lines(&exact, 40), vec![exact]);
    }

    #[test]
    fn wrapping_hard_cuts_an_unbreakable_token() {
        // A build's sourcestamp URL has no spaces to break on.
        let url = "https://linux-images.flipp.dev/#/builders/11/builds/692";
        let lines = wrap_lines(url, 40);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].chars().count(), 40);
        assert_eq!(lines.concat(), url);
    }
}
