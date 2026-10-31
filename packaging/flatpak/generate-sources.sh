#!/bin/sh
# Regenerate the vendored sources of the Flatpak manifest (#82) after a lockfile changes:
#
#   sh packaging/flatpak/generate-sources.sh
#
# Writes next to this script:
#   cargo-sources.json      the crates of src/app/Cargo.lock   (the desktop app)
#   cargo-sources-cli.json  the crates of src/Cargo.lock       (the canvas sidecar `fantastic`)
#   node-sources.json       the npm packages of src/app/package-lock.json (the Tauri CLI)
#
# Uses flatpak-cargo-generator.py and flatpak-node-generator from
# https://github.com/flatpak/flatpak-builder-tools, installed into a throwaway venv
# (needs uv, git and network). Nothing is installed system-wide.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

git clone -q --depth 1 https://github.com/flatpak/flatpak-builder-tools.git "$TMP/tools"
uv venv -q "$TMP/venv"
uv pip install -q --python "$TMP/venv/bin/python" aiohttp PyYAML tomlkit "$TMP/tools/node"

"$TMP/venv/bin/python" "$TMP/tools/cargo/flatpak-cargo-generator.py" "$REPO/src/app/Cargo.lock" -o "$HERE/cargo-sources.json"
"$TMP/venv/bin/python" "$TMP/tools/cargo/flatpak-cargo-generator.py" "$REPO/src/Cargo.lock" -o "$HERE/cargo-sources-cli.json"
"$TMP/venv/bin/flatpak-node-generator" npm "$REPO/src/app/package-lock.json" -o "$HERE/node-sources.json"
