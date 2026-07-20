//! Cursive/Crossterm TUI frontend.
//!
//! This frontend is a thin view over the shared [`Controller`]: every widget
//! action calls a controller method, and a single subscriber re-renders the
//! screen from each broadcast [`AppState`] snapshot. Because the controller is
//! the single source of truth, selections made here show up live in the GUI and
//! vice-versa.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use cursive::event::{Event, Key};
use cursive::style::Effect;
use cursive::view::ScrollStrategy;
use cursive::traits::{Nameable, Resizable, Scrollable};
use cursive::utils::markup::StyledString;
use cursive::views::{
    BoxedView, Button, Checkbox, Dialog, LinearLayout, OnEventView, Panel, SelectView, TextView,
};
use cursive::Cursive;

use crate::core::model::{AppState, Phase};
use crate::core::Controller;

/// The Install row on the overview; the first four rows open a submenu.
const SEC_INSTALL: usize = 4;

/// Minimum terminal width at which the submenu is drawn in the right details
/// pane alongside the overview. Below this we fall back to a single full-screen
/// pane (the submenu replaces the overview), like the GUI.
const TWO_PANE_MIN_COLS: usize = 74;

/// Per-session state stored in Cursive's user data.
struct TuiData {
    ctrl: Arc<Controller>,
    /// Signature of the last-rendered data lists, so we only rebuild the
    /// interactive widgets when the underlying data actually changes.
    sig: RefCell<String>,
    /// Active submenu section (0..=3), or None on the overview.
    active: RefCell<Option<usize>>,
    /// Whether the active submenu is a full-screen layer (true) or lives in the
    /// details pane (false).
    layered: RefCell<bool>,
    /// Open details popup target: (section, build id), or None when closed.
    detail: RefCell<Option<(usize, String)>>,
    /// (section, build id) whose manifest fetch has been kicked off, so scrolling
    /// the list doesn't re-spawn duplicate fetches. Cleared on Refresh.
    requested: RefCell<HashSet<(usize, String)>>,
}

