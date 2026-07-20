#!/bin/sh
# Regenerate THIRD-PARTY-LICENSES.md — the license attribution for every crate
# linked into the shipped binary — from the current Cargo.lock.
#
# Run this whenever dependencies change. Requires `cargo-about`:
#     cargo install cargo-about --features cli
#
# Config lives in about.toml (accepted licenses, target, dev-dep exclusion) and
# the output layout in about.hbs.
set -eu

cd "$(dirname "$0")/.."

if ! cargo about --version >/dev/null 2>&1; then
    echo "error: cargo-about not found — install with:" >&2
    echo "    cargo install cargo-about --features cli" >&2
    exit 1
fi

# --fail: error out if any linked crate uses a license not in about.toml's
# `accepted` list, so a new dependency can never silently ship unlicensed.
cargo about generate --fail -c about.toml about.hbs -o THIRD-PARTY-LICENSES.md
echo "wrote THIRD-PARTY-LICENSES.md"
