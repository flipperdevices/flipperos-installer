// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! Record the source revision the installer is built from, for it to report
//! as `FLIPPEROS_INSTALLER_REVISION`.
//!
//! A build from a git checkout asks git. A build from a source tarball, as
//! Buildroot makes one, has no repository to ask, so the packaging passes the
//! revision in the variable of the same name, which also wins over git. With
//! neither, the revision is left empty and the installer reports none.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const VAR: &str = "FLIPPEROS_INSTALLER_REVISION";

fn main() {
    println!("cargo:rerun-if-env-changed={VAR}");
    // Naming any file below replaces Cargo's default of rerunning on every
    // change in the package, so name what that default covered too.
    for path in ["build.rs", "Cargo.toml", "Cargo.lock", "src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let revision = env::var(VAR)
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .or_else(git_revision)
        .unwrap_or_default();
    println!("cargo:rustc-env={VAR}={revision}");
}

/// The commit checked out, with `-dirty` when tracked files differ from it.
fn git_revision() -> Option<String> {
    let dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    // Only the package's own repository counts. A tarball unpacked inside some
    // other checkout — Buildroot's output tree, say — would otherwise report
    // that one's commit as the installer's.
    let top = git(&dir, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&top).canonicalize().ok()? != dir.canonicalize().ok()? {
        return None;
    }
    let head = git(&dir, &["rev-parse", "HEAD"])?;

    // A new commit or a checkout moves HEAD, or the branch it points at.
    let mut watch = vec![git(&dir, &["rev-parse", "--git-path", "HEAD"])?];
    if let Some(branch) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
        watch.extend(git(&dir, &["rev-parse", "--git-path", &branch]));
        watch.extend(git(&dir, &["rev-parse", "--git-path", "packed-refs"]));
    }
    for path in watch {
        println!("cargo:rerun-if-changed={}", dir.join(path).display());
    }

    let clean = Command::new("git")
        .args(["diff", "--quiet", "HEAD", "--"])
        .current_dir(&dir)
        .status()
        .ok()?
        .success();
    Some(if clean { head } else { format!("{head}-dirty") })
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
