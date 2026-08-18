//! The [`Controller`] owns the single source of truth ([`AppState`]) and
//! broadcasts immutable snapshots to every subscribed frontend.
//!
//! Both the TUI and the GUI register a subscriber that marshals the snapshot
//! into their own event loop, so a change made in one frontend (e.g. selecting a
//! device via the serial console) is immediately reflected in the other (the
//! on-device screen). All mutation goes through methods on `Controller`, which
//! lock the state, apply the change and then [`Controller::notify`] subscribers.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::core::menu::LoadRequest;
use crate::core::model::*;
use crate::core::{board, bundle, catalog, install, provision, removable};

/// Runtime configuration for the installer.
#[derive(Clone, Debug)]
pub struct Config {
    /// Base URL of the image server holding the two-level U-Boot / rootfs
    /// catalog. Only used for *custom development build* installs.
    pub server_url: String,
    /// Bucket the update bundles are published in. Its listing API is what makes
    /// the channel / build hierarchy browsable.
    pub bundle_bucket: String,
    /// Listing endpoint. Derived from [`Self::bundle_bucket`] unless overridden.
    pub bundle_list_url: Option<String>,
    /// Base URL bundle objects are served from. The public host serves objects
    /// but cannot list directories, hence the separate listing endpoint.
    pub bundle_base_url: String,
    /// Prefix inside the bucket that holds the channels.
    pub bundle_prefix: String,
    /// Explicit local bundles: an unpacked directory or a `*.tar.zst`.
    pub bundle_paths: Vec<String>,
    /// Scratch directory for verified artifacts and unpacked archives.
    pub cache_dir: String,
    /// Keep the scratch files after the run instead of deleting them.
    pub keep_cache: bool,
    /// Mount removable media read-only during discovery and scan it for bundles.
    pub automount: bool,
    /// Which kind of source the installer starts on.
    pub mode: InstallMode,
    /// Whether artifacts are verified before the target is touched.
    pub fetch: FetchMode,
    /// When true, destructive operations are logged but not executed.
    pub dry_run: bool,
    /// Check a UFS target's logical units against the Flipper provisioning
    /// scheme when it is selected, and offer to reprovision on a mismatch.
    pub ufs_check: bool,
    /// Read the provisioning scheme from this file instead of the compiled-in
    /// one. Only useful for trying a scheme out without a rebuild.
    pub ufs_scheme: Option<String>,
    /// Reprovision a mismatched UFS target without asking. For unattended and
    /// factory runs, where there is nobody to answer the prompt.
    pub ufs_reprovision: bool,
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
            bundle_bucket: "flipper-dev-update-server-euw1-dev".to_string(),
            bundle_list_url: None,
            bundle_base_url: "https://update.flipper.net.flipp.dev".to_string(),
            bundle_prefix: "bundles".to_string(),
            bundle_paths: Vec::new(),
            cache_dir: "/run/flipperos-cache".to_string(),
            keep_cache: false,
            automount: true,
            mode: InstallMode::default(),
            fetch: FetchMode::default(),
            dry_run: true,
            ufs_check: true,
            ufs_scheme: None,
            ufs_reprovision: false,
            kms_device: "/dev/dri/by-path/platform-2acf0000.spi-cs-0-card".to_string(),
            debug_keys: false,
        }
    }
}

impl Config {
    /// The bundle repository these settings describe.
    pub fn repo(&self) -> bundle::Repo {
        bundle::Repo {
            list_url: self.bundle_list_url.clone().unwrap_or_else(|| {
                format!(
                    "https://storage.googleapis.com/storage/v1/b/{}/o",
                    self.bundle_bucket
                )
            }),
            base_url: self.bundle_base_url.trim_end_matches('/').to_string(),
            prefix: self.bundle_prefix.trim_matches('/').to_string(),
        }
    }
}

/// Whether a UFS provisioning check may raise the destructive reprovisioning
/// prompt, or should only report what it found.
///
/// The distinction is who chose the device. A target the installer picked for the
/// operator at startup gets reported; one the operator picked gets the offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Prompt,
    ReportOnly,
}

/// A callback invoked with a fresh snapshot every time the state changes.
type Subscriber = Box<dyn Fn(AppState) + Send + 'static>;

