#!/bin/sh
# pack.sh — roll the ts/ frontend kernel into a sovereign, self-describing
# artifact: dist/js_kernel.zip = readme.md + ONE bundle.min.js (all TS + three +
# xterm + xterm.css inlined, no chunks) + bundle.min.js.map.
#
# NO npm, NO node, NO Python. The packer is the vendored, sha256-pinned esbuild
# Go binary under tools/esbuild/ (see tools/esbuild/README.md). The only other
# deps are coreutils: shasum/sha256sum, awk, grep, sed, zip.
#
# An LLM revives the artifact by reading ONLY readme.md out of the zip (unzip -l
# + unzip -p) and serving bundle.min.js through the host's generic `file` agent —
# see readme.md.
set -eu

# ── locate ts/ (this script lives in ts/scripts/) ────────────────────────────
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TS=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$TS"

OUT="dist/pack"              # isolated staging dir (keeps dev tsc output out)
ZIP="dist/js_kernel.zip"
PLACEHOLDER="__BUNDLE_SHA256__"

# ── portable sha256 (darwin: shasum -a 256 / linux: sha256sum) ───────────────
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ── select the vendored esbuild for this platform ───────────────────────────
case "$(uname -sm)" in
  "Darwin arm64")   PLAT=darwin-arm64 ;;
  "Darwin x86_64")  PLAT=darwin-x64 ;;
  "Linux x86_64")   PLAT=linux-x64 ;;
  "Linux aarch64")  PLAT=linux-arm64 ;;
  *) echo "pack.sh: unsupported platform '$(uname -sm)'. Vendor esbuild for it" \
          "under tools/esbuild/<platform>/ and add it to SHA256SUMS." >&2; exit 1 ;;
esac
ESBUILD="tools/esbuild/$PLAT/esbuild"
SUMS="tools/esbuild/SHA256SUMS"
if [ ! -x "$ESBUILD" ]; then
  echo "pack.sh: missing vendored esbuild: $ESBUILD" >&2
  echo "  rebuild it (sha-verified below) — see tools/esbuild/README.md:" >&2
  echo "    GOBIN=\"\$PWD/tools/esbuild/$PLAT\" go install github.com/evanw/esbuild/cmd/esbuild@v0.25.0" >&2
  exit 1
fi

# ── integrity gate: refuse a tampered/unpinned packer ───────────────────────
EXPECTED=$(awk -v p="$PLAT/esbuild" '$2==p {print $1}' "$SUMS")
[ -n "$EXPECTED" ] || { echo "pack.sh: no SHA256SUMS entry for $PLAT/esbuild" >&2; exit 1; }
GOT=$(sha256 "$ESBUILD")
if [ "$EXPECTED" != "$GOT" ]; then
  echo "pack.sh: esbuild sha256 MISMATCH — refusing to run." >&2
  echo "  expected $EXPECTED" >&2
  echo "  got      $GOT" >&2
  exit 1
fi
echo "pack.sh: esbuild $("$ESBUILD" --version) ($PLAT) verified."

# ── bundle: one file, everything inlined, no chunks ─────────────────────────
rm -rf "$OUT"; mkdir -p "$OUT"
"$ESBUILD" src/bundle.ts \
  --bundle --format=esm --platform=browser --target=esnext \
  --minify --sourcemap \
  --alias:three=./src/vendor/three.module.js \
  --alias:@xterm/xterm=./src/vendor/xterm.js \
  --alias:@xterm/addon-fit=./src/vendor/addon-fit.js \
  --loader:.css=text \
  --outfile="$OUT/bundle.min.js"

# ── assert exactly one .js (no chunk files) ─────────────────────────────────
JS_COUNT=$(ls "$OUT"/*.js 2>/dev/null | wc -l | tr -d ' ')
if [ "$JS_COUNT" != "1" ]; then
  echo "pack.sh: expected exactly one .js, found $JS_COUNT (chunks?):" >&2
  ls "$OUT"/*.js >&2; exit 1
fi
[ -f "$OUT/bundle.min.js.map" ] || { echo "pack.sh: missing sourcemap" >&2; exit 1; }

# ── assert no residual bare vendor imports (all must be inlined) ─────────────
if grep -Eq 'import\(["'\''](three|@xterm/)' "$OUT/bundle.min.js" \
   || grep -Eq 'from[[:space:]]*"(three|@xterm/)' "$OUT/bundle.min.js"; then
  echo "pack.sh: residual bare vendor import in bundle — NOT fully inlined." >&2
  exit 1
fi

# ── sanity: vendors actually inlined (three ~1.2MB + xterm ~0.5MB) ──────────
BYTES=$(wc -c < "$OUT/bundle.min.js" | tr -d ' ')
if [ "$BYTES" -lt 500000 ]; then
  echo "pack.sh: bundle is only ${BYTES}B — vendors likely NOT inlined." >&2
  exit 1
fi

# ── stamp the bundle's sha256 into the readme (zip copy only) ───────────────
BUNDLE_SHA=$(sha256 "$OUT/bundle.min.js")
sed "s/$PLACEHOLDER/$BUNDLE_SHA/g" readme.md > "$OUT/readme.md"

# ── zip (flat: readme.md + bundle.min.js + bundle.min.js.map) ───────────────
rm -f "$ZIP"
( cd "$OUT" && zip -q -j "$TS/$ZIP" readme.md bundle.min.js bundle.min.js.map )

echo "pack.sh: wrote $ZIP"
echo "  bundle.min.js  ${BYTES}B  sha256=$BUNDLE_SHA"
unzip -l "$ZIP"
