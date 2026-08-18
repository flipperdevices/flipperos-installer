//! Cursive/Crossterm TUI frontend.
//!
//! This frontend is a thin view over the shared [`Controller`]: every widget
//! action calls a controller method, and a single subscriber re-renders the
//! screen from each broadcast [`AppState`] snapshot. Because the controller is
//! the single source of truth, selections made here show up live in the GUI and
//! vice-versa.
//!
//! What the menus *contain* comes from [`crate::core::menu`], so this file only
//! decides how a level is drawn and where the cursor is. The navigation position
//! ([`Nav`]) is deliberately local: it is the one piece of UI state the two
//! frontends do not share, which is what lets this one show a level beside the
//! overview while the GUI shows it as a popup.

use std::cell::RefCell;
use std::sync::Arc;

use cursive::event::{Event, Key};
use cursive::style::Effect;
use cursive::traits::{Nameable, Resizable, Scrollable};
use cursive::utils::markup::StyledString;
use cursive::view::ScrollStrategy;
use cursive::views::{
    Button, Dialog, LinearLayout, OnEventView, Panel, SelectView, TextView,
};
use cursive::Cursive;

use crate::core::menu::{self, DetailsTarget, Level, Marker, Nav};
use crate::core::model::{AppState, Phase};
use crate::core::Controller;

/// Minimum terminal width at which a level is drawn in the right details pane
/// alongside the overview. Below this we fall back to a single full-screen pane
/// (the level replaces the overview), like the GUI.
const TWO_PANE_MIN_COLS: usize = 74;

/// Per-session state stored in Cursive's user data.
struct TuiData {
    ctrl: Arc<Controller>,
    /// Where this frontend is in the menu tree.
    nav: RefCell<Nav>,
    /// Fingerprint of the last-rendered level, so the list widget is only rebuilt
    /// when what it displays actually changes.
    sig: RefCell<String>,
    /// Whether the open level is a full-screen layer (true) or lives in the
    /// details pane (false).
    layered: RefCell<bool>,
    /// What the open details popup is showing, or None when closed.
    detail: RefCell<Option<DetailsTarget>>,
    /// Whether the core's modal prompt is currently on screen, so a repeated
    /// snapshot does not stack another copy of it.
    prompt_open: RefCell<bool>,
}

/// Build the UI, wire it to `ctrl`, and run the blocking event loop.
pub fn run(ctrl: Arc<Controller>) {
    let mut siv = cursive::default();
    siv.set_user_data(TuiData {
        ctrl: Arc::clone(&ctrl),
        nav: RefCell::new(Nav::new()),
        sig: RefCell::new(String::new()),
        layered: RefCell::new(false),
        detail: RefCell::new(None),
        prompt_open: RefCell::new(false),
    });

    // Left: overview of the current selections (Enter opens a level / installs).
    // Right: the details pane, which shows live status and doubles as the level
    // host when the terminal is wide enough.
    let overview = SelectView::<usize>::new()
        .on_submit(|s, index: &usize| activate_overview(s, *index))
        .with_name("overview");

    let main = LinearLayout::horizontal()
        .child(Panel::new(overview).title("Selections").fixed_width(34))
        .child(
            Panel::new(LinearLayout::vertical().with_name("detail"))
                .title("Details")
                .full_width(),
        );

    let root = LinearLayout::vertical()
        .child(main.full_height())
        .child(LinearLayout::horizontal().with_name("buttons"));

    siv.add_fullscreen_layer(root);
    siv.call_on_name("detail", |ll: &mut LinearLayout| fill_status(ll));

    siv.add_global_callback('q', Cursive::quit);
    siv.add_global_callback(Key::Backspace, go_back);
    siv.add_global_callback(Key::Esc, go_back);

    // Alt+letter mnemonics for the bottom action buttons (each guarded so it
    // only fires when its action is currently available).
    siv.add_global_callback(Event::AltChar('i'), |s| {
        if snapshot(s).can_install() {
            action(s, |c| c.start_install());
        }
    });
    siv.add_global_callback(Event::AltChar('r'), refresh);
    siv.add_global_callback(Event::AltChar('d'), open_details);
    siv.add_global_callback(Event::AltChar('q'), Cursive::quit);
    // Alt+R is Refresh, so Reboot takes B — which is also the GUI's RUN key.
    siv.add_global_callback(Event::AltChar('b'), |s| {
        if snapshot(s).can_reboot() {
            action(s, |c| c.request_reboot());
        }
    });

    // Subscribe: marshal every snapshot into the Cursive event loop.
    let cb_sink = siv.cb_sink().clone();
    ctrl.subscribe(move |snapshot| {
        let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| render(siv, &snapshot)));
    });

    // Close this frontend when anything (either frontend's Reboot action) asks
    // the installer to shut down. Quitting through the event loop is what lets
    // cursive's backend restore the terminal on the way out; a failed send just
    // means the loop is already gone, which is the state we wanted anyway.
    let quit_sink = siv.cb_sink().clone();
    ctrl.on_exit(move || {
        let _ = quit_sink.send(Box::new(|s: &mut Cursive| s.quit()));
    });

    siv.run();
}

