#!/usr/bin/env bash
# GPU variant build — NVIDIA CUDA base (Ubuntu 24.04 ships Python 3.12,
# which satisfies the kernel's `requires-python >= 3.11`).
# Source of truth is ../generic/Containerfile; this script just picks
# the BASE_IMAGE arg.
#
# Verified targets: NVIDIA Ampere+ (RTX 3090 / 4090, A100, H100). Host
# needs the NVIDIA driver + NVIDIA Container Toolkit + a CDI spec
# (`sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`).
# Run the container with `--device nvidia.com/gpu=all`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

IMG="${IMG:-fantastic-canvas-gpubase:dev}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/nvidia/cuda:12.8.2-runtime-ubuntu24.04}"

exec podman build \
    -f containerfiles/generic/Containerfile \
    --build-arg "BASE_IMAGE=$BASE_IMAGE" \
    -t "$IMG" \
    "$@" \
    .
