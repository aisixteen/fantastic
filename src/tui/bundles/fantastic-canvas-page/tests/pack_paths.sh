#!/bin/sh
# pack_paths.sh — the pack scripts' path handling and sanity checks, with NO real
# esbuild (none is vendored in a clean checkout), so it runs on any POSIX sh:
#   1. the library's abs() makes POSIX, relative and drive-letter zip paths
#      absolute (build.rs hands it D:/… on Windows);
#   2. the canvas pack.sh checks the bundle where the library stages it, and
#      fails when that bundle is missing, tiny, or still imports a vendor.
# A stub library stands in for the real packer and writes the bundle where the
# real one stages it: $(dirname ZIP)/pack/bundle.min.js.
# Run from anywhere: sh src/tui/bundles/fantastic-canvas-page/tests/pack_paths.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CRATE=$(CDPATH= cd -- "$HERE/.." && pwd)
SRC=$(CDPATH= cd -- "$CRATE/../../.." && pwd)
LIB_PACK="$SRC/lib/ts/scripts/pack.sh"
CANVAS_PACK="$SRC/tui/web/canvas/scripts/pack.sh"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
FAIL=0

# ── 1. abs() in the library, extracted and run as written ───────────────────
ABS_LINE=$(grep '^abs() ' "$LIB_PACK")
check_abs() {
  got=$(CALLER=/caller; eval "$ABS_LINE"; abs "$1")
  if [ "$got" = "$2" ]; then printf 'ok: abs %s -> %s\n' "$1" "$got"
  else printf 'FAIL: abs %s -> %s, want %s\n' "$1" "$got" "$2" >&2; FAIL=1; fi
}
check_abs '/tmp/x/canvas.zip' '/tmp/x/canvas.zip'
check_abs 'out/canvas.zip' '/caller/out/canvas.zip'
check_abs 'D:/a/b/canvas.zip' 'D:/a/b/canvas.zip'
check_abs 'D:\a\b\canvas.zip' 'D:/a/b/canvas.zip'

# ── 2. canvas pack.sh against a stub library, in a throwaway tree ───────────
mkdir -p "$T/src/tui/web/canvas/scripts" "$T/src/lib/ts/scripts"
cp "$CANVAS_PACK" "$T/src/tui/web/canvas/scripts/pack.sh"
: > "$T/src/tui/web/canvas/readme.md"
cat > "$T/src/lib/ts/scripts/pack.sh" <<'STUB'
#!/bin/sh
# stub packer: stages $(dirname ZIP)/pack/bundle.min.js like the real one.
set -eu
case "$2" in /*|[A-Za-z]:[/\\]*) Z=$2 ;; *) Z="$(pwd)/$2" ;; esac
mkdir -p "$(dirname "$Z")/pack" "$(dirname "$Z")"
if [ "${STUB_MODE:-write}" = write ]; then
  { head -c "${STUB_BYTES:-600000}" /dev/zero | tr '\0' 'a'; printf '%s' "${STUB_TAIL:-}"; } \
    > "$(dirname "$Z")/pack/bundle.min.js"
fi
: > "$Z"
STUB

# run_pack <zip arg> <want exit> <want text in log or -> <label> [stub mode] [stub bytes] [stub tail]
run_pack() {
  rc=0
  (export STUB_MODE=${5:-write} STUB_BYTES=${6:-600000} STUB_TAIL=${7:-}
   cd "$T/src/tui/web/canvas" && sh scripts/pack.sh "$1") > "$T/log" 2>&1 || rc=$?
  if [ "$rc" != "$2" ]; then
    echo "FAIL: $4: exit $rc, want $2" >&2; cat "$T/log" >&2; FAIL=1; return 0
  fi
  if [ "$3" != "-" ] && ! grep -q "$3" "$T/log"; then
    echo "FAIL: $4: log lacks '$3'" >&2; cat "$T/log" >&2; FAIL=1; return 0
  fi
  echo "ok: $4 (exit $rc)"
}

# (a) POSIX absolute zip: packs, bundle checks run and pass.
run_pack "$T/out-a/canvas.zip" 0 - "posix absolute"
[ -f "$T/out-a/pack/bundle.min.js" ] || { echo "FAIL: bundle not at out-a/pack" >&2; FAIL=1; }

# (b) relative zip: resolves against the app dir; bundle beside it.
run_pack "out-b/canvas.zip" 0 - "relative"
[ -f "$T/src/tui/web/canvas/out-b/pack/bundle.min.js" ] || { echo "FAIL: relative bundle missing" >&2; FAIL=1; }

# (c) missing bundle is a hard failure (the old check skipped it silently).
run_pack "$T/out-c/canvas.zip" 1 "no bundle at" "missing bundle fails" skip

# (d) tiny bundle (vendors not inlined) is a hard failure.
run_pack "$T/out-d/canvas.zip" 1 "vendors likely NOT inlined" "tiny bundle fails" write 1000

# (e) residual bare vendor import is a hard failure.
run_pack "$T/out-e/canvas.zip" 1 "residual bare vendor import" "residual import fails" write 600000 'import("three")'

if [ "$FAIL" != 0 ]; then echo "pack_paths: FAILED" >&2; exit 1; fi
echo "ok: pack paths and bundle checks"
