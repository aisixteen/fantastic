#!/bin/sh
# Every runtime's artifact, one profile. `swift` is skipped off macOS.
. "$(dirname "$0")/common.sh"
parse_args "$@"
[ -z "$OUT" ] || { echo "build/all: --out names ONE file; drop it" >&2; exit 2; }

for rt in python rust ts; do sh "$REPO/scripts/build/$rt.sh" --profile "$PROFILE"; done
[ "$(uname -s)" = Darwin ] && sh "$REPO/scripts/build/swift.sh" --profile "$PROFILE"
