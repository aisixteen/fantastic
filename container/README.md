# Universal Fantastic kernel container

One fat OCI image — a **headless runtime collection** the app's runner spawns as
a `container`-backend node. Same recipe under **podman and docker**, shipped as
**two separate per-arch image tags** (`:amd64` + `:arm64`) — pick the one for your
machine (no merged manifest).

- **Two execution runtimes:** `python` (canonical) + `rust` (prebuilt binary).
- **One bundled runtime:** the static `js_kernel.zip` (the browser frontend),
  copied prebuilt from `ts/` and discovered at runtime — **no JS engine** runs it
  (no node/bun/deno, by security decision).
- **No swift** — its HTTP server is `Network.framework`-only (can't serve on
  Linux); it stays native. Re-containerizing it needs a SwiftNIO/Hummingbird port.

The kernels are self-describing; given only their `reflect` + the zip's readme an
LLM weaves/​revives the wiring itself (the emergent-code capability).

**Published images (public, GHCR) — pick your architecture:**
- `ghcr.io/alexadar/fantastic:amd64` — **Intel / AMD x86-64** (a.k.a. x64); pinned `:vX.Y.Z-amd64`
- `ghcr.io/alexadar/fantastic:arm64` — **Apple silicon / ARM** (aarch64); pinned `:vX.Y.Z-arm64`

These are **single-arch images** — the registry does NOT auto-select, so pull the
one matching `uname -m` (`x86_64`→`:amd64`, `arm64`/`aarch64`→`:arm64`). There is
**no merged `:latest` manifest** (by design — pick explicitly).

## Quickstart for an AI chat (e.g. Claude Code) — the distribution model

> **Tagline:** *"Pull `ghcr.io/alexadar/fantastic:<your-arch>`, run it, then
> read its head to learn what it does and how to drive it."*

1. **Image tag — choose by arch:** `:amd64` (Intel/AMD x86-64) or `:arm64` (Apple
   silicon / ARM). `uname -m` → `x86_64`/`amd64` ⇒ `:amd64`; `arm64`/`aarch64` ⇒
   `:arm64`.
2. **Run it** (defaults to the `python` kernel; set `-e FANTASTIC_RUNTIME=rust` for
   the rust host). The kernel can bind any unprivileged port; `8088` is the
   documented suggestion:
   ```sh
   podman|docker run -d --name fantastic -p 127.0.0.1:8088:8088 -v "$PWD":/work \
     ghcr.io/alexadar/fantastic:arm64        # :amd64 on Intel/AMD
   ```
   > **The image composes NOTHING (no agent autocreation).** It boots exactly what
   > `/work/.fantastic` already contains. So either **mount a project that carries
   > its own web stack** (e.g. a migrated app — it serves immediately), **or have
   > your AI compose one** — it drives the kernel to create `web`/`web_ws`/`rest`.
   > A blank workdir serves nothing until a web host is composed (the entrypoint
   > prints the exact `create_agent` hint when it finds none).
3. **Read the head** — once a web host exists, with head on (default) its `/`
   serves the descriptive head page; the kernel is also drivable headless:
   - headful: open `GET http://127.0.0.1:<port>/` → the all-readmes head page.
   - headless self-description: `curl -s -X POST -H 'Content-Type: application/json'
     http://127.0.0.1:<port>/<rest_id>/kernel -d '{"type":"reflect","readme":true,"bundles":"all"}'`
     → the full root readme + bundle catalog.

   A composed web's `/` is always the live agent-tree index; serve the descriptive
   head through a gated `file_bridge` over `/opt/fantastic/head` (see "The head").
4. **Connect an LLM to the kernel and let it BUILD — this is the intended way.**
   The running kernel is fully drivable over `web_ws` (`GET /web/ws`) and `rest`
   (`POST /rest/<target>`) — **no client library, the protocol IS the API.** An LLM
   reads `reflect` (step 3), then **composes agents with `create_agent` and wires
   them with `send`** — building your app *inside this kernel*, operating on the
   folder you mounted at **`/work`** (the `file_bridge` / `terminal_backend` /
   `python_runtime` agents all act on it). Hand it a **[recipe](recipes.md)** plus
   the `reflect readme=true bundles=all` output and it assembles a working
   approximation — capability emerges from self-description, no bespoke glue.
   - **Federate, too:** the image also carries `ws_bridge` + `local_runner` /
     `ssh_runner`, so the same chat can spawn and manage **other** kernels (one per
     project dir, local or remote) and treat each container as a unit at `host:port`
     — running and building fleets of kernels from one session.

