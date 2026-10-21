#!/bin/sh
# The fantastic OS image (`os/mkosi`) — Debian 13 with the kiosk, as a UEFI disk.
#
#   sh scripts/build/os.sh [--arch x86-64|arm64] [--profile ci]
#
# The packages must already be in `dist/deb/` (`scripts/build/deb.sh`): mkosi
# makes them a local repository, and apt resolves their Depends from Debian.
# `--profile ci` adds what the QEMU boot check needs (`os/mkosi/mkosi.profiles/ci`);
# the default is the image that ships. Needs mkosi v27.1 (Ubuntu's apt has 20.2:
# CI uses the `systemd/mkosi@v27.1` action).
#
# Writes `dist/os/fantastic-os_<version>.raw` and prints its path.
#
# (Not `common.sh`'s options: `--profile` here is a mkosi profile, and the
# image is built for an architecture, not a rust target.)
set -eu

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

usage() {
  echo "usage: $(basename "$0") [--arch x86-64|arm64] [--profile ci]"
}

case "$(uname -m)" in
  aarch64|arm64) ARCH=arm64 ;;
  *) ARCH=x86-64 ;;
esac
MKOSI_PROFILE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="${2:?--arch needs a value}"; shift 2 ;;
    --profile) MKOSI_PROFILE="${2:?--profile needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "os.sh: unknown option '$1'" >&2; usage >&2; exit 2 ;;
  esac
done
case "$ARCH" in
  x86-64|arm64) ;;
  *) echo "os.sh: --arch must be x86-64|arm64, got '$ARCH'" >&2; exit 2 ;;
esac
case "$MKOSI_PROFILE" in
  ""|ci) ;;
  *) echo "os.sh: --profile must be ci (or none), got '$MKOSI_PROFILE'" >&2; exit 2 ;;
esac

# deb.sh's rule, so the image, the packages and the app agree
VERSION="${FANTASTIC_VERSION:-$(git -C "$REPO" describe --tags --abbrev=0 | sed 's/^v//')}"
[ -n "$VERSION" ] || { echo "os.sh: no version (no tag, and FANTASTIC_VERSION is unset)" >&2; exit 1; }

for pkg in "fantastic-app-*.deb" "fantastic-session_${VERSION}_all.deb"; do
  # shellcheck disable=SC2086
  set -- "$REPO"/dist/deb/$pkg
  [ -f "$1" ] || { echo "os.sh: no $REPO/dist/deb/$pkg: build the packages first: sh scripts/build/deb.sh --target <CLI triple>" >&2; exit 1; }
done
# As root. Unprivileged, mkosi v27.1 without systemd-nsresourced maps a single uid
# (it writes /proc/self/uid_map itself), so every file not owned by root lands
# owned by root: the kiosk's home among them, and the kiosk can't enter it.
[ "$(id -u)" -eq 0 ] || { echo "os.sh: run as root (sudo FANTASTIC_VERSION=<version> sh scripts/build/os.sh ...): unprivileged, mkosi maps one uid and the image's files lose their owners" >&2; exit 1; }
command -v mkosi >/dev/null || { echo "os.sh: mkosi is not installed (v27.1: https://github.com/systemd/mkosi/tree/v27.1)" >&2; exit 1; }

# every path is absolute: -C changes directory
set -- -C "$REPO/os/mkosi" --architecture="$ARCH" --image-version="$VERSION" \
  --package-directory="$REPO/dist/deb" --output-directory="$REPO/dist/os"
[ -z "$MKOSI_PROFILE" ] || set -- "$@" --profile="$MKOSI_PROFILE"
echo "build/os: mkosi $* -f build"
mkosi "$@" -f build

IMAGE="$REPO/dist/os/fantastic-os_$VERSION.raw"
[ -f "$IMAGE" ] || { echo "os.sh: mkosi wrote no $IMAGE" >&2; ls -l "$REPO/dist/os" >&2; exit 1; }
echo "build/os: wrote $IMAGE"
echo "$IMAGE"
