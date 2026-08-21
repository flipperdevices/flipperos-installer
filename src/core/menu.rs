//! The menu tree, built once here and rendered by both frontends.
//!
//! [`level`] maps a navigation position plus the current [`AppState`] to a
//! renderable [`Level`]: a title, a breadcrumb and a list of [`MenuItem`]s.
//! [`activate`] maps a row to the controller calls it implies plus a [`Move`] for
//! the frontend to apply to its own stack. Everything in between — labels,
//! selection markers, placeholder rows, which lazy fetch a row needs — lives
//! here rather than in the TUI and the GUI separately.
//!
//! The *position* ([`Nav`]) stays per-frontend, deliberately: the two frontends
//! run concurrently and share only [`Selection`], the TUI can show a level beside
//! the summary while the GUI cannot, and cursor movement must not take the state
//! lock or force a full snapshot broadcast on every keypress.

use std::sync::Arc;

use crate::core::model::*;
use crate::core::{bundle, provision, Controller};

/// Channels shown first, in this order, when the bucket lists them. Anything
/// else the bucket publishes follows, alphabetically.
const CHANNEL_ORDER: [&str; 4] = ["release", "testing", "nightly", bundle::DEV_CHANNEL];

/// Identifies one level of the menu tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuKey {
    /// The summary screen. Always the bottom of the stack.
    Root,
    /// Where to install from: channels, local bundles, or the custom flow.
    Source,
    /// Bundle builds at a channel path (`nightly`, `dev/alchark/topic`).
    Builds(String),
    /// An intermediate level of names (`dev` → users, `dev/alchark` → branches).
    Dirs(String),
    /// Bundles found on removable media or given on the command line.
    Local,
    /// The legacy free-form flow: pick a U-Boot build and a rootfs build.
    Custom,
    Uboot,
    Snapshot,
    Device,
    Profiles,
    Fetch,
}

/// What a row does when activated. Data rather than a closure, so a [`MenuItem`]
/// stays `Clone + Debug` and both frontends resolve a row identically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Open(MenuKey),
    PickDevice(String),
    PickUboot(String),
    PickSnapshot(String),
    PickBundle(String),
    PickFetch(FetchMode),
    ToggleProfile { name: String, on: bool },
    ToggleAllProfiles(bool),
    StartInstall,
    /// Offer to rewrite a UFS target's logical units to the Flipper scheme.
    ReprovisionUfs(provision::Target),
    /// Placeholder rows: `(loading…)`, `(none found)`, `(failed: …)`.
    Inert,
}

/// A blocking fetch a level needs before it can show anything real.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LoadRequest {
    Channels,
    Builds(String),
    Dirs(String),
    Local,
    /// Resolve the selected bundle's manifest.
    Bundle(String),
    /// A legacy U-Boot build's manifest (size, digest, details).
    UbootContents(String),
    /// A legacy rootfs build's profile packs.
    SnapshotProfiles(String),
}

/// What the details popup is showing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DetailsTarget {
    Uboot(String),
    Snapshot(String),
    /// The selected bundle, whose manifest is already loaded.
    Bundle(String),
    /// A UFS target's logical units and how they compare to the scheme.
    Ufs(String),
}

/// Left-hand state marker for a row.
///
/// Only multi-choice levels carry one. A single choice is shown by parking the
/// cursor on it when the level opens ([`Level::preferred_cursor`]) rather than by
/// decorating the row, which is how the rest of the UI has always done it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    None,
    Unchecked,
    Checked,
}

impl Marker {
    /// Encoding handed to the GUI, whose row widget takes an int.
    pub fn as_int(&self) -> i32 {
        match self {
            Marker::None => 0,
            Marker::Unchecked => 1,
            Marker::Checked => 2,
        }
    }
}

/// Which glyph the GUI shows for a row's origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    None,
    Network,
    SdCard,
}

impl Icon {
    pub fn as_int(&self) -> i32 {
        match self {
            Icon::None => 0,
            Icon::Network => 1,
            Icon::SdCard => 2,
        }
    }

    pub fn for_source(source: &Source) -> Self {
        match source {
            Source::Server => Icon::Network,
            Source::Removable { .. } | Source::Local { .. } => Icon::SdCard,
        }
    }
}

/// How a level behaves, which decides a frontend's confirm caption and whether
/// confirming leaves the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelKind {
    /// Rows open other levels or run an action.
    Menu,
    /// Rows are mutually exclusive choices; picking one returns.
    SinglePick,
    /// Rows are toggles; the level stays open.
    MultiPick,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    /// Left-hand label.
    pub text: String,
    /// Right-justified secondary text (a size, a timestamp, a value).
    pub detail: String,
    pub icon: Icon,
    pub marker: Marker,
    /// This level's committed choice. Not drawn: it is where the cursor is put
    /// when the level opens.
    pub selected: bool,
    /// Whether the row opens another level (drawn with a `›`).
    pub drill: bool,
    pub dim: bool,
    pub action: Action,
    /// A fetch to start while this row is on screen.
    pub on_focus: Option<LoadRequest>,
    /// Set when the row has a details popup.
    pub details: Option<DetailsTarget>,
}

impl MenuItem {
    fn plain(text: impl Into<String>, action: Action) -> Self {
        Self {
            text: text.into(),
            detail: String::new(),
            icon: Icon::None,
            marker: Marker::None,
            selected: false,
            drill: matches!(action, Action::Open(_)),
            dim: false,
            action,
            on_focus: None,
            details: None,
        }
    }

