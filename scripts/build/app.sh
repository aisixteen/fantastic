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
# This is a plain Cargo build. `tauri build` adds the `tauri/custom-protocol`
# feature a production app needs, and so does this script: a build without that
# feature loads the dev URL instead of the bundled page, so never ship one. The
# version reaches `tauri.conf.json` through the `TAURI_CONFIG` env var (a JSON
# string tauri-build and tauri-codegen merge into the parsed config:
# tauri-build lib.rs, tauri-codegen lib.rs, tauri-utils acl/build.rs). The build
# links the home kernel, which packs the canvas page: it needs Go, and `zip` on Windows.
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

# --target-dir pins the output to src/app/target even when CARGO_TARGET_DIR is set
set -- build --locked --target "$APP_TARGET" --target-dir "$APP/target" --features tauri/custom-protocol
[ "$PROFILE" = debug ] || set -- "$@" --release
say "cargo $* (version $VERSION)"
( cd "$APP" && TAURI_CONFIG="{\"version\":\"$VERSION\"}" cargo "$@" )

[ -n "$OUT" ] || OUT="$REPO/dist/fantastic-app-$APP_TARGET.tar"
default_out app tar
rm -f "$OUT"
tar -cf "$OUT" -C "$APP/target/$APP_TARGET/$PROFILE" "fantastic-app$BIN_EXE"
say "wrote $OUT"
echo "$OUT"
