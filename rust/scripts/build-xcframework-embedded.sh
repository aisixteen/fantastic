#!/usr/bin/env bash
# build-xcframework-embedded.sh — sandboxed/embedded tier.
#
# Builds an XCFramework with the `embedded` feature set:
#   - No PTY-using bundles (terminal_backend, local_runner not registered)
#   - No subprocess spawning (compile-time guarantee for the iOS sandbox)
#   - All Apple slices: ios-arm64, ios-arm64-simulator, macos-arm64_x86_64
#
# This is the XCFramework FantasticLite (multi-platform: iOS, iPadOS,
# macOS Lite) links against.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=build-xcframework.lib.sh
source "$SCRIPT_DIR/build-xcframework.lib.sh"
xcf_lib_init

# Configuration -------------------------------------------------------------
FEATURES="embedded"
SWIFT_MODULE_NAME="FantasticKernelEmbedded"
PKG_DIR="$RUST_ROOT/packaging/FantasticKernelEmbedded"
XCF_NAME="Fantastic-Embedded.xcframework"

TARGETS_DEVICE=("aarch64-apple-ios")
# x86_64-apple-ios-sim removed from upstream rustc stable; Apple-Silicon-only.
TARGETS_SIM=("aarch64-apple-ios-sim")
TARGETS_MAC=("aarch64-apple-darwin" "x86_64-apple-darwin")
# ---------------------------------------------------------------------------

build_xcframework_variant
