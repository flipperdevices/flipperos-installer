//! Off-device visual test for the Slint GUI.
//!
//! Renders the real [`MainWindow`] through Slint's software renderer — the same
//! renderer the on-device LinuxKMS backend uses — into a pixel buffer and writes
//! it out as a PNG. This lets us eyeball the 256x144 layout (summary screen and
//! each submenu, scrolled) without a Flipper One panel or a DRM device.
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

use flipperos_installer::gui::MainWindow;

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

fn strs(items: &[&str]) -> ModelRc<SharedString> {
    let v: Vec<SharedString> = items.iter().map(|s| SharedString::from(*s)).collect();
    ModelRc::new(VecModel::from(v))
}

fn bools(items: &[bool]) -> ModelRc<bool> {
    ModelRc::new(VecModel::from(items.to_vec()))
}

fn ints(items: &[i32]) -> ModelRc<i32> {
    ModelRc::new(VecModel::from(items.to_vec()))
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
    ui.set_info_text("3 target(s), 14 build(s) found".into());
    ui.set_installing(false);
    ui.set_progress(0.0);
    ui.set_can_install(true);
    ui.set_install_status_text("ready".into());
    ui.set_status_text("unpacking Minimal_stock…".into());

    // Split into name (left) + detail (right, gray); builds also get a source
    // icon (1 = network/server, 2 = sdcard/media), mirroring the real apply().
    let devices = [
        "/dev/mmcblk0 [eMMC] SDINDDH4-32G",
        "/dev/mmcblk1 [SD] SC64G",
        "! /dev/sda [USB] SanDisk Ultra",
        "! /dev/sdb [USB] Kingston DT",
    ];
    let devices_detail = ["29.7 GiB", "59.5 GiB", "14.9 GiB", "3.8 GiB"];
    let devices_dim = [false, false, true, true];
    ui.set_devices(strs(&devices));
    ui.set_devices_detail(strs(&devices_detail));
    ui.set_devices_dim(bools(&devices_dim));

    // Identifiers are the manifest build number once fetched; the last row shows
    // the label fallback (media build whose manifest hasn't been fetched).
    let uboots = ["#512", "#489", "#455", "2023.10-rk3576"];
    let uboots_detail = ["2024-10-02", "2024-07-15", "2024-04-11", "2023-10-30"];
    let uboots_icon = [1, 1, 1, 2]; // last one from removable media
    ui.set_uboots(strs(&uboots));
    ui.set_uboots_detail(strs(&uboots_detail));
    ui.set_uboots_icon(ints(&uboots_icon));

    let snapshots = [
        "#704", "#703", "#698", "#695", "#690",
        "#684", "#679", "#672", "nightly-7850", "stable-7800",
    ];
    let snapshots_detail = [
        "2024-10-05 08:43", "2024-10-05 06:11", "2024-10-01 22:07", "2024-09-20 14:30",
        "2024-09-18 09:52", "2024-09-12 03:18", "2024-09-01 17:44", "2024-08-28 11:05",
        "2024-08-22 20:39", "2024-08-10 08:00",
    ];
    let snapshots_icon = [1, 1, 1, 1, 1, 1, 1, 1, 2, 2]; // last two from media
    ui.set_snapshots(strs(&snapshots));
    ui.set_snapshots_detail(strs(&snapshots_detail));
    ui.set_snapshots_icon(ints(&snapshots_icon));

    let profiles = ["Minimal (always)", "Desktop", "Developer", "Gaming", "Media"];
    let profiles_detail = ["128.4 MiB", "642.1 MiB", "311.0 MiB", "1.2 GiB", "498.7 MiB"];
    let profile_checked = [true, true, false, true, false];
    ui.set_profiles(strs(&profiles));
    ui.set_profiles_detail(strs(&profiles_detail));
    ui.set_profile_checked(bools(&profile_checked));

    // Summary-row values.
    ui.set_device_sel_text("/dev/mmcblk0 29.7 GiB".into());
    ui.set_has_device(true);
    ui.set_uboot_sel_text("2024.10-rk3576".into());
    ui.set_has_uboot(true);
    ui.set_snapshot_sel_text("nightly-8123".into());
    ui.set_has_snapshot(true);
    ui.set_profiles_sel_text("Minimal +2".into());

    std::fs::create_dir_all("target/screenshots").expect("mkdir");

    // Summary screen, "Snapshot" row highlighted.
    ui.set_screen(0);
    ui.set_menu_index(2);
    shoot(&window, "01-summary");

    // Summary screen, "Install" row highlighted.
    ui.set_menu_index(4);
    shoot(&window, "02-summary-install");

    // Summary during installation: progress bar + latest log line replace the
    // idle info line, and the Install soft button disappears.
    ui.set_installing(true);
    ui.set_busy(true); // Phase::Installing is busy → Refresh soft button hidden.
    ui.set_progress(0.42);
    ui.set_can_install(false);
    ui.set_install_status_text("in progress".into());
    shoot(&window, "02b-summary-installing");
    ui.set_installing(false);
    ui.set_busy(false);
    ui.set_install_status_text("ready".into());
    ui.set_can_install(true);
    ui.set_progress(0.0);

    // Device submenu (short list, one greyed non-boot-capable entry).
    ui.set_screen(1);
    ui.set_sub_section(0);
    ui.set_cursor(0);
    shoot(&window, "03-submenu-device");

    // Snapshot submenu scrolled down, so the scrollbar/scroll is visible.
    ui.set_sub_section(2);
    ui.set_cursor(9);
    shoot(&window, "04-submenu-snapshot-scrolled");

    // Profiles submenu (checkboxes; Minimal is always on).
    ui.set_sub_section(3);
    ui.set_cursor(1);
    shoot(&window, "05-submenu-profiles");

    // Details popup (screen 2): sourcestamps for a snapshot build.
    ui.set_sub_section(2);
    ui.set_details_title("Snapshot details".into());
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

    println!("wrote PNGs to target/screenshots/");
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
