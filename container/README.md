# Universal Fantastic kernel container

One fat OCI image — a **headless runtime collection** the app's runner spawns as
a `container`-backend node. Same image under **podman and docker**, amd64 + arm64.

- **Two execution runtimes:** `python` (canonical) + `rust` (prebuilt binary).
- **One bundled runtime:** the static `js_kernel.zip` (the browser frontend),
  copied prebuilt from `ts/` and discovered at runtime — **no JS engine** runs it
  (no node/bun/deno, by security decision).
- **No swift** — its HTTP server is `Network.framework`-only (can't serve on
  Linux); it stays native. Re-containerizing it needs a SwiftNIO/Hummingbird port.

The kernels are self-describing; given only their `reflect` + the zip's readme an
LLM weaves/​revives the wiring itself (the emergent-code capability).

**Published image:** `ghcr.io/alexadar/fantastic-canvas/base:latest` — multi-arch
manifest (`linux/amd64` + `linux/arm64`, one tag), currently **private** (the host
must `podman/docker login ghcr.io`, or make the package public).

## Build (local) + publish

```sh
sh container/build.sh                               # host arch → fantastic:latest (local)
PLATFORM=linux/amd64,linux/arm64 sh container/build.sh   # multi-arch manifest (local)
# publish the universal `base` tag (opt-in):
PUSH=1 PLATFORM=linux/amd64,linux/arm64 \
  TAG=ghcr.io/alexadar/fantastic-canvas/base:latest sh container/build.sh
```

`build.sh` first ensures `ts/dist/js_kernel.zip` exists (runs `ts/scripts/pack.sh`
if missing) — the image **copies** that prebuilt bundle, it does not build JS.

## Run contract

```sh
podman|docker run -d --name fantastic-<nodeId> \
  --init \                          # or rely on the baked tini
  [--userns=keep-id] \              # podman rootless; docker omits
  -p 127.0.0.1:<H>:<C> \            # host loopback only
  -v <hostWorkdir>:/work[:Z] \      # :Z = SELinux relabel (podman on RHEL/Fedora)
  -e FANTASTIC_RUNTIME=python \     # python (default) | rust | ts
  -e FANTASTIC_PORT=<C> \           # bound 0.0.0.0:<C> INSIDE the container
  -e FANTASTIC_WORKDIR=/work \
  fantastic:latest
```

- **Env:** `FANTASTIC_RUNTIME` (default `python`), `FANTASTIC_PORT` (default 8888),
  `FANTASTIC_WORKDIR` (default `/work`), `FANTASTIC_JS_KERNEL_ZIP`
  (default `/opt/fantastic/js_kernel.zip` — always exported for discovery).
- **Port:** the kernel binds `0.0.0.0:<C>` inside the isolated namespace; reach it
  via host loopback `-p 127.0.0.1:<H>:<C>`. Recommend `H == C` so the port in
  `lock.json` stays host-valid; else the runner records `H` in its accounting.
- **Workdir:** bind-mount `/work`; the kernel writes `/work/.fantastic/` (records,
  `lock.json`). Runs as `USER fantastic` (uid 1000); with `--userns=keep-id`
  (podman) the files come out host-owned. Works with/without `:Z` and `keep-id`;
  the image never relies on `:U`.
- **Signals / PID1:** `tini` is the entrypoint init (the kernel `exec`s under it);
  `SIGTERM` → graceful (release `lock.json`, drain the HTTP server), no zombies.
  Passing `--init` as well is harmless.
- **lock.json:** holds the **container-internal pid** — the host must **never
  `kill()`** it (maps to an unrelated host process). Stop by **container name**;
  liveness is a **port ping** (connectability-first), so the namespaced pid never
  matters.

## Call surface (composed at boot)

The entrypoint composes a **callable** kernel, not just a renderer — on `<C>`:

| surface | route | use |
|---|---|---|
| `web` | `GET /` , `GET /<id>/…` | HTTP host (rendering + child routes) |
| `web_ws` | `GET /web/ws` | WebSocket verb calls — the primary client transport |
| `rest` | `POST /rest/<target>` (body = payload) | REST diagnostics / one-shot verb calls |

Read the kernel's runtime in one round-trip:
`curl -s -X POST http://127.0.0.1:<H>/rest/kernel -d '{"type":"reflect"}'` →
`{… "runtime": "python"|"rust"|"ts", …}` — gate the app's create-type chooser on it.

## Headless + headful — both, on one port (they do NOT dim each other)

The container is simultaneously **headless** and **headful** on the **same**
mapped port `<H>`; they are just different routes on the one `web` host, so
turning to one never disables the other:

- **Headless** (machines / the runner / an LLM): `web_ws` (`/web/ws`) + `rest`
  (`POST /rest/<target>`) — call verbs, stream events, `reflect`. No browser
  needed; this is how a program drives the kernel.
- **Headful** (a human or an LLM with a browser): `GET /` renders, and the
  embedded frontend is servable (`GET /js_kernel/file/bundle.min.js`, esp. with
  `FANTASTIC_RUNTIME=ts`) — a visible UI.

Because both surfaces are live at once, an LLM pointed at the URL can **read** the
page *and* `reflect` for the machine self-description (`reflect readme=true
bundles=all` returns the full root readme + bundle catalog) from the same origin —
enough to figure out the whole system unaided. Neither mode is a separate
build/flag; one running container serves both.

## Runtimes

| `FANTASTIC_RUNTIME` | what runs |
|---|---|
| `python` (default) | the canonical python kernel daemon |
| `rust` | the prebuilt rust kernel daemon (same CLI surface, same `.fantastic`) |
| `ts` | a python host that also serves the embedded `js_kernel.zip` via a `file` agent (`/js_kernel/file/…`); the LLM pulls the zip's readme + revives the frontend on demand. No JS process. |

The embedded JS bundle is always present at `$FANTASTIC_JS_KERNEL_ZIP`; pull its
guide without unpacking: `unzip -p "$FANTASTIC_JS_KERNEL_ZIP" readme.md`.

## Future seam (not implemented)

A later always-on **brain kernel** — a long-lived kernel on a reserved internal
port (the "container runtime head"), separate from the on-demand spawned kernels
(analogous to the app's own brain kernel). The entrypoint's runtime/port dispatch
is structured to admit it; it is intentionally not built in this pass.

## Test the build

```sh
sh container/test/build_smoke.sh    # builds + smoke-tests the image (podman/docker)
```

Separate from the main test suites — it validates the **build + run contract**
(both runtimes boot + bind, `reflect.runtime` correct, embedded zip pullable,
SIGTERM-clean, no JS engine / compilers in the final image), not kernel logic.
