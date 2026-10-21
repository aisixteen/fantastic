#!/bin/sh
# The fantastic server's image (Phase 12, #158): os/container/Containerfile, built
# with docker or podman. ONE place builds it, for CI and the publish workflow.
#
#   sh scripts/build/container.sh --arch amd64                 one arch, loaded locally
#   sh scripts/build/container.sh --arch arm64 --tag fantastic-server:arm64
#   sh scripts/build/container.sh --arch all --tag ghcr.io/aisixteen/fantastic:1.2.3 \
#       --tag ghcr.io/aisixteen/fantastic:latest --push
#
# `--tag` may repeat. `--arch all` builds the multi-arch manifest with docker
# buildx and needs `--push`: a multi-platform image is not loadable. A push needs a
# registry login first (the publish workflow logs in with GITHUB_TOKEN).
#
# Each arch's `fantastic` is the one its release archive holds (`cli.sh` leaves
# dist/fantastic-<triple>.tar.gz; CI keeps it as the release-<triple> artifact).
# It is staged at dist/container/<arch>/fantastic, where the Containerfile copies
# it from: the bytes the fantastic-app .deb carries, so the server and the kiosk
# run one build. Build the archives first (`cli.sh --target <triple>`).
#
# Engine: $ENGINE (docker or podman) when set, else podman, else docker. Podman
# builds in docker's image format, so the image keeps its HEALTHCHECK (OCI drops it).
# Version: FANTASTIC_VERSION, else the last git tag (deb.sh's rule), baked into the
# image's version label.
set -eu

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ARCH=""
TAGS=""
PUSH=0
VERSION="${FANTASTIC_VERSION:-}"

usage() {
  echo "usage: container.sh --arch amd64|arm64|all [--tag <image:tag>]... [--push]" >&2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="${2:?--arch needs amd64, arm64 or all}"; shift 2 ;;
    --tag) TAGS="$TAGS ${2:?--tag needs an image name}"; shift 2 ;;
    --push) PUSH=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "container.sh: unknown option '$1'" >&2; usage; exit 2 ;;
  esac
done

case "$ARCH" in
  amd64|arm64|all) ;;
  "") echo "container.sh: --arch is required (amd64, arm64 or all)" >&2; usage; exit 2 ;;
  *) echo "container.sh: --arch must be amd64, arm64 or all, got '$ARCH'" >&2; exit 2 ;;
esac
if [ "$ARCH" = all ] && [ "$PUSH" != 1 ]; then
  echo "container.sh: --arch all builds a multi-arch manifest, which only --push can hold" >&2
  exit 2
fi

[ -n "$VERSION" ] || VERSION="$(git -C "$REPO" describe --tags --abbrev=0 2>/dev/null | sed 's/^v//')"
[ -n "$VERSION" ] || { echo "container.sh: no version (no tag, and FANTASTIC_VERSION is unset)" >&2; exit 1; }
[ -n "$TAGS" ] || TAGS=" fantastic:$VERSION"

# ── the engine ──────────────────────────────────────────────────────────────
if [ -z "${ENGINE:-}" ]; then
  if command -v podman >/dev/null 2>&1; then ENGINE=podman
  elif command -v docker >/dev/null 2>&1; then ENGINE=docker
  else echo "container.sh: need podman or docker" >&2; exit 1; fi
fi
case "$ENGINE" in
  docker|podman) ;;
  *) echo "container.sh: ENGINE must be docker or podman, got '$ENGINE'" >&2; exit 2 ;;
esac
if [ "$ARCH" = all ] && [ "$ENGINE" != docker ]; then
  echo "container.sh: --arch all needs docker buildx (ENGINE=$ENGINE)" >&2
  exit 2
fi

# ── stage one arch's binary from its release archive ─────────────────────────
stage() {  # amd64|arm64
  case "$1" in
    amd64) triple=x86_64-unknown-linux-musl ;;
    arm64) triple=aarch64-unknown-linux-musl ;;
  esac
  archive="$REPO/dist/fantastic-$triple.tar.gz"
  [ -f "$archive" ] || { echo "container.sh: $archive is missing: run sh scripts/build/cli.sh --target $triple" >&2; exit 1; }
  mkdir -p "$REPO/dist/container/$1"
  tar -xzf "$archive" -C "$REPO/dist/container/$1" fantastic
  chmod 0755 "$REPO/dist/container/$1/fantastic"
  echo "container.sh: staged $archive -> dist/container/$1/fantastic"
}

# podman's default image format is OCI, which has no HEALTHCHECK
FORMAT_ARG=""
if [ "$ENGINE" = podman ]; then FORMAT_ARG="--format docker"; fi

# the -t flags for each tag
tag_args() {
  for t in $TAGS; do printf '%s\n' "-t" "$t"; done
}

cd "$REPO"
case "$ARCH" in
  amd64|arm64)
    stage "$ARCH"
    FIRST=$(printf '%s' "$TAGS" | awk '{ print $1 }')
    echo "container.sh: $ENGINE build linux/$ARCH -> $TAGS (version $VERSION)"
    # shellcheck disable=SC2046,SC2086
    "$ENGINE" build $FORMAT_ARG --platform "linux/$ARCH" --build-arg "FANTASTIC_VERSION=$VERSION" \
      -f os/container/Containerfile $(tag_args) .
    if [ "$PUSH" = 1 ]; then
      for t in $TAGS; do "$ENGINE" push "$t"; done
    fi
    echo "container.sh: done -> $FIRST"
    ;;
  all)
    stage amd64
    stage arm64
    echo "container.sh: docker buildx linux/amd64,linux/arm64 -> $TAGS (version $VERSION)"
    # shellcheck disable=SC2046
    docker buildx build --platform linux/amd64,linux/arm64 --build-arg "FANTASTIC_VERSION=$VERSION" \
      -f os/container/Containerfile $(tag_args) --push .
    echo "container.sh: done -> $TAGS"
    ;;
esac
