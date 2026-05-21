#!/usr/bin/env bash
# Slim variant, linux/amd64 — build + push to GHCR.
# Requires `podman login ghcr.io -u <github_user>` first (PAT scope: write:packages).

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="${REGISTRY:-ghcr.io}"
NAMESPACE="${NAMESPACE:-alexadar/fantastic-canvas}"
TAG_BASE="${TAG:-dev}"
IMG="$REGISTRY/$NAMESPACE/base:$TAG_BASE-amd64"

echo "[push] target: $IMG"

podman build \
    --platform linux/amd64 \
    --build-arg BASE_IMAGE=python:3.11-slim \
    -f containerfiles/generic/Containerfile \
    -t "$IMG" \
    "$REPO_ROOT"

podman push "$IMG"
echo "[push] done. Pull with: podman pull $IMG"
