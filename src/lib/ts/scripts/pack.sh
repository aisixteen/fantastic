#!/bin/sh
# pack.sh — roll an APP's page into a sovereign, self-describing artifact with
# the library's vendored esbuild: <out.zip> = readme.md + ONE bundle.min.js (the
# entry and everything it imports — the library included — inlined, no chunks)
# + bundle.min.js.map.
#
# The library has no page of its own: the APP names its entry, its output and
# its readme (the revive guide that travels inside the zip), and passes any
# esbuild flags its page needs (vendor aliases, loaders) after them:
#
#   sh pack.sh <entry.ts> <out.zip> <readme.md> [esbuild flags …]
#
# e.g. the canvas app (src/tui/web/canvas/scripts/pack.sh):
#   sh ../../../lib/ts/scripts/pack.sh src/bundle.ts dist/canvas.zip readme.md \
#     --alias:three=./src/vendor/three.module.js --loader:.css=text …
#
# Paths are relative to the CALLER's directory. PACK_PROFILE=release (default,
# minified) | debug (the same bundle, unminified — real identifiers in traces).
#
# NO npm, NO node, NO Python. The packer is the vendored, sha256-pinned esbuild
# Go binary under tools/esbuild/ (see tools/esbuild/README.md). The only other
# deps are coreutils: shasum/sha256sum, awk, grep, sed, zip (on Windows: Git for
# Windows' sh and tools, plus Info-ZIP `zip`).
set -eu

if [ "$#" -lt 3 ]; then
  echo "usage: sh pack.sh <entry.ts> <out.zip> <readme.md> [esbuild flags …]" >&2
  echo "  the library has no default page — the app names what it packs" >&2
  exit 2
fi

# ── the caller's paths, made absolute before anything moves ─────────────────
CALLER=$(pwd)
# A drive-letter path (Windows, from build.rs) is absolute too; its backslashes
# become slashes so Git-for-Windows' sh and esbuild both read D:/a/b.
abs() { case "$1" in /*) printf '%s\n' "$1" ;; [A-Za-z]:[/\\]*) printf '%s\n' "$1" | tr '\\' '/' ;; *) printf '%s\n' "$CALLER/$1" ;; esac; }
ENTRY=$(abs "$1")
ZIP=$(abs "$2")
README=$(abs "$3")
shift 3
[ -f "$ENTRY" ] || { echo "pack.sh: no entry at $ENTRY" >&2; exit 1; }
[ -f "$README" ] || { echo "pack.sh: no readme at $README" >&2; exit 1; }
OUT="$(dirname "$ZIP")/pack"   # isolated staging dir beside the zip
PLACEHOLDER="__BUNDLE_SHA256__"

# ── locate the library (this script lives in <lib>/scripts/) ────────────────
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
LIB=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

# ── portable sha256 (darwin: shasum -a 256 / linux: sha256sum) ───────────────
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ── select the vendored esbuild for this platform ───────────────────────────
# Windows runs this under Git for Windows' sh (MSYS2), whose `uname -s` names
# the MSYS2 flavour and the NT build; Go names the binary `esbuild.exe` there.
EXE=""
case "$(uname -sm)" in
  "Darwin arm64")   PLAT=darwin-arm64 ;;
  "Darwin x86_64")  PLAT=darwin-x64 ;;
  "Linux x86_64")   PLAT=linux-x64 ;;
  "Linux aarch64")  PLAT=linux-arm64 ;;
  MINGW64_NT*" x86_64"|MSYS_NT*" x86_64") PLAT=win32-x64; EXE=.exe ;;
  *) echo "pack.sh: unsupported platform '$(uname -sm)'. Vendor esbuild for it" \
          "under tools/esbuild/<platform>/ and add it to SHA256SUMS." >&2; exit 1 ;;
esac
ESBUILD="$LIB/tools/esbuild/$PLAT/esbuild$EXE"
SUMS="$LIB/tools/esbuild/SHA256SUMS"
# PROVISION it from pinned source when absent (a clean checkout, `cargo install`):
# the pinned esbuild release, built by the pinned Go toolchain, reproducibly
# (`-trimpath`, no cgo) — so the binary it makes is the one SHA256SUMS names,
# and the gate below verifies it like a vendored one. Go fetches the toolchain
# and the module through its checksum database. Needs Go ≥ 1.21 on PATH.
ESBUILD_VERSION=v0.25.0
GO_TOOLCHAIN=go1.23.4
if [ ! -x "$ESBUILD" ]; then
  if ! command -v go >/dev/null 2>&1; then
    echo "pack.sh: no esbuild at $ESBUILD, and no Go to build it." >&2
    echo "  install Go ≥ 1.21 (https://go.dev/dl/) and build again — it is built" >&2
    echo "  once, from pinned source, and sha-verified (tools/esbuild/README.md)." >&2
    exit 1
  fi
  GO_MINOR=$(go env GOVERSION | sed -n 's/^go1\.\([0-9]*\).*/\1/p')
  if [ -z "$GO_MINOR" ] || [ "$GO_MINOR" -lt 21 ]; then
    echo "pack.sh: Go $(go env GOVERSION) cannot fetch $GO_TOOLCHAIN (GOTOOLCHAIN needs Go ≥ 1.21)." >&2
    exit 1
  fi
  echo "pack.sh: building esbuild $ESBUILD_VERSION with $GO_TOOLCHAIN for ${PLAT}…" >&2
  GOTOOLCHAIN=$GO_TOOLCHAIN CGO_ENABLED=0 GOFLAGS=-trimpath \
    GOBIN="$LIB/tools/esbuild/$PLAT" \
    go install "github.com/evanw/esbuild/cmd/esbuild@$ESBUILD_VERSION"
fi

# ── integrity gate: refuse a tampered/unpinned packer ───────────────────────
EXPECTED=$(awk -v p="$PLAT/esbuild$EXE" '$2==p {print $1}' "$SUMS")
[ -n "$EXPECTED" ] || { echo "pack.sh: no SHA256SUMS entry for $PLAT/esbuild$EXE" >&2; exit 1; }
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
case "${PACK_PROFILE:-release}" in
  release) MIN_FLAG="--minify" ;;
  debug)   MIN_FLAG="" ;;
  *) echo "pack.sh: PACK_PROFILE must be release|debug, got '$PACK_PROFILE'" >&2; exit 1 ;;
