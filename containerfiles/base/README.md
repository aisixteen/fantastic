# fantastic-canvas/base — container operator guide

## What this is

A containerized Fantastic Kernel. The image ships with the **full
canvas stack pre-seeded** on first boot (`web` on port 8080 + `web_ws`
+ `web_rest` + `canvas_webapp` + its auto-spawned `canvas_backend`)
and all 20+ standard bundles installed in the image's venv — they're
available, just not yet *added* to the tree. The container's `.fantastic/`
schema is identical to running `~/.local/bin/fantastic` locally in the
workdir, so the bind-mounted state is portable between container and
local-CLI modes (just not concurrently — the kernel's `lock.json`
prevents that).

## Image location

Two separate per-arch images on GHCR (no combined multi-arch manifest
yet — keeps each tag's footprint to one architecture's actual bytes):

```
ghcr.io/alexadar/fantastic-canvas/base:dev-amd64    # x86_64 / Intel-AMD
ghcr.io/alexadar/fantastic-canvas/base:dev-arm64    # Apple Silicon / aarch64
```

Tag scheme: `<base>-<arch>` where `<arch>` ∈ `amd64`, `arm64`. The
`<base>` part:
- `dev` — latest from the `container` branch
- `<git-sha>` — snapshot per commit
- semver — on releases

Per-arch dirs hold thin wrappers that pin `--platform` and call the
shared `generic/Containerfile`:

- `containerfiles/base-amd64/{build.sh, push.sh}` — x86_64
- `containerfiles/base-arm64/{build.sh, push.sh}` — aarch64

Push is manual (no CI yet) — one run per arch:

```bash
echo "$GHCR_PAT" | podman login ghcr.io -u <github_user> --password-stdin
# native arch on the Mac (Apple Silicon) → fast:
./containerfiles/base-arm64/push.sh
# the other arch via qemu → ~5× slower:
./containerfiles/base-amd64/push.sh
```

PAT scope: `write:packages`. The image's
`org.opencontainers.image.source` label auto-links the package to the
repo's "Packages" sidebar and inherits repo visibility.

## Pull

Pick the tag matching the host you're pulling on:

```bash
# x86_64 Linux server
podman pull ghcr.io/alexadar/fantastic-canvas/base:dev-amd64

# Apple Silicon / aarch64
podman pull ghcr.io/alexadar/fantastic-canvas/base:dev-arm64
```

## Build (local, for iteration)

```bash
# pick the arch dir matching your host
./containerfiles/base-arm64/build.sh    # Apple Silicon / aarch64 native
./containerfiles/base-amd64/build.sh    # x86_64 native (or via qemu on arm64)
```

Both wrap `podman build` with `BASE_IMAGE=python:3.11-slim` against
the shared recipe at `containerfiles/generic/Containerfile` — only
`--platform` differs. Override the output tag with `IMG=...`. Direct
invocation if you'd rather:

```bash
podman build \
  --platform linux/arm64 \
  -f containerfiles/generic/Containerfile \
  --build-arg BASE_IMAGE=python:3.11-slim \
  -t fantastic-canvas-base:dev-arm64 .
```

Use this when iterating without round-tripping through GHCR.

## Run

Single command. The name is **derived from the workdir path** so two
`podman run` invocations against the same workdir hit the **same**
container (idempotency by convention — industry standard). Replace
`<arch>` with `amd64` or `arm64` to match your host:

```bash
NAME="fantastic-$(echo "$PWD" | shasum | head -c8)"
ARCH=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
podman run -d --name "$NAME" \
  -v "$PWD:/workdir" \
  -p 8080:8080 \
  ghcr.io/alexadar/fantastic-canvas/base:dev-$ARCH
```

If files in the workdir come out owned by an unexpected UID inside the
container (rootless podman UID-mapping gotcha), add `--userns=keep-id`.

## Probe it works

```bash
curl http://localhost:8080/
```

Returns the substrate tree index (HTML with ↗ visit links). Then walk
deeper — find the `web_rest` id and hit its reflect:

```bash
WEB_ID=$(podman exec "$NAME" ls /workdir/.fantastic/agents | grep '^web_')
REST_ID=$(podman exec "$NAME" ls "/workdir/.fantastic/agents/$WEB_ID/agents" | grep '^web_rest_')
curl -s "http://localhost:8080/$REST_ID/_reflect" | python3 -m json.tool | head -40
```

`available_bundles` in the reply lists every standard bundle the
image ships — what you can `create_agent` from.

## The canvas

Find the canvas id and open it:

```bash
CANVAS_ID=$(podman exec "$NAME" ls /workdir/.fantastic/agents | grep '^canvas_webapp_')
open "http://localhost:8080/$CANVAS_ID/"   # macOS; use xdg-open on Linux
```

You get an empty Liquid-Glass canvas. **Double-click on empty canvas**
spawns a `terminal_webapp` tile via `canvas_backend.add_agent` —
that's the operator's main interaction loop. Terminals, html_agents,
and gl_agents land on the canvas this way (or via REST / WS
`add_agent` calls against the `canvas_backend` id).

## Install more bundles

Third-party bundles install into the image's venv:

```bash
podman exec "$NAME" fantastic install-bundle git+https://github.com/user/bundle
podman restart "$NAME"
```

`podman restart` is **graceful** — SIGTERM → kernel tree-walk
shutdown → clean exit → next boot picks up the new entry points. The
persisted `.fantastic/` survives untouched.

## Persistence

Everything in `.fantastic/` survives `stop` / `restart` / `rm + run`.
The bind-mounted workdir **is** the durable state. To start fresh:

```bash
podman stop "$NAME" && podman rm "$NAME"
rm -rf .fantastic
```

## Stop

```bash
podman stop "$NAME"
```

SIGTERM is handled by the kernel's graceful-shutdown path — PTYs
close, uvicorn drains, the agent tree walks down depth-first, the
container exits 0.

## Inspect

```bash
podman exec "$NAME" fantastic reflect return_readme=true   # live primer w/ bundle readmes
podman logs "$NAME"                                         # kernel stdout/stderr
```

`reflect return_readme=true` returns the bootstrap reflection an LLM
needs to drive the system: transports, `available_bundles`, agent
tree, binary protocol, browser bus — same shape as the WS bootstrap,
plus per-bundle readmes inline.

## One container = one workdir

The name is hash-derived from `$PWD`, so two `podman run` invocations
on the same workdir are idempotent — the second fails with "container
already exists". **That's the safety, not a bug**: it prevents two
kernels racing on the same `.fantastic/lock.json`. Different workdir
= different hash = different container, fully isolated.
