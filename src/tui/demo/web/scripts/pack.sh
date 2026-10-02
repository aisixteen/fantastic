#!/bin/sh
# pack.sh — the demo as ONE self-contained file: dist/index.html, with its
# stylesheet and its script inlined. It opens from file://, deploys as a static
# site anywhere, and is what an html agent can hold.
#
# The script is bundled by the library's packer (src/lib/ts/scripts/pack.sh:
# the vendored, sha256-pinned esbuild — NO npm, NO node); this script then
# inlines it and the css into the page template.
#
#   sh scripts/pack.sh            # → dist/index.html  (PACK_PROFILE=debug: unminified)
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
APP=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$APP"

sh ../../../lib/ts/scripts/pack.sh src/main.ts dist/demo.zip readme.md >/dev/null
BUNDLE="dist/pack/bundle.min.js"
[ -s "$BUNDLE" ] || { echo "pack.sh: no bundle at $BUNDLE" >&2; exit 1; }

# a literal </script> inside the bundle would end the inline script early
if grep -q '</script' "$BUNDLE"; then
  echo "pack.sh: the bundle contains '</script' — it cannot be inlined." >&2
  exit 1
fi

OUT="dist/index.html"
awk -v css="src/style.css" -v js="$BUNDLE" '
  function slurp(f,   l, s) { s = ""; while ((getline l < f) > 0) s = s l "\n"; close(f); return s }
  index($0, "/*STYLE*/")  { sub(/\/\*STYLE\*\//, "");  printf "%s", substr($0, 1, index($0, "</style>") - 1); printf "%s", slurp(css); print substr($0, index($0, "</style>")); next }
  index($0, "/*SCRIPT*/") { sub(/\/\*SCRIPT\*\//, ""); printf "%s", substr($0, 1, index($0, "</script>") - 1); printf "%s", slurp(js); print substr($0, index($0, "</script>")); next }
  { print }
' src/page.html > "$OUT"
rm -f dist/demo.zip
rm -rf dist/pack
echo "pack.sh: demo → $APP/$OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