/// Best-effort check for whether there is an interactive terminal to drive.
///
/// The Crossterm backend expects a real TTY for its rendering surface; when
/// stdout is a pipe or a file (or there is no controlling terminal at all) the
/// TUI can't run. Callers use this to skip the TUI instead of failing, so a
/// non-interactive invocation degrades gracefully to whatever other frontend is
/// available.
pub fn terminal_available() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Fetch the controller from user data and run `f` with it.
fn action<F: FnOnce(&Arc<Controller>)>(siv: &mut Cursive, f: F) {
    if let Some(data) = siv.user_data::<TuiData>() {
        let ctrl = Arc::clone(&data.ctrl);
        f(&ctrl);
    }
}

fn snapshot(siv: &mut Cursive) -> AppState {
    siv.user_data::<TuiData>()
        .map(|d| d.ctrl.snapshot())
        .unwrap_or_default()
}

/// A copy of the navigation position. Cloned out rather than borrowed, because a
/// `RefCell` borrow may not be held across a `call_on_name`, which needs `siv`.
fn nav(siv: &mut Cursive) -> Nav {
    siv.user_data::<TuiData>()
        .map(|d| d.nav.borrow().clone())
        .unwrap_or_default()
}

fn set_nav(siv: &mut Cursive, value: Nav) {
    if let Some(d) = siv.user_data::<TuiData>() {
        *d.nav.borrow_mut() = value;
    }
}

fn layered(siv: &mut Cursive) -> bool {
    siv.user_data::<TuiData>()
        .map(|d| *d.layered.borrow())
        .unwrap_or(false)
}

/// The level this frontend is currently showing.
fn current_level(siv: &mut Cursive) -> Level {
    let nav = nav(siv);
    let state = snapshot(siv);
    menu::level(&nav, &state)
}

// --- navigation ------------------------------------------------------------

/// Enter on an overview row: open its level, or run its action.
fn activate_overview(siv: &mut Cursive, index: usize) {
    let state = snapshot(siv);
    // Don't open selection levels while an install is running.
    if matches!(state.phase, Phase::Installing) {
        return;
    }
    let root = menu::root_level(&state);
    let mut nav = nav(siv);
    nav.set_cursor(index);
    set_nav(siv, nav);
    apply_activation(siv, &root, index);
}

/// Enter on a row of an open level.
fn activate_row(siv: &mut Cursive, index: usize) {
    let level = current_level(siv);
    apply_activation(siv, &level, index);
}

