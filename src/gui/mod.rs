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

slint::slint! {
    // HaxrCorp 4090 (FlipCTL): a compiled-in pixel font, vendored as a git
    // submodule under third_party/flipctl-fonts. Embedding it (the Slint compiler
    // pre-renders the glyphs into the binary) means the on-device GUI needs no
    // system fonts and no fontconfig at runtime. Path is relative to this file.
    //
    // Three fonts, matching Flipper's own role split: HaxrCorp 4090 for values,
    // status and titles; Busy9px for normal menu labels; Born2bSportyV2 (heavier)
    // for the selected/active label. All three are pixel fonts with a 16px native
    // size, so the UI is pinned to 16px to keep them crisp.
    import "../../third_party/flipctl-fonts/HaxrCorp4090-FlipCTL/HaxrCorp4090-FlipCTL.ttf";
    import "../../third_party/flipctl-fonts/Busy9px-FlipCTL/Busy9px-FlipCTL.ttf";
    import "../../third_party/flipctl-fonts/Born2bSportyV2-FlipCTL/Born2bSportyV2-FlipCTL.ttf";

    // Crisp pixel-art rounded frame, reproducing Flipper's `ResponsiveFrame` /
    // `MenuSelectorFrame`. The 1px outline's corners are radius-3 staircases
    // (pixel insets 2,1,0) built from single pixels, so nothing is antialiased
    // like Slint's built-in `border-radius`. Options:
    //   filled  — paint the (staircase) interior with `fill`.
    //   shadow  — 1px drop shadow on the bottom+right edges, the "raised" look
    //             used for the current selection.
    component OutlineFrame inherits Rectangle {
        in property <brush> stroke: #000000;
        in property <brush> fill: #ffffff;
        in property <bool> filled;
        in property <bool> shadow;
        // Right-edge "drill in" chevron (cf. the reference ComponentSelectorFrame):
        // a full-height grey tab with a centred white ">". Its right corners
        // follow the box's radius-3 staircase (left edge straight) so it fills the
        // box's right end without spilling past the rounded outline.
        in property <bool> chevron;
        in property <length> chevron-w: 9px;
        background: transparent;

        // --- staircase interior fill (drawn first, under the outline) ---
        if root.filled : Rectangle { x: 0px; y: 2px; width: parent.width; height: parent.height - 4px; background: root.fill; }
        if root.filled : Rectangle { x: 2px; y: 0px; width: parent.width - 4px; height: 1px; background: root.fill; }
        if root.filled : Rectangle { x: 1px; y: 1px; width: parent.width - 2px; height: 1px; background: root.fill; }
        if root.filled : Rectangle { x: 1px; y: parent.height - 2px; width: parent.width - 2px; height: 1px; background: root.fill; }
        if root.filled : Rectangle { x: 2px; y: parent.height - 1px; width: parent.width - 4px; height: 1px; background: root.fill; }

        // --- chevron tab (under the outline; the black edges + corners paint
        //     over its right side, clipping it to the radius-3 rounded corner) ---
        if root.chevron : Rectangle {
            x: parent.width - root.chevron-w;
            y: 0px;
            width: root.chevron-w;
            height: parent.height;
            background: transparent;
            // Grey fill: full height in the middle, right side stepped in by 3/2/1
            // px at the top and bottom rows to trace the corner; left edge straight.
            Rectangle { x: 0px; y: 0px;                 width: root.chevron-w - 3px; height: 1px; background: #666666; }
            Rectangle { x: 0px; y: 1px;                 width: root.chevron-w - 2px; height: 1px; background: #666666; }
            Rectangle { x: 0px; y: 2px;                 width: root.chevron-w - 1px; height: 1px; background: #666666; }
            Rectangle { x: 0px; y: 3px;                 width: root.chevron-w;       height: parent.height - 6px; background: #666666; }
            Rectangle { x: 0px; y: parent.height - 3px; width: root.chevron-w - 1px; height: 1px; background: #666666; }
            Rectangle { x: 0px; y: parent.height - 2px; width: root.chevron-w - 2px; height: 1px; background: #666666; }
            Rectangle { x: 0px; y: parent.height - 1px; width: root.chevron-w - 3px; height: 1px; background: #666666; }
            Text {
                text: ">";
                font-family: "HaxrCorp 4090";
                color: #ffffff;
                x: 0px;
                width: parent.width;
                height: parent.height;
                horizontal-alignment: center;
                vertical-alignment: center;
            }
        }

        // --- 1px outline edges (inset 3px at both ends to clear the corners) ---
        Rectangle { x: 3px; y: 0px; width: parent.width - 6px; height: 1px; background: root.stroke; }
        Rectangle { x: 3px; y: parent.height - 1px; width: parent.width - 6px; height: 1px; background: root.stroke; }
        Rectangle { x: 0px; y: 3px; width: 1px; height: parent.height - 6px; background: root.stroke; }
        Rectangle { x: parent.width - 1px; y: 3px; width: 1px; height: parent.height - 6px; background: root.stroke; }

        // --- radius-3 corner staircases (three 1px steps per corner) ---
        Rectangle { x: 0px; y: 2px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: 1px; y: 1px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: 2px; y: 0px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 3px; y: 0px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 2px; y: 1px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 1px; y: 2px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: 0px; y: parent.height - 3px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: 1px; y: parent.height - 2px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: 2px; y: parent.height - 1px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 3px; y: parent.height - 1px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 2px; y: parent.height - 2px; width: 1px; height: 1px; background: root.stroke; }
        Rectangle { x: parent.width - 1px; y: parent.height - 3px; width: 1px; height: 1px; background: root.stroke; }

        // --- optional drop shadow: 1px below + 1px right, plus two BR weight px ---
        if root.shadow : Rectangle { x: 3px; y: parent.height; width: parent.width - 5px; height: 1px; background: root.stroke; }
        if root.shadow : Rectangle { x: parent.width; y: 3px; width: 1px; height: parent.height - 5px; background: root.stroke; }
        if root.shadow : Rectangle { x: parent.width - 2px; y: parent.height - 1px; width: 1px; height: 1px; background: root.stroke; }
        if root.shadow : Rectangle { x: parent.width - 1px; y: parent.height - 2px; width: 1px; height: 1px; background: root.stroke; }
    }

    // The Flipper One panel is a black-on-white reflective LCD lit by an orange
    // backlight, and only 6-bit grayscale even though the KMS device advertises
    // ARGB. The whole UI is therefore monochrome: white is the lit background,
    // black is ink, and mid-gray (#8c8c8c) marks de-emphasised or disabled text
    // (it quantises cleanly on 6-bit gray). No saturated colours anywhere.

    // Thin outlined progress bar with a solid black fill. Square corners: the
    // software renderer antialiases rounded corners, which looks muddy on the
    // 1-bit-ish grayscale LCD, so every outline in this UI is a crisp rectangle.
    component ProgressBar inherits Rectangle {
        in property <float> progress;
        height: 6px;
        border-width: 1px;
        border-color: #000000;
        background: #ffffff;
        Rectangle {
            x: 1px;
            y: 1px;
            width: (parent.width - 2px) * max(0.0, min(1.0, root.progress));
            height: parent.height - 2px;
            background: #000000;
        }
    }

    // Vertical scrollbar: a light track with a black thumb sized to the visible
    // fraction of the list and positioned by the first visible row. Collapses to
    // nothing (and reclaims its width) when the whole list already fits.
    component ScrollBar inherits Rectangle {
        in property <int> total;
        in property <int> visible-count;
        in property <int> offset;
        property <bool> active: root.total > root.visible-count;
        width: root.active ? 3px : 0px;
        background: root.active ? #d0d0d0 : transparent;
        Rectangle {
            visible: root.active;
            x: 0px;
            width: parent.width;
            height: root.active ? parent.height * root.visible-count / root.total : 0px;
            y: root.active ? parent.height * root.offset / root.total : 0px;
            background: #000000;
        }
    }

    // Dark heading tab (FlipCTL popup style, e.g. "Visible networks"): a solid
    // black body with 2px-rounded top corners and a square bottom that merges into
    // the popup frame beneath it. White HaxrCorp caption; width tracks the text.
    component DarkTab inherits Rectangle {
        in property <string> caption;
        height: 16px;
        width: label.preferred-width + 12px;
        background: transparent;
        Rectangle { x: 0px; y: 2px; width: parent.width; height: parent.height - 2px; background: #000000; }
        Rectangle { x: 2px; y: 0px; width: parent.width - 4px; height: 1px; background: #000000; }
        Rectangle { x: 1px; y: 1px; width: parent.width - 2px; height: 1px; background: #000000; }
        label := Text {
            x: 6px;
            height: parent.height;
            text: root.caption;
            color: #ffffff;
            font-family: "HaxrCorp 4090";
            vertical-alignment: center;
        }
    }

    // One row inside a scrolling submenu list, in FlipCTL's compact info-list
    // style (Wi-Fi settings / visible networks): a single HaxrCorp 4090 label
    // with NO font swap on selection — selection is shown only by the raised
    // (shadowed) rounded frame — plus a 1px separator beneath. The bold
    // Born2bSportyV2 swap is a 20px-menu convention and clips at this height.
    component ListRow inherits Rectangle {
        in property <string> text;
        // Right-justified gray detail (build timestamp, device/profile size).
        in property <string> detail;
        // Trailing source icon: 0 none, 1 network (server), 2 sdcard (media).
        // The PNGs are the Flipper One's own FlipCTL icons, decoded from its
        // icons.js (first-party art) to stay pixel-exact at this size.
        in property <int> icon-kind;
        in property <bool> selected;
        in property <bool> dim;
        in property <bool> checkbox;
        in property <bool> checked;
        height: 13px;
        // 1px separator beneath the row, inset to the selection box width minus
        // its rounded corners (so it stops short of the frame's rounded parts,
        // like the reference popup's row dividers).
        Rectangle { x: 4px; y: parent.height - 1px; width: parent.width - 9px; height: 1px; background: #cccccc; }
        if root.selected : OutlineFrame {
            x: 1px;
            y: 0px;
            width: parent.width - 3px;
            height: parent.height - 2px;
            shadow: true;
        }
        HorizontalLayout {
            padding-left: 5px;
            padding-right: 5px;
            spacing: 3px;
            if root.checkbox : Text {
                text: root.checked ? "[x]" : "[ ]";
                font-family: "HaxrCorp 4090";
                color: root.dim ? #999999 : #000000;
                vertical-alignment: center;
                width: 18px;
            }
            // Name — takes the slack so the detail + icon sit flush right.
            Text {
                text: root.text;
                font-family: "HaxrCorp 4090";
                color: root.dim ? #999999 : #000000;
                vertical-alignment: center;
                horizontal-stretch: 1;
                overflow: elide;
            }
            if root.detail != "" : Text {
                text: root.detail;
                font-family: "HaxrCorp 4090";
                color: #767676;
                vertical-alignment: center;
                horizontal-stretch: 0;
            }
            if root.icon-kind == 1 : Image {
                source: @image-url("icons/network.png");
                width: 13px;
                height: 11px;
            }
            if root.icon-kind == 2 : Image {
                source: @image-url("icons/sdcard.png");
                width: 14px;
                height: 10px;
            }
        }
    }

    // One row on the summary screen, modelled on the recorder's MenuDropdownLine:
    // a left-aligned HaxrCorp 4090 caption (no font swap on selection) paired with
    // the current value in a right-anchored light-grey chip — FlipCTL's cue for a
    // configurable field. Drill-in rows (all but the Install action) also raise a
    // shadowed selection frame with the grey/white ">" chevron when selected; the
    // Install action instead shows a plain value with no chip and no chevron.
    component MenuRow inherits Rectangle {
        in property <string> label;
        in property <string> value;
        in property <bool> selected;
        in property <bool> dim-value;
        in property <bool> action;
        property <bool> chevron: root.selected && !root.action;
        height: 16px;

        // Caption — HaxrCorp 4090 throughout (matches the recorder's row titles).
        Text {
            x: 6px;
            height: parent.height;
            text: root.label;
            font-family: "HaxrCorp 4090";
            color: #000000;
            vertical-alignment: center;
        }

        // Configurable-field value in a #E5E5E5 chip, right-anchored (5px gutter).
        // Sized so the selection box (y0..parent.height-2) leaves an even 1px gap
        // above and below the chip.
        if !root.action : OutlineFrame {
            x: parent.width - 5px - self.width;
            y: 1px;
            width: 158px;
            height: parent.height - 4px;
            filled: true;
            fill: #e5e5e5;
            stroke: #e5e5e5;
            Text {
                x: 3px;
                width: parent.width - 6px;
                height: parent.height;
                text: root.value;
                font-family: "HaxrCorp 4090";
                color: root.dim-value ? #767676 : #000000;
                horizontal-alignment: center;
                vertical-alignment: center;
                overflow: elide;
            }
        }
        // Install (action) value — plain text, no chip.
        if root.action : Text {
            x: parent.width - 5px - self.width;
            height: parent.height;
            text: root.value;
            font-family: "HaxrCorp 4090";
            color: root.dim-value ? #999999 : #000000;
            vertical-alignment: center;
        }

        // Selection frame + chevron on top, so the chevron sits over the chip's
        // right end exactly as ComponentSelectorFrame does in the reference.
        if root.selected : OutlineFrame {
            x: 1px;
            y: 0px;
            width: parent.width - 3px;
            height: parent.height - 2px;
            shadow: true;
            chevron: root.chevron;
        }
    }

    // A bottom-bar soft button in Flipper's notched-tab style: a white body with
    // black outline whose top two corners are radius-3 pixel staircases and whose
    // bottom sits flush against the screen edge (square bottom corners). Mirrors
    // the physical action buttons; hidden when it has no caption.
    component SoftButton inherits Rectangle {
        in property <string> caption;
        in property <bool> primary;   // accepted for API compatibility (unused)
        in property <bool> enabled: true;
        property <brush> ink: root.enabled ? #000000 : #999999;
        visible: root.caption != "";
        // Fixed width — the tab is aligned to a physical button under the
        // screen, so callers pin both width and x to a slot. 48px matches the
        // reference bar's MAX_BTN_W and is the widest that lets five slots sit
        // flush across 256px without overlapping. Overridable per instance.
        width: 48px;
        height: 14px;

        // White body, top two rows inset to clear the rounded top corners.
        Rectangle { x: 0px; y: 2px; width: parent.width; height: parent.height - 2px; background: #ffffff; }
        Rectangle { x: 2px; y: 0px; width: parent.width - 4px; height: 1px; background: #ffffff; }
        Rectangle { x: 1px; y: 1px; width: parent.width - 2px; height: 1px; background: #ffffff; }

        // Outline: top edge (inset 3) and full-height sides. No bottom edge —
        // the tab sits flush against the screen bottom, so its lower edge is open.
        Rectangle { x: 3px; y: 0px; width: parent.width - 6px; height: 1px; background: root.ink; }
        Rectangle { x: 0px; y: 3px; width: 1px; height: parent.height - 3px; background: root.ink; }
        Rectangle { x: parent.width - 1px; y: 3px; width: 1px; height: parent.height - 3px; background: root.ink; }
        // Top-corner staircases (bottom corners left square, flush to the edge).
        Rectangle { x: 0px; y: 2px; width: 1px; height: 1px; background: root.ink; }
        Rectangle { x: 1px; y: 1px; width: 1px; height: 1px; background: root.ink; }
        Rectangle { x: 2px; y: 0px; width: 1px; height: 1px; background: root.ink; }
        Rectangle { x: parent.width - 3px; y: 0px; width: 1px; height: 1px; background: root.ink; }
        Rectangle { x: parent.width - 2px; y: 1px; width: 1px; height: 1px; background: root.ink; }
        Rectangle { x: parent.width - 1px; y: 2px; width: 1px; height: 1px; background: root.ink; }

        label := Text {
            x: 0px;
            width: parent.width;
            y: 0px;
            height: parent.height - 1px;
            text: root.caption;
            font-family: "HaxrCorp 4090";
            color: root.ink;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
    }

    export component MainWindow inherits Window {
        // The Flipper One panel is a fixed 256x144 DRM/KMS device.
        width: 256px;
        height: 144px;
        background: #ffffff;
        default-font-family: "HaxrCorp 4090";
        // HaxrCorp 4090 is a pixel font whose design pixel is 64 font units
        // (1024 units/em), so it is only crisp at an integer multiple of its
        // 16px native size. Rendering it smaller (e.g. 8px) antialiases every
        // glyph into an illegible blur, so we pin it to 16px.
        default-font-size: 16px;
        title: "FlipperOS Installer";

        // Row height for submenu lists and how many rows are visible at once.
        // Compact 13px rows, matching FlipCTL's info-packed screens (Wi-Fi
        // settings, saved/visible networks) rather than the 20px MenuLine used
        // for icon menus. More builds on screen at once, no per-row icons.
        property <length> row-h: 13px;
        property <int> visible-rows: 8;

        // Soft-button geometry. The five FlipCTL keys (Escape, View, Power,
        // Edit, Run) sit at fixed positions under the screen, so their on-screen
        // tabs are fixed-width and pinned to five evenly-spaced slots: slot 0
        // flush left, slot 4 flush right. `sb-x(i)` is the left edge of slot i.
        property <length> sb-w: 48px;
        pure function sb-x(slot: int) -> length {
            return slot * (root.width - root.sb-w) / 4;
        }

        // --- inputs from Rust (shared state) ---
        // "Device type: <model> [<id>]" for the header's second line.
        in property <string> device-type-text;
        // Idle summary line, e.g. "3 targets, 14 builds found".
        in property <string> info-text;
        // Latest activity-log line, shown under the progress bar while running.
        in property <string> status-text;
        in property <float> progress;
        in property <bool> can-install;
        // True once installation has started; gates the progress bar + log line
        // (which replace the idle info line).
        in property <bool> installing;
        // True while discovery or an install is running (Phase::is_busy). Hides
        // the Refresh soft button — re-scanning mid-run makes no sense.
        in property <bool> busy;

        // Full selectable lists, shown one section at a time in a submenu. Each
        // list carries the row NAME (left); parallel `-detail` arrays hold the
        // gray right-justified detail (size / timestamp), and the build lists add
        // an `-icon` array (0 none, 1 network, 2 sdcard) for the source glyph.
        in property <[string]> devices;
        in property <[string]> devices-detail;
        in property <[bool]> devices-dim;
        in property <[string]> uboots;
        in property <[string]> uboots-detail;
        in property <[int]> uboots-icon;
        in property <[string]> snapshots;
        in property <[string]> snapshots-detail;
        in property <[int]> snapshots-icon;
        in property <[string]> profiles;
        in property <[string]> profiles-detail;
        in property <[bool]> profile-checked;

        // Current selection, shown as the value on each summary row.
        in property <string> device-sel-text;
        in property <string> uboot-sel-text;
        in property <string> snapshot-sel-text;
        in property <string> profiles-sel-text;
        in property <bool> has-device;
        in property <bool> has-uboot;
        in property <bool> has-snapshot;

        // Details popup (screen 2): sourcestamps for the highlighted build,
        // pre-wrapped into display lines for line-based scrolling.
        in property <string> details-title;
        in property <[string]> details-model;

        // --- actions back to Rust ---
        callback select-device(int);
        callback select-uboot(int);
        callback select-snapshot(int);
        callback toggle-profile(int, bool);
        callback start-install();
        // Re-query the image server / removable media for builds (the Edit
        // soft button on the summary). Mirrors the TUI's Refresh action.
        callback refresh();
        // Open the details popup for a U-Boot / snapshot list entry (by index).
        callback show-uboot-details(int);
        callback show-snapshot-details(int);
        // Per-keypress trace (text, screen, menu-index, cursor) for input
        // debugging. Only wired by Rust when `--debug-keys` is set — otherwise it
        // is a no-op, so nothing hits stderr / the shared kernel console.
        callback trace-key(string, int, int, int);
        // Ask Rust to lazily fetch the manifest (for the build number shown in
        // the row) of the entries currently on screen: (section, first-row,
        // count). Fired when a build submenu opens and as it scrolls.
        callback ensure-loaded(int, int, int);

        // --- navigation state ---
        // screen: 0 = summary, 1 = submenu. Exposed as in-out so an off-device
        // screenshot harness can drive the view; on-device it is set by the keys.
        in-out property <int> screen: 0;
        // Highlighted summary row: 0 device, 1 u-boot, 2 snapshot, 3 profiles, 4 install.
        in-out property <int> menu-index: 0;
        // Active submenu section: 0 device, 1 u-boot, 2 snapshot, 3 profiles.
        in-out property <int> sub-section: 0;
        in-out property <int> cursor: 0;
        // Scroll offset (in rows) for the details popup on screen 2.
        in-out property <int> details-scroll: 0;

        function sub-count() -> int {
            if (root.sub-section == 0) { return root.devices.length; }
            if (root.sub-section == 1) { return root.uboots.length; }
            if (root.sub-section == 2) { return root.snapshots.length; }
            return root.profiles.length;
        }
        // First visible row, chosen so the cursor stays on screen.
        function sub-offset() -> int {
            return min(max(root.cursor - root.visible-rows + 1, 0),
                       max(root.sub-count() - root.visible-rows, 0));
        }
        function sub-title() -> string {
            if (root.sub-section == 0) { return "Target device"; }
            if (root.sub-section == 1) { return "U-Boot build"; }
            if (root.sub-section == 2) { return "Snapshot build"; }
            return "Profiles";
        }
        // --- submenu popup geometry (a modal over the dimmed summary) ---
        // Rows the popup shows at once: the whole list until it exceeds the cap.
        function popup-rows() -> int { return min(root.sub-count(), root.visible-rows); }
        // List-body frame height: visible rows + 2px top / 1px bottom inner pad.
        function popup-frame-h() -> length { return popup-rows() * root.row-h + 3px; }
        // Frame top y. The tab overhangs ~14px above the frame; centre the whole
        // tab+frame block in the area above the 14px soft-button bar.
        function popup-frame-y() -> length {
            return (root.height - 14px - 14px - popup-frame-h()) / 2 + 14px;
        }
        // Confirm the highlighted submenu item. Selecting a device/build closes
        // the submenu; toggling a profile stays put so several can be picked.
        function confirm-sub() {
            if (root.sub-section == 0) { root.select-device(root.cursor); root.screen = 0; }
            else if (root.sub-section == 1) { root.select-uboot(root.cursor); root.screen = 0; }
            else if (root.sub-section == 2) { root.select-snapshot(root.cursor); root.screen = 0; }
            else { root.toggle-profile(root.cursor, !root.profile-checked[root.cursor]); }
        }
        // Rows visible at once in the details popup (screen 2), at the 14px pitch.
        function details-visible() -> int { return 7; }

        forward-focus: fs;
        fs := FocusScope {
            width: 100%;
            height: 100%;
            key-pressed(event) => {
                // Trace every key the backend delivers, before any matching, so
                // an operator can tell "libinput delivered nothing" (no events at
                // all) apart from "delivered the wrong text". Routed to Rust so it
                // only prints under `--debug-keys`; unwired it is a no-op (no
                // stderr output to garble the TUI on the shared kernel console).
                root.trace-key(event.text, root.screen, root.menu-index, root.cursor);

                // Input model:
                //   D-pad Up/Down  — move the selection
                //   ENTER (centre) — confirm the current action
                //   BACKSPACE      — go back (dedicated back key)
                // The five soft action buttons keep their conventional meanings;
                // the two used here are RUN (B = execute/start) and EXIT
                // (Z = cancel/stop). Confirm and back live on ENTER/BACKSPACE, so
                // they never depend on a soft button. Escape/Left are kept as
                // conveniences for desktop testing against a vkms device.

                // Back / exit.
                if (event.text == Key.Backspace || event.text == Key.Escape
                    || event.text == "z" || event.text == "Z") {
                    if (root.screen == 2) { root.screen = 1; }
                    else if (root.screen == 1) { root.screen = 0; }
                    return accept;
                }

                // Details popup (screen 2): scroll only.
                if (root.screen == 2) {
                    if (event.text == Key.UpArrow) {
                        if (root.details-scroll > 0) { root.details-scroll -= 1; }
                        return accept;
                    }
                    if (event.text == Key.DownArrow) {
                        if (root.details-scroll < max(0, root.details-model.length - root.details-visible())) {
                            root.details-scroll += 1;
                        }
                        return accept;
                    }
                    return reject;
                }

                // RUN (B): the dedicated execute button. Starts the install from
                // the summary; confirms the current item inside a submenu.
                if (event.text == "b" || event.text == "B") {
                    if (root.screen == 0) { root.start-install(); }
                    else { root.confirm-sub(); }
                    return accept;
                }

                // VIEW (X): open the details popup for a U-Boot / snapshot entry.
                if (event.text == "x" || event.text == "X") {
                    if (root.screen == 1 && root.sub-section == 1) {
                        root.show-uboot-details(root.cursor);
                        root.details-scroll = 0;
                        root.screen = 2;
                    } else if (root.screen == 1 && root.sub-section == 2) {
                        root.show-snapshot-details(root.cursor);
                        root.details-scroll = 0;
                        root.screen = 2;
                    }
                    return accept;
                }

                // EDIT (V): refresh the source lists. Available on the summary
                // and inside the U-Boot / snapshot build lists (where it is most
                // useful — those are the lists it repopulates). Suppressed while
                // discovery / install is running, matching where the soft button
                // is shown.
                if ((event.text == "v" || event.text == "V") && !root.busy
                        && (root.screen == 0
                            || (root.screen == 1
                                && (root.sub-section == 1 || root.sub-section == 2)))) {
                    root.refresh();
                    return accept;
                }

                if (root.screen == 0) {
                    // Wrap-around at both ends, like the FlipCTL menus.
                    if (event.text == Key.UpArrow) {
                        root.menu-index = root.menu-index > 0 ? root.menu-index - 1 : 4;
                        return accept;
                    }
                    if (event.text == Key.DownArrow) {
                        root.menu-index = root.menu-index < 4 ? root.menu-index + 1 : 0;
                        return accept;
                    }
                    if (event.text == Key.Return) {
                        if (root.menu-index == 4) {
                            root.start-install();
                        } else {
                            root.sub-section = root.menu-index;
                            root.cursor = 0;
                            root.screen = 1;
                            // Fetch build numbers for the rows now on screen.
                            root.ensure-loaded(root.sub-section, 0, root.visible-rows);
                        }
                        return accept;
                    }
                    return reject;
                } else {
                    if (event.text == Key.LeftArrow) { root.screen = 0; return accept; }
                    // Wrap-around at both ends, like the FlipCTL menus. After each
                    // move, (re)fetch the build numbers of the now-visible rows.
                    if (event.text == Key.UpArrow) {
                        root.cursor = root.cursor > 0 ? root.cursor - 1 : root.sub-count() - 1;
                        root.ensure-loaded(root.sub-section, root.sub-offset(), root.visible-rows);
                        return accept;
                    }
                    if (event.text == Key.DownArrow) {
                        root.cursor = root.cursor < root.sub-count() - 1 ? root.cursor + 1 : 0;
                        root.ensure-loaded(root.sub-section, root.sub-offset(), root.visible-rows);
                        return accept;
                    }
                    if (event.text == Key.Return) {
                        root.confirm-sub();
                        return accept;
                    }
                    return reject;
                }
            }

            // ===== SUMMARY SCREEN =====
            // Rendered for the submenu (screen 1) too, so it shows (dimmed) behind
            // the submenu popup. Its own soft-button bar is gated to screen 0.
            if root.screen == 0 || root.screen == 1 : VerticalLayout {
                width: 100%;
                height: 100%;

                VerticalLayout {
                    vertical-stretch: 1;
                    padding: 3px;
                    padding-bottom: 0px;
                    spacing: 1px;

                Text {
                    text: "FlipperOS Installation";
                    color: #000000;
                }
                Text {
                    text: root.device-type-text;
                    color: #000000;
                    overflow: elide;
                }
                Rectangle { height: 1px; background: #000000; }

                MenuRow {
                    label: "Device";
                    value: root.device-sel-text;
                    dim-value: !root.has-device;
                    selected: root.menu-index == 0;
                }
                MenuRow {
                    label: "U-Boot";
                    value: root.uboot-sel-text;
                    dim-value: !root.has-uboot;
                    selected: root.menu-index == 1;
                }
                MenuRow {
                    label: "Snapshot";
                    value: root.snapshot-sel-text;
                    dim-value: !root.has-snapshot;
                    selected: root.menu-index == 2;
                }
                MenuRow {
                    label: "Profiles";
                    value: root.profiles-sel-text;
                    selected: root.menu-index == 3;
                }
                MenuRow {
                    label: "Install";
                    value: root.can-install ? "ready" : "incomplete";
                    dim-value: !root.can-install;
                    selected: root.menu-index == 4;
                    action: true;
                }

                Rectangle { vertical-stretch: 1; }

                // Idle: a one-line summary. Running: progress bar + latest log.
                if !root.installing : Text {
                    text: root.info-text;
                    color: #8c8c8c;
                    overflow: elide;
                }
                if root.installing : ProgressBar { progress: root.progress; }
                if root.installing : Text {
                    text: root.status-text;
                    color: #8c8c8c;
                    overflow: elide;
                }
                }

                // Soft-button bar: fixed-width tabs on physical-button slots.
                // Refresh = Edit (slot 3), Install = Run (slot 4). Refresh drops
                // out while busy (discovery/install), leaving its slot empty.
                // Hidden while a submenu popup is open (screen 1).
                if root.screen == 0 : Rectangle {
                    height: 14px;
                    if !root.busy : SoftButton {
                        x: root.sb-x(3); width: root.sb-w; caption: "Refresh";
                    }
                    SoftButton {
                        x: root.sb-x(4); width: root.sb-w;
                        caption: root.installing ? "" : "Install";
                        primary: true;
                        enabled: root.can-install;
                    }
                }
            }

            // ===== SUBMENU POPUP (screen 1) =====
            // A modal list floating over the dimmed summary, styled like the Wi-Fi
            // "Visible networks" popup: a dark heading tab, a white rounded body
            // frame sized to the visible rows, inset row dividers, and a scrollbar
            // in the right gutter. The summary renders behind (see its condition)
            // beneath a semi-transparent wash.
            if root.screen == 1 : Rectangle {
                width: 100%;
                height: 100%;

                // Semi-transparent wash dimming the summary behind the popup.
                Rectangle { background: #ffffffcc; }

                // White body frame, sized to the number of visible rows.
                OutlineFrame {
                    x: 4px;
                    y: root.popup-frame-y();
                    width: 248px;
                    height: root.popup-frame-h();
                    filled: true;
                    fill: #ffffff;
                    clip: true;

                    if root.sub-section == 0 : VerticalLayout {
                        x: 2px;
                        width: parent.width - 4px;
                        y: 2px - root.sub-offset() * root.row-h;
                        for name[idx] in root.devices : ListRow {
                            text: name;
                            detail: root.devices-detail[idx];
                            selected: idx == root.cursor;
                            dim: root.devices-dim[idx];
                        }
                    }
                    if root.sub-section == 1 : VerticalLayout {
                        x: 2px;
                        width: parent.width - 4px;
                        y: 2px - root.sub-offset() * root.row-h;
                        for name[idx] in root.uboots : ListRow {
                            text: name;
                            detail: root.uboots-detail[idx];
                            icon-kind: root.uboots-icon[idx];
                            selected: idx == root.cursor;
                        }
                    }
                    if root.sub-section == 2 : VerticalLayout {
                        x: 2px;
                        width: parent.width - 4px;
                        y: 2px - root.sub-offset() * root.row-h;
                        for name[idx] in root.snapshots : ListRow {
                            text: name;
                            detail: root.snapshots-detail[idx];
                            icon-kind: root.snapshots-icon[idx];
                            selected: idx == root.cursor;
                        }
                    }
                    if root.sub-section == 3 : VerticalLayout {
                        x: 2px;
                        width: parent.width - 4px;
                        y: 2px - root.sub-offset() * root.row-h;
                        for name[idx] in root.profiles : ListRow {
                            text: name;
                            detail: root.profiles-detail[idx];
                            selected: idx == root.cursor;
                            checkbox: true;
                            checked: root.profile-checked[idx];
                        }
                    }
                }

                // Scrollbar in the frame's right gutter (collapses when it fits).
                ScrollBar {
                    x: 4px + 248px - 5px;
                    y: root.popup-frame-y() + 2px;
                    height: root.popup-frame-h() - 4px;
                    total: root.sub-count();
                    visible-count: root.visible-rows;
                    offset: root.sub-offset();
                }

                // Dark heading tab, overhanging above the frame's top-left corner
                // (its bottom merges into the frame's top border, clear of row 1).
                DarkTab {
                    x: 6px;
                    y: root.popup-frame-y() - 14px;
                    caption: root.sub-title();
                }

                // Soft-button bar (kept for discoverability), over the wash.
                // Back = Escape (0), Details = View (1), Refresh = Edit (3),
                // Select/Toggle = Run (4).
                Rectangle {
                    x: 0px;
                    y: parent.height - 14px;
                    width: parent.width;
                    height: 14px;
                    SoftButton { x: root.sb-x(0); width: root.sb-w; caption: "Back"; }
                    if (root.sub-section == 1 || root.sub-section == 2) : SoftButton {
                        x: root.sb-x(1); width: root.sb-w; caption: "Details";
                    }
                    if (!root.busy && (root.sub-section == 1 || root.sub-section == 2)) : SoftButton {
                        x: root.sb-x(3); width: root.sb-w; caption: "Refresh";
                    }
                    SoftButton {
                        x: root.sb-x(4); width: root.sb-w;
                        caption: root.sub-section == 3 ? "Toggle" : "Select";
                        primary: true;
                    }
                }
            }

            // ===== DETAILS POPUP (screen 2) =====
            if root.screen == 2 : VerticalLayout {
                width: 100%;
                height: 100%;

                VerticalLayout {
                    vertical-stretch: 1;
                    padding: 3px;
                    padding-bottom: 0px;
                    spacing: 2px;

                    // Heading tab — a 16px header like the reference popup's
                    // TabHeader, carrying the build's detail title.
                    OutlineFrame {
                        height: 16px;
                        Text {
                            x: 6px;
                            y: 0px;
                            height: parent.height;
                            text: root.details-title;
                            font-family: "HaxrCorp 4090";
                            color: #000000;
                            vertical-alignment: center;
                            overflow: elide;
                        }
                    }

                    // Bordered, scrolling text panel. Roomier than before: 3px
                    // inner padding and a 14px line pitch (vs the old crammed
                    // 12px) so the sourcestamps read like the reference popup.
                    OutlineFrame {
                        clip: true;
                        vertical-stretch: 1;
                        VerticalLayout {
                            width: 100%;
                            padding: 3px;
                            y: -root.details-scroll * 14px;
                            for line[i] in root.details-model : Text {
                                height: 14px;
                                text: line;
                                font-family: "HaxrCorp 4090";
                                color: #000000;
                                vertical-alignment: center;
                                overflow: elide;
                            }
                        }
                    }
                }

                // Soft-button bar — Back on the Escape slot, matching the others.
                Rectangle {
                    height: 14px;
                    SoftButton { x: root.sb-x(0); width: root.sb-w; caption: "Back"; }
                }
            }
        }
    }
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
