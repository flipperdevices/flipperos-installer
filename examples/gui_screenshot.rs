//! Off-device visual test for the Slint GUI.
//!
//! Renders the real [`MainWindow`] through Slint's software renderer — the same
//! renderer the device draws with — into a pixel buffer and writes it out as a
//! PNG. This lets us eyeball the 256x144 layout (summary screen and each
//! submenu, scrolled) without a Flipper One panel or a DRM device.
//!
//! It stands in for the on-device platform rather than using it: this one keeps
//! the frame in RGB so the PNGs are comparable by eye, where the device renders
//! straight to the panel's greyscale.
//!
//! Run with:
//!     cargo run --example gui_screenshot --features gui
//! PNGs are written to target/screenshots/.

use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, SharedString, VecModel};

use flipperos_installer::gui::{MainWindow, MenuEntry};

const W: usize = 256;
const H: usize = 144;
/// Integer upscale factor for the inspection copy (nearest-neighbour, so pixels
/// stay crisp). The native 256x144 image is always written too.
const SCALE: usize = 4;

/// A headless Slint platform whose only window is a software-rendered buffer.
struct ScreenshotPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for ScreenshotPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
}

/// One menu row. The tuple mirrors `core::menu::MenuItem`'s rendered fields:
/// `(text, detail, icon, marker, drill, dim, action)` where `icon` is 0 none /
/// 1 network / 2 sdcard and `marker` is 0 none / 1 unchecked / 2 checked /
/// 3 committed choice.
type Row<'a> = (&'a str, &'a str, i32, i32, bool, bool, bool);

fn rows(items: &[Row]) -> ModelRc<MenuEntry> {
    let v: Vec<MenuEntry> = items
        .iter()
        .map(|(text, detail, icon, marker, drill, dim, action)| MenuEntry {
            text: SharedString::from(*text),
            detail: SharedString::from(*detail),
            icon: *icon,
            marker: *marker,
            drill: *drill,
            dim: *dim,
            action: *action,
        })
        .collect();
    ModelRc::new(VecModel::from(v))
}

