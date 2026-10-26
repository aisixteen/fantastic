#!/bin/sh
# zip.sh — a REPRODUCIBLE zip: the same files and the same SOURCE_DATE_EPOCH give
# the same bytes, whatever the clock, umask, timezone, uid or staging dir.
#
#   sh zip.sh <out.zip> <dir> <file…>     # files are <dir>-relative, zipped flat,
#                                         # in exactly the order given
#
# Every entry is stamped SOURCE_DATE_EPOCH (when set and numeric) or 1980-01-01
# 00:00:00 (the DOS minimum), rounded up to DOS's 2 s and clamped to >= 1980;
# mode 644; no extra fields (-X: no UT/ux), no directory entries (-D).
# Needs Info-ZIP `zip`.
set -eu

if [ "$#" -lt 3 ]; then
  echo "usage: sh zip.sh <out.zip> <dir> <file…>" >&2
  exit 2
fi
ZIP=$1; DIR=$2; shift 2
case "$ZIP" in /*|[A-Za-z]:[/\\]*) ;; *) ZIP="$(pwd)/$ZIP" ;; esac

MIN=315532800   # 1980-01-01 00:00:00 UTC
EPOCH=$MIN
case "${SOURCE_DATE_EPOCH:-}" in
  ''|*[!0-9]*) ;;
  *) EPOCH=$SOURCE_DATE_EPOCH ;;
esac
EPOCH=$(( (EPOCH + 1) / 2 * 2 ))
[ "$EPOCH" -ge "$MIN" ] || EPOCH=$MIN

# touch -t stamp for EPOCH, in UTC (GNU date, then BSD date).
STAMP=$(date -u -d "@$EPOCH" +%Y%m%d%H%M.%S 2>/dev/null || date -u -r "$EPOCH" +%Y%m%d%H%M.%S)

mkdir -p "$(dirname "$ZIP")"
STAGE=$(mktemp -d "$(dirname "$ZIP")/.zipstage.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
for f in "$@"; do
  cp "$DIR/$f" "$STAGE/$f"
  chmod 644 "$STAGE/$f"
  TZ=UTC touch -t "$STAMP" "$STAGE/$f"
done

rm -f "$ZIP"
unset ZIPOPT ZIP_OPT
( cd "$STAGE" && TZ=UTC zip -q -X -D -j -9 "$ZIP" "$@" )
