#!/bin/sh
# The swift kernel host binary. Mirrors the `swift` job in
# .github/workflows/release.yml.
#
#   swift build -c <profile> --product fantastic_kernel   (cwd src/lib/swift)
#   tar -czf <out> -C .build/<profile> fantastic_kernel
#
# FANTASTIC_BUILD_SWIFTC adds -Xswiftc flags (e2e passes -DSCENARIO_PROBES,
# which a shipped kernel deliberately does not carry).
. "$(dirname "$0")/common.sh"
parse_args "$@"
default_out swift

set -- -c "$PROFILE" --product fantastic_kernel
for f in ${FANTASTIC_BUILD_SWIFTC:-}; do set -- "$@" -Xswiftc "$f"; done

say "swift build $*"
( cd "$REPO/src/lib/swift" && swift build "$@" )
tar -czf "$OUT" -C "$REPO/src/lib/swift/.build/$PROFILE" fantastic_kernel
say "wrote $OUT"
echo "$OUT"