/// A plain drill-in row.
fn drill(text: &str, marker: i32) -> Row<'_> {
    (text, "", 0, marker, true, false, false)
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(ScreenshotPlatform {
        window: window.clone(),
    }))
    .expect("set_platform");

    let ui = MainWindow::new().expect("MainWindow::new");
    window.set_size(PhysicalSize::new(W as u32, H as u32));
    ui.show().expect("show");

    // --- representative data (long lists, so scrolling is exercised) ---
    ui.set_device_type_text("Device type: Flipper One [flipper-one]".into());
    ui.set_info_text("3 target(s), 8 bundle(s) found".into());
    ui.set_installing(false);
    ui.set_progress(0.0);
    ui.set_can_install(true);
    ui.set_status_text("verifying Minimal (full): 40%…".into());

    // The five summary rows, as core::menu::root_level builds them.
    let summary: [Row; 5] = [
        ("Source", "nightly #15", 0, 0, true, false, false),
        ("Device", "/dev/mmcblk0 29.7 GiB", 0, 0, true, false, false),
        ("Profiles", "Minimal +2", 0, 0, true, false, false),
        ("Fetch", "verify first", 0, 0, true, false, false),
        ("Install", "ready", 0, 0, false, false, true),
    ];
    ui.set_summary_items(rows(&summary));

    std::fs::create_dir_all("target/screenshots").expect("mkdir");

    // Summary screen, "Profiles" row highlighted.
    ui.set_screen(0);
    ui.set_menu_index(2);
    shoot(&window, "01-summary");

    // Summary screen, "Install" row highlighted.
    ui.set_menu_index(4);
    shoot(&window, "02-summary-install");

    // Summary during installation: progress bar + latest log line replace the
    // idle info line, and the Install soft button disappears.
    let installing_summary: [Row; 5] = [
        ("Source", "nightly #15", 0, 0, true, false, false),
        ("Device", "/dev/mmcblk0 29.7 GiB", 0, 0, true, false, false),
        ("Profiles", "Minimal +2", 0, 0, true, false, false),
        ("Fetch", "verify first", 0, 0, true, false, false),
        ("Install", "in progress", 0, 0, false, false, true),
    ];
    ui.set_summary_items(rows(&installing_summary));
    ui.set_installing(true);
    ui.set_busy(true); // Phase::Installing is busy → Refresh soft button hidden.
    ui.set_progress(0.42);
    ui.set_can_install(false);
    shoot(&window, "02b-summary-installing");

    // Summary after a successful install (Phase::Done): the progress bar stays
    // at 100%, Refresh is back, and the RUN slot now offers Reboot instead of
    // Install. `can_reboot` is what swaps that caption. Done is not busy and the
    // selection is still complete, so the Install row reads "ready" again.
    let done_summary: [Row; 5] = [
        ("Source", "nightly #15", 0, 0, true, false, false),
        ("Device", "/dev/mmcblk0 29.7 GiB", 0, 0, true, false, false),
        ("Profiles", "Minimal +2", 0, 0, true, false, false),
        ("Fetch", "verify first", 0, 0, true, false, false),
        ("Install", "ready", 0, 0, false, false, true),
    ];
    ui.set_summary_items(rows(&done_summary));
    ui.set_busy(false);
    ui.set_progress(1.0);
    ui.set_can_install(true);
    ui.set_can_reboot(true);
    ui.set_status_text("installation complete".into());
    shoot(&window, "02c-summary-done");
    ui.set_can_reboot(false);

    ui.set_summary_items(rows(&summary));
    ui.set_installing(false);
    ui.set_busy(false);
    ui.set_can_install(true);
    ui.set_progress(0.0);
    ui.set_status_text("verifying Minimal (full): 40%…".into());

    // The source picker: channels, local bundles, and the legacy custom flow.
    // The bullet marks where the current selection came from.
    ui.set_screen(1);
    ui.set_level_can_details(false);
    ui.set_level_can_refresh(true);
    ui.set_level_multi(false);
    ui.set_level_title("Source".into());
    let source: [Row; 6] = [
        ("Release", "", 1, 0, true, false, false),
        ("Release candidate", "", 1, 0, true, false, false),
        ("Nightly", "", 1, 3, true, false, false),
        ("Dev", "", 1, 0, true, false, false),
        ("Local bundle", "1", 2, 0, true, false, false),
        drill("Custom development build", 0),
    ];
    ui.set_level_items(rows(&source));
    ui.set_cursor(2);
    shoot(&window, "03-level-source");

    // A channel's builds: more than fit, so the scrollbar shows. The selected
    // build carries its number and a details popup.
    ui.set_level_title("Nightly".into());
    ui.set_level_can_details(true);
    let builds: [Row; 10] = [
        ("20260812-83ddb68-15", "#15", 1, 3, false, false, false),
        ("20260811-83ddb68-14", "", 1, 0, false, false, false),
        ("20260810-83ddb68-13", "", 1, 0, false, false, false),
        ("20260809-83ddb68-12", "", 1, 0, false, false, false),
        ("20260808-83ddb68-11", "", 1, 0, false, false, false),
        ("20260807-83ddb68-8", "", 1, 0, false, false, false),
        ("20260806-83ddb68-6", "", 1, 0, false, false, false),
        ("20260805-83ddb68-5", "", 1, 0, false, false, false),
        ("20260804-83ddb68-4", "", 1, 0, false, false, false),
        ("20260803-83ddb68-3", "", 1, 0, false, false, false),
    ];
    ui.set_level_items(rows(&builds));
    ui.set_cursor(9);
    shoot(&window, "04-level-builds-scrolled");

    // The profiles level: checkboxes, with Minimal pinned on and dimmed.
    ui.set_level_title("Profiles".into());
    ui.set_level_multi(true);
    ui.set_level_can_details(false);
    let profiles: [Row; 6] = [
        ("Minimal (always)", "844.2 MiB", 0, 2, false, true, false),
        ("(all extra profiles)", "", 0, 1, false, false, false),
        ("Desktop", "360.8 MiB", 0, 2, false, false, false),
        ("No-Graphics", "638 B", 0, 1, false, false, false),
        ("Router", "5.0 KiB", 0, 1, false, false, false),
        ("TV-Media-Box", "49.2 MiB", 0, 2, false, false, false),
    ];
    ui.set_level_items(rows(&profiles));
    ui.set_cursor(2);
    shoot(&window, "05-level-profiles");

    // A dev drill-down level, whose heading is the deepest path segment.
    ui.set_level_title("Alchark".into());
    ui.set_level_multi(false);
    let branches: [Row; 3] = [
        drill("update-bundles", 0),
        drill("ufs-boot-lu", 0),
        drill("wip/kernel-6.12", 0),
    ];
    ui.set_level_items(rows(&branches));
    ui.set_cursor(0);
    shoot(&window, "07-level-dev-branches");

    // The fetch-mode pick: two rows, so the popup frame is at its smallest.
    ui.set_level_title("Fetch mode".into());
    ui.set_level_can_refresh(false);
    let fetch: [Row; 2] = [
        ("Download & verify", "check before writing", 0, 3, false, false, false),
        ("Stream", "check while writing", 0, 0, false, false, false),
    ];
    ui.set_level_items(rows(&fetch));
    ui.set_cursor(0);
    shoot(&window, "08-level-fetch");

    // Details popup (screen 2): sourcestamps for a bundle.
    ui.set_details_title("Bundle details".into());
    let raw = "Build #692 (rootfs)\n  2026-07-16 09:42\n  https://linux-images.flipp.dev/#/builders/11/builds/692\n\nlinux-mainline\n  branch: flipper-devel\n  rev: 895107b0c84dd4f9af8791f15d66447fbe48743e\n  https://github.com/flipperdevices/flipper-linux-kernel.git\n\nbuildscripts\n  branch: dev\n  rev: ae979d1abf2a8f833e303185e28eb177e7008a0b";
    let lines: Vec<SharedString> = raw
        .split('\n')
        .flat_map(|l| {
            let c: Vec<char> = l.chars().collect();
            if c.len() <= 40 {
                vec![l.to_string()]
            } else {
                c.chunks(40).map(|ch| ch.iter().collect()).collect()
            }
        })
        .map(SharedString::from)
        .collect();
    ui.set_details_model(ModelRc::new(VecModel::from(lines)));
    ui.set_details_scroll(0);
    ui.set_screen(2);
    shoot(&window, "06-details");

    // The target-device level of a UFS board whose logical units do not match the
    // Flipper provisioning scheme: the last row offers to rewrite them.
    ui.set_level_title("Target device".into());
    ui.set_level_can_refresh(false);
    ui.set_level_can_details(true);
    let targets: [Row; 3] = [
        ("/dev/sda [ufs] BWUFS256", "237.9 GiB", 0, 3, false, false, false),
        ("! /dev/sdb [usb] SanDisk", "29.7 GiB", 2, 0, false, true, false),
        ("Reprovision UFS\u{2026}", "unprovisioned", 0, 0, false, false, false),
    ];
    ui.set_level_items(rows(&targets));
    ui.set_cursor(2);
    ui.set_screen(1);
    shoot(&window, "09-level-device-ufs");

    // The reprovisioning prompt (screen 3), with the wording
    // `provision::Status::prompt_lines` produces for a factory-provisioned part.
    // The caution has to be readable without scrolling on a 256x144 panel.
    ui.set_prompt_title("Reprovision UFS?".into());
    ui.set_prompt_confirm("Confirm".into());
    ui.set_prompt_cancel("Cancel".into());
    let prompt = "ALL DATA ON /dev/sda WILL BE PERMANENTLY LOST.\n\
                  \n\
                  Logical units, now and wanted:\n\
                  LU 0: 238.3 GiB \u{2192} 237.9 GiB\n\
                  LU 1: 4.0 MiB boot A \u{2192} 16.0 MiB boot A\n\
                  LU 2: 4.0 MiB boot B \u{2192} 16.0 MiB boot B\n\
                  LU 3: 8.0 MiB \u{2192} 128.0 MiB";
    ui.set_prompt_model(wrapped(prompt));
    ui.set_prompt_scroll(0);
    ui.set_screen(3);
    shoot(&window, "10-prompt-reprovision");

    // The same prompt for a factory-blank device, which has nothing to lose and so
    // must not be worded as though it had. Text as `prompt_lines` produces it on a
    // blank BIWIN 128 GB part.
    ui.set_prompt_title("Provision UFS?".into());
    let blank = "BIWIN BWU2A0526B128G has no logical units yet. \
                 Provisioning creates them:\n\
                 \n\
                 Logical units, now and wanted:\n\
                 LU 0: absent \u{2192} 118.7 GiB\n\
                 LU 1: absent \u{2192} 16.0 MiB boot A\n\
                 LU 2: absent \u{2192} 16.0 MiB boot B\n\
                 LU 3: absent \u{2192} 128.0 MiB";
    ui.set_prompt_model(wrapped(blank));
    ui.set_prompt_scroll(0);
    shoot(&window, "11-prompt-provision-blank");

    println!("wrote PNGs to target/screenshots/");
}

