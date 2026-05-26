#!/usr/bin/env bash
#
# Cut a `fantastic` CLI release.
#
# Mirrors the historical `rust/scripts/release.sh` operator-driven
# pattern: ALL work happens locally on the maintainer's Mac. CI does
# lint only (no automated release workflow). The maintainer reviews
# the artifacts, then runs the printed `git tag` + `gh release
# create` commands themselves.
#
# Scope: macOS-only. Single universal binary (arm64 + x86_64). No
# Linux target — Swift cross-compile to Linux from macOS is brittle
# and the Python kernel covers non-Apple deployments.
#
# Usage:
#   swift/scripts/release.sh 0.1.0
#
# Pre-conditions:
#   - clean working tree
#   - on `main` (warns otherwise; doesn't block)
#   - all swift tests green
#
# Post-conditions:
#   - dist/fantastic-macos-universal.tar.gz
#   - dist/sha256sums.txt
#   - prints the two commands to run next (git tag + gh release)
#
# Signing: not done here. The binary is ad-hoc-signed by Swift's
# linker. For Developer ID + notarization, add a sign-and-notarize
# step before the tarball; the empty hook lives below at
# `sign_and_notarize`.

set -euo pipefail

# ── prelude ─────────────────────────────────────────────────────────

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <version>" >&2
    echo "       e.g. $0 0.1.0" >&2
    exit 2
fi

VERSION="$1"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$ ]]; then
    echo "error: version must be semver-ish (e.g. 0.1.0, 0.1.0-rc1)" >&2
    exit 2
fi

# Resolve to repo root regardless of cwd.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SWIFT_DIR="$REPO_ROOT/swift"
DIST_DIR="$REPO_ROOT/dist"

cd "$REPO_ROOT"

# ── pre-flight ──────────────────────────────────────────────────────

echo "==> pre-flight"

if ! git diff-index --quiet HEAD --; then
    echo "error: working tree has uncommitted changes" >&2
    git status --short >&2
    exit 1
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" != "main" ]]; then
    echo "warning: not on main (on '$BRANCH'); continuing anyway"
fi

if ! command -v swift >/dev/null 2>&1; then
    echo "error: swift not on PATH" >&2
    exit 1
fi

SWIFT_VERSION="$(swift --version | head -n1)"
echo "    swift:  $SWIFT_VERSION"
echo "    branch: $BRANCH"
echo "    version: $VERSION"

# ── quality gate ────────────────────────────────────────────────────

echo "==> quality gate"
(
    cd "$SWIFT_DIR"
    swift build -c release 2>&1 | tail -3
    swift test 2>&1 | tail -3
)

# ── build universal binary ──────────────────────────────────────────

echo "==> building universal binary"
(
    cd "$SWIFT_DIR"
    # `swift build` with two --arch flags produces a `lipo`'d
    # universal binary at `.build/apple/Products/Release/fantastic`.
    swift build -c release --arch arm64 --arch x86_64
)

UNIVERSAL_BIN="$SWIFT_DIR/.build/apple/Products/Release/fantastic"
if [[ ! -x "$UNIVERSAL_BIN" ]]; then
    echo "error: universal binary not at expected path: $UNIVERSAL_BIN" >&2
    echo "       (Swift toolchain layout may have changed; check .build/)" >&2
    exit 1
fi

echo "    built: $UNIVERSAL_BIN"
echo "    arches:"
lipo -info "$UNIVERSAL_BIN" | sed 's/^/      /'

# ── sign + notarize (hook left empty until Developer ID is wired) ──

sign_and_notarize() {
    # TODO: when ready to ship signed binaries:
    #
    #   codesign --force --options runtime --timestamp \
    #       --sign "Developer ID Application: <Team> (<TeamID>)" \
    #       "$1"
    #
    #   ditto -c -k --keepParent "$1" "$1.zip"
    #   xcrun notarytool submit "$1.zip" \
    #       --apple-id "$APPLE_ID" \
    #       --team-id "$TEAM_ID" \
    #       --password "$APP_PASSWORD" \
    #       --wait
    #   xcrun stapler staple "$1"
    #   rm "$1.zip"
    return 0
}

sign_and_notarize "$UNIVERSAL_BIN"

# ── stage + tarball ─────────────────────────────────────────────────

echo "==> staging release tree"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

STAGE="$DIST_DIR/stage"
mkdir -p "$STAGE"

cp "$UNIVERSAL_BIN" "$STAGE/fantastic"

# SwiftPM emits resource bundles as siblings of the executable when
# a target uses `.copy(...)`. They MUST travel with the binary or
# Bundle.module lookups fail at runtime (canvas.html, transport.js,
# Three.js, xterm.* would be missing).
BUNDLES_SRC="$(dirname "$UNIVERSAL_BIN")"
shopt -s nullglob
for bundle in "$BUNDLES_SRC"/*.bundle; do
    cp -R "$bundle" "$STAGE/"
done
shopt -u nullglob

echo "    staged:"
ls -la "$STAGE" | sed 's/^/      /'

TARBALL="$DIST_DIR/fantastic-macos-universal.tar.gz"
(cd "$STAGE" && tar -czf "$TARBALL" .)

(cd "$DIST_DIR" && shasum -a 256 "$(basename "$TARBALL")" > sha256sums.txt)

echo "==> built:"
echo "    $TARBALL"
echo "    $DIST_DIR/sha256sums.txt"
ls -lh "$DIST_DIR"/*.tar.gz "$DIST_DIR"/sha256sums.txt | sed 's/^/      /'

# ── next-step instructions ──────────────────────────────────────────

TAG="swift-v$VERSION"
cat <<EOF

==> NEXT STEPS (run by hand; this script doesn't push or release)

  # 1. tag + push
  git tag $TAG
  git push origin $TAG

  # 2. cut the GitHub release with the universal tarball + checksums
  gh release create $TAG \\
      --title "fantastic CLI $VERSION" \\
      --notes "macOS universal (arm64 + x86_64). See swift/RELEASING.md." \\
      "$TARBALL" \\
      "$DIST_DIR/sha256sums.txt"

  # download URL after release:
  #   https://github.com/Alexadar/fantastic_canvas/releases/download/$TAG/fantastic-macos-universal.tar.gz

EOF
