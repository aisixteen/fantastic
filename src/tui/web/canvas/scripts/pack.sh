#!/bin/sh
# pack.sh — the canvas page as a sovereign artifact: dist/canvas.zip =
# readme.md (the revive guide) + ONE bundle.min.js (the page, its views, the ts
# kernel library, three + xterm + xterm.css — all inlined, no chunks) +
# bundle.min.js.map.
#
# The packing itself is the library's (`src/lib/ts/scripts/pack.sh`, with its
# vendored, sha256-pinned esbuild — NO npm, NO node); this script names what the
# CANVAS packs: its entry, its output, its readme, and the flags its vendors
# need. Then it checks that the vendors really are inside.
#
#   sh scripts/pack.sh            # → dist/canvas.zip  (PACK_PROFILE=debug: unminified)
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
APP=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$APP"

ZIP="dist/canvas.zip"
sh ../../../lib/ts/scripts/pack.sh src/bundle.ts "$ZIP" readme.md \
  --alias:three=./src/vendor/three.module.js \
  --alias:@xterm/xterm=./src/vendor/xterm.js \
  --alias:@xterm/addon-fit=./src/vendor/addon-fit.js \
  --alias:@xterm/addon-webgl=./src/vendor/addon-webgl.js \
  --loader:.css=text \
  --loader:.html=text

BUNDLE="dist/pack/bundle.min.js"

# ── no residual bare vendor imports (all must be inlined) ───────────────────
if grep -Eq 'import\(["'\''](three|@xterm/)' "$BUNDLE" \
   || grep -Eq 'from[[:space:]]*"(three|@xterm/)' "$BUNDLE"; then
  echo "pack.sh: residual bare vendor import in bundle — NOT fully inlined." >&2
  exit 1
fi

# ── sanity: vendors actually inlined (three ~1.2MB + xterm ~0.5MB) ──────────
BYTES=$(wc -c < "$BUNDLE" | tr -d ' ')
if [ "$BYTES" -lt 500000 ]; then
  echo "pack.sh: bundle is only ${BYTES}B — vendors likely NOT inlined." >&2
  exit 1
fi
echo "pack.sh: canvas → $APP/$ZIP"