/// Perform a row's action and move the stack the way the core says to.
fn apply_activation(siv: &mut Cursive, level: &Level, index: usize) {
    let mv = {
        let mut moved = menu::Move::Stay;
        if let Some(d) = siv.user_data::<TuiData>() {
            let ctrl = Arc::clone(&d.ctrl);
            moved = menu::activate(&ctrl, level, index);
        }
        moved
    };
    let mut nav = nav(siv);
    nav.apply(mv.clone());
    set_nav(siv, nav);

    match mv {
        menu::Move::Push(_) => show_level(siv),
        menu::Move::Pop | menu::Move::ToRoot => leave_level(siv),
        menu::Move::Stay => {
            // A toggle: the level stays up and is repainted by the snapshot that
            // the controller call has already broadcast.
        }
    }
}

/// Draw the current level, in the details pane or as a full-screen layer.
fn show_level(siv: &mut Cursive) {
    let level = current_level(siv);
    let two_pane = siv.screen_size().x >= TWO_PANE_MIN_COLS;

    // Only ever one level layer exists: a second view named "submenu" would be
    // found before this one, since cursive resolves a name from the bottom layer
    // upwards.
    if layered(siv) {
        siv.pop_layer();
    }
    if let Some(d) = siv.user_data::<TuiData>() {
        *d.layered.borrow_mut() = !two_pane;
        *d.sig.borrow_mut() = menu::signature(&level);
    }

    let list = level_view(&level, nav(siv).cursor());
    let title = level.crumb.clone();
    if two_pane {
        siv.call_on_name("detail", move |ll: &mut LinearLayout| {
            clear_layout(ll);
            ll.add_child(TextView::new(title));
            ll.add_child(list);
        });
    } else {
        siv.add_fullscreen_layer(Panel::new(list).title(title));
    }
    let _ = siv.focus_name("submenu");

    // Start whatever this level (and its focused row) needs fetched.
    if let Some(d) = siv.user_data::<TuiData>() {
        let ctrl = Arc::clone(&d.ctrl);
        menu::on_open(&ctrl, &level);
        menu::on_focus(&ctrl, &level, nav(siv).cursor());
    }
    let state = snapshot(siv);
    rebuild_buttons(siv, &state);
}

/// Leave the current level: show its parent, or the overview at the bottom.
fn leave_level(siv: &mut Cursive) {
    if nav(siv).depth() > 0 {
        show_level(siv);
        return;
    }
    if layered(siv) {
        siv.pop_layer();
        if let Some(d) = siv.user_data::<TuiData>() {
            *d.layered.borrow_mut() = false;
        }
    } else {
        siv.call_on_name("detail", |ll: &mut LinearLayout| fill_status(ll));
    }
    let _ = siv.focus_name("overview");
    let state = snapshot(siv);
    render(siv, &state);
}

/// Back out one level. A no-op on the overview.
fn go_back(siv: &mut Cursive) {
    let mut nav = nav(siv);
    if !nav.pop() {
        return;
    }
    set_nav(siv, nav);
    leave_level(siv);
}

fn refresh(siv: &mut Cursive) {
    if snapshot(siv).phase.is_busy() {
        return;
    }
    action(siv, |c| {
        let c = Arc::clone(c);
        std::thread::spawn(move || c.refresh_sources());
    });
}

fn clear_layout(ll: &mut LinearLayout) {
    while ll.len() > 0 {
        ll.remove_child(0);
    }
}

/// Populate the details pane with the status + scrollable activity log.
fn fill_status(ll: &mut LinearLayout) {
    clear_layout(ll);
    ll.add_child(TextView::new("").with_name("status"));
    ll.add_child(TextView::new("\nActivity:"));
    ll.add_child(TextView::new("").with_name("log").scrollable().full_height());
}

// --- level rendering -------------------------------------------------------

/// One row's text: a checkbox on a multi-choice level, the label, and any
/// secondary detail. A single choice is not decorated — the cursor opens on it.
fn row_label(item: &menu::MenuItem) -> String {
    let marker = match item.marker {
        Marker::None => "",
        Marker::Unchecked => "[ ] ",
        Marker::Checked => "[x] ",
    };
    let mut out = format!("{marker}{}", item.text);
    if !item.detail.is_empty() {
        out.push_str(&format!("  ({})", item.detail));
    }
    if item.drill {
        out.push_str(" \u{203a}");
    }
    out
}

