#!/usr/bin/env bash
# build-xcframework.sh — convenience: build BOTH variants in one shot.
#
# Replaces the old monolithic builder. The actual logic now lives in:
#   - build-xcframework.lib.sh           — shared functions
#   - build-xcframework-embedded.sh      — sandboxed/iOS tier (Lite)
#   - build-xcframework-full.sh          — desktop/PTY tier (Pro)
#
# This wrapper runs both in sequence. Two XCFrameworks land in:
#   - packaging/FantasticKernelEmbedded/Fantastic-Embedded.xcframework
#   - packaging/FantasticKernelFull/Fantastic-Full.xcframework
#
# Skip the wrapper and invoke a single variant if you only want one.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "================================================================"
echo "  XCFramework build — variant 1/2: embedded (iOS-safe)"
echo "================================================================"
"$SCRIPT_DIR/build-xcframework-embedded.sh"

echo
echo "================================================================"
echo "  XCFramework build — variant 2/2: full (Mac-only, PTY tier)"
echo "================================================================"
"$SCRIPT_DIR/build-xcframework-full.sh"

echo
echo "================================================================"
echo "  ✓ Both XCFrameworks built. Consume from apple/project.yml as:"
echo "    FantasticLite → FantasticKernelEmbedded"
echo "    FantasticPro  → FantasticKernelFull"
echo "================================================================"
