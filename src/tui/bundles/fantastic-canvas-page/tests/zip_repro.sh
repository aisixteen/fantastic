#!/bin/sh
# zip_repro.sh — the canvas zip is REPRODUCIBLE: the library's zip.sh packs the
# same bytes whatever the clock, umask, timezone or staging dir, and stamps
# every entry from SOURCE_DATE_EPOCH (a fixed default when unset). No esbuild
# needed. Run from anywhere: sh src/tui/bundles/fantastic-canvas-page/tests/zip_repro.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SRC=$(CDPATH= cd -- "$HERE/../../../.." && pwd)
ZIP_SH="$SRC/lib/ts/scripts/zip.sh"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
FAIL=0

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
ok()   { printf 'ok: %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; FAIL=1; }

# stage <dir>: the three fixture files, fixed content.
stage() {
  mkdir -p "$1"
  printf '# readme\nsha __x__\n' > "$1/readme.md"
  head -c 600000 /dev/zero | tr '\0' 'a' > "$1/bundle.min.js"
  printf '{"version":3,"sources":["a.ts"]}\n' > "$1/bundle.min.js.map"
}
# pack <dir> <zip> : run zip.sh with the caller's env.
pack() { sh "$ZIP_SH" "$2" "$1" readme.md bundle.min.js bundle.min.js.map; }

[ -f "$ZIP_SH" ] || { echo "FAIL: no $ZIP_SH" >&2; exit 1; }

# 1. byte-identical: different dir, umask, TZ, source mtimes.
stage "$T/a"; stage "$T/b"
touch -t 202001010101 "$T/b/readme.md" "$T/b/bundle.min.js"
touch -t 203012312359 "$T/b/bundle.min.js.map"
(export SOURCE_DATE_EPOCH=1700000000 TZ=UTC; umask 022; pack "$T/a" "$T/oa/canvas.zip")
(export SOURCE_DATE_EPOCH=1700000000 TZ=America/Los_Angeles; umask 077; pack "$T/b" "$T/ob/canvas.zip")
if [ "$(sha "$T/oa/canvas.zip")" = "$(sha "$T/ob/canvas.zip")" ]; then ok zip_is_byte_identical_for_same_epoch
else fail zip_is_byte_identical_for_same_epoch; fi

# 2. fixed entry order.
got=$(unzip -Z1 "$T/oa/canvas.zip" | tr '\n' ' ')
if [ "$got" = "readme.md bundle.min.js bundle.min.js.map " ]; then ok zip_entry_order_is_fixed
else fail "zip_entry_order_is_fixed: $got"; fi

# 3. mtimes follow SOURCE_DATE_EPOCH (1700000000 = 2023-11-14 22:13:20 UTC; DOS 2s).
n=$(unzip -Z -T "$T/oa/canvas.zip" | grep -c '20231114.221320' || true)
if [ "$n" = 3 ]; then ok zip_mtimes_follow_source_date_epoch
else fail "zip_mtimes_follow_source_date_epoch: $n entries stamped"; unzip -Z -T "$T/oa/canvas.zip" >&2; fi
stage "$T/c"; stage "$T/d"
(unset SOURCE_DATE_EPOCH; export TZ=UTC; pack "$T/c" "$T/oc/canvas.zip")
(unset SOURCE_DATE_EPOCH; export TZ=Asia/Tokyo; touch -t 202501010101 "$T/d/readme.md"; pack "$T/d" "$T/od/canvas.zip")
n=$(unzip -Z -T "$T/oc/canvas.zip" | grep -c '19800101.000000' || true)
if [ "$n" = 3 ] && [ "$(sha "$T/oc/canvas.zip")" = "$(sha "$T/od/canvas.zip")" ]; then ok zip_default_epoch_is_fixed
else fail "zip_default_epoch_is_fixed: $n entries at 1980"; fi

# 4. a different epoch gives different bytes.
stage "$T/e"
(export SOURCE_DATE_EPOCH=1700000100; pack "$T/e" "$T/oe/canvas.zip")
if [ "$(sha "$T/oa/canvas.zip")" != "$(sha "$T/oe/canvas.zip")" ]; then ok zip_changes_with_different_epoch
else fail zip_changes_with_different_epoch; fi

# 5. fixed permissions, no extra fields.
info=$(unzip -Z -l "$T/oa/canvas.zip" 2>/dev/null || true)
modes=$(unzip -Z "$T/oa/canvas.zip" | grep -c '^-rw-r--r--' || true)
extra=$(unzip -Z -v "$T/oa/canvas.zip" | grep -i 'extra field' | grep -vc ': *0 bytes' || true)
if [ "$modes" = 3 ] && [ "$extra" = 0 ]; then ok zip_has_fixed_permissions_and_no_extra_fields
else fail "zip_has_fixed_permissions_and_no_extra_fields: modes=$modes extra=$extra"; fi

if [ "$FAIL" != 0 ]; then echo "zip_repro: FAILED" >&2; exit 1; fi
echo "ok: canvas zip is reproducible"
