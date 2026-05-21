#!/usr/bin/env bash
# Slim variant, linux/amd64 (x86_64) — local build.
# Source of truth: ../generic/Containerfile (parameterized by BASE_IMAGE).
# Operator guide + selftest: ../base/.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

IMG="${IMG:-fantastic-canvas-base:dev-amd64}"
exec podman build \
    --platform linux/amd64 \
    --build-arg BASE_IMAGE=python:3.11-slim \
    -f containerfiles/generic/Containerfile \
    -t "$IMG" \
    "$@" \
    .