/// A scrollable list for any level; the payload is the row index, which is what
/// [`menu::activate`] takes.
fn level_view(level: &Level, cursor: usize) -> impl cursive::View {
    let mut v = SelectView::<usize>::new();
    for (i, item) in level.items.iter().enumerate() {
        v.add_item(row_label(item), i);
    }
    // The core's preferred row wins when the level has just materialised (the
    // cursor is still at 0), otherwise keep where the operator was.
    let start = if cursor == 0 {
        level.preferred_cursor.unwrap_or(0)
    } else {
        cursor
    };
    if start < level.items.len() {
        v.set_selection(start);
    }
    let v = v.on_select(|s, index: &usize| {
        let index = *index;
        let mut nav = nav(s);
        nav.set_cursor(index);
        set_nav(s, nav);
        let level = current_level(s);
        if let Some(d) = s.user_data::<TuiData>() {
            let ctrl = Arc::clone(&d.ctrl);
            menu::on_focus(&ctrl, &level, index);
        }
        // The Details button is per-row, so its availability can change.
        let state = snapshot(s);
        rebuild_buttons(s, &state);
    });
    let v = v.on_submit(|s, index: &usize| activate_row(s, *index));
    v.with_name("submenu").scrollable().full_height()
}

// --- details popup ---------------------------------------------------------

/// Open a scrollable details popup for the highlighted row, if it has one.
fn open_details(siv: &mut Cursive) {
    let level = current_level(siv);
    let cursor = nav(siv).cursor();
    let Some(target) = level.details_at(cursor) else {
        return;
    };
    if let Some(d) = siv.user_data::<TuiData>() {
        *d.detail.borrow_mut() = Some(target.clone());
    }

    let state = snapshot(siv);
    let (title, text) = menu::details_text(&state, &target);
    let dialog = Dialog::around(
        TextView::new(text)
            .with_name("details")
            .scrollable()
            .max_height(16)
            .min_width(44),
    )
    .title(title)
    .button("Close", close_details);
    siv.add_layer(
        OnEventView::new(dialog)
            .on_event(Key::Backspace, close_details)
            .on_event(Key::Esc, close_details),
    );
}

fn close_details(siv: &mut Cursive) {
    siv.pop_layer();
    if let Some(d) = siv.user_data::<TuiData>() {
        *d.detail.borrow_mut() = None;
    }
}

// --- the core's modal prompt -----------------------------------------------

/// Answer the prompt affirmatively. The controller clears it, and the snapshot
/// that follows is what takes the dialog down.
fn accept_prompt(siv: &mut Cursive) {
    action(siv, |c| c.confirm_prompt());
}

/// Turn the prompt down, from its own button or from Backspace/Escape.
fn cancel_prompt(siv: &mut Cursive) {
    action(siv, |c| c.dismiss_prompt());
}