    /// An unselectable placeholder row.
    fn inert(text: impl Into<String>) -> Self {
        let mut item = Self::plain(text, Action::Inert);
        item.dim = true;
        item
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    fn with_marker(mut self, marker: Marker) -> Self {
        self.marker = marker;
        self
    }

    /// Mark this row as the level's committed choice, so the cursor opens on it.
    fn selected_if(mut self, yes: bool) -> Self {
        self.selected = yes;
        self
    }

    fn with_icon(mut self, icon: Icon) -> Self {
        self.icon = icon;
        self
    }
}

/// One renderable level.
#[derive(Clone, Debug)]
pub struct Level {
    pub key: MenuKey,
    /// Short caption for the GUI's heading tab and the TUI's panel title.
    pub title: String,
    /// Full path for the TUI, e.g. `Source › Development › alchark`.
    pub crumb: String,
    pub kind: LevelKind,
    pub items: Vec<MenuItem>,
    /// True while the level's contents are still being fetched.
    pub loading: bool,
    /// Contents this level needs; started by [`on_open`].
    pub load: Option<LoadRequest>,
    /// Row to put the cursor on when the level first materialises.
    pub preferred_cursor: Option<usize>,
    pub can_refresh: bool,
}

impl Level {
    /// Whether any row on this level has a details popup.
    pub fn can_details(&self) -> bool {
        self.items.iter().any(|i| i.details.is_some())
    }

    /// The details target of one row, if it has one.
    pub fn details_at(&self, index: usize) -> Option<DetailsTarget> {
        self.items.get(index).and_then(|i| i.details.clone())
    }
}

/// One frame of a frontend's navigation stack.
#[derive(Clone, Debug)]
pub struct Frame {
    pub key: MenuKey,
    /// Where the cursor was when this level was left.
    pub cursor: usize,
}

/// A frontend's navigation position. Never stored in [`AppState`].
#[derive(Clone, Debug)]
pub struct Nav {
    stack: Vec<Frame>,
}

impl Default for Nav {
    fn default() -> Self {
        Self::new()
    }
}

impl Nav {
    pub fn new() -> Self {
        Self {
            stack: vec![Frame {
                key: MenuKey::Root,
                cursor: 0,
            }],
        }
    }

    /// 0 while on the summary; greater once a level is open.
    pub fn depth(&self) -> usize {
        self.stack.len() - 1
    }

    /// The level currently being shown.
    pub fn key(&self) -> &MenuKey {
        // The stack always holds at least the root frame.
        &self.stack.last().expect("nav stack is never empty").key
    }

    pub fn cursor(&self) -> usize {
        self.stack.last().map(|f| f.cursor).unwrap_or(0)
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        if let Some(frame) = self.stack.last_mut() {
            frame.cursor = cursor;
        }
    }

    /// Open a level below the current one, remembering where the cursor was.
    pub fn push(&mut self, key: MenuKey) {
        self.stack.push(Frame { key, cursor: 0 });
    }

    /// Leave the current level. False when already on the summary.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }

    /// Return to the summary, e.g. when an install starts.
    pub fn reset(&mut self) {
        self.stack.truncate(1);
    }

    /// Keep the cursor inside a level whose contents just changed.
    pub fn clamp(&mut self, len: usize) {
        let max = len.saturating_sub(1);
        if self.cursor() > max {
            self.set_cursor(max);
        }
    }

    /// Apply the outcome of [`activate`].
    pub fn apply(&mut self, mv: Move) {
        match mv {
            Move::Stay => {}
            Move::Push(key) => self.push(key),
            Move::Pop => {
                self.pop();
            }
            Move::ToRoot => self.reset(),
        }
    }
}

/// What a frontend should do to its own [`Nav`] after activating a row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    Stay,
    Push(MenuKey),
    Pop,
    ToRoot,
}

// --- level construction ----------------------------------------------------

/// Build the level a navigation position points at.
pub fn level(nav: &Nav, state: &AppState) -> Level {
    build(nav.key(), state)
}

/// The five summary rows. Exposed separately because the GUI draws them behind
/// an open level.
pub fn root_level(state: &AppState) -> Level {
    build(&MenuKey::Root, state)
}

fn build(key: &MenuKey, state: &AppState) -> Level {
    match key {
        MenuKey::Root => root(state),
        MenuKey::Source => source(state),
        MenuKey::Builds(path) => builds(state, path),
        MenuKey::Dirs(path) => dirs(state, path),
        MenuKey::Local => local(state),
        MenuKey::Custom => custom(state),
        MenuKey::Uboot => uboot(state),
        MenuKey::Snapshot => snapshot(state),
        MenuKey::Device => device(state),
        MenuKey::Profiles => profiles(state),
        MenuKey::Fetch => fetch(state),
    }
}

fn level_of(key: MenuKey, title: &str, kind: LevelKind, items: Vec<MenuItem>) -> Level {
    let preferred_cursor = items.iter().position(|i| i.selected);
    Level {
        key,
        title: title.to_string(),
        crumb: title.to_string(),
        kind,
        items,
        loading: false,
        load: None,
        preferred_cursor,
        can_refresh: false,
    }
}

/// The summary screen.
///
/// Exactly five rows: the GUI's summary is already vertically full at five
/// 16 px rows plus its header and status lines, so a sixth would not fit. New
/// choices belong in a level, not here.
fn root(state: &AppState) -> Level {
    let device = state
        .target()
        .map(|d| format!("{} {}", d.path, d.human_size()));
    let profiles_value = match state.selection.profiles.len() {
        0 => "Minimal".to_string(),
        n => format!("Minimal +{n}"),
    };

    let mut items = vec![
        MenuItem::plain("Source", Action::Open(MenuKey::Source))
            .with_detail(state.source_label()),
        MenuItem::plain("Device", Action::Open(MenuKey::Device))
            .with_detail(device.clone().unwrap_or_else(|| "(select)".to_string())),
        MenuItem::plain("Profiles", Action::Open(MenuKey::Profiles))
            .with_detail(profiles_value),
        MenuItem::plain("Fetch", Action::Open(MenuKey::Fetch))
            .with_detail(state.selection.fetch.label()),
        MenuItem::plain("Install", Action::StartInstall)
            .with_detail(state.install_status_label()),
    ];
    // Dim a value that is still a placeholder, and the Install row until it can
    // actually run (but not while a run is in progress).
    items[0].dim = state.selected_build().is_none();
    items[1].dim = device.is_none();
    items[4].dim = !state.can_install() && !matches!(state.phase, Phase::Installing);

    debug_assert_eq!(items.len(), 5, "the summary screen fits exactly five rows");
    let mut level = level_of(MenuKey::Root, "FlipperOS Installation", LevelKind::Menu, items);
    level.can_refresh = true;
    level
}

