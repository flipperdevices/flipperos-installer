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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::core::model::{human_bytes, human_time, AppState, Phase, Source};
use crate::core::Controller;

/// Source glyph for a build row: 1 = network (server), 2 = sdcard (removable
/// media). Matches the `icon-kind` the Slint `ListRow` expects.
fn source_icon(source: &Source) -> i32 {
    match source {
        Source::Server => 1,
        Source::Removable { .. } => 2,
    }
}

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
    export { MainWindow } from "ui/main.slint";
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

    // A cache of the latest snapshot so index-based callbacks can resolve the
    // concrete device/version/snapshot the operator picked.
    let cache: Arc<Mutex<AppState>> = Arc::new(Mutex::new(ctrl.snapshot()));

    // The build whose details popup is open (1 = u-boot, 2 = snapshot), so every
    // snapshot can refresh the popup as its sourcestamps arrive.
    let detail_target: Arc<Mutex<Option<(u8, String)>>> = Arc::new(Mutex::new(None));

    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        win.on_select_device(move |idx| {
            let state = cache.lock().unwrap();
            if let Some(dev) = state.devices.get(idx as usize) {
                ctrl.select_device(&dev.path);
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        win.on_select_uboot(move |idx| {
            let state = cache.lock().unwrap();
            if let Some(b) = state.uboot_builds.get(idx as usize) {
                ctrl.select_uboot(&b.id);
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        win.on_select_snapshot(move |idx| {
            let id = {
                let state = cache.lock().unwrap();
                state.snapshot_builds.get(idx as usize).map(|b| b.id.clone())
            };
            if let Some(id) = id {
                ctrl.select_snapshot_build(&id);
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        win.on_toggle_profile(move |idx, on| {
            let name = {
                let state = cache.lock().unwrap();
                state
                    .selected_build()
                    .and_then(|b| b.profiles.get(idx as usize))
                    .map(|p| p.name.clone())
            };
            if let Some(name) = name {
                ctrl.toggle_profile(&name, on);
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        win.on_start_install(move || ctrl.start_install());
    }
    // Build ids whose manifest fetch has already been kicked off, so scrolling
    // back and forth doesn't re-spawn duplicate in-flight fetches. Cleared on
    // Refresh (the lists are rebuilt, so their manifests want re-fetching).
    let requested: Arc<Mutex<HashSet<(i32, String)>>> = Arc::new(Mutex::new(HashSet::new()));

    {
        // Re-scan sources off the event-loop thread, like the TUI's Refresh.
        let ctrl = Arc::clone(&ctrl);
        let requested = Arc::clone(&requested);
        win.on_refresh(move || {
            requested.lock().unwrap().clear();
            let ctrl = Arc::clone(&ctrl);
            std::thread::spawn(move || ctrl.refresh_sources());
        });
    }
    {
        // Lazily fetch the manifest (for the build number) of the rows currently
        // on screen in a U-Boot (1) / snapshot (2) submenu. Fetches run off the
        // event loop; each completion updates state and re-renders the row.
        let ctrl = Arc::clone(&ctrl);
        let requested = Arc::clone(&requested);
        win.on_ensure_loaded(move |section, first, count| {
            if section != 1 && section != 2 {
                return;
            }
            let first = first.max(0) as usize;
            let count = count.max(0) as usize;
            let state = ctrl.snapshot();
            let ids: Vec<String> = if section == 1 {
                state
                    .uboot_builds
                    .iter()
                    .skip(first)
                    .take(count)
                    .filter(|b| b.build_number().is_none())
                    .map(|b| b.id.clone())
                    .collect()
            } else {
                state
                    .snapshot_builds
                    .iter()
                    .skip(first)
                    .take(count)
                    .filter(|b| b.resolved_build_number().is_none())
                    .map(|b| b.id.clone())
                    .collect()
            };
            let mut req = requested.lock().unwrap();
            for id in ids {
                if req.insert((section, id.clone())) {
                    let ctrl = Arc::clone(&ctrl);
                    std::thread::spawn(move || {
                        if section == 1 {
                            ctrl.load_uboot_details(&id);
                        } else {
                            ctrl.load_snapshot_details(&id);
                        }
                    });
                }
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        let detail_target = Arc::clone(&detail_target);
        let weak = win.as_weak();
        win.on_show_uboot_details(move |idx| {
            let id = cache.lock().unwrap().uboot_builds.get(idx as usize).map(|b| b.id.clone());
            if let Some(id) = id {
                *detail_target.lock().unwrap() = Some((1, id.clone()));
                let c = Arc::clone(&ctrl);
                let idc = id.clone();
                std::thread::spawn(move || c.load_uboot_details(&idc));
                if let Some(win) = weak.upgrade() {
                    apply_details(&win, &cache.lock().unwrap(), &detail_target.lock().unwrap());
                }
            }
        });
    }
    {
        let ctrl = Arc::clone(&ctrl);
        let cache = Arc::clone(&cache);
        let detail_target = Arc::clone(&detail_target);
        let weak = win.as_weak();
        win.on_show_snapshot_details(move |idx| {
            let id = cache.lock().unwrap().snapshot_builds.get(idx as usize).map(|b| b.id.clone());
            if let Some(id) = id {
                *detail_target.lock().unwrap() = Some((2, id.clone()));
                let c = Arc::clone(&ctrl);
                let idc = id.clone();
                std::thread::spawn(move || c.load_snapshot_details(&idc));
                if let Some(win) = weak.upgrade() {
                    apply_details(&win, &cache.lock().unwrap(), &detail_target.lock().unwrap());
                }
            }
        });
    }

    // Subscribe: marshal every snapshot into the Slint event loop.
    let weak = win.as_weak();
    let detail_for_sub = Arc::clone(&detail_target);
    ctrl.subscribe(move |snapshot| {
        let weak = weak.clone();
        let cache = Arc::clone(&cache);
        let detail_target = Arc::clone(&detail_for_sub);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(win) = weak.upgrade() {
                *cache.lock().unwrap() = snapshot.clone();
                apply(&win, &snapshot);
                apply_details(&win, &snapshot, &detail_target.lock().unwrap());
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

/// Hard-wrap each line of `text` to at most `width` characters, so the details
/// popup can scroll by whole lines.
fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.chars().count() <= width {
            out.push(line.to_string());
        } else {
            let chars: Vec<char> = line.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let end = (i + width).min(chars.len());
                out.push(chars[i..end].iter().collect());
                i = end;
            }
        }
    }
    out
}

/// Fill the details popup (title + wrapped source-stamp lines) for the currently
/// open target, resolving it against the freshest snapshot.
fn apply_details(win: &MainWindow, state: &AppState, target: &Option<(u8, String)>) {
    use slint::{ModelRc, SharedString, VecModel};

    let (title, text) = match target {
        Some((1, id)) => (
            "U-Boot details",
            state
                .uboot_builds
                .iter()
                .find(|b| &b.id == id)
                .map(|b| b.details_text())
                .unwrap_or_default(),
        ),
        Some((2, id)) => (
            "Snapshot details",
            state
                .snapshot_builds
                .iter()
                .find(|b| &b.id == id)
                .map(|b| b.details_text())
                .unwrap_or_default(),
        ),
        _ => ("", String::new()),
    };
    win.set_details_title(title.into());
    let lines: Vec<SharedString> = wrap_lines(&text, 40).into_iter().map(SharedString::from).collect();
    win.set_details_model(ModelRc::new(VecModel::from(lines)));
}

/// Push a state snapshot into the Slint properties.
fn apply(win: &MainWindow, state: &AppState) {
    use slint::{ModelRc, SharedString, VecModel};

    // Header: "Device type: <model> [<id>]". The model is the device-tree human
    // string; the bracketed id is the image-server device type we mapped it to.
    win.set_device_type_text(
        format!("Device type: {} [{}]", state.board.model, state.board.board_id).into(),
    );
    win.set_progress(state.progress);
    win.set_can_install(state.can_install());
    win.set_install_status_text(state.install_status_label().into());

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

    // Device rows: name = "<path> [<kind>] <model>" (leading "! " when the boot
    // ROM can't boot it); detail = size. Non-boot-capable devices are dimmed.
    let mut device_names: Vec<SharedString> = Vec::new();
    let mut device_details: Vec<SharedString> = Vec::new();
    let mut devices_dim: Vec<bool> = Vec::new();
    for d in &state.devices {
        let mark = if d.boot_rom_capable() { "" } else { "! " };
        let name = if d.model.is_empty() {
            format!("{mark}{} [{}]", d.path, d.kind.as_str())
        } else {
            format!("{mark}{} [{}] {}", d.path, d.kind.as_str(), d.model)
        };
        device_names.push(name.into());
        device_details.push(d.human_size().into());
        devices_dim.push(!d.boot_rom_capable());
    }
    win.set_devices(ModelRc::new(VecModel::from(device_names)));
    win.set_devices_detail(ModelRc::new(VecModel::from(device_details)));
    win.set_devices_dim(ModelRc::new(VecModel::from(devices_dim)));

    // Build rows: name = label; detail = timestamp; icon = source (server/media).
    let mut uboot_names: Vec<SharedString> = Vec::new();
    let mut uboot_details: Vec<SharedString> = Vec::new();
    let mut uboot_icons: Vec<i32> = Vec::new();
    for b in &state.uboot_builds {
        uboot_names.push(b.display_name().into());
        uboot_details.push(human_time(&b.mtime).into());
        uboot_icons.push(source_icon(&b.source));
    }
    win.set_uboots(ModelRc::new(VecModel::from(uboot_names)));
    win.set_uboots_detail(ModelRc::new(VecModel::from(uboot_details)));
    win.set_uboots_icon(ModelRc::new(VecModel::from(uboot_icons)));

    let mut snap_names: Vec<SharedString> = Vec::new();
    let mut snap_details: Vec<SharedString> = Vec::new();
    let mut snap_icons: Vec<i32> = Vec::new();
    for b in &state.snapshot_builds {
        snap_names.push(b.display_name().into());
        snap_details.push(human_time(&b.mtime).into());
        snap_icons.push(source_icon(&b.source));
    }
    win.set_snapshots(ModelRc::new(VecModel::from(snap_names)));
    win.set_snapshots_detail(ModelRc::new(VecModel::from(snap_details)));
    win.set_snapshots_icon(ModelRc::new(VecModel::from(snap_icons)));

    // Profiles of the selected build: name = profile (+ " (always)" for Minimal);
    // detail = pack size. Minimal is always deployed.
    let build = state.selected_build();
    let mut profile_names: Vec<SharedString> = Vec::new();
    let mut profile_details: Vec<SharedString> = Vec::new();
    let mut checked: Vec<bool> = Vec::new();
    if let Some(b) = build {
        for p in &b.profiles {
            let suffix = if p.is_minimal() { " (always)" } else { "" };
            profile_names.push(format!("{}{suffix}", p.name).into());
            let size = p
                .incremental
                .as_ref()
                .or(p.full.as_ref())
                .map(|pk| human_bytes(pk.size_bytes))
                .unwrap_or_else(|| "?".to_string());
            profile_details.push(size.into());
            checked.push(p.is_minimal() || state.selection.profiles.iter().any(|n| n == &p.name));
        }
    }
    win.set_profiles(ModelRc::new(VecModel::from(profile_names)));
    win.set_profiles_detail(ModelRc::new(VecModel::from(profile_details)));
    win.set_profile_checked(ModelRc::new(VecModel::from(checked)));

    // Current-selection values for the summary rows. `has_*` drives the grayed
    // "(select)" placeholder when nothing is chosen yet.
    let device_sel = state
        .target()
        .map(|d| format!("{} {}", d.path, d.human_size()));
    win.set_has_device(device_sel.is_some());
    win.set_device_sel_text(device_sel.unwrap_or_else(|| "(select)".to_string()).into());

    let uboot_sel = state.selected_uboot().map(|b| b.display_name());
    win.set_has_uboot(uboot_sel.is_some());
    win.set_uboot_sel_text(uboot_sel.unwrap_or_else(|| "(select)".to_string()).into());

    let snapshot_sel = state.selected_build().map(|b| b.display_name());
    win.set_has_snapshot(snapshot_sel.is_some());
    win.set_snapshot_sel_text(snapshot_sel.unwrap_or_else(|| "(select)".to_string()).into());

    let extra = state.selection.profiles.len();
    let profiles_sel = if extra > 0 {
        format!("Minimal +{extra}")
    } else {
        "Minimal".to_string()
    };
    win.set_profiles_sel_text(profiles_sel.into());
}
