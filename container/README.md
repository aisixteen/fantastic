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
   you don't need to choose anything. The kernel binds **`:8088` inside** the
   container (an unprivileged port — no root, no caps); map it straight through to
   host **`:8088`** (the documented default):
   ```sh
   podman|docker run -d --name fantastic -p 127.0.0.1:8088:8088 -v "$PWD":/work \
     ghcr.io/alexadar/fantastic:latest
   ```
   Only set `-e FANTASTIC_RUNTIME=rust|ts` if you specifically want another
   runtime; otherwise the safe default is `python`.
3. **Read the head** — **every container serves a descriptive head page at `/` by
   default** (it costs almost nothing and makes a fresh container self-explaining).
   This is all you need — no client library:
   - headful: open `GET http://127.0.0.1:8088/` → the all-readmes head page.
   - headless self-description: `curl -s -X POST -H 'Content-Type: application/json'
     http://127.0.0.1:8088/rest/kernel -d '{"type":"reflect","readme":true,"bundles":"all"}'`
     → the full root readme + bundle catalog.

   Don't want the head? Set `-e FANTASTIC_HEAD=off` and `/` shows the plain
   agent-tree index instead (the flag turns the head **off**, never on).
4. **Connect an LLM to the kernel and let it BUILD — this is the intended way.**
   The running kernel is fully drivable over `web_ws` (`GET /web/ws`) and `rest`
   (`POST /rest/<target>`) — **no client library, the protocol IS the API.** An LLM
   reads `reflect` (step 3), then **composes agents with `create_agent` and wires
   them with `send`** — building your app *inside this kernel*, operating on the
   folder you mounted at **`/work`** (the `file` / `terminal_backend` /
   `python_runtime` agents all act on it). Hand it a **[recipe](recipes.md)** plus
   the `reflect readme=true bundles=all` output and it assembles a working
   approximation — capability emerges from self-description, no bespoke glue.
   - **Federate, too:** the image also carries `kernel_bridge` + `local_runner` /
     `ssh_runner`, so the same chat can spawn and manage **other** kernels (one per
     project dir, local or remote) and treat each container as a unit at `host:port`
     — running and building fleets of kernels from one session.

## Build (local) + publish

```sh
sh container/build.sh                               # host arch → fantastic:latest (local)
PLATFORM=linux/amd64,linux/arm64 sh container/build.sh   # multi-arch manifest (local)
# publish (opt-in):
PUSH=1 PLATFORM=linux/amd64,linux/arm64 \
  TAG=ghcr.io/alexadar/fantastic:latest sh container/build.sh
```

> **One tag — `fantastic`.** There is a single universal image
> (`ghcr.io/alexadar/fantastic:latest`). `ts` / `rust` are **runtime modes** chosen
> at launch via `FANTASTIC_RUNTIME`, **not** separate tags (and the head page is on
> by default in every mode). Tag it `fantastic` (no `-head` / `-gpu` / per-runtime
> variants).

`build.sh` first ensures `ts/dist/js_kernel.zip` exists (runs `ts/scripts/pack.sh`
if missing) — the image **copies** that prebuilt bundle, it does not build JS.

## Run contract

```sh
podman|docker run -d --name fantastic-<nodeId> \
  --init \                          # or rely on the baked tini
  [--userns=keep-id] \              # podman rootless; docker omits
  -p 127.0.0.1:8088:8088 \          # host loopback :8088 → container :8088
  -v <hostWorkdir>:/work[:Z] \      # :Z = SELinux relabel (podman on RHEL/Fedora)
  -e FANTASTIC_RUNTIME=python \     # python (default) | rust | ts
  -e FANTASTIC_HEAD=on \            # head page at / (default on; `off` disables)
  -e FANTASTIC_WORKDIR=/work \
  fantastic:latest
```

