// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! Which installer this is: the crate's version, and the source revision it
//! was built from, as `build.rs` found it.

/// The crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const REVISION: &str = env!("FLIPPEROS_INSTALLER_REVISION");

/// The commit the installer was built from, suffixed `-dirty` when built from
/// a tree that differed from it; `None` when the build could not tell.
pub fn revision() -> Option<&'static str> {
    (!REVISION.is_empty()).then_some(REVISION)
}

/// The version with the revision, as people quote it: `0.1.0 (58911d4fd948)`.
/// The commit is shortened to the twelve digits the build server's artifact
/// names carry.
pub fn describe() -> String {
    match revision() {
        Some(rev) => format!("{VERSION} ({})", short(rev)),
        None => VERSION.to_string(),
    }
}

fn short(rev: &str) -> String {
    let (commit, suffix) = match rev.split_once('-') {
        Some((commit, suffix)) => (commit, format!("-{suffix}")),
        None => (rev, String::new()),
    };
    if commit.len() > 12 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("{}{suffix}", &commit[..12])
    } else {
        rev.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_a_commit_and_nothing_else() {
        let full = "58911d4fd948a3a4882e86e85624871983bbd14c";
        assert_eq!(short(full), "58911d4fd948");
        assert_eq!(short(&format!("{full}-dirty")), "58911d4fd948-dirty");
        // A branch name passed in by the packaging stays as it is.
        assert_eq!(short("dev"), "dev");
        assert_eq!(short("release-candidate"), "release-candidate");
    }
}