esac
echo "pack.sh: profile ${PACK_PROFILE:-release}."

# The app's own flags ride after ours; they resolve against the CALLER's dir.
( cd "$CALLER" && "$ESBUILD" "$ENTRY" \
  --bundle --format=esm --platform=browser --target=esnext \
  $MIN_FLAG --sourcemap \
  "$@" \
  --outfile="$OUT/bundle.min.js" )

# ── assert exactly one .js (no chunk files) ─────────────────────────────────
JS_COUNT=$(ls "$OUT"/*.js 2>/dev/null | wc -l | tr -d ' ')
if [ "$JS_COUNT" != "1" ]; then
  echo "pack.sh: expected exactly one .js, found $JS_COUNT (chunks?):" >&2
  ls "$OUT"/*.js >&2; exit 1
fi
[ -f "$OUT/bundle.min.js.map" ] || { echo "pack.sh: missing sourcemap" >&2; exit 1; }

# ── stamp the bundle's sha256 into the readme (zip copy only) ───────────────
BUNDLE_SHA=$(sha256 "$OUT/bundle.min.js")
sed "s/$PLACEHOLDER/$BUNDLE_SHA/g" "$README" > "$OUT/readme.md"

# ── zip (flat: readme.md + bundle.min.js + bundle.min.js.map) ───────────────
mkdir -p "$(dirname "$ZIP")"
rm -f "$ZIP"
( cd "$OUT" && zip -q -j "$ZIP" readme.md bundle.min.js bundle.min.js.map )

BYTES=$(wc -c < "$OUT/bundle.min.js" | tr -d ' ')
echo "pack.sh: wrote $ZIP"
echo "  bundle.min.js  ${BYTES}B  sha256=$BUNDLE_SHA"
unzip -l "$ZIP"