/// Wrap to the 40 columns the popups hold, through the very function the device
/// uses, so a screenshot cannot flatter the real layout.
fn wrapped(text: &str) -> ModelRc<SharedString> {
    let lines: Vec<SharedString> = flipperos_installer::gui::wrap_lines(text, 40)
        .into_iter()
        .map(SharedString::from)
        .collect();
    ModelRc::new(VecModel::from(lines))
}

fn shoot(window: &Rc<MinimalSoftwareWindow>, name: &str) {
    let mut buffer = vec![PremultipliedRgbaColor::default(); W * H];
    window.request_redraw();
    let drawn = window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, W);
    });
    assert!(drawn, "nothing was drawn for {name}");

    // Native 256x144.
    let native: Vec<u8> = buffer
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue])
        .collect();
    write_png(&format!("target/screenshots/{name}.png"), W, H, &native);

    // Nearest-neighbour upscale for legibility inspection.
    let out_w = W * SCALE;
    let out_h = H * SCALE;
    let mut big = Vec::with_capacity(out_w * out_h * 3);
    for y in 0..out_h {
        for x in 0..out_w {
            let p = buffer[(y / SCALE) * W + (x / SCALE)];
            big.push(p.red);
            big.push(p.green);
            big.push(p.blue);
        }
    }
    write_png(&format!("target/screenshots/{name}@{SCALE}x.png"), out_w, out_h, &big);
    println!("  {name}.png ({W}x{H}) + @{SCALE}x");
}

fn write_png(path: &str, w: usize, h: usize, rgb: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(rgb)
        .expect("png data");
}
