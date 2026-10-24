#!/bin/sh
# no_source_writes.sh — two parallel builds of fantastic-canvas-page against ONE
# checkout, each with its own CARGO_TARGET_DIR, must both succeed and leave the
# source tree untouched (git status --porcelain empty). Run from anywhere:
#   sh src/tui/bundles/fantastic-canvas-page/tests/no_source_writes.sh
# Run it on a clean tree (commit or stash first), or the final check fails.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CRATE=$(CDPATH= cd -- "$HERE/.." && pwd)
REPO=$(git -C "$CRATE" rev-parse --show-toplevel)
T1=$(mktemp -d)
T2=$(mktemp -d)
trap 'rm -rf "$T1" "$T2"' EXIT

cd "$CRATE"
CARGO_TARGET_DIR="$T1" cargo build -q > "$T1.log" 2>&1 &
P1=$!
CARGO_TARGET_DIR="$T2" cargo build -q > "$T2.log" 2>&1 &
P2=$!

FAIL=0
wait "$P1" || { echo "build 1 failed:"; cat "$T1.log"; FAIL=1; }
wait "$P2" || { echo "build 2 failed:"; cat "$T2.log"; FAIL=1; }
rm -f "$T1.log" "$T2.log"
[ "$FAIL" = 0 ] || exit 1

DIRTY=$(git -C "$REPO" status --porcelain)
if [ -n "$DIRTY" ]; then
  echo "source tree was written by the build:" >&2
  echo "$DIRTY" >&2
  exit 1
fi
echo "ok: two parallel builds, source tree clean"