/// Build the UI, wire it to `ctrl`, and run the blocking event loop.
pub fn run(ctrl: Arc<Controller>) {
    let mut siv = cursive::default();
    siv.set_user_data(TuiData {
        ctrl: Arc::clone(&ctrl),
        sig: RefCell::new(String::new()),
        active: RefCell::new(None),
        layered: RefCell::new(false),
        detail: RefCell::new(None),
        requested: RefCell::new(HashSet::new()),
    });

    // Left: overview of the current selections (Enter opens a submenu / installs).
    // Right: the details pane, which shows live status and doubles as the submenu
    // host when the terminal is wide enough.
    let overview = SelectView::<usize>::new()
        .on_submit(|s, section: &usize| open_section(s, *section))
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
    siv.add_global_callback(Key::Backspace, close_submenu);
    siv.add_global_callback(Key::Esc, close_submenu);

    // Alt+letter mnemonics for the bottom action buttons (each guarded so it
    // only fires when its action is currently available).
    siv.add_global_callback(Event::AltChar('i'), |s| {
        if snapshot(s).can_install() {
            action(s, |c| c.start_install());
        }
    });
    siv.add_global_callback(Event::AltChar('r'), |s| {
        if !snapshot(s).phase.is_busy() {
            action(s, |c| {
                let c = Arc::clone(c);
                std::thread::spawn(move || c.refresh_sources());
            });
        }
    });
    siv.add_global_callback(Event::AltChar('d'), |s| {
        if let Some(section) = get_active(s).0.filter(|x| *x == 1 || *x == 2) {
            open_details(s, section);
        }
    });
    siv.add_global_callback(Event::AltChar('q'), Cursive::quit);

    // Subscribe: marshal every snapshot into the Cursive event loop.
    let cb_sink = siv.cb_sink().clone();
    ctrl.subscribe(move |snapshot| {
        let _ = cb_sink.send(Box::new(move |siv: &mut Cursive| render(siv, &snapshot)));
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

// --- navigation ------------------------------------------------------------

fn snapshot(siv: &mut Cursive) -> AppState {
    siv.user_data::<TuiData>()
        .map(|d| d.ctrl.snapshot())
        .unwrap_or_default()
}

fn set_active(siv: &mut Cursive, active: Option<usize>, layered: bool) {
    if let Some(d) = siv.user_data::<TuiData>() {
        *d.active.borrow_mut() = active;
        *d.layered.borrow_mut() = layered;
    }
}

fn get_active(siv: &mut Cursive) -> (Option<usize>, bool) {
    siv.user_data::<TuiData>()
        .map(|d| (*d.active.borrow(), *d.layered.borrow()))
        .unwrap_or((None, false))
}

fn section_title(section: usize) -> &'static str {
    match section {
        0 => "Target device",
        1 => "U-Boot build",
        2 => "Snapshot build",
        _ => "Profiles",
    }
}

/// Open the submenu for `section`, or trigger install for the Install row.
///
/// When the terminal is wide enough the submenu is drawn in the right details
/// pane (overview stays visible on the left); otherwise it takes over the whole
/// screen as a full-screen layer.
fn open_section(siv: &mut Cursive, section: usize) {
    if section == SEC_INSTALL {
        action(siv, |c| c.start_install());
        return;
    }
    let state = snapshot(siv);
    // Don't open selection submenus while an install is running.
    if matches!(state.phase, Phase::Installing) {
        return;
    }
    let two_pane = siv.screen_size().x >= TWO_PANE_MIN_COLS;

    let submenu = if section == 3 {
        BoxedView::boxed(profiles_submenu(&state))
    } else {
        BoxedView::boxed(list_submenu(section, &state))
    };
    let focus = if section == 3 { "profiles" } else { "submenu" };

    set_active(siv, Some(section), !two_pane);

    if two_pane {
        siv.call_on_name("detail", move |ll: &mut LinearLayout| {
            clear_layout(ll);
            ll.add_child(TextView::new(section_title(section)));
            ll.add_child(submenu);
        });
    } else {
        siv.add_fullscreen_layer(Panel::new(submenu).title(section_title(section)));
    }
    let _ = siv.focus_name(focus);
    // Fetch the initially-highlighted build's number (on_select only fires on
    // subsequent moves, not for the selection set at construction).
    if section == 1 || section == 2 {
        if let Some(id) = siv
            .call_on_name("submenu", |v: &mut SelectView<String>| {
                v.selection().map(|r| (*r).clone())
            })
            .flatten()
        {
            request_build_details(siv, section, &id);
        }
    }
    // Refresh the button bar so the context-sensitive Details button appears.
    rebuild_buttons(siv, &state);
}

/// Return from a submenu: pop the layer or restore the status pane, then refocus
/// the overview. A no-op when already on the overview.
fn close_submenu(siv: &mut Cursive) {
    if get_active(siv).0.is_none() {
        return;
    }
    let layered = get_active(siv).1;
    set_active(siv, None, false);
    if layered {
        siv.pop_layer();
    } else {
        siv.call_on_name("detail", |ll: &mut LinearLayout| fill_status(ll));
    }
    let _ = siv.focus_name("overview");
    let state = snapshot(siv);
    render(siv, &state);
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

// --- submenu construction --------------------------------------------------

/// (label, value) pairs for a list submenu section (0 device, 1 u-boot, 2 snap).
fn submenu_items(section: usize, state: &AppState) -> Vec<(String, String)> {
    match section {
        0 => state
            .devices
            .iter()
            .map(|d| {
                let mark = if d.boot_rom_capable() { " " } else { "!" };
                (format!("{mark}{}", d.summary()), d.path.clone())
            })
            .collect(),
        1 => state
            .uboot_builds
            .iter()
            .map(|b| (b.summary(), b.id.clone()))
            .collect(),
        2 => state
            .snapshot_builds
            .iter()
            .map(|b| (b.summary(), b.id.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The currently-selected value for `section`, used to keep the cursor put.
fn submenu_selected(section: usize, state: &AppState) -> Option<String> {
    match section {
        0 => state.selection.target_device.clone(),
        1 => state.selection.uboot.clone(),
        2 => state.selection.snapshot_build.clone(),
        _ => None,
    }
}

/// A scrollable single-select list for the device/u-boot/snapshot sections.
fn list_submenu(section: usize, state: &AppState) -> impl cursive::View {
    let items = submenu_items(section, state);
    let mut v = SelectView::<String>::new();
    for (label, value) in &items {
        v.add_item(label.clone(), value.clone());
    }
    if let Some(sel) = submenu_selected(section, state) {
        if let Some(i) = items.iter().position(|(_, val)| val == &sel) {
            v.set_selection(i);
        }
    }
    // Lazily fetch the highlighted build's manifest (for its #<n> identifier) as
    // the list scrolls; a no-op for the device list.
    let v = v.on_select(move |s, value: &String| request_build_details(s, section, value));
    let v = v.on_submit(move |s, value: &String| {
        let value = value.clone();
        action(s, move |c| match section {
            0 => c.select_device(&value),
            1 => c.select_uboot(&value),
            2 => c.select_snapshot_build(&value),
            _ => {}
        });
        close_submenu(s);
    });
    v.with_name("submenu").scrollable().full_height()
}

/// Kick off a background manifest fetch for a build row so its build number can
/// replace the placeholder label. Only fires for the U-Boot (1) / snapshot (2)
/// lists, and at most once per build per session (deduped via `requested`).
fn request_build_details(siv: &mut Cursive, section: usize, id: &str) {
    if section != 1 && section != 2 {
        return;
    }
    if let Some(d) = siv.user_data::<TuiData>() {
        if !d.requested.borrow_mut().insert((section, id.to_string())) {
            return;
        }
        let ctrl = Arc::clone(&d.ctrl);
        let id = id.to_string();
        std::thread::spawn(move || match section {
            1 => ctrl.load_uboot_details(&id),
            2 => ctrl.load_snapshot_details(&id),
            _ => {}
        });
    }
}

/// A scrollable multi-select (checkbox) list for the profiles section.
fn profiles_submenu(state: &AppState) -> impl cursive::View {
    let mut list = LinearLayout::vertical();
    populate_profiles(&mut list, state);
    list.with_name("profiles").scrollable().full_height()
}

fn populate_profiles(list: &mut LinearLayout, state: &AppState) {
    clear_layout(list);
    let build = state.selected_build();
    let loaded = build.map(|b| b.loaded).unwrap_or(false);
    if !loaded {
        list.add_child(TextView::new("  (loading…)"));
        return;
    }
    // Minimal is always deployed.
    let minimal_label = build
        .and_then(|b| b.minimal())
        .map(|p| p.summary())
        .unwrap_or_else(|| "Minimal".to_string());
    list.add_child(TextView::new(format!("  [x] {minimal_label} (always)")));

    let extras: Vec<(String, String, bool)> = build
        .map(|b| {
            b.extra_profiles()
                .map(|p| {
                    let checked = state.selection.profiles.iter().any(|n| n == &p.name);
                    (p.name.clone(), p.summary(), checked)
                })
                .collect()
        })
        .unwrap_or_default();
    if !extras.is_empty() {
        let all = extras.iter().all(|(_, _, c)| *c);
        list.add_child(
            LinearLayout::horizontal()
                .child(
                    Checkbox::new()
                        .with_checked(all)
                        .on_change(|s, on| action(s, move |c| c.select_all_profiles(on))),
                )
                .child(TextView::new(" (all extra profiles)")),
        );
    }
    for (name, label, checked) in &extras {
        let n = name.clone();
        list.add_child(
            LinearLayout::horizontal()
                .child(Checkbox::new().with_checked(*checked).on_change(move |s, on| {
                    let n = n.clone();
                    action(s, move |c| c.toggle_profile(&n, on));
                }))
                .child(TextView::new(format!(" {label}"))),
        );
    }
}

// --- details popup ---------------------------------------------------------

/// Open a scrollable details popup for the highlighted U-Boot / snapshot entry,
/// kicking off a background fetch of its sourcestamps.
fn open_details(siv: &mut Cursive, section: usize) {
    if section != 1 && section != 2 {
        return;
    }
    let id = siv
        .call_on_name("submenu", |v: &mut SelectView<String>| {
            v.selection().map(|r| (*r).clone())
        })
        .flatten();
    let Some(id) = id else {
        return;
    };

    if let Some(d) = siv.user_data::<TuiData>() {
        let ctrl = Arc::clone(&d.ctrl);
        let idc = id.clone();
        std::thread::spawn(move || match section {
            1 => ctrl.load_uboot_details(&idc),
            2 => ctrl.load_snapshot_details(&idc),
            _ => {}
        });
        *d.detail.borrow_mut() = Some((section, id.clone()));
    }

    let title = if section == 1 {
        "U-Boot details"
    } else {
        "Snapshot details"
    };
    let text = detail_text(siv, section, &id);
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

/// The details text for a build, or a loading placeholder.
fn detail_text(siv: &mut Cursive, section: usize, id: &str) -> String {
    let st = snapshot(siv);
    let found = match section {
        1 => st.uboot_builds.iter().find(|b| b.id == id).map(|b| b.details_text()),
        2 => st.snapshot_builds.iter().find(|b| b.id == id).map(|b| b.details_text()),
        _ => None,
    };
    found.unwrap_or_else(|| "Loading…".to_string())
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

/// Switch the details pane to the install view (closing any open submenu), once,
/// when installation starts.
fn ensure_install_pane(siv: &mut Cursive) {
    if siv.call_on_name("inst_bar", |_: &mut TextView| ()).is_some() {
        return; // already showing it
    }
    let (active, layered) = get_active(siv);
    if active.is_some() {
        set_active(siv, None, false);
        if layered {
            siv.pop_layer();
        }
    }
    siv.call_on_name("detail", |ll: &mut LinearLayout| fill_install(ll));
    let _ = siv.focus_name("overview");
}

/// Re-render the whole screen from a state snapshot.
fn render(siv: &mut Cursive, state: &AppState) {
    // Overview rows (preserve the highlighted row across the rebuild).
    let labels = overview_labels(state);
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

    // Refresh the active submenu's list only when the underlying data changed.
    let sig = data_signature(state);
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
        match get_active(siv).0 {
            Some(3) => {
                siv.call_on_name("profiles", |ll: &mut LinearLayout| populate_profiles(ll, state));
            }
            Some(section) => {
                let items = submenu_items(section, state);
                let sel = submenu_selected(section, state);
                siv.call_on_name("submenu", |v: &mut SelectView<String>| {
                    // Preserve the current highlight across the rebuild (a lazily
                    // fetched build number rebuilds the list while the user may be
                    // mid-scroll); fall back to the committed selection.
                    let cur = v.selection().map(|r| (*r).clone());
                    v.clear();
                    for (label, value) in &items {
                        v.add_item(label.clone(), value.clone());
                    }
                    let restore = cur
                        .as_ref()
                        .or(sel.as_ref())
                        .and_then(|id| items.iter().position(|(_, val)| val == id));
                    if let Some(i) = restore {
                        v.set_selection(i);
                    }
                });
            }
            None => {}
        }
    }

    // Keep an open details popup in sync as its sourcestamps arrive.
    let target = siv.user_data::<TuiData>().and_then(|d| d.detail.borrow().clone());
    if let Some((section, id)) = target {
        let text = detail_text(siv, section, &id);
        siv.call_on_name("details", |v: &mut TextView| v.set_content(text));
    }
}

/// The five overview rows: label + current selection.
fn overview_labels(state: &AppState) -> Vec<String> {
    let dev = state
        .target()
        .map(|d| format!("{} {}", d.path, d.human_size()))
        .unwrap_or_else(|| "(not selected)".to_string());
    let ub = state
        .selected_uboot()
        .map(|b| b.display_name())
        .unwrap_or_else(|| "(not selected)".to_string());
    let sn = state
        .selected_build()
        .map(|b| b.display_name())
        .unwrap_or_else(|| "(not selected)".to_string());
    let extra = state.selection.profiles.len();
    let pr = if extra > 0 {
        format!("Minimal +{extra}")
    } else {
        "Minimal".to_string()
    };
    let inst = state.install_status_label();
    vec![
        format!("{:<9}{}", "Device", dev),
        format!("{:<9}{}", "U-Boot", ub),
        format!("{:<9}{}", "Snapshot", sn),
        format!("{:<9}{}", "Profiles", pr),
        format!("{:<9}{}", "Install", inst),
    ]
}

/// Rebuild the bottom button bar, showing each action only when it is allowed.
/// Buttons carry an underlined mnemonic letter, also bound as Alt+letter.
fn rebuild_buttons(siv: &mut Cursive, state: &AppState) {
    let can_install = state.can_install();
    let busy = state.phase.is_busy();
    // Details is available while a U-Boot / snapshot submenu is open.
    let details_section = get_active(siv).0.filter(|s| *s == 1 || *s == 2);
    siv.call_on_name("buttons", |ll: &mut LinearLayout| {
        clear_layout(ll);
        if can_install {
            ll.add_child(Button::new_raw(mnemonic("Install", 'I'), |s| {
                action(s, |c| c.start_install())
            }));
            ll.add_child(TextView::new("  "));
        }
        if !busy {
            ll.add_child(Button::new_raw(mnemonic("Refresh", 'R'), |s| {
                // Rebuilt lists want their manifests re-fetched.
                if let Some(d) = s.user_data::<TuiData>() {
                    d.requested.borrow_mut().clear();
                }
                action(s, |c| {
                    let c = Arc::clone(c);
                    std::thread::spawn(move || c.refresh_sources());
                })
            }));
            ll.add_child(TextView::new("  "));
        }
        if let Some(section) = details_section {
            ll.add_child(Button::new_raw(mnemonic("Details", 'D'), move |s| {
                open_details(s, section)
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
    format!(
        "Board   : {} ({})\nBoard id: {}\nPhase   : {}\n{bar}\n\nTarget  : {target}\nU-Boot  : {uboot}\nSnapshot: {build}\nProfiles: {}\nReady   : {}",
        state.board.model,
        state.board.soc,
        state.board.board_id,
        state.phase.label(),
        profiles.join(", "),
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

/// A stable fingerprint of the selectable data, used to decide when to rebuild.
fn data_signature(state: &AppState) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(state.board.board_id.clone());
    for d in &state.devices {
        parts.push(d.path.clone());
    }
    // Include the build number so the list rebuilds (switching label -> #<n>)
    // when a lazily-fetched manifest arrives.
    for b in &state.uboot_builds {
        parts.push(format!("u:{}:{:?}", b.id, b.build_number()));
    }
    for b in &state.snapshot_builds {
        parts.push(format!("s:{}:{}:{:?}", b.id, b.loaded, b.resolved_build_number()));
    }
    if let Some(b) = state.selected_build() {
        for p in &b.profiles {
            parts.push(format!("pp:{}", p.name));
        }
    }
    // Selection affects checkbox/highlight state, so include it.
    if let Some(t) = &state.selection.target_device {
        parts.push(format!("t:{t}"));
    }
    if let Some(u) = &state.selection.uboot {
        parts.push(format!("v:{u}"));
    }
    if let Some(b) = &state.selection.snapshot_build {
        parts.push(format!("b:{b}"));
    }
    for p in &state.selection.profiles {
        parts.push(format!("p:{p}"));
    }
    parts.push(if matches!(state.phase, Phase::Installing) {
        "busy".to_string()
    } else {
        "idle".to_string()
    });
    parts.join("|")
}