/// Show or hide the question the core raised, driven purely by the snapshot: the
/// buttons only answer the controller, and the layer appears and disappears with
/// [`AppState::prompt`]. That is what keeps the two frontends in step — answering
/// in the serial console closes the same prompt on the device's screen.
fn sync_prompt(siv: &mut Cursive, state: &AppState) {
    let open = siv
        .user_data::<TuiData>()
        .map(|d| *d.prompt_open.borrow())
        .unwrap_or(false);
    match &state.prompt {
        Some(prompt) if !open => {
            // Only the topmost layer can be removed again, so an open details
            // popup goes first.
            if siv
                .user_data::<TuiData>()
                .map(|d| d.detail.borrow().is_some())
                .unwrap_or(false)
            {
                close_details(siv);
            }
            if let Some(d) = siv.user_data::<TuiData>() {
                *d.prompt_open.borrow_mut() = true;
            }
            let dialog = Dialog::around(
                TextView::new(prompt.lines.join("\n"))
                    .scrollable()
                    .max_height(14)
                    .min_width(48),
            )
            .title(prompt.title.clone())
            // Cancel first, so cursive opens with the harmless button focused —
            // these prompts destroy data.
            .button(prompt.cancel.clone(), cancel_prompt)
            .button(prompt.confirm.clone(), accept_prompt);
            siv.add_layer(
                OnEventView::new(dialog)
                    .on_event(Key::Backspace, cancel_prompt)
                    .on_event(Key::Esc, cancel_prompt),
            );
        }
        None if open => {
            siv.pop_layer();
            if let Some(d) = siv.user_data::<TuiData>() {
                *d.prompt_open.borrow_mut() = false;
            }
        }
        _ => {}
    }
}

// --- rendering -------------------------------------------------------------

/// Progress-bar + phase header line shown at the top of the install pane.
fn install_status(state: &AppState) -> String {
    format!("Phase : {}\n{}", state.phase.label(), progress_bar(state.progress))
}

/// Fill the details pane with the install view: progress header + a log that
/// auto-scrolls to the newest line.
fn fill_install(ll: &mut LinearLayout) {
    clear_layout(ll);
    ll.add_child(TextView::new("").with_name("inst_bar"));
    ll.add_child(TextView::new("\nLog:"));
    ll.add_child(
        TextView::new("")
            .with_name("inst_log")
            .scrollable()
            .scroll_strategy(ScrollStrategy::StickToBottom)
            .full_height(),
    );
}

/// Switch the details pane to the install view (closing any open level), once,
/// when installation starts.
fn ensure_install_pane(siv: &mut Cursive) {
    if siv.call_on_name("inst_bar", |_: &mut TextView| ()).is_some() {
        return; // already showing it
    }
    if nav(siv).depth() > 0 {
        let mut n = nav(siv);
        n.reset();
        set_nav(siv, n);
    }
    if layered(siv) {
        siv.pop_layer();
        if let Some(d) = siv.user_data::<TuiData>() {
            *d.layered.borrow_mut() = false;
        }
    }
    siv.call_on_name("detail", |ll: &mut LinearLayout| fill_install(ll));
    let _ = siv.focus_name("overview");
}