## Build (local) + publish

```sh
sh container/build.sh                               # host arch → fantastic:latest (local)
ARCH=arm64 sh container/build.sh                    # one arch (native) → fantastic:arm64
ARCH=amd64 sh container/build.sh                    # one arch (emulated on arm host) → fantastic:amd64
# publish ONE arch (opt-in) — push its own tag:
PUSH=1 ARCH=arm64 TAG=ghcr.io/alexadar/fantastic:arm64 sh container/build.sh
PUSH=1 ARCH=amd64 TAG=ghcr.io/alexadar/fantastic:amd64 sh container/build.sh
```

CI builds each arch on its OWN native runner (amd64 on `ubuntu-latest`, arm64 on
`ubuntu-24.04-arm`) and pushes its own tag — see `.github/workflows/release.yml`.

> **Per-arch tags, not a merged manifest.** The image is published as **`:amd64`**
> and **`:arm64`** (+ `:vX.Y.Z-<arch>`) — pick one explicitly. `python` / `rust` are
> **runtime modes** chosen at launch via `FANTASTIC_RUNTIME`, **not** separate tags
> (and the head page is on by default in every mode). No `-head` / `-gpu` variants.

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
  -e FANTASTIC_WORKDIR=/work \
  ghcr.io/alexadar/fantastic:arm64   # or :amd64 — pick your arch
```

- **Env:** `FANTASTIC_RUNTIME` (default `python`), `FANTASTIC_PORT` (default
  **8088**, bound inside the container), `FANTASTIC_WORKDIR`
  (default `/work`), `FANTASTIC_JS_KERNEL_ZIP` (default
  `/opt/fantastic/js_kernel.zip` — always exported for discovery). (`/` is always
  the agent-tree index; the head page rides the gated `/head/file/index.html`
  route — no env back-channel.)
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

## Call surface (composed by the operator, not the image)

Once a web host is composed (by the project or your AI — the image autocreates
nothing), it is a **callable** kernel, not just a renderer:

| surface | route | use |
|---|---|---|
| `web` | `GET /` , `GET /<id>/…` | HTTP host (the head page at `/` + child routes) |
| `web_ws` | `GET /<web_ws_id>/ws` | WebSocket verb calls — the primary client transport |
| `rest` | `POST /<rest_id>/<target>` (body = payload) | REST diagnostics / one-shot verb calls |

To compose it explicitly (the documented procedure the image used to do for you):
```sh
fantastic <root> create_agent handler_module=web.tools id=web port=8088
fantastic web   create_agent handler_module=web_ws.tools id=web_ws
fantastic web   create_agent handler_module=web_rest.tools id=rest
```

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
  embedded frontend is servable (`GET /js_kernel/file/bundle.min.js`, the
  copy-from-zip bundle you serve) — a visible UI.

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

The frontend is not a runtime — it is the copy-from-zip JS bundle you serve. The
embedded JS bundle is at `$FANTASTIC_JS_KERNEL_ZIP`; pull its guide
without unpacking (`unzip -p "$FANTASTIC_JS_KERNEL_ZIP" readme.md`) — then COPY
`bundle.min.js` out of it into your project and serve it (copy-from-zip; the image
is not a CDN).

## The head — served through the gated file route (no env back-channel)

The **descriptive head** is the all-readmes page (main → kernels → containers + the
GitHub URL), baked at `/opt/fantastic/head/index.html`. On EVERY runtime the web's
`/` is the live **agent-tree index** — the kernel's own web owns no `fs` surface, so
there is no `FANTASTIC_WEB_INDEX`/`FANTASTIC_HEAD` env that reads a landing file off
disk (deleted to match Python; what Python deletes, rust + swift delete). To show
the head, serve it the one gated way — a read-only `file_bridge`. The fs edge clamps
every root INSIDE the running dir, so copy the baked head into the workdir first,
then root the bridge at the relative dir:

```sh
cp /opt/fantastic/head/index.html "$FANTASTIC_WORKDIR/head/"     # into the workdir
$BIN <web> create_agent handler_module=file_bridge.tools id=head \
     root=head readonly=true ingress_rule=allow_all
