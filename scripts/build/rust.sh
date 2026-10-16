#!/bin/sh
# The rust kernel host binary. This script IS the release recipe, run locally.
#
#   cargo build [--release] --bin fantastic_kernel   (cwd src/lib/rust)
#   tar -czf <out> -C target/<profile> fantastic_kernel
#
# FANTASTIC_BUILD_FEATURES adds cargo features (e2e passes `scenario-probes`,
# which a shipped kernel deliberately does not carry).
. "$(dirname "$0")/common.sh"
parse_args "$@"
default_out rust

set -- --bin fantastic_kernel
[ "$PROFILE" = release ] && set -- --release "$@"
[ -n "${FANTASTIC_BUILD_FEATURES:-}" ] && set -- "$@" --features "$FANTASTIC_BUILD_FEATURES"

say "cargo build $*"
( cd "$REPO/src/lib/rust" && cargo build "$@" )
tar -czf "$OUT" -C "$REPO/src/lib/rust/target/$PROFILE" "fantastic_kernel$EXE"
say "wrote $OUT"
echo "$OUT"