/// Where to install from.
fn source(state: &AppState) -> Level {
    let mut items: Vec<MenuItem> = Vec::new();
    let selected_channel = state
        .bundle
        .as_ref()
        .map(|b| b.reference.channel.clone())
        .or_else(|| state.selected_bundle_ref().map(|r| r.channel.clone()));

    for name in ordered_channels(&state.catalog.channels.items) {
        let is_dev = name == bundle::DEV_CHANNEL;
        let key = if is_dev {
            MenuKey::Dirs(name.clone())
        } else {
            MenuKey::Builds(name.clone())
        };
        // A dev bundle's channel is the full `dev/<user>/<branch>` path, so the
        // channel row it came from is a prefix of it.
        let is_current = matches!(&selected_channel,
            Some(sel) if sel == &name || sel.starts_with(&format!("{name}/")));
        items.push(
            MenuItem::plain(title_case(&name), Action::Open(key))
                .selected_if(is_current)
                .with_icon(Icon::Network),
        );
    }

    let local_selected = state
        .selected_bundle_ref()
        .map(|r| r.channel.starts_with("local") || r.channel.starts_with("media "))
        .unwrap_or(false);
    items.push(
        MenuItem::plain("Local bundle", Action::Open(MenuKey::Local))
            .selected_if(local_selected)
            .with_icon(Icon::SdCard)
            .with_detail(match state.catalog.local.items.len() {
                0 => String::new(),
                n => n.to_string(),
            }),
    );
    items.push(
        MenuItem::plain("Custom development build", Action::Open(MenuKey::Custom))
            .selected_if(state.selection.mode == InstallMode::Custom),
    );

    let mut level = level_of(MenuKey::Source, "Source", LevelKind::Menu, items);
    level.load = Some(LoadRequest::Channels);
    level.can_refresh = true;
    // Channels come from the bucket, so this level can be empty while loading.
    if state.catalog.channels.items.is_empty() {
        if let Some(rows) = placeholder(&state.catalog.channels.state, "channels") {
            level.loading = state.catalog.channels.is_pending();
            // Keep the local and custom entries reachable even with no network.
            let tail: Vec<MenuItem> = level.items.drain(level.items.len() - 2..).collect();
            level.items = rows.into_iter().chain(tail).collect();
        }
    }
    level
}

/// Bundle builds at a channel path.
fn builds(state: &AppState, path: &str) -> Level {
    let listing = state.catalog.builds.get(path);
    let items = listing
        .map(|l| bundle_items(state, &l.items))
        .unwrap_or_default();
    let mut level = level_of(
        MenuKey::Builds(path.to_string()),
        &crumb_title(path),
        LevelKind::SinglePick,
        items,
    );
    level.crumb = format!("Source \u{203a} {}", crumb(path));
    level.load = Some(LoadRequest::Builds(path.to_string()));
    level.can_refresh = true;
    let state_of = listing.map(|l| l.state.clone()).unwrap_or_default();
    if level.items.is_empty() {
        if let Some(rows) = placeholder(&state_of, "builds") {
            level.items = rows;
        }
        level.loading = listing.map(|l| l.is_pending()).unwrap_or(true);
    }
    level
}

