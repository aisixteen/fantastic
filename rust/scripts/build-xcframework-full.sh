#!/usr/bin/env bash
# build-xcframework-full.sh — unsandboxed/full tier.
#
# Builds an XCFramework with the `full` feature set:
#   - PTY-using bundles (terminal_backend, local_runner) registered
#   - python_runtime + ssh_runner once they're ported
#   - Mac-only (no iOS slices — iOS sandbox forbids these features)
#
# This is the XCFramework FantasticPro (Mac, Developer ID, unsandboxed)
# links against when running the in-process kernel (Persist=off).

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=build-xcframework.lib.sh
source "$SCRIPT_DIR/build-xcframework.lib.sh"
xcf_lib_init

# Configuration -------------------------------------------------------------
FEATURES="full"
SWIFT_MODULE_NAME="FantasticKernelFull"
PKG_DIR="$RUST_ROOT/packaging/FantasticKernelFull"
XCF_NAME="Fantastic-Full.xcframework"

# Mac only. iOS device + simulator are explicitly excluded — full-tier
# bundles use subprocess / PTY syscalls the iOS sandbox forbids; shipping
# a `full` build to iOS would just produce a binary that crashes at runtime.
TARGETS_DEVICE=()
TARGETS_SIM=()
TARGETS_MAC=("aarch64-apple-darwin" "x86_64-apple-darwin")
# ---------------------------------------------------------------------------

build_xcframework_variant