- **Env:** `FANTASTIC_RUNTIME` (default `python`), `FANTASTIC_PORT` (default
  **8088**, bound inside the container), `FANTASTIC_HEAD` (default `on` — the head
  page at `/`; `off` shows the plain agent-tree index), `FANTASTIC_WORKDIR`
  (default `/work`), `FANTASTIC_JS_KERNEL_ZIP` (default
  `/opt/fantastic/js_kernel.zip` — always exported for discovery).
- **Port:** the kernel binds `0.0.0.0:8088` inside the isolated namespace by
  default — `8088` is **unprivileged**, so uid 1000 binds it with no root / no
  capabilities. Reach it via host loopback `-p 127.0.0.1:8088:8088` (same port in
  and out — the documented default). The host port is arbitrary; pick any free one
  (`-p <H>:8088`, recommend `H == 8088` so the port recorded in `lock.json` stays
  host-valid). To change the **inside** port too, set `-e FANTASTIC_PORT=<C>` and
  `-p <H>:<C>`.
- **Workdir:** bind-mount `/work` (your own project folder works — the kernel
  reads your existing files and writes `/work/.fantastic/`: records, `lock.json`).
  Runs as `USER fantastic` (uid 1000), so mounting a host folder it can't write is
  the one gotcha — the entrypoint fails fast with the fix if so:
  - **macOS (podman/docker):** just works — the VM maps mount ownership, files
    come out host-owned, no flag needed.
  - **rootless podman on Linux:** add **`--userns=keep-id`** (maps your host user
    into the container → host-owned, writable).
  - **docker on Linux:** add **`-u $(id -u):$(id -g)`** (run as your host uid).
  `:Z` (SELinux relabel) is supported; the image never relies on `:U`.
- **Signals / PID1:** `tini` is the entrypoint init (the kernel `exec`s under it);
  `SIGTERM` → graceful (release `lock.json`, drain the HTTP server), no zombies.
  Passing `--init` as well is harmless.
- **lock.json:** holds the **container-internal pid** — the host must **never
  `kill()`** it (maps to an unrelated host process). Stop by **container name**;
  liveness is a **port ping** (connectability-first), so the namespaced pid never
  matters.

## Call surface (composed at boot)

The entrypoint composes a **callable** kernel, not just a renderer — on `:8088`
inside (host `:8088` by default):

| surface | route | use |
|---|---|---|
| `web` | `GET /` , `GET /<id>/…` | HTTP host (the head page at `/` + child routes) |
| `web_ws` | `GET /web/ws` | WebSocket verb calls — the primary client transport |
| `rest` | `POST /rest/<target>` (body = payload) | REST diagnostics / one-shot verb calls |

Read the kernel's runtime in one round-trip (send the JSON content-type — rust's
REST surface requires it): `curl -s -X POST -H 'Content-Type: application/json'
http://127.0.0.1:8088/rest/kernel -d '{"type":"reflect"}'` →
`{… "runtime": "python"|"rust"|"ts", …}` — gate the app's create-type chooser on it.

## Headless + headful — both, on one port (they do NOT dim each other)

The container is simultaneously **headless** and **headful** on the **same**
mapped port (host `:8088` → container `:8088`); they are just different routes on the
one `web` host, so turning to one never disables the other:

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

**Every runtime serves the descriptive head page at `/` by default** (`python`,
`rust`, `ts` alike — see below). `FANTASTIC_RUNTIME=head` is still accepted as a
**back-compat alias of `python`**; you no longer need it — the head is on by
default everywhere. Set `FANTASTIC_HEAD=off` to drop it.

The embedded JS bundle is always present at `$FANTASTIC_JS_KERNEL_ZIP`; pull its
guide without unpacking: `unzip -p "$FANTASTIC_JS_KERNEL_ZIP" readme.md`.

## The head — served always by default (the brain-kernel seam)

The **descriptive head** is the all-readmes page (main → kernels → containers + the
GitHub URL) served at `/`. It is **on by default for every runtime** — serving it
costs almost nothing and makes a fresh container self-explaining, so it is the
right default. The same endpoint is BOTH the human-readable head page (`/`) AND a
live reflectable/bridgeable kernel (`reflect` / `web_ws` / `web_rest`) — the first
cut of the always-on **brain kernel** / "container runtime head".

