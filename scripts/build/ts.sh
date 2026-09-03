#!/bin/sh
# The browser kernel archive. Mirrors the `js` job in
# .github/workflows/release.yml, which runs this same `pack.sh`.
#
# `release` minifies; `debug` is the same bundle unminified, so a stack trace
# names real identifiers. pack.sh writes src/lib/ts/dist/js_kernel.zip; --out
# copies it where the caller asked.
. "$(dirname "$0")/common.sh"
parse_args "$@"
default_out ts zip

say "pack.sh (PACK_PROFILE=$PROFILE)"
( cd "$REPO/src/lib/ts" && PACK_PROFILE="$PROFILE" sh scripts/pack.sh >/dev/null )
[ "$OUT" = "$REPO/src/lib/ts/dist/js_kernel.zip" ] || cp "$REPO/src/lib/ts/dist/js_kernel.zip" "$OUT"
say "wrote $OUT"
echo "$OUT"
