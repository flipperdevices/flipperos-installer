#!/bin/sh
# Regenerate THIRD-PARTY-LICENSES.md — the license attribution for every crate
# linked into the shipped binary — from the current Cargo.lock.
#
# Run this whenever dependencies change. Requires `cargo-about`, pinned:
#     cargo install cargo-about --version 0.9.1 --features cli
#
# The pin is deliberate. 0.9.2 emits a separate section per distinct license
# *text* (a crate like `ring` ships several copies of the same ISC text) but
# still counts one "crate(s)" per section in the summary at the top, so that
# summary contradicts the body it introduces — 0.9.2 reports 19 ISC crates
# where there are 4. The per-crate attribution is identical either way; only
# the summary is wrong. Re-check when a later release fixes it.
#
# Config lives in about.toml (accepted licenses, target, dev-dep exclusion) and
# the output layout in about.hbs.
set -eu

cd "$(dirname "$0")/.."

if ! cargo about --version >/dev/null 2>&1; then
    echo "error: cargo-about not found — install with:" >&2
    echo "    cargo install cargo-about --version 0.9.1 --features cli" >&2
    exit 1
fi

# Warn rather than refuse: a newer cargo-about still produces correct per-crate
# attribution, it just miscounts the summary (see the note above).
about_version=$(cargo about --version | awk '{print $2}')
if [ "$about_version" != "0.9.1" ]; then
    echo "warning: cargo-about $about_version is not the pinned 0.9.1;" >&2
    echo "         check the crate counts at the top of the output" >&2
fi

# --fail: error out if any linked crate uses a license not in about.toml's
# `accepted` list, so a new dependency can never silently ship unlicensed.
cargo about generate --fail -c about.toml about.hbs -o THIRD-PARTY-LICENSES.md
echo "wrote THIRD-PARTY-LICENSES.md"
