# containerfiles/gpubase — GPU variant operator guide

Like `../base/`, but the image base is `nvidia/cuda:12.8.2-runtime-ubuntu24.04`
(Ubuntu 24.04 ships Python 3.12 — satisfies the kernel's
`requires-python >= 3.11`). The CUDA runtime image ships no Python, so
the shared Containerfile apt-installs `python3` + `python3-pip` before
running `uv sync`. Verified targets: NVIDIA Ampere+
(RTX 3090 / 4090, A100, H100). **Validated end-to-end on RTX 3090 /
Ubuntu 24.04 / driver 580.126.20 (2026-05-19)** — see `selftest.md`
results table.

## Pull the prebuilt image

```bash
podman pull ghcr.io/alexadar/fantastic-canvas/gpubase:dev-amd64
podman run -d --name fantastic --device nvidia.com/gpu=all \
  -v "$PWD:/workdir" -p 8080:8080 \
  ghcr.io/alexadar/fantastic-canvas/gpubase:dev-amd64
```

## Host setup (one-time per GPU server)

```bash
# 1. NVIDIA driver — 535+ for CUDA 12.4 compatibility.
sudo apt install -y nvidia-driver-535 nvidia-utils-535
nvidia-smi   # should show the GPU + driver version

# 2. NVIDIA Container Toolkit + CDI spec (the podman-native path).
distribution=$(. /etc/os-release; echo $ID$VERSION_ID)
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
  | sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -s -L https://nvidia.github.io/libnvidia-container/$distribution/libnvidia-container.list \
  | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
  | sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list
sudo apt update && sudo apt install -y nvidia-container-toolkit
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml

# 3. Sanity check from a throwaway container.
podman run --rm --device nvidia.com/gpu=all \
  docker.io/nvidia/cuda:12.8.2-base-ubuntu24.04 nvidia-smi
```

If the throwaway container prints the GPU → host is ready.

## Build

```bash
./containerfiles/gpubase/build.sh
```

Produces `fantastic-canvas-gpubase:dev`. Override with `IMG=...` or
`BASE_IMAGE=...`. Larger than the slim variant (CUDA runtime libs).

## Run

Same shape as the slim variant, plus the GPU device flag:

```bash
NAME="fantastic-$(echo "$PWD" | shasum | head -c8)"
podman run -d --name "$NAME" \
  --device nvidia.com/gpu=all \
  -v "$PWD:/workdir" \
  -p 8080:8080 \
  fantastic-canvas-gpubase:dev
```

Verify the kernel sees the GPU once it's up:
```bash
podman exec "$NAME" nvidia-smi
```

Everything else — the seeded host stack (`web` + `web_ws` + `web_rest`),
install-bundle, persistence, graceful stop, and serving the TS frontend
weakly — behaves identically to the slim variant. See `../base/README.md`
for the full operator flow; only the build image + run flag differ.

## Why a separate variant

Slim doesn't ship CUDA runtime libs (~2 GB). GPU bundles (vLLM, ollama,
PyTorch-based vision agents) need them. Keeping the variants separate
means slim users don't pay the CUDA storage cost; GPU users get a
self-contained image without manual layering.

## Status

**Validated 2026-05-19** on NVIDIA RTX 3090 / Ubuntu 24.04 / driver
580.126.20 / podman 4.9.3 / nvidia-container-toolkit 1.19.0. All
selftest probes (gpu-host + 1–9) PASS. See `selftest.md` for the
results table — note that run predates the host-only refactor that
dropped the canvas/view bundles; the table there is mapped onto the
current pure-host probe set.

## Known gaps

- `fantastic install-bundle git+<url>` reaches `uv pip install` but uv
  errors with "Git executable not found" — the runtime image doesn't
  ship `git`. Add `git` to the apt-install line in the final stage if
  bundle installs from git URLs inside the container are wanted.
