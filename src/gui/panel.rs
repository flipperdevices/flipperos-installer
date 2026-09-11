//! The panel and the buttons: the half of the GUI that Slint no longer supplies.
//!
//! Slint is taken with `renderer-software` and no backend at all, so there is no
//! event loop, no DRM output and no input handling behind [`MainWindow`] — this
//! module is all three. The pieces come from `flipper-ui`, shared with flipctl
//! and the Falcon boot menu so the three programs drive the same panel the same
//! way:
//!
//! * [`KmsSink`] opens the display through the `drm` crate's ioctls and commits
//!   a greyscale frame to it. It prefers an `R8` framebuffer and falls back to
//!   `XRGB8888` on a kernel without one.
//! * [`EvdevSource`] reads the buttons straight from `/dev/input/event*`,
//!   finding them by their sysfs name rather than through udev. They are
//!   soldered on and cannot hotplug, so enumeration happens once.
//! * [`FlipperSlintPlatform`] is the whole `slint::platform::Platform` impl: a
//!   window adapter and a clock. Everything else is [`Loop::run`] below.
//!
//! Between them that removes libinput, libudev and libxkbcommon from the binary
//! — four shared libraries and a keymap compiler to service thirteen buttons.

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use flipper_ui::evdev::EvdevSource;
use flipper_ui::kms::KmsSink;
use flipper_ui::slint_render::render_into;
use flipper_ui::{FlipperKey, Frame, FrameSink, Gray8, InputSource, PANEL_H, PANEL_W};
use slint::platform::software_renderer::MinimalSoftwareWindow;
use slint::platform::WindowEvent;
use slint::SharedString;

/// How long a turn of the loop blocks in `poll(2)` when nothing is happening.
///
/// A button wakes the poll immediately. A state change does not — the worker
/// threads hand snapshots over a channel, which no file descriptor watches — so
/// this also bounds how long a download's progress takes to reach the screen.
/// One panel commit is 16-19ms, so a shorter wait could not draw sooner anyway.
const TICK: Duration = Duration::from_millis(16);

/// The text Slint's `FocusScope` sees for one of the panel's buttons.
///
/// The inverse of `flipctl_app::Key::from_slint`, which is not exported: the
/// D-pad, Enter and Backspace travel as Slint's private-use key characters, and
/// the five soft buttons as the plain letters the silkscreen order maps them to
/// (`ui/main.slint` matches both cases, so lowercase is enough).
///
/// `AppSwitch` returns `None` deliberately. It is the one button the installer
/// has no use for, and the obvious encoding — Tab — would move focus out of the
/// `FocusScope` and take the UI's keyboard input with it.
fn key_text(key: FlipperKey) -> Option<SharedString> {
    use slint::platform::Key as K;

    let named = |k: K| -> SharedString { char::from(k).into() };
    Some(match key {
        FlipperKey::Up => named(K::UpArrow),
        FlipperKey::Down => named(K::DownArrow),
        FlipperKey::Left => named(K::LeftArrow),
        FlipperKey::Right => named(K::RightArrow),
        FlipperKey::Ok => named(K::Return),
        FlipperKey::Back => named(K::Backspace),
        FlipperKey::Escape => "z".into(),
        FlipperKey::View => "x".into(),
        FlipperKey::Power => "c".into(),
        FlipperKey::Edit => "v".into(),
        FlipperKey::Run => "b".into(),
        FlipperKey::Ptt => "a".into(),
        FlipperKey::AppSwitch => return None,
    })
}

/// The panel, the buttons, and the frame buffer between them.
pub(crate) struct Panel {
    sink: KmsSink,
    /// `None` when no button device was found. Not an error: the buttons are on
    /// i2c and their probe can fail, and a device whose buttons are dead still
    /// has to show an install running.
    input: Option<EvdevSource>,
    /// Reused across frames, so the steady state allocates nothing.
    frame: Vec<Gray8>,
    debug_keys: bool,
}

impl Panel {
    /// Open the display and the buttons.
    ///
    /// `kms_device` empty means auto-detect, which matches on the driver name
    /// `flipper_one_display` — more robust than a `by-path` symlink, which is
    /// only stable while the SPI address is.
    pub(crate) fn open(kms_device: &str, debug_keys: bool) -> Result<Self, String> {
        let explicit = (!kms_device.is_empty()).then(|| Path::new(kms_device));
        let sink = KmsSink::open(explicit).map_err(|e| format!("panel: {e}"))?;

        let (w, h) = sink.size();
        if (w, h) != (PANEL_W, PANEL_H) {
            return Err(format!(
                "panel reports {w}x{h}, this build is compiled for {PANEL_W}x{PANEL_H}"
            ));
        }
        if debug_keys {
            eprintln!("gui panel: {w}x{h}, {}", sink.format());
        }

        let input = match EvdevSource::open() {
            Ok(source) => Some(source),
            Err(e) => {
                eprintln!("warning: no on-device buttons: {e}");
                None
            }
        };

        Ok(Self { sink, input, frame: Vec::new(), debug_keys })
    }

