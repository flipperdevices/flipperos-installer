//! Bringing the machine down once an install is finished.
//!
//! Kept separate from the frontends on purpose: rebooting must happen *after*
//! every event loop has returned, because that is when the TUI's terminal is
//! restored (see [`crate::core::Controller::request_reboot`]). So a frontend only
//! ever records the request, and `main` calls in here last.

use crate::core::Config;

/// Flush filesystem buffers and reboot. On success this does not return.
///
/// Honours `--dry-run` as a backstop. The frontends already hide the Reboot
/// action in a dry run, so that branch should be unreachable — but `--dry-run`
/// is the default and the installer runs as root, so a stray caller must not be
/// able to reboot a developer's workstation.
pub fn reboot(cfg: &Config) -> Result<(), String> {
    if cfg.dry_run {
        eprintln!("[dry-run] would sync and reboot now");
        return Ok(());
    }

    eprintln!("rebooting…");

    // The install path already ran `sync` and unmounted the target
    // (`install::unmount`); this covers whatever is still dirty elsewhere.
    unsafe { libc::sync() };

    // The syscall directly rather than busybox `reboot`: inside the initramfs we
    // may or may not be PID 1 and may or may not have that binary, whereas
    // RB_AUTOBOOT only needs the root privileges the installer already requires.
    if unsafe { libc::reboot(libc::RB_AUTOBOOT) } == -1 {
        return Err(format!("reboot: {}", std::io::Error::last_os_error()));
    }

    // Unreachable in practice: the kernel does not come back from RB_AUTOBOOT.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dry-run backstop must return without touching the test runner.
    #[test]
    fn dry_run_does_not_reboot() {
        let cfg = Config {
            dry_run: true,
            ..Config::default()
        };
        assert!(reboot(&cfg).is_ok());
    }
}