/// Re-render the whole screen from a state snapshot.
fn render(siv: &mut Cursive, state: &AppState) {
    // Overview rows (preserve the highlighted row across the rebuild).
    let root = menu::root_level(state);
    let labels: Vec<String> = root
        .items
        .iter()
        .map(|i| format!("{:<9}{}", i.text, i.detail))
        .collect();
    siv.call_on_name("overview", |v: &mut SelectView<usize>| {
        let sel = v.selected_id().unwrap_or(0);
        v.clear();
        for (i, l) in labels.into_iter().enumerate() {
            v.add_item(l, i);
        }
        if sel < v.len() {
            v.set_selection(sel);
        }
    });

    // Bottom action buttons: shown only while their action is available.
    rebuild_buttons(siv, state);

    // Before the install-view branch below returns early: a prompt can be raised
    // at any phase, and it has to be answerable in all of them.
    sync_prompt(siv, state);

    let tail: Vec<String> = state.log.iter().rev().take(400).rev().cloned().collect();

    // Once installation starts, the details pane becomes a dedicated install
    // view: a progress bar plus the auto-scrolling activity log. It stays up
    // through Done/Failed so the outcome (and any error) remain visible.
    if matches!(state.phase, Phase::Installing | Phase::Done | Phase::Failed(_)) {
        ensure_install_pane(siv);
        siv.call_on_name("inst_bar", |v: &mut TextView| v.set_content(install_status(state)));
        siv.call_on_name("inst_log", |v: &mut TextView| v.set_content(tail.join("\n")));
        return;
    }

    // Not installing: restore the status pane if the install view was showing.
    if siv.call_on_name("inst_bar", |_: &mut TextView| ()).is_some() {
        siv.call_on_name("detail", |ll: &mut LinearLayout| fill_status(ll));
    }
    siv.call_on_name("status", |v: &mut TextView| v.set_content(render_status(state)));
    siv.call_on_name("log", |v: &mut TextView| v.set_content(tail.join("\n")));

    // Rebuild the open level's list only when what it renders changed. The
    // fingerprint comes from the built level, so a newly listed channel or a
    // lazily fetched build number cannot be forgotten here.
    if nav(siv).depth() > 0 {
        let nav_now = nav(siv);
        let level = menu::level(&nav_now, state);
        let sig = menu::signature(&level);
        let changed = siv
            .user_data::<TuiData>()
            .map(|d| {
                let mut last = d.sig.borrow_mut();
                if *last != sig {
                    *last = sig.clone();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(true);
        if changed {
            let cursor = nav_now.cursor().min(level.items.len().saturating_sub(1));
            let labels: Vec<String> = level.items.iter().map(row_label).collect();
            siv.call_on_name("submenu", |v: &mut SelectView<usize>| {
                v.clear();
                for (i, l) in labels.into_iter().enumerate() {
                    v.add_item(l, i);
                }
                if cursor < v.len() {
                    v.set_selection(cursor);
                }
            });
            // A level that just finished loading may have fewer rows than the
            // placeholder cursor allowed for.
            let mut n = nav_now;
            n.clamp(level.items.len());
            set_nav(siv, n);
        }
    }

    // Keep an open details popup in sync as its sourcestamps arrive.
    let target = siv.user_data::<TuiData>().and_then(|d| d.detail.borrow().clone());
    if let Some(target) = target {
        let (_, text) = menu::details_text(state, &target);
        siv.call_on_name("details", |v: &mut TextView| v.set_content(text));
    }
}

/// Rebuild the bottom button bar, showing each action only when it is allowed.
/// Buttons carry an underlined mnemonic letter, also bound as Alt+letter.
fn rebuild_buttons(siv: &mut Cursive, state: &AppState) {
    let can_install = state.can_install();
    let can_reboot = state.can_reboot();
    let busy = state.phase.is_busy();
    // Details and Refresh are per-level, and Details is per-row.
    let (has_details, can_refresh) = if nav(siv).depth() > 0 {
        let level = current_level(siv);
        (
            level.details_at(nav(siv).cursor()).is_some(),
            level.can_refresh,
        )
    } else {
        (false, true)
    };
    siv.call_on_name("buttons", move |ll: &mut LinearLayout| {
        clear_layout(ll);
        if can_install {
            ll.add_child(Button::new_raw(mnemonic("Install", 'I'), |s| {
                action(s, |c| c.start_install())
            }));
            ll.add_child(TextView::new("  "));
        }
        if !busy && can_refresh {
            ll.add_child(Button::new_raw(mnemonic("Refresh", 'R'), refresh));
            ll.add_child(TextView::new("  "));
        }
        if has_details {
            ll.add_child(Button::new_raw(mnemonic("Details", 'D'), open_details));
            ll.add_child(TextView::new("  "));
        }
        // Last of the actions, next to Quit: after a successful install this is
        // the expected next step, and it is the only one that ends the session.
        if can_reboot {
            ll.add_child(Button::new_raw(mnemonic("Reboot", 'B'), |s| {
                action(s, |c| c.request_reboot())
            }));
            ll.add_child(TextView::new("  "));
        }
        ll.add_child(Button::new_raw(mnemonic("Quit", 'Q'), Cursive::quit));
    });
}

/// A `<label>` button caption with the mnemonic letter underlined (Alt+letter).
fn mnemonic(label: &str, key: char) -> StyledString {
    let mut s = StyledString::new();
    s.append_plain("<");
    let mut hit = false;
    for ch in label.chars() {
        if !hit && ch.eq_ignore_ascii_case(&key) {
            s.append_styled(ch.to_string(), Effect::Underline);
            hit = true;
        } else {
            s.append_plain(ch.to_string());
        }
    }
    s.append_plain(">");
    s
}

/// Detailed status shown in the right pane.
fn render_status(state: &AppState) -> String {
    let target = state
        .target()
        .map(|d| d.summary())
        .unwrap_or_else(|| "—".to_string());
    let uboot = state
        .selected_uboot()
        .map(|b| b.summary())
        .unwrap_or_else(|| "—".to_string());
    let build = state
        .selected_build()
        .map(|b| b.summary())
        .unwrap_or_else(|| "—".to_string());
    let mut profiles = vec!["Minimal".to_string()];
    profiles.extend(state.selection.profiles.iter().cloned());
    let bar = progress_bar(state.progress);
    let source = match &state.bundle {
        Some(b) => format!("{} {} ({})", b.channel, b.version, b.reference.source.label()),
        None => match &state.bundle_error {
            Some(e) => format!("unavailable — {e}"),
            None => state.source_label(),
        },
    };
    format!(
        "Board   : {} ({})\nBoard id: {}\nPhase   : {}\n{bar}\n\nSource  : {source}\nTarget  : {target}\nU-Boot  : {uboot}\nSnapshot: {build}\nProfiles: {}\nFetch   : {}\nReady   : {}",
        state.board.model,
        state.board.soc,
        state.board.board_id,
        state.phase.label(),
        profiles.join(", "),
        state.selection.fetch.label(),
        if state.can_install() { "yes" } else { "no" },
    )
}

fn progress_bar(progress: f32) -> String {
    let width = 24usize;
    let filled = (progress.clamp(0.0, 1.0) * width as f32).round() as usize;
    let bar: String = std::iter::repeat('#')
        .take(filled)
        .chain(std::iter::repeat('-').take(width - filled))
        .collect();
    format!("[{bar}] {:.0}%", progress * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::menu::{Action, Icon, MenuItem};
    use cursive::style::Effects;

    fn item(text: &str, detail: &str, marker: Marker, drill: bool) -> MenuItem {
        MenuItem {
            text: text.to_string(),
            detail: detail.to_string(),
            icon: Icon::None,
            marker,
            selected: false,
            drill,
            dim: false,
            action: Action::Inert,
            on_focus: None,
            details: None,
        }
    }

    /// The underlined letter has to be the one bound as Alt+letter, and Reboot's
    /// cannot be the obvious `R` — Refresh already owns that.
    #[test]
    fn button_mnemonics_underline_the_bound_letter() {
        for (label, key, expected) in [("Reboot", 'B', "b"), ("Refresh", 'R', "R")] {
            let styled = mnemonic(label, key);
            assert_eq!(styled.source(), format!("<{label}>"));
            let underlined: Vec<&str> = styled
                .spans()
                .filter(|s| s.attr.effects == Effects::only(Effect::Underline))
                .map(|s| s.content)
                .collect();
            assert_eq!(underlined, vec![expected], "{label}");
        }
    }

    #[test]
    fn row_labels_show_state_and_affordances() {
        assert_eq!(
            row_label(&item("Desktop", "378.0 MiB", Marker::Checked, false)),
            "[x] Desktop  (378.0 MiB)"
        );
        assert_eq!(
            row_label(&item("Router", "", Marker::Unchecked, false)),
            "[ ] Router"
        );
        // A drill-in row carries the chevron and nothing else; the committed
        // choice is conveyed by the cursor, not by a marker.
        assert_eq!(
            row_label(&item("Release", "", Marker::None, true)),
            "Release \u{203a}"
        );
        assert_eq!(
            row_label(&item("#15", "2026-08-12 02:25", Marker::None, false)),
            "#15  (2026-08-12 02:25)"
        );
    }
}