- **Default mapping:** container binds `:8088` (unprivileged — no root/caps); map
  it straight through to host `:8088` (`-p 8088:8088`) → open
  `http://<host>:8088/`. The host port is arbitrary; `8088` is just the documented
  default.
- **Turn it off:** `-e FANTASTIC_HEAD=off` → `/` serves the plain agent-tree index
  instead. The flag only ever turns the head **off** (it's on by default).

It's intended to run alongside on-demand spawned kernels (which take their own
`FANTASTIC_PORT`). Examples + recipes will be embedded into the head later.

## Test the build

```sh
sh container/test/build_smoke.sh    # builds + smoke-tests the image (podman/docker)
```

Separate from the main test suites — it validates the **build + run contract**
(both runtimes boot + bind, `reflect.runtime` correct, the head page served at `/`
by default + `FANTASTIC_HEAD=off` falls back to the agent index, embedded zip
pullable, SIGTERM-clean, no JS engine / compilers in the final image), not kernel
logic.

## Quickstart recipes — what to build (hand any of these to an LLM)

These are **general recipes**: paste one to an LLM together with the kernel's own
self-description (`POST /rest/kernel {"type":"reflect","readme":true,"bundles":"all"}`)
and it assembles a working approximation by itself — `reflect` gives the live tree,
`bundles` what it can `create_agent`, each agent's `verbs` how to call them.
Capability **emerges** from self-description. Full versions: **[`recipes.md`](recipes.md)**.

Everything splits across **two kernels**: the **host** (this image — data/compute/
transport: `fs_loader web web_ws web_rest file python_runtime terminal_backend ai_*
yaml_state scheduler kernel_bridge local_runner ssh_runner`) and the **frontend**
(the embedded `js_kernel.zip` — the VIEW: `canvas terminal_view html_agent gl_agent
ai_view`). The host serves the frontend + relays the WS bus; panels are frontend
agents the **canvas** iframes (any agent answering `get_webapp`). Binding is weak —
by **id** + duck-typed verbs. Mount your project at `/work` so file/terminal/python
agents see it.

1. **Spatial canvas of panels** — `web`+`web_ws` host + `canvas` frontend; any agent
   answering `get_webapp` becomes a draggable, persisted tile. *(the base for the rest)*
2. **Terminal / dev console** — `terminal_backend` (PTY, cwd=project) + `terminal_view`
   (xterm), bound by id; flow-control + clipboard-image paste. *(PTY runs in-image)*
3. **AI chat with tool-use** — an `ai_*` backend (+ a `file` for history) + `ai_view`;
   the model calls `python_runtime`/`file`/`yaml_state` as tools and routes its own
   output (emergent, no `reply_to`). *(key via `-e ANTHROPIC_KEY`)*
4. **Background compute / training runner** — `python_runtime.start` → `job_id` +
   streamed `progress`/`job_done` → a live html panel (or the job's own UI via a
   `file` agent). *(⚠ GPU = host's; this image is CPU-only)*
5. **Live data / WebGL panel** — `gl_agent` (frontend) fed frames by a `python_runtime`
   job; assets via `/<file>/file/…`. *(⚠ your shaders are app content; headless WebGL off)*
6. **Generative audio-visual panel** — `html_agent`/`gl_agent` (WebAudio+WebGL) driven
   by a media `python_runtime`; serve audio via a `file` agent. *(⚠ WebAudio needs
   iframe `allow=autoplay`; cross-panel sync must go through a HOST bus agent — you wire it)*
7. **Federated multi-project canvas** — `local_runner` (local dir) / `ssh_runner`
   (remote) + `kernel_bridge` per peer; one canvas tile per project, each its **own**
   kernel. Each project can be a **container = a unit at `host:port`** (no shared
   network): bridge `host.containers.internal:<port>` same-host, `ws://<ip>:<port>`
   remote. *(the distribution shape)*