    /// Drain every pending button event into `window`.
    ///
    /// Drained rather than sampled: a press and its release are two events and
    /// both have to be seen on the turn they land, or a held key reads as stuck.
    /// Returns whether anything arrived, which is what marks the frame dirty.
    pub(crate) fn pump_keys(&mut self, window: &slint::Window) -> bool {
        let mut any = false;
        while let Some(event) = self.input.as_mut().and_then(InputSource::poll) {
            let text = key_text(event.key);
            if self.debug_keys {
                match &text {
                    Some(text) => eprintln!(
                        "gui key: {:?} {} text=<{text}>",
                        event.key,
                        if event.down { "down" } else { "up" }
                    ),
                    None => eprintln!("gui key: {:?} unmapped", event.key),
                }
            }
            let Some(text) = text else { continue };
            any = true;
            window.dispatch_event(if event.down {
                WindowEvent::KeyPressed { text }
            } else {
                WindowEvent::KeyReleased { text }
            });
        }
        any
    }

    /// Render whatever the window is holding and put it on the panel.
    ///
    /// `render_into` returns `None` when nothing needed repainting, and then
    /// there is nothing to commit: re-transmitting an identical frame would cost
    /// a full 37KB SPI write for no change.
    pub(crate) fn present(&mut self, window: &MinimalSoftwareWindow) -> Result<(), String> {
        let Some(damage) = render_into(window, &mut self.frame) else {
            return Ok(());
        };
        self.sink
            .commit(Frame::new(&self.frame, PANEL_W, PANEL_H), damage)
            .map_err(|e| format!("panel commit: {e}"))
    }

    /// Block until a button is pressed or [`TICK`] elapses.
    pub(crate) fn wait(&self) {
        let borrowed = match self.input.as_ref() {
            Some(source) => source.fds(),
            None => Vec::new(),
        };
        if borrowed.is_empty() {
            std::thread::sleep(TICK);
            return;
        }
        let mut fds: Vec<libc::pollfd> = borrowed
            .iter()
            .map(|fd| libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 })
            .collect();
        // Errors are deliberately ignored: EINTR and anything else just mean
        // this turn waited less than it meant to, and the loop copes with that
        // by design. Failing the UI over a poll(2) return code would not.
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, TICK.as_millis() as libc::c_int)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four keys `ui/main.slint` navigates with, and the two soft buttons it
    /// labels, have to arrive as exactly the text its `FocusScope` compares
    /// against — `Key.UpArrow` and friends are private-use characters, not names.
    #[test]
    fn navigation_keys_carry_slints_own_characters() {
        use slint::platform::Key as K;

        assert_eq!(key_text(FlipperKey::Up).unwrap(), SharedString::from(char::from(K::UpArrow)));
        assert_eq!(
            key_text(FlipperKey::Down).unwrap(),
            SharedString::from(char::from(K::DownArrow))
        );
        assert_eq!(key_text(FlipperKey::Ok).unwrap(), SharedString::from(char::from(K::Return)));
        assert_eq!(
            key_text(FlipperKey::Back).unwrap(),
            SharedString::from(char::from(K::Backspace))
        );
    }

    /// RUN starts the install and EXIT cancels; `ui/main.slint` matches them as
    /// the letters "b" and "z", so a change to the silkscreen mapping upstream
    /// has to fail here rather than silently stop the install button working.
    #[test]
    fn soft_buttons_are_the_letters_the_ui_matches() {
        assert_eq!(key_text(FlipperKey::Run).unwrap(), "b");
        assert_eq!(key_text(FlipperKey::Escape).unwrap(), "z");
        assert_eq!(key_text(FlipperKey::View).unwrap(), "x");
        assert_eq!(key_text(FlipperKey::Edit).unwrap(), "v");
    }

    /// Tab would move focus out of the FocusScope and take every subsequent
    /// keypress with it, so the app-switcher key has to be swallowed here.
    #[test]
    fn app_switch_is_swallowed() {
        assert!(key_text(FlipperKey::AppSwitch).is_none());
    }
}
