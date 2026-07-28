//! The [`Controller`] owns the single source of truth ([`AppState`]) and
//! broadcasts immutable snapshots to every subscribed frontend.
//!
//! Both the TUI and the GUI register a subscriber that marshals the snapshot
//! into their own event loop, so a change made in one frontend (e.g. selecting a
//! device via the serial console) is immediately reflected in the other (the
//! on-device screen). All mutation goes through methods on `Controller`, which
//! lock the state, apply the change and then [`Controller::notify`] subscribers.

use std::sync::{Arc, Mutex};

use crate::core::model::*;
use crate::core::{board, catalog, install, removable};

/// Runtime configuration for the installer.
#[derive(Clone, Debug)]
pub struct Config {
    /// Base URL of the image server, e.g. `https://images.flipperos.example`.
    pub server_url: String,
    /// When true, destructive operations are logged but not executed.
    pub dry_run: bool,
    /// DRM/KMS device node for the on-device screen.
    pub kms_device: String,
    /// When true, the GUI logs each keypress to stderr (input debugging). Off by
    /// default so nothing garbles the TUI on a shared serial/kernel console.
    pub debug_keys: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "https://dl-linux-images.flipp.dev".to_string(),
            dry_run: true,
            kms_device: "/dev/dri/by-path/platform-2acf0000.spi-cs-0-card".to_string(),
            debug_keys: false,
        }
    }
}

/// A callback invoked with a fresh snapshot every time the state changes.
type Subscriber = Box<dyn Fn(AppState) + Send + 'static>;

pub struct Controller {
    state: Mutex<AppState>,
    subscribers: Mutex<Vec<Subscriber>>,
    config: Config,
}

impl Controller {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(AppState::default()),
            subscribers: Mutex::new(Vec::new()),
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Register a frontend subscriber. It is immediately called once with the
    /// current snapshot so the frontend can render its initial state.
    pub fn subscribe<F>(&self, f: F)
    where
        F: Fn(AppState) + Send + 'static,
    {
        let snapshot = self.snapshot();
        f(snapshot);
        self.subscribers.lock().unwrap().push(Box::new(f));
    }

    /// Cheap immutable copy of the current state.
    pub fn snapshot(&self) -> AppState {
        self.state.lock().unwrap().clone()
    }

    /// Broadcast the current state to every subscriber.
    fn notify(&self) {
        let snapshot = self.state.lock().unwrap().clone();
        for sub in self.subscribers.lock().unwrap().iter() {
            sub(snapshot.clone());
        }
    }

    /// Apply a mutation under the lock, then notify subscribers.
    fn update<F: FnOnce(&mut AppState)>(&self, f: F) {
        {
            let mut state = self.state.lock().unwrap();
            f(&mut state);
        }
        self.notify();
    }

    /// Append a line to the shared activity log.
    pub fn log(&self, line: impl Into<String>) {
        let line = line.into();
        log::info!("{line}");
        self.update(|s| {
            s.log.push(line);
            const MAX: usize = 500;
            if s.log.len() > MAX {
                let drop = s.log.len() - MAX;
                s.log.drain(0..drop);
            }
        });
    }

    pub fn set_progress(&self, progress: f32) {
        self.update(|s| s.progress = progress.clamp(0.0, 1.0));
    }

    pub fn set_phase(&self, phase: Phase) {
        self.update(|s| s.phase = phase);
    }

    // --- Discovery -------------------------------------------------------

    /// Probe the board and enumerate local storage. Safe to run on a worker
    /// thread; it only reads sysfs / device-tree.
    pub fn discover(&self) {
        self.set_phase(Phase::Discovering);
        self.log("discovering board identity…");
        let board = board::detect();
        self.log(format!(
            "board: {} ({}, id={})",
            board.model, board.soc, board.board_id
        ));
        self.update(|s| s.board = board);

        self.log("enumerating boot-ROM capable storage…");
        let devices = storage_list();
        for d in &devices {
            self.log(format!(
                "  found {} — boot-capable: {}",
                d.summary(),
                d.boot_rom_capable()
            ));
        }
        self.update(|s| {
            s.devices = devices;
            // Auto-select a single obvious target (first boot-capable, non-removable).
            if s.selection.target_device.is_none() {
                if let Some(dev) = s
                    .devices
                    .iter()
                    .find(|d| d.boot_rom_capable() && !d.removable)
                {
                    s.selection.target_device = Some(dev.path.clone());
                }
            }
        });
    }

