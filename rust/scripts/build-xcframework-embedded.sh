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
XCF_NAME="FantasticUniFFIEmbedded.xcframework"

TARGETS_DEVICE=(
    "aarch64-apple-ios"
    "aarch64-apple-visionos"      # Tier 2 — Rust 1.74+
    "aarch64-apple-tvos"          # Tier 3 — built clean on Rust 1.95 (Homebrew stable)
    "aarch64-apple-watchos"       # Tier 3 — built clean on Rust 1.95 (arm64 only;
                                  # arm64_32 for older watches has no stable artifact)
)
# x86_64 simulators removed from upstream rustc stable for iOS/tvOS/watchOS;
# Apple-Silicon-only.
TARGETS_SIM=(
    "aarch64-apple-ios-sim"
    "aarch64-apple-visionos-sim"
    "aarch64-apple-tvos-sim"
    "aarch64-apple-watchos-sim"
)
TARGETS_MAC=("aarch64-apple-darwin" "x86_64-apple-darwin")
# ---------------------------------------------------------------------------

build_xcframework_variant
