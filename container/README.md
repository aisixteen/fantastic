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

**Published image:** `ghcr.io/alexadar/fantastic:latest` — multi-arch
manifest (`linux/amd64` + `linux/arm64`, one tag), currently **private** (the host
must `podman/docker login ghcr.io`, or make the package public).

## Quickstart for an AI chat (e.g. Claude Code) — the distribution model

> **Tagline:** *"Pull `ghcr.io/alexadar/fantastic:latest`, run it, then
> read its head to learn what it does and how to drive it."*

1. **Image tag:** `ghcr.io/alexadar/fantastic:latest` (multi-arch; one tag
   serves amd64 + arm64).
2. **Run** — **not technical? just run it, no flags.** With no `FANTASTIC_RUNTIME`
   set, the image **defaults to the `python` kernel** (the canonical reference) —
   you don't need to choose anything:
   ```sh
   podman|docker run -d --name fantastic -p 127.0.0.1:8888:8888 -v "$PWD":/work \
     ghcr.io/alexadar/fantastic:latest
   ```
   Only set `-e FANTASTIC_RUNTIME=rust|ts|head` if you specifically want another
   mode; otherwise the safe default is `python`.
3. **Read the head** (this is all you need — no client library):
   - headful: `GET http://127.0.0.1:8888/`
   - headless self-description: `curl -s -X POST -H 'Content-Type: application/json'
     http://127.0.0.1:8888/rest/kernel -d '{"type":"reflect","readme":true,"bundles":"all"}'`
     → the full root readme + bundle catalog. (Or run `FANTASTIC_RUNTIME=head` and
     just open `/` — the all-readmes page.)
4. **Then drive / build more from the chat:** the image carries `kernel_bridge` +
   `local_runner`, so an AI chat that pulled+ran this image can spawn and manage
   **other** Fantastic kernels (e.g. one per project dir) over the bridge — running
   and building fleets of kernels straight from a chat session.

## Build (local) + publish

```sh
sh container/build.sh                               # host arch → fantastic:latest (local)
PLATFORM=linux/amd64,linux/arm64 sh container/build.sh   # multi-arch manifest (local)
# publish (opt-in):
PUSH=1 PLATFORM=linux/amd64,linux/arm64 \
  TAG=ghcr.io/alexadar/fantastic:latest sh container/build.sh
```

> **One tag — `fantastic`.** There is a single universal image
> (`ghcr.io/alexadar/fantastic:latest`). `head` / `ts` / `rust` are **runtime
> modes** chosen at launch via `FANTASTIC_RUNTIME`, **not** separate tags. Tag it
> `fantastic` (no `-head` / `-gpu` / per-runtime variants).

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

Read the kernel's runtime in one round-trip (send the JSON content-type — rust's
REST surface requires it): `curl -s -X POST -H 'Content-Type: application/json'
http://127.0.0.1:<H>/rest/kernel -d '{"type":"reflect"}'` →
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
| `head` | the **descriptive head**: a python kernel that serves the **all-readmes page** at `/` (main → kernels → containers + the GitHub URL) AND stays a reflectable/bridgeable brain kernel (`reflect` / `web_ws` / `web_rest`). Binds a rootless-safe port inside → map host **`:80`** with `-p 80:8080`. `docker run -d -p 80:8080 -e FANTASTIC_RUNTIME=head <image>` → open `http://<host>/`. |

The embedded JS bundle is always present at `$FANTASTIC_JS_KERNEL_ZIP`; pull its
guide without unpacking: `unzip -p "$FANTASTIC_JS_KERNEL_ZIP" readme.md`.

## The head as a brain kernel

`FANTASTIC_RUNTIME=head` is the first cut of the always-on **brain kernel** /
"container runtime head": one endpoint that is BOTH the human-readable all-readmes
page (`/`) AND a live reflectable/bridgeable kernel (`reflect` / `web_ws` /
`web_rest`). It's intended to run alongside on-demand spawned kernels (which use
their own `FANTASTIC_PORT`); a host maps it to `:80`. Examples + recipes will be
embedded into the head later.

## Test the build

```sh
sh container/test/build_smoke.sh    # builds + smoke-tests the image (podman/docker)
```

Separate from the main test suites — it validates the **build + run contract**
(both runtimes boot + bind, `reflect.runtime` correct, embedded zip pullable,
SIGTERM-clean, no JS engine / compilers in the final image), not kernel logic.
