#!/bin/sh
# The desktop app (`src/app`, Tauri 2) — ONE place builds the shell.
#
#   sh scripts/build/app.sh [--profile debug|release] --target <CLI triple> [--out <tar>]
#
# `--target` is the CLI's triple: the app is built for the same OS and arch, on
# glibc where the CLI is musl (the app links WebKitGTK and glibc; the static musl
# CLI runs on any glibc system). The CLI must already be built
# (`scripts/build/cli.sh`): it is the app's canvas sidecar, staged as
# `src/app/binaries/fantastic-<app triple>` for `externalBin`.
#
# `tauri build` adds the `tauri/custom-protocol` feature a production app needs,
# so never ship a plain `cargo build`. The build links the home kernel, which
# packs the canvas page: it needs Go, and `zip` on Windows.
#
# Writes `dist/fantastic-app-<app triple>.tar` holding `fantastic-app[.exe]`.
. "$(dirname "$0")/common.sh"
parse_args "$@"

[ -n "$TARGET" ] || { echo "app.sh: --target <CLI triple> is required" >&2; usage >&2; exit 2; }
case "$TARGET" in
  *-unknown-linux-musl) APP_TARGET="${TARGET%-musl}-gnu" ;;
  *) APP_TARGET="$TARGET" ;;
esac
case "$TARGET" in
  *-windows-*) BIN_EXE=.exe ;;
  *) BIN_EXE="" ;;
esac

CLI="$REPO/src/target/$TARGET/$PROFILE/fantastic$BIN_EXE"
if [ ! -f "$CLI" ]; then
  echo "build the CLI first: sh scripts/build/cli.sh --profile $PROFILE --target $TARGET" >&2
  exit 1
fi
APP="$REPO/src/app"
mkdir -p "$APP/binaries"
cp "$CLI" "$APP/binaries/fantastic-$APP_TARGET$BIN_EXE"
say "staged the canvas sidecar: binaries/fantastic-$APP_TARGET$BIN_EXE"

VERSION="${FANTASTIC_VERSION:-$(git -C "$REPO" describe --tags --abbrev=0 | sed 's/^v//')}"
[ -n "$VERSION" ] || { echo "app.sh: no version (no tag, and FANTASTIC_VERSION is unset)" >&2; exit 1; }

say "npm ci"
npm ci --prefix "$APP" --no-audit --no-fund

set -- build --no-bundle --ci --target "$APP_TARGET" --config "{\"version\":\"$VERSION\"}"
[ "$PROFILE" = debug ] && set -- "$@" --debug
say "tauri $* (version $VERSION)"
( cd "$APP" && npx tauri "$@" )

[ -n "$OUT" ] || OUT="$REPO/dist/fantastic-app-$APP_TARGET.tar"
default_out app tar
rm -f "$OUT"
tar -cf "$OUT" -C "$APP/target/$APP_TARGET/$PROFILE" "fantastic-app$BIN_EXE"
say "wrote $OUT"
echo "$OUT"