# → http://<host>:8088/head/file/index.html  (piped via read_stream, chunked)
```

That web is BOTH a human-readable surface AND a live reflectable/bridgeable kernel
(`reflect` / `web_ws` / `web_rest`). The head appears once there's a web — not on a
blank workdir (the image composes nothing).

- **Default mapping:** container binds `:8088` (unprivileged — no root/caps); map
  it straight through to host `:8088` (`-p 8088:8088`) → open
  `http://<host>:8088/`. The host port is arbitrary; `8088` is just the documented
  default.

It's intended to run alongside on-demand spawned kernels (which take their own
`FANTASTIC_PORT`).

## Test the build

```sh
sh container/test/build_smoke.sh    # builds + smoke-tests the image (podman/docker)
```

Separate from the main test suites — it validates the **build + run contract**
(both runtimes boot + bind a **composed** web, `reflect.runtime` correct, `/` is
the agent-tree index + the head page rides the gated `/head/file/index.html`
route, **a blank workdir autocreates nothing**, embedded zip pullable,
SIGTERM-clean, no JS engine / compilers in the final image), not kernel logic.

## Quickstart recipes — what to build (hand any of these to an LLM)

These are **general recipes**: paste one to an LLM together with the kernel's own
self-description (`POST /rest/kernel {"type":"reflect","readme":true,"bundles":"all"}`)
and it assembles a working approximation by itself — `reflect` gives the live tree,
`bundles` what it can `create_agent`, each agent's `verbs` how to call them.
Capability **emerges** from self-description. Full versions: **[`recipes.md`](recipes.md)**.

Everything splits across **two kernels**: the **host** (this image — data/compute/
transport: `kernel_state web web_ws web_rest file_bridge python_runtime terminal_backend ai_*
yaml_state scheduler ws_bridge local_runner ssh_runner`) and the **frontend**
(the embedded `js_kernel.zip` — the VIEW: `canvas terminal_view html_agent gl_agent
ai_view`). The host serves the frontend + relays the WS bus; panels are frontend
agents the **canvas** iframes (any agent answering `get_webapp`). Binding is weak —
by **id** + duck-typed verbs. Mount your project at `/work` so file/terminal/python
agents see it.

1. **Spatial canvas of panels** — `web`+`web_ws` host + `canvas` frontend; any agent
   answering `get_webapp` becomes a draggable, persisted tile. *(the base for the rest)*
2. **Terminal / dev console** — `terminal_backend` (PTY, cwd=project) + `terminal_view`
   (xterm), bound by id; flow-control + clipboard-image paste. *(PTY runs in-image)*
3. **AI chat with tool-use** — an `ai_*` backend (+ a `file_bridge` for history) + `ai_view`;
   the model calls `python_runtime`/`file_bridge`/`yaml_state` as tools and routes its own
   output (emergent, no `reply_to`). *(key via `-e ANTHROPIC_KEY`)*
4. **Background compute / training runner** — `python_runtime.start` → `job_id` +
   streamed `progress`/`job_done` → a live html panel (or the job's own UI via a
   `file_bridge` agent). *(⚠ GPU = host's; this image is CPU-only)*
5. **Live data / WebGL panel** — `gl_agent` (frontend) fed frames by a `python_runtime`
   job; assets via `/<file>/file/…`. *(⚠ your shaders are app content; headless WebGL off)*
6. **Generative audio-visual panel** — `html_agent`/`gl_agent` (WebAudio+WebGL) driven
   by a media `python_runtime`; serve audio via a `file_bridge` agent. *(⚠ WebAudio needs
   iframe `allow=autoplay`; cross-panel sync must go through a HOST bus agent — you wire it)*
7. **Federated multi-project canvas** — `local_runner` (local dir) / `ssh_runner`
   (remote) + `ws_bridge` per peer; one canvas tile per project, each its **own**
   kernel. Each project can be a **container = a unit at `host:port`** (no shared
   network): bridge `host.containers.internal:<port>` same-host, `ws://<ip>:<port>`
   remote. *(the distribution shape)*
