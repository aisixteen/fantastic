#!/usr/bin/env bash
# Single-arch build + push of the GPU variant to GHCR.
#
# Defaults to amd64: `nvidia/cuda:*-runtime-ubuntu24.04` is amd64-only
# in the standard variant. arm64 CUDA builds exist for Jetson/Grace
# but use a different base path — override BASE_IMAGE + PLATFORM if
# targeting one of those.
#
# Tag scheme:  <namespace>/<image>:<tag>-<arch>
#   ghcr.io/alexadar/fantastic-canvas/gpubase:dev-amd64
#
# Auth: `podman login ghcr.io -u <github_user>` first.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="${REGISTRY:-ghcr.io}"
NAMESPACE="${NAMESPACE:-alexadar/fantastic-canvas}"
IMG_NAME="${IMG_NAME:-gpubase}"
TAG_BASE="${TAG:-dev}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/nvidia/cuda:12.8.2-runtime-ubuntu24.04}"
PLATFORM="${PLATFORM:-linux/amd64}"
ARCH_SUFFIX="${PLATFORM##*/}"
IMG="$REGISTRY/$NAMESPACE/$IMG_NAME:$TAG_BASE-$ARCH_SUFFIX"

echo "[push] platform: $PLATFORM"
echo "[push] target:   $IMG"
echo "[push] base:     $BASE_IMAGE"

podman build \
    --platform "$PLATFORM" \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" \
    -f containerfiles/generic/Containerfile \
    -t "$IMG" \
    "$REPO_ROOT"

echo "[push] pushing"
podman push "$IMG"

echo "[push] done."
echo "  pull: podman pull $IMG"
