# containerfiles/generic

The **single source of truth** for fantastic-canvas image builds.
Parameterized via `ARG BASE_IMAGE`; sibling dirs (`../base`, `../gpubase`)
just pick which base image to fill in and invoke podman via their
`build.sh`.

```
containerfiles/
├── generic/                          ← this dir
│   ├── Containerfile                 the shared recipe (ARG BASE_IMAGE)
│   ├── entrypoint.sh                 seeds .fantastic/ + execs fantastic
│   └── README.md                     (this file)
├── base/                             slim variant — Python 3.13-slim
│   ├── build.sh                      podman build invocation
│   ├── README.md                     operator guide
│   └── selftest.md                   end-to-end probes
└── gpubase/                          GPU variant — nvidia/cuda
    ├── build.sh
    └── ... (selftest + README when tested)
```

## Why split this way

- **One Containerfile**, multiple variants → no duplication. Variants
  differ ONLY in the base image string.
- **Per-variant `build.sh`** → a single command per variant
  (`./containerfiles/base/build.sh`) without remembering the
  `--build-arg`. Override `IMG` or `BASE_IMAGE` env vars to deviate.
- **Variant-specific docs + selftest** stay in the variant dir
  (different probes apply: GPU variant has `nvidia-smi` checks, slim
  doesn't).

## Build directly (without the wrapper)

```bash
podman build \
  -f containerfiles/generic/Containerfile \
  --build-arg BASE_IMAGE=python:3.13-slim \
  -t fantastic-canvas-base:dev \
  .
```

Build context **must be the repo root** so `COPY . .` picks up the
workspace.

## What's in the recipe

- Multi-stage. **Builder** stage installs `uv`, copies the repo, runs
  `uv sync --no-dev --frozen --no-cache` against `uv.lock`, prunes
  caches + dev artefacts (`.git`, `tests/`, `containerfiles/`).
- **Final** stage starts from the same `BASE_IMAGE`, re-installs `uv`
  (needed by `fantastic install-bundle` at runtime), copies the built
  `/app` into `/opt/fantastic`, sets `PATH`, `WORKDIR=/workdir`,
  `EXPOSE 8080`, copies + chmods the shared entrypoint, applies OCI
  labels for GHCR, sets `ENTRYPOINT`.

The entrypoint seeds `web + web_ws + web_rest + canvas_webapp` on
first boot, then `exec fantastic`. Identical across variants — the
base image is the only thing that changes.
