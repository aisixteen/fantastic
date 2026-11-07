#!/bin/sh
# The kiosk's Debian packages, into `dist/kiosk/deb/`:
#
#   sh kiosk/os/build/debs.sh --target <CLI triple>
#
#   fantastic-app-<app triple>.deb       the base's app .deb (`scripts/build/deb.sh`)
#                                        with `/usr/bin/fantastic`, the app's sidecar,
#                                        REPLACED by the `fantastic-kiosk` binary
#   fantastic-session_<version>_all.deb  `fantastic-session`: the kiosk (kiosk/os/session)
#
# Inputs, already in place: `dist/deb/fantastic-app-<app triple>.deb` (the base's
# deb.sh) and `kiosk/target/<CLI triple>/release/fantastic-kiosk`
# (`cargo build --release --target <CLI triple>` in kiosk/). Release packages only;
# unsigned.
set -eu

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
say() { echo "kiosk/debs: $*"; }

TARGET=""
while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="${2:?--target needs a triple}"; shift 2 ;;
    *) echo "debs.sh: unknown option '$1'" >&2; echo "usage: debs.sh --target <CLI triple>" >&2; exit 2 ;;
  esac
done
[ -n "$TARGET" ] || { echo "debs.sh: --target <CLI triple> is required" >&2; exit 2; }
case "$TARGET" in
  *-unknown-linux-musl) APP_TARGET="${TARGET%-musl}-gnu" ;;
  *-unknown-linux-gnu) APP_TARGET="$TARGET" ;;
  *) echo "debs.sh: Debian packages are for Linux targets, not '$TARGET'" >&2; exit 2 ;;
esac

VERSION="${FANTASTIC_VERSION:-$(git -C "$REPO" describe --tags --abbrev=0 | sed 's/^v//')}"
[ -n "$VERSION" ] || { echo "debs.sh: no version (no tag, and FANTASTIC_VERSION is unset)" >&2; exit 1; }

BASE_DEB="$REPO/dist/deb/fantastic-app-$APP_TARGET.deb"
KIOSK_BIN="$REPO/kiosk/target/$TARGET/release/fantastic-kiosk"
[ -f "$BASE_DEB" ] || { echo "debs.sh: $BASE_DEB is missing: sh scripts/build/deb.sh --target $TARGET" >&2; exit 1; }
[ -f "$KIOSK_BIN" ] || { echo "debs.sh: $KIOSK_BIN is missing: (cd kiosk && cargo build --release --target $TARGET)" >&2; exit 1; }

OUT_DIR="$REPO/dist/kiosk/deb"
mkdir -p "$OUT_DIR"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# ── the app .deb, with the kiosk binary as its sidecar ──────────────────────
APP_DEB="$OUT_DIR/fantastic-app-$APP_TARGET.deb"
dpkg-deb -R "$BASE_DEB" "$STAGE/app"
[ -f "$STAGE/app/usr/bin/fantastic" ] || { echo "debs.sh: $BASE_DEB has no /usr/bin/fantastic" >&2; exit 1; }
install -m 755 "$KIOSK_BIN" "$STAGE/app/usr/bin/fantastic"
# the sums name the bytes the package carries
( cd "$STAGE/app" && find . -type f ! -path './DEBIAN/*' | sed 's|^\./||' | LC_ALL=C sort | tr '\n' '\0' | xargs -0 md5sum > DEBIAN/md5sums )
rm -f "$APP_DEB"
dpkg-deb --build --root-owner-group "$STAGE/app" "$APP_DEB"

# ── fantastic-session: dpkg-deb over kiosk/os/session ───────────────────────
mkdir -p "$STAGE/session"
cp -R "$REPO/kiosk/os/session/." "$STAGE/session/"
sed "s/@VERSION@/$VERSION/g" "$STAGE/session/DEBIAN/control.in" > "$STAGE/session/DEBIAN/control"
rm "$STAGE/session/DEBIAN/control.in"
find "$STAGE/session" -type d -exec chmod 755 {} +
find "$STAGE/session" -type f -exec chmod 644 {} +
chmod 755 "$STAGE/session/DEBIAN/postinst" "$STAGE/session/DEBIAN/prerm" "$STAGE/session/DEBIAN/postrm"
( cd "$STAGE/session" && find . -type f ! -path './DEBIAN/*' | sed 's|^\./||' | LC_ALL=C sort | tr '\n' '\0' | xargs -0 md5sum > DEBIAN/md5sums )
chmod 644 "$STAGE/session/DEBIAN/md5sums"
SESSION_DEB="$OUT_DIR/fantastic-session_${VERSION}_all.deb"
rm -f "$SESSION_DEB"
dpkg-deb --build --root-owner-group "$STAGE/session" "$SESSION_DEB"

say "wrote $APP_DEB"
say "wrote $SESSION_DEB"
echo "$APP_DEB"
echo "$SESSION_DEB"