    /// Query the image server and any removable-media mirror for U-Boot and
    /// snapshot builds, newest first.
    pub fn refresh_sources(&self) {
        let board_id = self.snapshot().board.board_id;
        const LIMIT: usize = 100;

        let server = catalog::Origin::Server {
            base: self.config.server_url.clone(),
        };

        // Pull the list of device types the server can install from the latest
        // U-Boot manifest, and choose the per-board U-Boot directory that matches
        // our detected device type (falling back to the generic build otherwise).
        let supported = catalog::supported_device_types(&server);
        if !supported.is_empty() {
            self.log(format!(
                "server supports device type(s): {}",
                supported.join(", ")
            ));
        }
        let board_dir: String = if supported.iter().any(|t| t == &board_id) {
            board_id.clone()
        } else {
            if !supported.is_empty() {
                self.log(format!(
                    "device type '{board_id}' not offered by server; using generic U-Boot"
                ));
            }
            catalog::board_dir(&board_id).to_string()
        };
        self.update(|s| s.supported_device_types = supported.clone());

        let mut origins: Vec<catalog::Origin> = vec![server];
        for (device, root) in removable::media_roots() {
            origins.push(catalog::Origin::Media { device, root });
        }

        self.log("querying image catalog…");
        let mut uboot_builds: Vec<UbootBuild> = Vec::new();
        let mut snapshot_builds: Vec<SnapshotBuild> = Vec::new();
        for origin in &origins {
            uboot_builds.extend(catalog::uboot_builds(origin, &board_dir, LIMIT));
            snapshot_builds.extend(catalog::snapshot_builds(origin, LIMIT));
        }
        // Newest first across all origins, then cap.
        uboot_builds.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        snapshot_builds.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        uboot_builds.truncate(LIMIT);
        snapshot_builds.truncate(LIMIT);

        self.log(format!(
            "catalog: {} u-boot build(s), {} snapshot build(s)",
            uboot_builds.len(),
            snapshot_builds.len()
        ));

        self.update(|s| {
            s.uboot_builds = uboot_builds;
            s.snapshot_builds = snapshot_builds;
            if s.selection.uboot.is_none() {
                if let Some(b) = s.uboot_builds.first() {
                    s.selection.uboot = Some(b.id.clone());
                }
            }
            if s.selection.snapshot_build.is_none() {
                if let Some(b) = s.snapshot_builds.first() {
                    s.selection.snapshot_build = Some(b.id.clone());
                }
            }
            if matches!(s.phase, Phase::Discovering) {
                s.phase = Phase::Ready;
            }
        });

        // Load the profiles of the auto-selected build so the UI can populate.
        if let Some(id) = self.snapshot().selection.snapshot_build.clone() {
            self.load_profiles(&id);
        }
    }

    /// Fetch and store the per-profile packs for a snapshot build.
    fn load_profiles(&self, id: &str) {
        let build = match self
            .snapshot()
            .snapshot_builds
            .iter()
            .find(|b| b.id == id)
            .cloned()
        {
            Some(b) => b,
            None => return,
        };
        if build.loaded {
            return;
        }
        self.log(format!("loading profiles for {}…", build.label));
        match catalog::load_profiles(&build) {
            Ok((number, profiles, home_pack)) => {
                self.log(format!("{} profile(s) available", profiles.len()));
                self.update(|s| {
                    if let Some(b) = s.snapshot_builds.iter_mut().find(|b| b.id == id) {
                        b.profiles = profiles;
                        b.build_number = number;
                        b.home_pack = home_pack;
                        b.loaded = true;
                        // The build number now surfaces via `display_name()`; no
                        // need to splice it into the label.
                    }
                });
            }
            Err(e) => self.log(format!("failed to load profiles: {e}")),
        }
    }

