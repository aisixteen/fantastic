#!/bin/sh
# The `fantastic` CLI product — what a GitHub Release ships: ONE archive holding
# the `fantastic` binary, built from `src/tui` with the release's backends.
#
#   sh scripts/build/cli.sh [--profile debug|release] [--target <triple>] [--out <archive>]
#
# `--target` defaults to the host's. The archive is `dist/fantastic-<target>.tar.gz`
# (`.zip` for a Windows target), the binary at its root. The binary itself stays
# at `src/target/<target>/<profile>/fantastic`, where `e2e/build.py` takes it from.
. "$(dirname "$0")/common.sh"
parse_args "$@"

#: The backends a release links (crate.sh links the same set).
FEATURES="vllm,ollama,responses,anthropic"

[ -n "$TARGET" ] || TARGET="$(rustc -vV | sed -n 's/^host: //p')"
case "$TARGET" in
  *-windows-*) BIN_EXE=.exe; EXT=zip ;;
  *) BIN_EXE=""; EXT=tar.gz ;;
esac
[ -n "$OUT" ] || OUT="$REPO/dist/fantastic-$TARGET.$EXT"
default_out cli "$EXT"

set -- --locked -p fantastic-tui --features "$FEATURES" --target "$TARGET"
[ "$PROFILE" = release ] && set -- --release "$@"
say "cargo build $*"
( cd "$REPO/src" && cargo build "$@" )

DIR="$REPO/src/target/$TARGET/$PROFILE"
rm -f "$OUT"
case "$EXT" in
  zip) ( cd "$DIR" && zip -q -j "$OUT" "fantastic$BIN_EXE" ) ;;
  *) tar -czf "$OUT" -C "$DIR" "fantastic$BIN_EXE" ;;
esac
say "wrote $OUT"
echo "$OUT"
