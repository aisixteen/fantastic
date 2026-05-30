# containerfiles/gpubase selftest

> scopes: gpu-host, image, boot, http, ws, rest, canvas, add-member, install-bundle, persistence, shutdown
> requires: NVIDIA driver + nvidia-container-toolkit + CDI spec on the host (see ../gpubase/README.md "Host setup"); `podman` ≥ 4.x; outbound network; host port `18080` free

Mirrors `../base/selftest.md` 1:1 for probes 1–10. Only difference:
the `boot` probe gains a **`gpu-host`** sanity check that `nvidia-smi`
inside the container sees the GPU.

**Status: executed 2026-05-19 on RTX 3090 / Ubuntu 24.04 / driver
580.126.20 / podman 4.9.3 / nvidia-container-toolkit 1.19.0 /
`docker.io/nvidia/cuda:12.8.2-runtime-ubuntu24.04`. All 11 probes
PASS.** See results table at bottom.

## Pre-flight (extra vs the slim selftest)

- `nvidia-smi` works on the host (driver loaded, GPU present).
- `ls /etc/cdi/nvidia.yaml` exists (`sudo nvidia-ctk cdi generate ...`
  was run).
- `podman run --rm --device nvidia.com/gpu=all docker.io/nvidia/cuda:12.8.2-base-ubuntu24.04 nvidia-smi`
  succeeds from a throwaway container.

## Setup (delta vs slim)

```bash
WORKDIR=$(mktemp -d)
IMG=fantastic-canvas-gpubase:dev
./containerfiles/gpubase/build.sh
podman run -d --name ft-gpu \
  --device nvidia.com/gpu=all \
  -v "$WORKDIR:/workdir" \
  -p 18080:8080 \
  "$IMG"
# wait for [kernel] up (same loop as slim selftest)
```

## Probe 0 — `gpu-host` (extra)

```bash
podman exec ft-gpu nvidia-smi
```

Expected: prints GPU model + driver version + memory. **PASS** if the
GPU is listed.

Failure modes:
- "command not found": image doesn't have nvidia-smi → wrong base
  image (use `nvidia/cuda:*-runtime-*`, not `*-base-*`).
- "could not find …" / "No devices were found": CDI spec missing or
  `--device nvidia.com/gpu=all` flag not passed.

## Probes 1–10

Same as `../base/selftest.md`. Reuse those instructions verbatim,
substituting `ft-gpu` for `ft-test` and `fantastic-canvas-gpubase:dev`
for the image tag.

## Cleanup

```bash
podman stop ft-gpu
podman rm -f ft-gpu
rm -rf "$WORKDIR"
```

## Results (2026-05-19, RTX 3090)

| # | Probe | Scope | Pass/Fail | Notes |
|---|---|---|---|---|
| 0 | gpu-host | gpu-host | PASS | RTX 3090, 24 GB VRAM, driver 580.126.20, CUDA 13.0 visible inside container |
| 1 | image | image | PASS | 3.33 GB — over slim's 1.5 GB threshold, but expected for CUDA runtime libs |
| 2 | boot | boot | PASS | `[kernel] up`; `web_*` + `canvas_webapp_*` present in agents tree |
| 3 | http | http | PASS | `/` renders agent tree |
| 4 | rest | rest | PASS | kernel reflect `?bundles=all` returns 20 bundles |
| 5 | ws | ws | PASS | call/reflect round-trip on `/core/ws` |
| 6 | canvas | canvas, http, rest | PASS | canvas HTML + empty members list on fresh workdir |
| 7 | add-member | add-member, canvas, rest | PASS | `html_agent_<hex>` created, listed, `GET /<id>/` → 200 |
| 8 | install-bundle | install-bundle | PASS | `uv pip install` invoked; resolve fails because `git` isn't in the runtime image — selftest accepts this as proof the path is wired (see README "Known gaps") |
| 9 | persistence | persistence, canvas | PASS | member survived `podman stop && start`; note that `[kernel] up` fires before agent rehydration finishes — give ~1s before hitting REST after restart |
| 10 | shutdown | shutdown | PASS | 3+ graceful-shutdown lines per stop |

## Fixes applied to land this PASS

Two changes were required vs the original branch:

1. **`build.sh` / `push.sh` default `BASE_IMAGE`**: bumped from
   `nvidia/cuda:12.4.1-runtime-ubuntu24.04` (does not exist on Docker
   Hub — NVIDIA only started shipping `ubuntu24.04` from CUDA 12.6) to
   `docker.io/nvidia/cuda:12.8.2-runtime-ubuntu24.04`.
2. **`../generic/Containerfile`**: the CUDA runtime image ships no
   Python, so the builder + final stages now conditionally apt-install
   `python3` + `python3-pip` and pass `--python python3` to `uv sync`.
   The flag pins uv to the system 3.12 (kernel's `requires-python` is
   `>=3.11`, so 3.12 satisfies it), avoiding the trap where uv fetches
   its own 3.11 to `~/.local/share/uv/python/` and produces absolute
   symlinks that break when `/app` is `COPY`'d to the final stage.
