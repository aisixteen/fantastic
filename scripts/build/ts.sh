#!/bin/sh
# The js KERNEL LIBRARY archive — the ts lib's public entry (src/index.ts:
# Kernel, WsBridge, ProxyLoader, Host, ram_store, the AI backend, `boot`) as one
# browser ESM bundle + its readme. NO page: an app (the CLI's canvas, in
# src/tui/web/canvas) packs its own page on top. This script IS the release
# recipe, run locally.
#
# `release` minifies; `debug` is the same bundle unminified, so a stack trace
# names real identifiers. pack.sh writes src/lib/ts/dist/js_kernel.zip; --out
# copies it where the caller asked.
. "$(dirname "$0")/common.sh"
parse_args "$@"
default_out ts zip

say "pack.sh (PACK_PROFILE=$PROFILE)"
( cd "$REPO/src/lib/ts" && PACK_PROFILE="$PROFILE" sh scripts/pack.sh src/index.ts dist/js_kernel.zip readme.md >/dev/null )
[ "$OUT" = "$REPO/src/lib/ts/dist/js_kernel.zip" ] || cp "$REPO/src/lib/ts/dist/js_kernel.zip" "$OUT"
say "wrote $OUT"
echo "$OUT"
