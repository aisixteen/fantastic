#!/bin/sh
# Build the universal Fantastic kernel image LOCALLY (push deferred).
#
#   sh container/build.sh                         # host-arch build, loaded locally
#   PLATFORM=linux/amd64,linux/arm64 sh …/build.sh   # multi-arch manifest (local)
#   PUSH=1 PLATFORM=linux/amd64,linux/arm64 \
#     TAG=ghcr.io/alexadar/fantastic:latest sh …/build.sh   # publish (opt-in)
#
# Works with podman OR docker. The prebuilt ts/dist/js_kernel.zip is COPIED into
# the image (not built there) — this script ensures it exists first.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$HERE/.." && pwd)
TAG="${TAG:-fantastic:latest}"
PLATFORM="${PLATFORM:-}"
PUSH="${PUSH:-0}"

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
echo "build.sh: $ENGINE build $TAG (platform='${PLATFORM:-host}', push=$PUSH)"

if [ -z "$PLATFORM" ]; then
  # Single-arch host build — fast path, loaded into the local store.
  "$ENGINE" build -f container/Containerfile -t "$TAG" .
  [ "$PUSH" = 1 ] && "$ENGINE" push "$TAG" || echo "build.sh: push skipped (set PUSH=1 to publish)"
else
  # Multi-arch manifest list.
  if [ "$ENGINE" = podman ]; then
    podman manifest rm "$TAG" 2>/dev/null || true
    podman build --platform "$PLATFORM" --manifest "$TAG" -f container/Containerfile .
    [ "$PUSH" = 1 ] && podman manifest push --all "$TAG" "docker://$TAG" \
      || echo "build.sh: push skipped (set PUSH=1 to publish)"
  else
    # docker buildx (creates/uses a builder; --load can't do multi-arch, so
    # multi-arch implies --push). Build single-arch per platform to load locally.
    docker buildx build --platform "$PLATFORM" -f container/Containerfile \
      $( [ "$PUSH" = 1 ] && echo --push || echo --output=type=image ) -t "$TAG" .
    [ "$PUSH" = 1 ] || echo "build.sh: multi-arch built (docker can't --load multi; PUSH=1 to publish)"
  fi
fi

echo "build.sh: done -> $TAG"
