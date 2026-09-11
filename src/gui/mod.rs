//! Slint GUI frontend for the Flipper One 256x144 DRM/KMS screen.
//!
//! Like the TUI, this is a thin view over the shared [`Controller`]. The Slint
//! window renders the current [`AppState`] snapshot and drives navigation with
//! the on-device buttons (read straight from evdev — see [`panel`]). All
//! selection changes and the install action call back into the controller, so
//! the GUI and the serial-console TUI stay in lock-step.
//!
//! Slint is taken with the software renderer and *no backend*, so it supplies
//! no event loop: [`run_window`] is the loop, and it owns the panel and the
//! buttons as well. The LinuxKMS backend would have done all three, but its
//! libinput, libudev and libxkbcommon dependencies are not optional, and this
//! binary runs from an initramfs where every shared library has to be staged
//! alongside it.
//!
//! One consequence runs through this file: `slint`'s `unsafe-single-threaded`
//! feature is on, so no Slint handle may leave this thread. Snapshots from the
//! controller's worker threads arrive over an [`mpsc`] channel carrying plain
//! [`AppState`] values, and the loop is what touches the window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use crate::core::menu::{self, DetailsTarget, Level, LevelKind, Nav};
use crate::core::model::{AppState, Phase};
use crate::core::Controller;

mod panel;

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

/// Everything [`run_window`] needs to drive the GUI: the window, the panel it is
/// drawn on, and the two ways the rest of the process talks to the loop.
///
/// None of this is `Send` — the window least of all — which is the point. It is
/// built on the thread that will run the loop and never leaves it.
pub struct Gui {
    window: MainWindow,
    /// The software-rendered surface behind `window`. Held separately because
    /// rendering goes through the adapter, not the component.
    surface: std::rc::Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    panel: panel::Panel,
    /// Snapshots from the controller's worker threads.
    snapshots: mpsc::Receiver<AppState>,
    /// Set by the controller's exit hook, from whichever thread asked to quit.
    quit: Arc<AtomicBool>,
    cache: Arc<Mutex<AppState>>,
    nav: Arc<Mutex<Nav>>,
    detail_target: Arc<Mutex<Option<DetailsTarget>>>,
}

/// Build the window and wire it to `ctrl`, without starting the loop.
///
/// This is what opens the DRM/KMS panel and the button devices, so it is the
/// step that fails when there is no screen. Callers that run the GUI alongside
/// the TUI use it to bring the GUI up on the main thread *before* the TUI takes
/// over the terminal, so a failure here still reaches a sane console.
///
/// Must be called on the thread that will call [`run_window`].
pub fn build(ctrl: Arc<Controller>) -> Result<Gui, slint::PlatformError> {
    // Open the panel and the buttons before the platform, so a display failure
    // is an error from here rather than a half-initialised Slint.
    let panel = panel::Panel::open(&ctrl.config().kms_device, ctrl.config().debug_keys)
        .map_err(slint::PlatformError::Other)?;

    // The whole platform: a window adapter and a clock. It has to be installed
    // before any component is created, and exactly once.
    let surface = flipper_ui::slint_render::FlipperSlintPlatform::install();

    let win = MainWindow::new()?;
    win.show()?;

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
    // installer to shut down. The hook runs on whichever thread asked, which is
    // what lets the TUI (on its own thread) end the GUI loop that owns the main
    // thread — without it, quitting the TUI would leave the process running and
    // nothing would ever reboot. A flag rather than a call into Slint, because
    // nothing off this thread may touch the window.
    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = Arc::clone(&quit);
        ctrl.on_exit(move || quit.store(true, Ordering::Release));
    }

    // Subscribe: hand every snapshot to the loop over a channel. `subscribe`
    // delivers the current state synchronously before it returns, so the first
    // frame the loop draws already has whatever discovery has found.
    let (tx, snapshots) = mpsc::channel();
    ctrl.subscribe(move |snapshot| {
        let _ = tx.send(snapshot);
    });

    Ok(Gui {
        window: win,
        surface,
        panel,
        snapshots,
        quit,
        cache,
        nav,
        detail_target,
    })
}

/// Build the window, wire it to `ctrl`, and run the loop.
pub fn run(ctrl: Arc<Controller>) -> Result<(), slint::PlatformError> {
    run_window(build(ctrl)?)
}

/// Drive an already-built GUI until something asks it to stop.
///
/// Split out from [`run`] so a caller can build on the main thread (bringing the
/// screen up) before starting other frontends, then block here.
///
/// Each turn drains the buttons, drains the snapshot channel, and repaints if
/// either produced anything. The repaint is conditional twice over: `dirty`
/// skips the work when nothing happened, and `Panel::present` skips the commit
/// when Slint reports no damage, so an idle installer transmits nothing over
/// SPI at all.
pub fn run_window(gui: Gui) -> Result<(), slint::PlatformError> {
    let Gui { window, surface, mut panel, snapshots, quit, cache, nav, detail_target } = gui;

    // Nothing here calls `slint::platform::update_timers_and_animations`, and
    // nothing repaints on a clock: this UI has no `animate` blocks and starts no
    // Slint timers, so every frame is a reaction to a key or a snapshot. Adding
    // an animation to the .slint would need both that call and a redraw while
    // `window.has_active_animations()`, or it would start and then stall.
    //
    // The first turn always draws: nothing has been on the panel yet.
    let mut dirty = true;
    while !quit.load(Ordering::Acquire) {
        if panel.pump_keys(window.window()) {
            dirty = true;
        }

        for snapshot in snapshots.try_iter() {
            dirty = true;
            apply_snapshot(&window, &snapshot, &cache, &nav, &detail_target);
        }

        // Checked again here: a key or a snapshot may have been the Reboot that
        // ends the run, and there is no point painting a frame nobody sees.
        if quit.load(Ordering::Acquire) {
            break;
        }

        if dirty {
            dirty = false;
            surface.request_redraw();
            panel.present(&surface).map_err(slint::PlatformError::Other)?;
        }

        panel.wait();
    }
    Ok(())
}

/// Apply one snapshot to the window, in the order the screens depend on.
fn apply_snapshot(
    win: &MainWindow,
    snapshot: &AppState,
    cache: &Mutex<AppState>,
    nav: &Mutex<Nav>,
    detail_target: &Mutex<Option<DetailsTarget>>,
) {
    *cache.lock().unwrap() = snapshot.clone();
    let mut nav_now = nav.lock().unwrap();
    // An install takes over the screen, so drop back to the summary.
    if matches!(snapshot.phase, Phase::Installing) {
        nav_now.reset();
    }
    let nav_copy = nav_now.clone();
    drop(nav_now);
    apply(win, snapshot);
    apply_nav(win, snapshot, &nav_copy);
    apply_details(win, snapshot, &detail_target.lock().unwrap());
    // Last, so it owns `screen`: a prompt outranks whatever level `apply_nav`
    // just decided to show.
    apply_prompt(win, snapshot, &nav_copy);
}

/// Best-effort check for whether there is a display to draw on.
///
/// Callers use this to choose frontends before committing to one, so it has to
/// be cheap and side-effect free: opening the panel for real acquires DRM master
/// and waits for it, which is [`build`]'s job, not this one's. A node that
/// exists but cannot be driven still fails there — but the common "no display at
/// all" case is caught here, and the failure is a returned error either way.
pub fn display_available() -> bool {
    std::fs::read_dir("/dev/dri")
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("card")
            })
        })
        .unwrap_or(false)
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
