#!/bin/sh
# Build the universal Fantastic kernel image LOCALLY (push deferred).
#
#   sh container/build.sh                         # host-arch build → fantastic:latest
#   ARCH=arm64 sh container/build.sh              # one arch (native) → fantastic:arm64
#   ARCH=amd64 sh container/build.sh              # one arch (emulated on arm host) → fantastic:amd64
#   PLATFORM=linux/amd64,linux/arm64 sh …/build.sh   # multi-arch manifest (local)
#   PUSH=1 ARCH=arm64 TAG=ghcr.io/alexadar/fantastic:0.3.0-linux-arm64 sh …/build.sh  # publish one arch
#
# `ARCH` builds ONE platform on whatever builder you run it on — native on a
# matching host (arm64 on Apple silicon), emulated otherwise. That's the local
# mirror of the per-arch CI release jobs. `PLATFORM` (a list) still does the
# combined manifest path. All three modes share the same prereqs below.
#
# Works with podman OR docker. The prebuilt ts/dist/js_kernel.zip is COPIED into
# the image (not built there) — this script ensures it exists first.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$HERE/.." && pwd)
TAG="${TAG:-fantastic:latest}"
ARCH="${ARCH:-}"
PLATFORM="${PLATFORM:-}"
PUSH="${PUSH:-0}"
# Baked into the image as FANTASTIC_VERSION → surfaced in the kernel's reflect.
# Defaults to "dev" for local builds; CI passes the release tag (e.g. v0.5.3).
VERSION="${FANTASTIC_VERSION:-dev}"

# ── prereq: the bundled JS runtime (copied prebuilt from ts/) ──────────────
if [ ! -f "$REPO/ts/dist/js_kernel.zip" ]; then
  echo "build.sh: ts/dist/js_kernel.zip missing — building it (cd ts && sh scripts/pack.sh)"
  ( cd "$REPO/ts" && sh scripts/pack.sh )
fi

# ── prereq: generate the descriptive head page from the repo readmes ───────
python3 "$HERE/head/gen_head.py" > "$HERE/head/index.html"
echo "build.sh: generated container/head/index.html ($(wc -c < "$HERE/head/index.html" | tr -d ' ')B)"

# ── pick an engine ─────────────────────────────────────────────────────────
if command -v podman >/dev/null 2>&1; then ENGINE=podman
elif command -v docker >/dev/null 2>&1; then ENGINE=docker
else echo "build.sh: need podman or docker" >&2; exit 1; fi

cd "$REPO"

# ── per-arch single build (ARCH=amd64|arm64) — the local mirror of the CI ───
# per-arch release jobs. Native on a matching host, emulated otherwise. Default
# tag is fantastic:<arch> (override with TAG, e.g. a GHCR per-arch tag).
if [ -n "$ARCH" ]; then
  case "$ARCH" in
    amd64|arm64) ;;
    *) echo "build.sh: ARCH must be 'amd64' or 'arm64' (got '$ARCH')" >&2; exit 2 ;;
  esac
  archtag="$TAG"
  [ "$TAG" = "fantastic:latest" ] && archtag="fantastic:$ARCH"
  echo "build.sh: $ENGINE build linux/$ARCH -> $archtag (push=$PUSH, version=$VERSION)"
  "$ENGINE" build --platform "linux/$ARCH" --build-arg "FANTASTIC_VERSION=$VERSION" \
    -f container/Containerfile -t "$archtag" .
  [ "$PUSH" = 1 ] && "$ENGINE" push "$archtag" \
    || echo "build.sh: push skipped (set PUSH=1 to publish)"
  echo "build.sh: done -> $archtag"
  exit 0
fi

echo "build.sh: $ENGINE build $TAG (platform='${PLATFORM:-host}', push=$PUSH)"

if [ -z "$PLATFORM" ]; then
  # Single-arch host build — fast path, loaded into the local store. Pin the
  # platform to the host arch EXPLICITLY: otherwise a stale wrong-arch base
  # image already in the local store (e.g. an amd64 python:slim left by a prior
  # multi-arch/QEMU build) gets reused, silently producing a cross-arch image
  # that then runs under emulation. Deriving it from `uname -m` keeps the build
  # native on both Apple-silicon and x86 hosts.
  case "$(uname -m)" in
    arm64|aarch64) HOSTPLAT=linux/arm64 ;;
    x86_64|amd64)  HOSTPLAT=linux/amd64 ;;
    *) HOSTPLAT="" ;;
  esac
  if [ -n "$HOSTPLAT" ]; then
    echo "build.sh: pinning host platform $HOSTPLAT"
    "$ENGINE" build --platform "$HOSTPLAT" --build-arg "FANTASTIC_VERSION=$VERSION" \
      -f container/Containerfile -t "$TAG" .
  else
    "$ENGINE" build --build-arg "FANTASTIC_VERSION=$VERSION" \
      -f container/Containerfile -t "$TAG" .
  fi
  [ "$PUSH" = 1 ] && "$ENGINE" push "$TAG" || echo "build.sh: push skipped (set PUSH=1 to publish)"
else
  # Multi-arch manifest list.
  if [ "$ENGINE" = podman ]; then
    podman manifest rm "$TAG" 2>/dev/null || true
    podman build --platform "$PLATFORM" --manifest "$TAG" \
      --build-arg "FANTASTIC_VERSION=$VERSION" -f container/Containerfile .
    [ "$PUSH" = 1 ] && podman manifest push --all "$TAG" "docker://$TAG" \
      || echo "build.sh: push skipped (set PUSH=1 to publish)"
  else
    # docker buildx (creates/uses a builder; --load can't do multi-arch, so
    # multi-arch implies --push). Build single-arch per platform to load locally.
    docker buildx build --platform "$PLATFORM" --build-arg "FANTASTIC_VERSION=$VERSION" \
      -f container/Containerfile \
      $( [ "$PUSH" = 1 ] && echo --push || echo --output=type=image ) -t "$TAG" .
    [ "$PUSH" = 1 ] || echo "build.sh: multi-arch built (docker can't --load multi; PUSH=1 to publish)"
  fi
fi

echo "build.sh: done -> $TAG"