/// An intermediate level of the `dev/` tree.
fn dirs(state: &AppState, path: &str) -> Level {
    let listing = state.catalog.dirs.get(path);
    let items: Vec<MenuItem> = listing
        .map(|l| {
            l.items
                .iter()
                .map(|name| {
                    let child = format!("{path}/{name}");
                    let key = if bundle::is_dev_leaf(&child) {
                        MenuKey::Builds(child)
                    } else {
                        MenuKey::Dirs(child)
                    };
                    MenuItem::plain(name.clone(), Action::Open(key))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut level = level_of(
        MenuKey::Dirs(path.to_string()),
        &crumb_title(path),
        LevelKind::Menu,
        items,
    );
    level.crumb = format!("Source \u{203a} {}", crumb(path));
    level.load = Some(LoadRequest::Dirs(path.to_string()));
    level.can_refresh = true;
    let state_of = listing.map(|l| l.state.clone()).unwrap_or_default();
    if level.items.is_empty() {
        if let Some(rows) = placeholder(&state_of, "entries") {
            level.items = rows;
        }
        level.loading = listing.map(|l| l.is_pending()).unwrap_or(true);
    }
    level
}

/// Bundles found locally.
fn local(state: &AppState) -> Level {
    let items = bundle_items(state, &state.catalog.local.items);
    let mut level = level_of(MenuKey::Local, "Local bundle", LevelKind::SinglePick, items);
    level.crumb = "Source \u{203a} Local bundle".to_string();
    level.load = Some(LoadRequest::Local);
    level.can_refresh = true;
    if level.items.is_empty() {
        if let Some(rows) = placeholder(&state.catalog.local.state, "local bundles") {
            level.items = rows;
        }
        level.loading = state.catalog.local.is_pending();
    }
    level
}

fn bundle_items(state: &AppState, refs: &[BundleRef]) -> Vec<MenuItem> {
    let selected = state.selection.bundle.as_deref();
    refs.iter()
        .map(|r| {
            let is_selected = selected == Some(r.id.as_str());
            let mut item = MenuItem::plain(r.label(), Action::PickBundle(r.id.clone()))
                .with_icon(Icon::for_source(&r.source))
                .selected_if(is_selected);
            // Details need the manifest, and only the selected bundle's has been
            // read — so the popup is offered once a bundle is picked rather than
            // fetching a manifest per row while the operator scrolls.
            if is_selected && state.bundle.is_some() {
                item.details = Some(DetailsTarget::Bundle(r.id.clone()));
                item.detail = state
                    .bundle
                    .as_ref()
                    .and_then(|b| b.build.resolved_build_number())
                    .map(|n| format!("#{n}"))
                    .unwrap_or_default();
            } else if r.archive.is_some() {
                item.detail = "archive".to_string();
            }
            item
        })
        .collect()
}

/// The legacy free-form flow.
fn custom(state: &AppState) -> Level {
    let uboot = state
        .uboot_builds
        .iter()
        .find(|b| Some(b.id.as_str()) == state.selection.uboot.as_deref())
        .map(|b| b.display_name())
        .unwrap_or_else(|| "(select)".to_string());
    let snapshot = state
        .snapshot_builds
        .iter()
        .find(|b| Some(b.id.as_str()) == state.selection.snapshot_build.as_deref())
        .map(|b| b.display_name())
        .unwrap_or_else(|| "(select)".to_string());

    let items = vec![
        MenuItem::plain("U-Boot build", Action::Open(MenuKey::Uboot)).with_detail(uboot),
        MenuItem::plain("Snapshot build", Action::Open(MenuKey::Snapshot)).with_detail(snapshot),
    ];
    let mut level = level_of(
        MenuKey::Custom,
        "Custom development build",
        LevelKind::Menu,
        items,
    );
    level.crumb = "Source \u{203a} Custom development build".to_string();
    level
}

fn uboot(state: &AppState) -> Level {
    let items: Vec<MenuItem> = state
        .uboot_builds
        .iter()
        .map(|b| {
            let mut item = MenuItem::plain(b.display_name(), Action::PickUboot(b.id.clone()))
                .with_detail(human_time(&b.mtime))
                .with_icon(Icon::for_source(&b.source))
                .selected_if(Some(b.id.as_str()) == state.selection.uboot.as_deref());
            item.on_focus = Some(LoadRequest::UbootContents(b.id.clone()));
            item.details = Some(DetailsTarget::Uboot(b.id.clone()));
            item
        })
        .collect();
    let mut level = level_of(MenuKey::Uboot, "U-Boot build", LevelKind::SinglePick, items);
    level.crumb = "Source \u{203a} Custom \u{203a} U-Boot build".to_string();
    level.can_refresh = true;
    if level.items.is_empty() {
        level.items = vec![MenuItem::inert("(none found)")];
    }
    level
}

fn snapshot(state: &AppState) -> Level {
    let items: Vec<MenuItem> = state
        .snapshot_builds
        .iter()
        .map(|b| {
            let mut item = MenuItem::plain(b.display_name(), Action::PickSnapshot(b.id.clone()))
                .with_detail(human_time(&b.mtime))
                .with_icon(Icon::for_source(&b.source))
                .selected_if(Some(b.id.as_str()) == state.selection.snapshot_build.as_deref());
            item.on_focus = Some(LoadRequest::SnapshotProfiles(b.id.clone()));
            item.details = Some(DetailsTarget::Snapshot(b.id.clone()));
            item
        })
        .collect();
    let mut level = level_of(
        MenuKey::Snapshot,
        "Snapshot build",
        LevelKind::SinglePick,
        items,
    );
    level.crumb = "Source \u{203a} Custom \u{203a} Snapshot build".to_string();
    level.can_refresh = true;
    if level.items.is_empty() {
        level.items = vec![MenuItem::inert("(none found)")];
    }
    level
}

fn device(state: &AppState) -> Level {
    let selected = state.selection.target_device.as_deref();
    // The provisioning probe result, but only while it still describes the
    // selected target: it is replaced asynchronously when the target changes.
    // A blank device has no block node to be selected, so its status is always
    // relevant — offering it is the only way out of the chicken-and-egg where an
    // unprovisioned device presents no target to provision. Otherwise the status
    // is shown for the selected device only.
    let ufs = match &state.ufs {
        Some(status) if status.target.disk.is_none() => Some(status),
        Some(status) if status.target.disk.as_deref() == selected => Some(status),
        _ => None,
    };
    let mut items: Vec<MenuItem> = state
        .devices
        .iter()
        .map(|d| {
            // A device the boot ROM cannot boot from is flagged and dimmed, but
            // still selectable: it is a valid target for testing.
            let mark = if d.boot_rom_capable() { "" } else { "! " };
            let name = if d.model.is_empty() {
                format!("{mark}{} [{}]", d.path, d.kind.as_str())
            } else {
                format!("{mark}{} [{}] {}", d.path, d.kind.as_str(), d.model)
            };
            let mut item = MenuItem::plain(name, Action::PickDevice(d.path.clone()))
                .with_detail(d.human_size())
                .selected_if(Some(d.path.as_str()) == selected);
            item.dim = !d.boot_rom_capable();
            // A probe result exists for the selected target only, and offering a
            // details popup with nothing behind it would be a lie.
            if ufs.is_some() && Some(d.path.as_str()) == selected {
                item.details = Some(DetailsTarget::Ufs(d.path.clone()));
            }
            item
        })
        .collect();

    // A UFS target that does not match the scheme can be (re)provisioned from
    // here, which is also how the operator gets the offer back after dismissing
    // it. The row stays visible when the layout is fine, showing that it is. For a
    // blank device this is the only row it has, so it carries the device's name.
    if let Some(status) = ufs {
        let text = if status.is_blank() {
            format!("Provision UFS\u{2026} {}", status.target.name)
        } else {
            "Reprovision UFS\u{2026}".to_string()
        };
        let mut item = MenuItem::plain(text, Action::ReprovisionUfs(status.target.clone()))
            .with_detail(status.short_label());
        item.dim = status.is_provisioned();
        item.details = Some(DetailsTarget::Ufs(status.target.label()));
        items.push(item);
    }

    let mut level = level_of(MenuKey::Device, "Target device", LevelKind::SinglePick, items);
    if level.items.is_empty() {
        level.items = vec![MenuItem::inert("(no targets found)")];
    }
    level
}

fn profiles(state: &AppState) -> Level {
    let mut items: Vec<MenuItem> = Vec::new();
    match state.selected_build() {
        None => items.push(MenuItem::inert("(no build selected)")),
        Some(build) if !build.loaded => items.push(MenuItem::inert("(loading\u{2026})")),
        Some(build) => {
            if let Some(minimal) = build.minimal() {
                // Minimal is always deployed, so it is shown checked and inert.
                let mut item = MenuItem::plain(format!("{} (always)", minimal.name), Action::Inert)
                    .with_marker(Marker::Checked)
                    .with_detail(pack_size(minimal));
                item.dim = true;
                items.push(item);
            }
            let extras: Vec<&ProfilePack> = build.extra_profiles().collect();
            if !extras.is_empty() {
                let all_on = extras
                    .iter()
                    .all(|p| state.selection.profiles.iter().any(|n| n == &p.name));
                items.push(
                    MenuItem::plain(
                        "(all extra profiles)",
                        Action::ToggleAllProfiles(!all_on),
                    )
                    .with_marker(if all_on {
                        Marker::Checked
                    } else {
                        Marker::Unchecked
                    }),
                );
            }
            for p in extras {
                let on = state.selection.profiles.iter().any(|n| n == &p.name);
                items.push(
                    MenuItem::plain(
                        p.name.clone(),
                        Action::ToggleProfile {
                            name: p.name.clone(),
                            on: !on,
                        },
                    )
                    .with_marker(if on { Marker::Checked } else { Marker::Unchecked })
                    .with_detail(pack_size(p)),
                );
            }
        }
    }
    level_of(MenuKey::Profiles, "Profiles", LevelKind::MultiPick, items)
}

fn pack_size(p: &ProfilePack) -> String {
    p.incremental
        .as_ref()
        .or(p.full.as_ref())
        .map(|pk| human_bytes(pk.size_bytes))
        .unwrap_or_else(|| "?".to_string())
}

fn fetch(state: &AppState) -> Level {
    let current = state.selection.fetch;
    let items = vec![
        MenuItem::plain("Download & verify", Action::PickFetch(FetchMode::VerifyFirst))
            .with_detail("check before writing")
            .selected_if(current == FetchMode::VerifyFirst),
        MenuItem::plain("Stream", Action::PickFetch(FetchMode::Stream))
            .with_detail("check while writing")
            .selected_if(current == FetchMode::Stream),
    ];
    level_of(MenuKey::Fetch, "Fetch mode", LevelKind::SinglePick, items)
}

/// Rows standing in for a level whose contents are missing, or `None` when the
/// listing simply came back empty and the caller has something better to show.
fn placeholder(state: &LoadState, what: &str) -> Option<Vec<MenuItem>> {
    match state {
        LoadState::Idle | LoadState::Loading => Some(vec![MenuItem::inert("(loading\u{2026})")]),
        LoadState::Loaded => Some(vec![MenuItem::inert(format!("(no {what} found)"))]),
        LoadState::Failed(e) => Some(vec![MenuItem::inert(format!("(failed: {e})"))]),
    }
}

/// Channels in a stable, familiar order, with anything unexpected last.
fn ordered_channels(names: &[String]) -> Vec<String> {
    let mut known: Vec<String> = Vec::new();
    for wanted in CHANNEL_ORDER {
        if let Some(found) = names.iter().find(|n| n.as_str() == wanted) {
            known.push(found.clone());
        }
    }
    let mut rest: Vec<String> = names
        .iter()
        .filter(|n| !CHANNEL_ORDER.contains(&n.as_str()))
        .cloned()
        .collect();
    rest.sort();
    known.extend(rest);
    known
}

/// `dev/alchark/topic` → `Dev › alchark › topic`.
fn crumb(path: &str) -> String {
    path.trim_matches('/')
        .split('/')
        .map(title_case)
        .collect::<Vec<_>>()
        .join(" \u{203a} ")
}

/// The last segment of a path, for a heading tab that has to fit 256 px.
fn crumb_title(path: &str) -> String {
    title_case(path.trim_matches('/').rsplit('/').next().unwrap_or(path))
}

fn title_case(name: impl AsRef<str>) -> String {
    let name = name.as_ref();
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// --- activation ------------------------------------------------------------

/// Perform a row's action and report what the frontend should do to its stack.
pub fn activate(ctrl: &Arc<Controller>, level: &Level, index: usize) -> Move {
    let Some(item) = level.items.get(index) else {
        return Move::Stay;
    };
    match &item.action {
        Action::Inert => Move::Stay,
        Action::Open(key) => Move::Push(key.clone()),
        Action::PickDevice(path) => {
            ctrl.select_device(path);
            Move::Pop
        }
        Action::PickUboot(id) => {
            ctrl.select_uboot(id);
            Move::Pop
        }
        Action::PickSnapshot(id) => {
            ctrl.select_snapshot_build(id);
            Move::Pop
        }
        Action::PickBundle(id) => {
            ctrl.select_bundle(id);
            // Back to the summary: the bundle also settles the u-boot and rootfs
            // choices, so there is nothing left to pick in the levels above.
            Move::ToRoot
        }
        Action::PickFetch(mode) => {
            ctrl.set_fetch_mode(*mode);
            Move::Pop
        }
        Action::ToggleProfile { name, on } => {
            ctrl.toggle_profile(name, *on);
            Move::Stay
        }
        Action::ToggleAllProfiles(on) => {
            ctrl.select_all_profiles(*on);
            Move::Stay
        }
        Action::StartInstall => {
            ctrl.start_install();
            Move::ToRoot
        }
        Action::ReprovisionUfs(target) => {
            // Raises the confirmation prompt rather than doing anything: the
            // operator has to agree to what happens to the device.
            ctrl.request_reprovision(target.clone());
            Move::Stay
        }
    }
}

/// Start whatever fetch a freshly opened level needs.
pub fn on_open(ctrl: &Arc<Controller>, level: &Level) {
    if let Some(req) = &level.load {
        ctrl.ensure_loaded(req.clone());
    }
}

/// Start the fetch the focused row needs, if any.
pub fn on_focus(ctrl: &Arc<Controller>, level: &Level, index: usize) {
    if let Some(req) = level.items.get(index).and_then(|i| i.on_focus.clone()) {
        ctrl.ensure_loaded(req);
    }
}

/// Start the fetches for the rows currently on screen.
pub fn on_visible(ctrl: &Arc<Controller>, level: &Level, first: usize, count: usize) {
    for index in first..(first + count).min(level.items.len()) {
        on_focus(ctrl, level, index);
    }
}

/// Fingerprint of everything a level renders, for a frontend's rebuild gate.
///
/// Derived from the rendered rows rather than from the state they came from, so
/// a new list cannot be forgotten here and silently stop refreshing.
pub fn signature(level: &Level) -> String {
    let mut out = String::with_capacity(64 + level.items.len() * 24);
    out.push_str(&level.title);
    out.push('|');
    out.push_str(if level.loading { "loading" } else { "ready" });
    for i in &level.items {
        out.push('|');
        out.push_str(&i.text);
        out.push('\u{1}');
        out.push_str(&i.detail);
        out.push('\u{1}');
        // `selected` is not drawn, but it decides where the cursor opens, so a
        // level whose committed choice moved still counts as changed.
        out.push_str(&format!(
            "{}{}{}{}{}",
            i.marker.as_int(),
            i.icon.as_int(),
            i.drill as u8,
            i.dim as u8,
            i.selected as u8
        ));
    }
    out
}

/// The details text for a popup target.
pub fn details_text(state: &AppState, target: &DetailsTarget) -> (String, String) {
    match target {
        DetailsTarget::Uboot(id) => {
            let build = state.uboot_builds.iter().find(|b| &b.id == id);
            (
                "U-Boot details".to_string(),
                build
                    .map(|b| b.details_text())
                    .unwrap_or_else(|| "(gone)".to_string()),
            )
        }
        DetailsTarget::Snapshot(id) => {
            let build = state.snapshot_builds.iter().find(|b| &b.id == id);
            (
                "Snapshot details".to_string(),
                build
                    .map(|b| b.details_text())
                    .unwrap_or_else(|| "(gone)".to_string()),
            )
        }
        DetailsTarget::Bundle(_) => {
            let text = match &state.bundle {
                Some(b) => {
                    let mut out = format!("{} {}\n{}", b.channel, b.version, b.description);
                    if !b.device_types.is_empty() {
                        out.push_str(&format!("\nboards: {}", b.device_types.join(", ")));
                    }
                    out.push_str(&format!("\n\n{}", b.build.details_text()));
                    out
                }
                None => "(not loaded)".to_string(),
            };
            ("Bundle details".to_string(), text)
        }
        DetailsTarget::Ufs(label) => {
            let text = match &state.ufs {
                Some(status) if status.target.label() == *label => status.report().join("\n"),
                // Either the probe has not finished or the target moved on.
                _ => "(not probed)".to_string(),
            };
            ("UFS provisioning".to_string(), text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState {
            devices: vec![StorageDevice {
                path: "/dev/mmcblk0".into(),
                kind: StorageKind::Emmc,
                model: "eMMC".into(),
                size_bytes: 32 * 1024 * 1024 * 1024,
                removable: false,
                logical_block_size: 512,
            }],
            ..AppState::default()
        }
    }

    fn refs() -> Vec<BundleRef> {
        ["20260812-83ddb68-15", "20260811-83ddb68-14"]
            .iter()
            .map(|dir| BundleRef {
                id: format!("nightly/{dir}"),
                channel: "nightly".into(),
                dir: (*dir).into(),
                location: BundleLocation::Remote {
                    base: format!("https://u.invalid/bundles/nightly/{dir}/"),
                },
                source: Source::Server,
                archive: None,
            })
            .collect()
    }

    #[test]
    fn summary_has_exactly_five_rows() {
        let level = root_level(&state());
        assert_eq!(level.items.len(), 5);
        let labels: Vec<&str> = level.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(labels, ["Source", "Device", "Profiles", "Fetch", "Install"]);
        // The Install row is an action, not a drill-in.
        assert_eq!(level.items[4].action, Action::StartInstall);
        assert!(!level.items[4].drill);
        assert!(level.items[0].drill);
    }

    #[test]
    fn summary_shows_the_selected_values() {
        let mut s = state();
        s.selection.target_device = Some("/dev/mmcblk0".into());
        s.selection.profiles = vec!["Desktop".into(), "Router".into()];
        s.selection.fetch = FetchMode::Stream;
        let level = root_level(&s);
        assert_eq!(level.items[1].detail, "/dev/mmcblk0 32.0 GiB");
        assert!(!level.items[1].dim);
        assert_eq!(level.items[2].detail, "Minimal +2");
        assert_eq!(level.items[3].detail, "stream");
    }

    #[test]
    fn navigation_restores_the_parent_cursor() {
        let mut nav = Nav::new();
        assert_eq!(nav.depth(), 0);
        nav.set_cursor(3);
        nav.push(MenuKey::Source);
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.cursor(), 0);
        nav.set_cursor(2);
        nav.push(MenuKey::Builds("nightly".into()));
        nav.set_cursor(7);

        assert!(nav.pop());
        assert_eq!(nav.key(), &MenuKey::Source);
        assert_eq!(nav.cursor(), 2, "the parent's cursor is restored");
        assert!(nav.pop());
        assert_eq!(nav.cursor(), 3);
        assert_eq!(nav.depth(), 0);
        // Already at the root: nothing to pop.
        assert!(!nav.pop());
        assert_eq!(nav.key(), &MenuKey::Root);
    }

    #[test]
    fn clamp_keeps_the_cursor_inside_a_shrunken_level() {
        let mut nav = Nav::new();
        nav.push(MenuKey::Device);
        nav.set_cursor(9);
        nav.clamp(3);
        assert_eq!(nav.cursor(), 2);
        // An empty level parks the cursor on the placeholder row.
        nav.clamp(0);
        assert_eq!(nav.cursor(), 0);
    }

    #[test]
    fn a_pending_level_shows_one_inert_placeholder() {
        let s = state();
        let level = build(&MenuKey::Builds("nightly".into()), &s);
        assert!(level.loading);
        assert_eq!(level.items.len(), 1);
        assert_eq!(level.items[0].action, Action::Inert);
        assert!(level.items[0].text.contains("loading"));
        assert_eq!(level.load, Some(LoadRequest::Builds("nightly".into())));
    }

    #[test]
    fn a_failed_level_reports_why_and_is_not_loading() {
        let mut s = state();
        let mut catalog = CatalogCache::default();
        catalog.builds.insert(
            "nightly".into(),
            Listing {
                items: Vec::new(),
                state: LoadState::Failed("GET …: connection refused".into()),
            },
        );
        s.catalog = Arc::new(catalog);
        let level = build(&MenuKey::Builds("nightly".into()), &s);
        assert!(!level.loading, "a failure must not look like progress");
        assert!(level.items[0].text.contains("failed"), "{:?}", level.items[0]);
    }

    #[test]
    fn build_rows_mark_the_selection_and_offer_details_for_it() {
        let mut s = state();
        let mut catalog = CatalogCache::default();
        catalog.builds.insert(
            "nightly".into(),
            Listing {
                items: refs(),
                state: LoadState::Loaded,
            },
        );
        s.catalog = Arc::new(catalog);
        s.selection.bundle = Some("nightly/20260812-83ddb68-15".into());

        let level = build(&MenuKey::Builds("nightly".into()), &s);
        assert_eq!(level.items.len(), 2);
        // The committed choice is not decorated; it is where the cursor opens.
        assert!(level.items[0].selected);
        assert!(!level.items[1].selected);
        assert_eq!(level.items[0].marker, Marker::None);
        assert_eq!(level.preferred_cursor, Some(0));
        // No manifest loaded yet, so no details popup is offered.
        assert!(!level.can_details());
        assert_eq!(
            level.items[0].action,
            Action::PickBundle("nightly/20260812-83ddb68-15".into())
        );
        // Rows carry no per-row fetch: the directory name is the label.
        assert!(level.items.iter().all(|i| i.on_focus.is_none()));
    }

    #[test]
    fn dev_levels_drill_until_the_builds_level() {
        let mut s = state();
        let mut catalog = CatalogCache::default();
        catalog.dirs.insert(
            "dev".into(),
            Listing {
                items: vec!["alchark".into()],
                state: LoadState::Loaded,
            },
        );
        catalog.dirs.insert(
            "dev/alchark".into(),
            Listing {
                items: vec!["topic".into()],
                state: LoadState::Loaded,
            },
        );
        s.catalog = Arc::new(catalog);

        let users = build(&MenuKey::Dirs("dev".into()), &s);
        assert_eq!(
            users.items[0].action,
            Action::Open(MenuKey::Dirs("dev/alchark".into()))
        );
        let branches = build(&MenuKey::Dirs("dev/alchark".into()), &s);
        // One level deeper the children are builds, not more names.
        assert_eq!(
            branches.items[0].action,
            Action::Open(MenuKey::Builds("dev/alchark/topic".into()))
        );
        assert_eq!(branches.crumb, "Source \u{203a} Dev \u{203a} Alchark");
    }

    #[test]
    fn source_keeps_local_and_custom_reachable_with_no_network() {
        let mut s = state();
        s.catalog = Arc::new(CatalogCache {
            channels: Listing {
                items: Vec::new(),
                state: LoadState::Failed("offline".into()),
            },
            ..CatalogCache::default()
        });
        let level = build(&MenuKey::Source, &s);
        let labels: Vec<&str> = level.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(labels.len(), 3, "{labels:?}");
        assert!(labels[0].contains("failed"));
        assert_eq!(labels[1], "Local bundle");
        assert_eq!(labels[2], "Custom development build");
    }

    #[test]
    fn channels_are_ordered_familiarly() {
        let names = vec![
            "zzz".to_string(),
            "nightly".to_string(),
            "dev".to_string(),
            "release".to_string(),
        ];
        assert_eq!(
            ordered_channels(&names),
            ["release", "nightly", "dev", "zzz"]
        );
    }

    #[test]
    fn profiles_level_is_multi_pick_with_minimal_pinned() {
        let mut s = state();
        s.bundle = None;
        s.selection.mode = InstallMode::Custom;
        s.snapshot_builds = vec![SnapshotBuild {
            id: "b".into(),
            label: "b".into(),
            mtime: String::new(),
            source: Source::Server,
            base_location: "https://u.invalid/b/".into(),
            build_number: Some(1),
            profiles: vec![
                ProfilePack {
                    name: "Minimal".into(),
                    build: "9".into(),
                    full: Some(PackFile {
                        location: "m".into(),
                        source: Source::Server,
                        size_bytes: 1024,
                        sha256: None,
                    }),
                    incremental: None,
                },
                ProfilePack {
                    name: "Desktop".into(),
                    build: "9".into(),
                    full: None,
                    incremental: Some(PackFile {
                        location: "d".into(),
                        source: Source::Server,
                        size_bytes: 2048,
                        sha256: None,
                    }),
                },
            ],
            home_pack: None,
            loaded: true,
            details: None,
        }];
        s.selection.snapshot_build = Some("b".into());

        let level = build(&MenuKey::Profiles, &s);
        assert_eq!(level.kind, LevelKind::MultiPick);
        let labels: Vec<&str> = level.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(labels, ["Minimal (always)", "(all extra profiles)", "Desktop"]);
        // Minimal cannot be toggled off.
        assert_eq!(level.items[0].action, Action::Inert);
        assert_eq!(level.items[0].marker, Marker::Checked);
        assert_eq!(level.items[2].marker, Marker::Unchecked);
        assert_eq!(
            level.items[2].action,
            Action::ToggleProfile {
                name: "Desktop".into(),
                on: true
            }
        );
        assert_eq!(level.items[2].detail, "2.0 KiB");
    }

    #[test]
    fn fetch_level_opens_on_the_current_mode() {
        let level = build(&MenuKey::Fetch, &state());
        assert_eq!(level.items.len(), 2);
        // VerifyFirst is the default, and the cursor opens on it rather than the
        // row being marked.
        assert!(level.items[0].selected);
        assert_eq!(level.items[0].marker, Marker::None);
        assert_eq!(level.preferred_cursor, Some(0));
        assert_eq!(
            level.items[1].action,
            Action::PickFetch(FetchMode::Stream)
        );
    }

    #[test]
    fn signature_tracks_what_is_rendered() {
        let mut s = state();
        let before = signature(&build(&MenuKey::Device, &s));
        s.selection.target_device = Some("/dev/mmcblk0".into());
        let after = signature(&build(&MenuKey::Device, &s));
        assert_ne!(before, after, "a new selection marker must redraw");

        // A pending level and a failed one look different, so the placeholder
        // gets replaced when the fetch finishes.
        let pending = signature(&build(&MenuKey::Local, &s));
        s.catalog = Arc::new(CatalogCache {
            local: Listing {
                items: Vec::new(),
                state: LoadState::Failed("nope".into()),
            },
            ..CatalogCache::default()
        });
        assert_ne!(pending, signature(&build(&MenuKey::Local, &s)));
    }

    #[test]
    fn dimmed_device_rows_stay_selectable() {
        let mut s = state();
        s.devices.push(StorageDevice {
            path: "/dev/sda".into(),
            kind: StorageKind::Usb,
            model: "stick".into(),
            size_bytes: 1024,
            removable: true,
            logical_block_size: 512,
        });
        let level = build(&MenuKey::Device, &s);
        assert!(level.items[1].dim);
        assert!(level.items[1].text.starts_with("! "));
        assert_eq!(level.items[1].action, Action::PickDevice("/dev/sda".into()));
    }

    #[test]
    fn a_ufs_target_offers_its_provisioning() {
        let mut s = state();
        s.devices = vec![StorageDevice {
            path: "/dev/sda".into(),
            kind: StorageKind::Ufs,
            model: "BWUFS256".into(),
            size_bytes: 256 * 1000 * 1000 * 1000,
            removable: false,
            logical_block_size: 4096,
        }];
        s.selection.target_device = Some("/dev/sda".into());

        // No probe yet: just the device row, and no provisioning affordance to
        // offer since there is nothing to say about it.
        let level = build(&MenuKey::Device, &s);
        assert_eq!(level.items.len(), 1);
        assert!(level.items[0].details.is_none());

        // Once the probe lands, the selected UFS row gets a details popup and the
        // level gains the reprovisioning row, labelled with the verdict.
        let target = provision::Target::for_test("/dev/sda");
        s.ufs = Some(unprovisioned_status(target.clone()));
        let level = build(&MenuKey::Device, &s);
        assert_eq!(level.items.len(), 2);
        assert_eq!(
            level.items[0].details,
            Some(DetailsTarget::Ufs("/dev/sda".into()))
        );
        let row = &level.items[1];
        assert_eq!(row.action, Action::ReprovisionUfs(target.clone()));
        assert_eq!(row.detail, "unprovisioned");
        assert!(!row.dim, "a device needing work must not look inert");
        assert!(level.can_details());
        // The report reaches the popup rather than being hidden in the log.
        let (title, text) = details_text(&s, &DetailsTarget::Ufs("/dev/sda".into()));
        assert_eq!(title, "UFS provisioning");
        assert!(text.contains("/dev/sda"), "{text}");

        // A device that is already right keeps the row, dimmed, so the operator
        // can still see the verdict and open the report.
        let mut ok = unprovisioned_status(target);
        ok.mismatches.clear();
        s.ufs = Some(ok);
        let level = build(&MenuKey::Device, &s);
        assert_eq!(level.items[1].detail, "ok");
        assert!(level.items[1].dim);
    }

    #[test]
    fn a_blank_ufs_device_is_offered_with_no_targets_at_all() {
        // The chicken-and-egg this exists to break: a factory-blank UFS device has
        // no logical units, so no block device, so nothing in the target list — and
        // without a row to activate there would be no way to provision it.
        let mut s = state();
        s.devices.clear();
        s.selection.target_device = None;
        s.ufs = Some(unprovisioned_status(provision::Target::blank_for_test()));

        let level = build(&MenuKey::Device, &s);
        assert_eq!(level.items.len(), 1, "{:?}", level.items);
        let row = &level.items[0];
        assert!(
            row.text.starts_with("Provision UFS"),
            "a device with no logical units is provisioned, not reprovisioned: {:?}",
            row.text
        );
        // Named, so the operator can tell which device they are about to set up.
        assert!(row.text.contains("BIWIN BWU2A0526B128G"), "{:?}", row.text);
        assert_eq!(row.detail, "unprovisioned");
        assert!(!row.dim);
        assert!(matches!(row.action, Action::ReprovisionUfs(_)));
        // And no "(no targets found)" placeholder, which is what the operator used
        // to be left staring at.
        assert!(!row.text.contains("no targets"));
    }

    /// A UFS probe result standing in for a device that needs reprovisioning,
    /// built without touching hardware.
    fn unprovisioned_status(target: provision::Target) -> provision::Status {
        use crate::core::provision::{Mismatch, Plan};
        use crate::core::ufs::{ConfigDescriptor, DeviceDescriptor, GeometryDescriptor};

        // Minimal but real descriptors, so `Status` behaves as it would after a
        // probe: a 22/26-byte configuration layout and 4 MiB allocation units.
        let mut device_desc = vec![0u8; 0x59];
        device_desc[0x00] = 0x59;
        device_desc[0x1A] = 0x16;
        device_desc[0x1B] = 0x1A;
        let dev = DeviceDescriptor::parse(&device_desc).expect("device descriptor");
        let mut geometry_desc = vec![0u8; 0x57];
        geometry_desc[0x00] = 0x57;
        geometry_desc[0x01] = 0x07;
        geometry_desc[0x0F] = 0x20; // dSegmentSize = 8192 sectors
        geometry_desc[0x11] = 0x01; // one segment per allocation unit
        let geometry = GeometryDescriptor::parse(&geometry_desc).expect("geometry descriptor");

        provision::Status {
            target,
            scheme_origin: "built-in default".to_string(),
            current: ConfigDescriptor::empty(&dev).lus(),
            plan: Plan {
                descriptor: ConfigDescriptor::empty(&dev),
                clear: Vec::new(),
                boot_lun_en: 1,
                summary: Vec::new(),
            },
            boot_enable: 0,
            boot_lun_en: 0,
            config_locked: false,
            power_on_wp: false,
            permanent_wp: false,
            geometry,
            mismatches: vec![Mismatch {
                what: "LU 1 is absent, want it enabled".to_string(),
                critical: true,
                descriptor_write: true,
            }],
        }
    }
}