/// A callback that quits one frontend's event loop. Registered by each frontend
/// so the core can shut the whole app down without knowing what a frontend is.
type ExitHook = Box<dyn Fn() + Send + 'static>;

pub struct Controller {
    state: Mutex<AppState>,
    subscribers: Mutex<Vec<Subscriber>>,
    config: Config,
    /// Lazy fetches already dispatched, so scrolling a list or opening a level
    /// twice does not spawn the same request again. Shared by both frontends,
    /// which is why it lives here rather than in each of them.
    inflight: Mutex<HashSet<LoadRequest>>,
    /// One teardown hook per frontend, see [`Controller::on_exit`].
    exit_hooks: Mutex<Vec<ExitHook>>,
    /// Whether the operator asked to reboot. Read once the frontends are gone,
    /// so it is not part of [`AppState`]: no view renders it.
    reboot_requested: AtomicBool,
}

impl Controller {
    pub fn new(config: Config) -> Arc<Self> {
        let state = AppState {
            selection: Selection {
                mode: config.mode,
                fetch: config.fetch,
                ..Selection::default()
            },
            dry_run: config.dry_run,
            ..AppState::default()
        };
        Arc::new(Self {
            state: Mutex::new(state),
            subscribers: Mutex::new(Vec::new()),
            config,
            inflight: Mutex::new(HashSet::new()),
            exit_hooks: Mutex::new(Vec::new()),
            reboot_requested: AtomicBool::new(false),
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

    // --- Shutdown --------------------------------------------------------

    /// Register a frontend teardown hook: a callback that makes that frontend's
    /// event loop return. Callable from any thread, because whoever asks for a
    /// shutdown is rarely the frontend that has to act on it.
    pub fn on_exit<F>(&self, f: F)
    where
        F: Fn() + Send + 'static,
    {
        self.exit_hooks.lock().unwrap().push(Box::new(f));
    }

    /// Ask the installer to close and reboot the machine.
    ///
    /// This only *records* the request and closes every frontend; the reboot
    /// itself happens in `main`, once the event loops have returned. That
    /// ordering is the whole point: the TUI restores the terminal (leaves the
    /// alternate screen, disables raw mode) while its event loop unwinds, and
    /// rebooting from inside a UI callback would skip that and leave the
    /// operator's serial console unusable.
    ///
    /// Idempotent, so a second press — or one press per frontend — is harmless.
    pub fn request_reboot(&self) {
        if self.reboot_requested.swap(true, Ordering::SeqCst) {
            return;
        }
        self.log("reboot requested — closing the installer");
        for hook in self.exit_hooks.lock().unwrap().iter() {
            hook();
        }
    }

    /// Whether a reboot was requested. Checked after the frontends have exited.
    pub fn reboot_requested(&self) -> bool {
        self.reboot_requested.load(Ordering::SeqCst)
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
    /// thread; it only reads sysfs / device-tree, and the UFS provisioning check
    /// it kicks off for the auto-selected target runs on a thread of its own.
    pub fn discover(self: &Arc<Self>) {
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

        // Report the auto-selected target's provisioning, but do not offer to wipe
        // it: nobody has chosen this device yet, and an operator who is about to
        // insert an SD card should not have to answer for the one soldered on.
        if let Some(path) = self.snapshot().selection.target_device {
            self.check_ufs_provisioning(&path, Offer::ReportOnly);
        }
    }

    /// Re-scan every source: mount removable media read-only, list the bundle
    /// channels and any local bundles, and (for the custom flow) query the image
    /// server's two-level catalog.
    ///
    /// Called once at startup and by the Refresh action, which also drops the
    /// dedupe set so a level that failed can be retried.
    pub fn refresh_sources(self: &Arc<Self>) {
        self.clear_inflight();

        if self.config.automount {
            let mounted = removable::automount_ro();
            for (device, mountpoint) in &mounted {
                self.log(format!("mounted {device} read-only at {mountpoint}"));
            }
        }

        // Bundles first: they are the default source, so the operator sees
        // something usable before the legacy catalog has been walked.
        self.load_channels();
        self.load_local();
        self.refresh_catalog();

        // Auto-select the newest build of the first channel so a plain run has a
        // complete selection without any drilling.
        let needs_bundle = {
            let s = self.state.lock().unwrap();
            s.selection.mode == InstallMode::Bundle && s.selection.bundle.is_none()
        };
        if needs_bundle {
            if let Some(channel) = self
                .snapshot()
                .catalog
                .channels
                .items
                .iter()
                .find(|c| c.as_str() != bundle::DEV_CHANNEL)
                .cloned()
            {
                self.load_builds(&channel);
                let newest = self
                    .snapshot()
                    .catalog
                    .builds
                    .get(&channel)
                    .and_then(|l| l.items.first())
                    .map(|r| r.id.clone());
                if let Some(id) = newest {
                    self.load_bundle(&id);
                }
            }
        }

        self.update(|s| {
            if matches!(s.phase, Phase::Discovering) {
                s.phase = Phase::Ready;
            }
        });
    }

    /// Query the image server and any removable-media mirror for U-Boot and
    /// snapshot builds, newest first. Feeds the *custom development build* flow.
    fn refresh_catalog(&self) {
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
        // In bundle mode the installable device types come from the bundle's own
        // manifest, so don't let the legacy catalog overwrite them.
        if self.snapshot().selection.mode == InstallMode::Custom {
            self.update(|s| s.supported_device_types = supported.clone());
        }

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
        });

        // Load the contents of the auto-selected builds so the UI can populate
        // and, in custom mode, so the digests are available to verify against.
        let selection = self.snapshot().selection;
        if let Some(id) = selection.snapshot_build {
            self.load_profiles(&id);
        }
        if let Some(id) = selection.uboot {
            self.load_uboot_contents(&id);
        }
    }

    // --- update bundles --------------------------------------------------

    /// Dispatch a lazy fetch on a worker thread, unless the same one is already
    /// in flight. Results arrive through the normal snapshot broadcast.
    pub fn ensure_loaded(self: &Arc<Self>, req: LoadRequest) {
        {
            let mut inflight = self.inflight.lock().unwrap();
            if !inflight.insert(req.clone()) {
                return;
            }
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            match &req {
                LoadRequest::Channels => this.load_channels(),
                LoadRequest::Builds(path) => this.load_builds(path),
                LoadRequest::Dirs(path) => this.load_dirs(path),
                LoadRequest::Local => this.load_local(),
                LoadRequest::Bundle(id) => this.load_bundle(id),
                LoadRequest::UbootContents(id) => this.load_uboot_contents(id),
                LoadRequest::SnapshotProfiles(id) => {
                    // One request reads everything the rootfs manifest offers:
                    // the packs to install and the metadata the popup shows.
                    this.load_profiles(id);
                    this.load_snapshot_details(id);
                }
            }
            // A completed fetch stays deduped only until the next Refresh: it has
            // either produced data or recorded why it could not.
        });
    }

    /// Forget which fetches have been dispatched, so Refresh really re-fetches
    /// (in particular, retries a level that failed).
    pub fn clear_inflight(&self) {
        self.inflight.lock().unwrap().clear();
    }

    /// Mark a listing as being fetched, and note whether it already holds data.
    fn begin_listing<F>(&self, pick: F)
    where
        F: FnOnce(&mut CatalogCache) -> &mut LoadState,
    {
        self.update(|s| {
            let cache = Arc::make_mut(&mut s.catalog);
            *pick(cache) = LoadState::Loading;
        });
    }

    /// List the channels published in the bucket.
    fn load_channels(&self) {
        self.begin_listing(|c| &mut c.channels.state);
        let repo = self.config.repo();
        match bundle::channels(&repo) {
            Ok(names) => {
                self.log(format!("update channels: {}", names.join(", ")));
                self.update(|s| {
                    let cache = Arc::make_mut(&mut s.catalog);
                    cache.channels = Listing {
                        items: names,
                        state: LoadState::Loaded,
                    };
                });
            }
            Err(e) => {
                self.log(format!("listing update channels: {e}"));
                self.update(|s| {
                    let cache = Arc::make_mut(&mut s.catalog);
                    cache.channels.state = LoadState::Failed(e);
                });
            }
        }
    }

    /// List the bundle builds under a channel path.
    fn load_builds(&self, path: &str) {
        let key = path.to_string();
        self.update(|s| {
            let cache = Arc::make_mut(&mut s.catalog);
            cache.builds.entry(key.clone()).or_default().state = LoadState::Loading;
        });
        let repo = self.config.repo();
        match bundle::builds(&repo, path) {
            Ok(items) => {
                self.log(format!("{}: {} bundle(s)", path, items.len()));
                self.update(|s| {
                    let cache = Arc::make_mut(&mut s.catalog);
                    cache.builds.insert(
                        key.clone(),
                        Listing {
                            items,
                            state: LoadState::Loaded,
                        },
                    );
                });
            }
            Err(e) => {
                self.log(format!("listing {path}: {e}"));
                self.update(|s| {
                    let cache = Arc::make_mut(&mut s.catalog);
                    cache.builds.entry(key.clone()).or_default().state = LoadState::Failed(e);
                });
            }
        }
    }

    /// List an intermediate level of the `dev/` tree.
    fn load_dirs(&self, path: &str) {
        let key = path.to_string();
        self.update(|s| {
            let cache = Arc::make_mut(&mut s.catalog);
            cache.dirs.entry(key.clone()).or_default().state = LoadState::Loading;
        });
        let repo = self.config.repo();
        match bundle::dirs(&repo, path) {
            Ok(items) => self.update(|s| {
                let cache = Arc::make_mut(&mut s.catalog);
                cache.dirs.insert(
                    key.clone(),
                    Listing {
                        items,
                        state: LoadState::Loaded,
                    },
                );
            }),
            Err(e) => {
                self.log(format!("listing {path}: {e}"));
                self.update(|s| {
                    let cache = Arc::make_mut(&mut s.catalog);
                    cache.dirs.entry(key.clone()).or_default().state = LoadState::Failed(e);
                });
            }
        }
    }

    /// Scan removable media and the `--bundle` paths for local bundles.
    fn load_local(&self) {
        self.begin_listing(|c| &mut c.local.state);
        let media = removable::media_roots();
        let items = bundle::discover_local(
            &self.config.bundle_paths,
            &media,
            &self.config.cache_dir,
        );
        if !items.is_empty() {
            self.log(format!("{} local bundle(s) found", items.len()));
        }
        self.update(|s| {
            let cache = Arc::make_mut(&mut s.catalog);
            cache.local = Listing {
                items,
                state: LoadState::Loaded,
            };
        });
    }

    /// Read a bundle's manifest and resolve it into the pinned pair the install
    /// pipeline consumes.
    fn load_bundle(&self, id: &str) {
        let (reference, board_id) = {
            let s = self.state.lock().unwrap();
            match s.bundle_ref_by_id(id) {
                Some(r) => (r.clone(), s.board.board_id.clone()),
                None => return,
            }
        };
        self.log(format!("reading bundle {}…", reference.dir));
        match bundle::load(&reference, &board_id) {
            Ok(bundle) => {
                self.log(format!(
                    "bundle {} {}: {} profile(s), u-boot for {}",
                    bundle.channel,
                    bundle.version,
                    bundle.build.profiles.len(),
                    bundle.board_dir,
                ));
                if bundle.board_dir != board_id {
                    self.log(format!(
                        "device type '{board_id}' not shipped by this bundle; \
                         using the {} U-Boot",
                        bundle.board_dir
                    ));
                }
                self.update(|s| {
                    s.supported_device_types = bundle.device_types.clone();
                    s.bundle = Some(bundle);
                    s.bundle_error = None;
                });
            }
            Err(e) => {
                self.log(format!("bundle {}: {e}", reference.dir));
                self.update(|s| {
                    s.bundle = None;
                    s.bundle_error = Some(e);
                });
            }
        }
    }

    /// Select a bundle and resolve it in the background.
    pub fn select_bundle(self: &Arc<Self>, id: &str) {
        self.update(|s| {
            s.selection.mode = InstallMode::Bundle;
            s.selection.bundle = Some(id.to_string());
            s.selection.profiles.clear();
            s.bundle = None;
            s.bundle_error = None;
        });
        let this = Arc::clone(self);
        let id = id.to_string();
        std::thread::spawn(move || this.load_bundle(&id));
    }

    pub fn set_fetch_mode(&self, mode: FetchMode) {
        self.update(|s| s.selection.fetch = mode);
        self.log(format!("fetch mode: {}", mode.label()));
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

    /// Fetch a U-Boot build's manifest: the image's size and digest, plus the
    /// build metadata for the details popup. Blocking, so call it from a worker
    /// thread; a no-op if already loaded or unknown.
    ///
    /// The counterpart of [`Self::load_profiles`]. Both kinds of build carry a
    /// manifest and are read exactly once through this pair, so the details
    /// popup, the verification pass and the install path all see the same fields.
    fn load_uboot_contents(&self, id: &str) {
        let (build, board_dir) = {
            let s = self.state.lock().unwrap();
            match s.uboot_builds.iter().find(|b| b.id == id) {
                Some(b) if !b.loaded => (b.clone(), self.board_dir(&s)),
                _ => return,
            }
        };
        match catalog::load_uboot_contents(&build, &board_dir) {
            Ok((size, sha256, mtime, details)) => self.update(|s| {
                if let Some(b) = s.uboot_builds.iter_mut().find(|b| b.id == id) {
                    b.size_bytes = size;
                    b.sha256 = sha256;
                    if !mtime.is_empty() {
                        b.mtime = mtime;
                    }
                    b.details = Some(details);
                    b.loaded = true;
                }
            }),
            Err(e) => self.log(format!("u-boot manifest: {e}")),
        }
    }

    /// Fetch the snapshot build's manifest details for the popup. Blocking, so
    /// call it from a worker thread; a no-op if already loaded or unknown.
    fn load_snapshot_details(&self, id: &str) {
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

    /// The per-board U-Boot directory to install from: the detected board when
    /// the server offers it, else the generic build.
    fn board_dir(&self, state: &AppState) -> String {
        let board_id = &state.board.board_id;
        if state.supported_device_types.iter().any(|t| t == board_id) {
            board_id.clone()
        } else {
            catalog::board_dir(board_id).to_string()
        }
    }

    // --- Selection actions (shared by both frontends) --------------------

    pub fn select_device(self: &Arc<Self>, path: &str) {
        self.update(|s| {
            s.selection.target_device = Some(path.to_string());
            // The old probe describes a different device.
            s.ufs = None;
        });
        // Picking a device by hand is a decision about that device, so this is
        // where an offer to reprovision it belongs.
        self.check_ufs_provisioning(path, Offer::Prompt);
    }

    // --- UFS provisioning -------------------------------------------------

    /// Probe a UFS target against the Flipper provisioning scheme, on a worker
    /// thread since it talks to the device. Non-UFS targets are left alone.
    ///
    /// `offer` decides whether a layout that does not match raises the
    /// reprovisioning prompt or is only reported. `--reprovision-ufs` skips the
    /// asking either way.
    pub fn check_ufs_provisioning(self: &Arc<Self>, path: &str, offer: Offer) {
        if !self.config.ufs_check {
            return;
        }
        let is_ufs = self
            .snapshot()
            .devices
            .iter()
            .any(|d| d.path == path && d.kind == StorageKind::Ufs);
        if !is_ufs {
            return;
        }
        let this = Arc::clone(self);
        let path = path.to_string();
        std::thread::spawn(move || this.probe_ufs(&path, offer));
    }

    /// Read the target's provisioning and publish it. Runs on a worker thread.
    fn probe_ufs(self: &Arc<Self>, path: &str, offer: Offer) {
        let (scheme, origin) = provision::Scheme::resolve(&self.config);
        let status = match provision::probe(path, &scheme, &origin) {
            Ok(status) => status,
            Err(e) => {
                self.log(probe_failure(path, &e));
                return;
            }
        };
        for line in status.report() {
            self.log(line);
        }
        let provisioned = status.is_provisioned();
        let device = status.device.clone();
        let prompt = match (provisioned, offer) {
            (false, Offer::Prompt) => Some(reprovision_prompt(&status)),
            _ => None,
        };
        self.update(|s| {
            // Only publish it if the operator has not moved on to another target.
            if s.selection.target_device.as_deref() != Some(path) {
                return;
            }
            s.ufs = Some(status);
            if prompt.is_some() {
                s.prompt = prompt.clone();
            }
        });
        if provisioned {
            return;
        }
        if self.config.ufs_reprovision {
            self.log(format!(
                "{device}: --reprovision-ufs given, reprovisioning without asking"
            ));
            self.dismiss_prompt();
            self.start_reprovision(&device);
        } else if offer == Offer::ReportOnly {
            // Say where the offer lives, since it is not being made here.
            self.log(format!(
                "{device} is not provisioned for FlipperOS; reprovision it from the \
                 target-device menu, or pick another target"
            ));
        }
    }

    /// Raise the reprovisioning prompt again for `path`, e.g. after it was
    /// dismissed. Re-probes first, so it never offers to fix something already
    /// fixed.
    pub fn request_reprovision(self: &Arc<Self>, path: &str) {
        let this = Arc::clone(self);
        let path = path.to_string();
        std::thread::spawn(move || {
            let (scheme, origin) = provision::Scheme::resolve(&this.config);
            match provision::probe(&path, &scheme, &origin) {
                Err(e) => this.log(probe_failure(&path, &e)),
                Ok(status) if status.is_provisioned() => {
                    for line in status.report() {
                        this.log(line);
                    }
                    this.log(format!("{path}: already matches the Flipper scheme"));
                    this.update(|s| s.ufs = Some(status));
                }
                Ok(status) => {
                    let prompt = reprovision_prompt(&status);
                    this.update(|s| {
                        s.ufs = Some(status);
                        s.prompt = Some(prompt);
                    });
                }
            }
        });
    }

    /// Answer the pending prompt affirmatively.
    ///
    /// Taking the prompt is what makes this safe to call from both frontends at
    /// once: whichever gets there first clears it under the lock, and the other
    /// finds nothing to act on. Two simultaneous confirmations must not start two
    /// destructive operations.
    pub fn confirm_prompt(self: &Arc<Self>) {
        let Some(prompt) = self.take_prompt() else {
            return;
        };
        match prompt.kind {
            PromptKind::ReprovisionUfs { device } => self.start_reprovision(&device),
        }
    }

    /// Dismiss the pending prompt without acting on it.
    pub fn dismiss_prompt(&self) {
        self.take_prompt();
    }

    /// Remove the pending prompt and hand it back, if there was one.
    fn take_prompt(&self) -> Option<Prompt> {
        let taken = self.state.lock().unwrap().prompt.take();
        if taken.is_some() {
            // Both frontends take the popup down off this snapshot.
            self.notify();
        }
        taken
    }

    /// Rewrite a UFS target's logical units on a worker thread. Destructive, so
    /// it only ever runs from a confirmed prompt or `--reprovision-ufs`.
    fn start_reprovision(self: &Arc<Self>, device: &str) {
        if self.snapshot().phase.is_busy() {
            self.log("cannot reprovision while another operation is running");
            return;
        }
        let this = Arc::clone(self);
        let device = device.to_string();
        std::thread::spawn(move || {
            this.set_phase(Phase::Provisioning);
            let (scheme, origin) = provision::Scheme::resolve(&this.config);
            let result = provision::apply(&this.config, &this, &device, &scheme, &origin);
            match result {
                Ok(()) => {
                    // The logical units may have changed size, or been renamed by
                    // the rescan, so the device list has to be rebuilt.
                    this.rediscover_storage(&device);
                    this.set_phase(Phase::Ready);
                }
                Err(e) => {
                    this.log(format!("reprovisioning failed: {e}"));
                    this.set_phase(Phase::Failed(e));
                }
            }
        });
    }

    /// Re-enumerate storage after a device was reprovisioned, keeping `preferred`
    /// selected when it is still there and falling back to the same rule
    /// [`Self::discover`] uses when it is not.
    fn rediscover_storage(self: &Arc<Self>, preferred: &str) {
        let devices = storage_list();
        for d in &devices {
            self.log(format!("  found {}", d.summary()));
        }
        let still_there = devices.iter().any(|d| d.path == preferred);
        self.update(|s| {
            s.devices = devices;
            if !still_there {
                s.selection.target_device = s
                    .devices
                    .iter()
                    .find(|d| d.boot_rom_capable() && !d.removable)
                    .map(|d| d.path.clone());
            }
        });
        if let Some(path) = self.snapshot().selection.target_device {
            // Straight after a reprovision, so report the outcome rather than
            // offering to do it again.
            self.probe_ufs(&path, Offer::ReportOnly);
        }
    }

    /// Pick a U-Boot build by hand, which is only meaningful for the custom flow,
    /// so it switches out of bundle mode. Its manifest is read in the background
    /// for the image's size and digest.
    pub fn select_uboot(self: &Arc<Self>, id: &str) {
        self.update(|s| {
            s.selection.mode = InstallMode::Custom;
            s.selection.uboot = Some(id.to_string());
        });
        let this = Arc::clone(self);
        let id = id.to_string();
        std::thread::spawn(move || this.load_uboot_contents(&id));
    }

    /// Pick a snapshot build by hand, resetting the extra-profile selection, and
    /// load its profiles in the background if not already loaded. Like
    /// [`Self::select_uboot`], this is the custom flow.
    pub fn select_snapshot_build(self: &Arc<Self>, id: &str) {
        self.update(|s| {
            s.selection.mode = InstallMode::Custom;
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

/// The prompt offering to bring a UFS target to the Flipper scheme. Built in one
/// place so the wording cannot drift between the paths that raise it.
fn reprovision_prompt(status: &provision::Status) -> Prompt {
    Prompt {
        kind: PromptKind::ReprovisionUfs {
            device: status.device.clone(),
        },
        title: "Reprovision UFS?".to_string(),
        lines: status.prompt_lines(),
        // Both captions have to fit a 48 px soft-button slot on the device's
        // screen, which "Reprovision" overflows. The title and the body say what
        // confirming does; the button only has to say that it does it.
        confirm: "Confirm".to_string(),
        cancel: "Cancel".to_string(),
    }
}

/// Why a UFS provisioning probe came back empty. Never fatal: the check is
/// advisory, and [`crate::core::install`]'s own boot-LU guard is what refuses a
/// device the bootloader cannot be written to.
fn probe_failure(path: &str, e: &str) -> String {
    format!("warning: cannot read {path}'s UFS provisioning: {e}")
}

/// Enumerate local storage; kept as a free function so tests and the
/// [`Controller`] share one implementation.
fn storage_list() -> Vec<StorageDevice> {
    crate::core::storage::enumerate(crate::core::storage::MIN_TARGET_SIZE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn dry_run_reaches_the_frontends_through_the_snapshot() {
        let ctrl = Controller::new(Config {
            dry_run: true,
            ..Config::default()
        });
        assert!(ctrl.snapshot().dry_run);

        let ctrl = Controller::new(Config {
            dry_run: false,
            ..Config::default()
        });
        assert!(!ctrl.snapshot().dry_run);
    }

    #[test]
    fn only_one_frontend_can_answer_a_prompt() {
        // Both frontends show the same prompt, so both can be pressed at once. A
        // destructive action must run for the first answer only.
        let ctrl = Controller::new(Config::default());
        let seen = Arc::new(AtomicUsize::new(0));
        {
            let seen = Arc::clone(&seen);
            ctrl.subscribe(move |s| {
                if s.prompt.is_none() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        ctrl.update(|s| {
            s.prompt = Some(Prompt {
                kind: PromptKind::ReprovisionUfs {
                    device: "/dev/sda".to_string(),
                },
                title: "Reprovision UFS?".to_string(),
                lines: vec!["ALL DATA WILL BE LOST.".to_string()],
                confirm: "Confirm".to_string(),
                cancel: "Cancel".to_string(),
            });
        });
        assert!(ctrl.snapshot().prompt.is_some());
        let before = seen.load(Ordering::SeqCst);

        assert!(ctrl.take_prompt().is_some(), "the first answer wins");
        assert!(ctrl.take_prompt().is_none(), "the second finds it gone");
        assert!(ctrl.snapshot().prompt.is_none());
        // Clearing it is broadcast, so the other frontend's popup comes down;
        // taking nothing broadcasts nothing.
        assert_eq!(seen.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn requesting_a_reboot_closes_every_frontend_once() {
        let ctrl = Controller::new(Config::default());
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let calls = Arc::clone(&calls);
            ctrl.on_exit(move || {
                calls.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert!(!ctrl.reboot_requested());

        ctrl.request_reboot();
        assert!(ctrl.reboot_requested());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one call per frontend");

        // A second press (or one press per frontend) must not re-run teardown.
        ctrl.request_reboot();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
