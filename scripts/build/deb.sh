#!/bin/sh
# The desktop app's Debian package, for the release and as the input of other images.
#
#   sh scripts/build/deb.sh --target <CLI triple>
#
# Writes to `dist/deb/`:
#   fantastic-app-<app triple>.deb       `aisixteen-fantastic`: the desktop app and
#                                        its sidecar, from `tauri bundle --bundles deb`
#
# `--out` is not supported: the packages always go to `dist/deb/`.
# `--target` is the CLI's triple, as in `app.sh`; the app is built for glibc
# where the CLI is musl. Both inputs must already be where `app.sh` and
# `cli.sh` leave them (the `app-<app triple>` and `release-<CLI triple>` CI
# artifacts, unpacked): the packages carry those bytes, unchanged. Release
# packages only. Unsigned: signing is the maintainer's, by hand (RELEASING.md).
. "$(dirname "$0")/common.sh"
parse_args "$@"

[ -z "$OUT" ] || { echo "deb.sh: --out is not supported: the packages go to dist/deb/" >&2; exit 2; }
[ "$PROFILE" = release ] || { echo "deb.sh: release packages only (--profile $PROFILE)" >&2; exit 2; }
[ -n "$TARGET" ] || { echo "deb.sh: --target <CLI triple> is required" >&2; usage >&2; exit 2; }
case "$TARGET" in
  *-unknown-linux-musl) APP_TARGET="${TARGET%-musl}-gnu" ;;
  *-unknown-linux-gnu) APP_TARGET="$TARGET" ;;
  *) echo "deb.sh: Debian packages are for Linux targets, not '$TARGET'" >&2; exit 2 ;;
esac

VERSION="${FANTASTIC_VERSION:-$(git -C "$REPO" describe --tags --abbrev=0 | sed 's/^v//')}"
[ -n "$VERSION" ] || { echo "deb.sh: no version (no tag, and FANTASTIC_VERSION is unset)" >&2; exit 1; }

APP="$REPO/src/app"
APP_BIN="$APP/target/$APP_TARGET/release/fantastic-app"
SIDECAR="$APP/binaries/fantastic-$APP_TARGET"
[ -f "$APP_BIN" ] || {
  echo "deb.sh: $APP_BIN is missing: unpack the app-$APP_TARGET artifact there, or run: sh scripts/build/app.sh --target $TARGET" >&2
  exit 1
}
[ -f "$SIDECAR" ] || {
  echo "deb.sh: $SIDECAR is missing: unpack the release-$TARGET archive's fantastic there, or run: sh scripts/build/cli.sh --target $TARGET" >&2
  exit 1
}

DEB_DIR="$REPO/dist/deb"
mkdir -p "$DEB_DIR"

# ── the app's .deb ──────────────────────────────────────────────────────────
say "npm ci"
npm ci --prefix "$APP" --no-audit --no-fund
say "tauri bundle --bundles deb (version $VERSION)"
( cd "$APP" && npx tauri bundle --ci --bundles deb --target "$APP_TARGET" --no-sign --no-binary-patching \
    --config "{\"version\":\"$VERSION\"}" )
BUNDLE_GLOB="$APP/target/$APP_TARGET/release/bundle/deb/*.deb"
# the file is named "<productName>_<version>_<arch>.deb", space included: only
# the star is left to the shell
set -- "$APP/target/$APP_TARGET/release/bundle/deb/"*.deb
{ [ "$#" -eq 1 ] && [ -f "$1" ]; } || { echo "deb.sh: $BUNDLE_GLOB matched $# files, not 1: $*" >&2; exit 1; }
APP_DEB="$DEB_DIR/fantastic-app-$APP_TARGET.deb"
cp "$1" "$APP_DEB"

say "wrote $APP_DEB"
echo "$APP_DEB"
