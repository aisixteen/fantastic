#!/bin/sh
# The swift kernel host binary. Mirrors the `swift` job in
# .github/workflows/release.yml.
#
#   swift build -c <profile> --product fantastic_kernel   (cwd src/lib/swift)
#   tar -czf <out> -C .build/<profile> fantastic_kernel *.bundle
#
# THE BUNDLES SHIP WITH THE BINARY. SwiftPM emits every target's `resources:`
# as a sibling `<Package>_<Target>.bundle` directory, and the generated
# `resource_bundle_accessor` looks for it BESIDE THE EXECUTABLE first, then in
# the build tree. Packaging the binary alone therefore appears to work on the
# machine that built it and dies anywhere else:
#   Fatal error: could not load resource bundle
# Measured 2026-09-03, after the build tree was pruned — the daemon crashed on
# boot (the root readme and the web assets live in those bundles), while the
# one-shot `reflect` path, which touches no resource, still answered. A release
# tarball had the same hole: nothing to fall back to on a user's machine.
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
# `*.bundle` is expanded by the SHELL inside the product dir, so the tarball
# carries whatever set that build actually produced — no hand-maintained list to
# drift when a target gains a resource.
( cd "$REPO/src/lib/swift/.build/$PROFILE" \
  && tar -czf "$OUT" fantastic_kernel *.bundle )
say "wrote $OUT"
echo "$OUT"