    /// Fetch the U-Boot build's manifest details for the popup. Blocking, so
    /// call it from a worker thread; a no-op if already loaded or unknown.
    pub fn load_uboot_details(&self, id: &str) {
        let loc = {
            let s = self.state.lock().unwrap();
            match s.uboot_builds.iter().find(|b| b.id == id) {
                Some(b) if b.details.is_none() => b.manifest_location.clone(),
                _ => return,
            }
        };
        match catalog::load_details(&loc) {
            Ok(d) => self.update(|s| {
                if let Some(b) = s.uboot_builds.iter_mut().find(|b| b.id == id) {
                    b.details = Some(d);
                }
            }),
            Err(e) => self.log(format!("u-boot details: {e}")),
        }
    }

    /// Fetch the snapshot build's manifest details for the popup. Blocking, so
    /// call it from a worker thread; a no-op if already loaded or unknown.
    pub fn load_snapshot_details(&self, id: &str) {
        let loc = {
            let s = self.state.lock().unwrap();
            match s.snapshot_builds.iter().find(|b| b.id == id) {
                Some(b) if b.details.is_none() => {
                    format!("{}manifest.json", b.base_location)
                }
                _ => return,
            }
        };
        match catalog::load_details(&loc) {
            Ok(d) => self.update(|s| {
                if let Some(b) = s.snapshot_builds.iter_mut().find(|b| b.id == id) {
                    b.details = Some(d);
                }
            }),
            Err(e) => self.log(format!("rootfs details: {e}")),
        }
    }

    // --- Selection actions (shared by both frontends) --------------------

    pub fn select_device(&self, path: &str) {
        self.update(|s| s.selection.target_device = Some(path.to_string()));
    }

    pub fn select_uboot(&self, id: &str) {
        self.update(|s| s.selection.uboot = Some(id.to_string()));
    }

    /// Select a snapshot build, resetting the extra-profile selection, and load
    /// its profiles in the background if not already loaded.
    pub fn select_snapshot_build(self: &Arc<Self>, id: &str) {
        self.update(|s| {
            s.selection.snapshot_build = Some(id.to_string());
            s.selection.profiles.clear();
        });
        let loaded = self
            .snapshot()
            .snapshot_builds
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.loaded)
            .unwrap_or(true);
        if loaded {
            return;
        }
        let this = Arc::clone(self);
        let id = id.to_string();
        std::thread::spawn(move || this.load_profiles(&id));
    }

    /// Toggle an extra profile on/off. Minimal is always deployed and cannot be
    /// toggled.
    pub fn toggle_profile(&self, name: &str, on: bool) {
        if name.eq_ignore_ascii_case("minimal") {
            return;
        }
        self.update(|s| {
            s.selection.profiles.retain(|p| p != name);
            if on {
                s.selection.profiles.push(name.to_string());
            }
        });
    }

    /// Select all extra profiles of the current build, or none.
    pub fn select_all_profiles(&self, on: bool) {
        self.update(|s| {
            let names: Vec<String> = if on {
                s.selected_build()
                    .map(|b| b.extra_profiles().map(|p| p.name.clone()).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            s.selection.profiles = names;
        });
    }

    // --- Installation ----------------------------------------------------

    /// Kick off the installation on a background thread. Progress and log
    /// updates are pushed to both frontends via the subscriber mechanism.
    pub fn start_install(self: &Arc<Self>) {
        {
            let state = self.state.lock().unwrap();
            if !state.can_install() {
                drop(state);
                self.log("cannot start install: incomplete selection");
                return;
            }
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            this.set_phase(Phase::Installing);
            this.set_progress(0.0);
            match install::run(&this) {
                Ok(()) => {
                    this.set_progress(1.0);
                    this.set_phase(Phase::Done);
                    this.log("installation complete");
                }
                Err(e) => {
                    this.log(format!("installation failed: {e}"));
                    this.set_phase(Phase::Failed(e.to_string()));
                }
            }
        });
    }
}

/// Enumerate local storage; kept as a free function so tests and the
/// [`Controller`] share one implementation.
fn storage_list() -> Vec<StorageDevice> {
    crate::core::storage::enumerate(crate::core::storage::MIN_TARGET_SIZE_BYTES)
}
