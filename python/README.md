# fantastic-kernel

A medium that unifies humans and AIs into a single workspace.
Plugin-discovered agents, one primitive (`send`), hermetic protocol.

**Emergent, self-described wiring.** Every agent is at once an *actor* and a
`reflect`-able *description of itself*, and reaches any peer through one primitive
(`send`) that the kernel routes local-or-remote. So an LLM can introspect the live
system from its own readme + `reflect` surface and *compose* it — weaving agents
across the federated two-kernel topology (this host Python kernel ⇄ a browser JS
frontend kernel) into **durable interactive wiring** (e.g. an HTML panel whose
button runs background Python and pushes the result to a sibling panel) that humans
then actuate with no model in the loop. Capabilities and topologies are emergent
compositions read out of the substrate's own self-account — not features engineered
into it; the running artifact is a function of its *descriptance*, not its code.

## Concept

- **Agent** — recursive node, the universal type. Every entity in the
  system is an `Agent`. Agents have a persistent record on disk
  (`agent.json`), an asyncio inbox, and a `_children` dict that's
  empty for leaves and populated for hosts. Same code at every depth.
  Each agent answers `{type:"reflect"}` returning a flat self-description.
- **Kernel** — tree-wide shared context object. NOT an agent. NOT a
  base class. Constructed explicitly: `kernel = Kernel()`. Holds
  `agents: dict[id, Agent]` (flat routing index — derived from the
  tree, never written externally), `root: Agent` (the tree root),
  state subscribers, bundle resolver cache, well-known names. Exposes
  `kernel.create/delete/update/list/get/send` for tree management.
- **`fs_loader`** — the tree root IS an `fs_loader` agent
  (`id="fs_loader"`, `handler_module="fs_loader.tools"`): the
  persistence/hydration root that owns `.fantastic/`. A fresh dir
  seeds `{id:"fs_loader", handler_module:"fs_loader.tools"}`. The
  bootstrap composes a stdout renderer child when stdin is a tty.
- **send** — the one primitive. `kernel.send(target_id, payload) →
  reply | None` (from outside any handler) or `agent.send(...)`
  (from inside) resolves the target via the flat index and
  dispatches.
- **Cascade delete** — `delete_agent` walks the subtree depth-first
  via `_children`. Each descendant runs its `on_delete` hook (kills
  PTY / drains uvicorn / closes clients — PROCESS state only)
  BEFORE detaching from `kernel.agents` and the parent's
  `_children`. DISK cleanup is decoupled: the `removed` state event
  drives the loader to rmtree the dir, NOT `on_delete`. Any
  `delete_lock` anywhere in the subtree blocks the entire cascade
  with `{locked, blocked_by, error}` — no partial mutations.
- **Persistence** — DECOUPLED from `Agent`, which never touches disk.
  A loader agent (`fs_loader`) owns the medium, driven by the kernel
  STATE STREAM (debounced flush). The kernel exposes `save()`/`load()`
  over a flat record list (weak-load of unknown bundles). `main.py`
  bootstraps `Kernel()` → `fs_loader.read_tree('.fantastic')` →
  `kernel.load(records)`, rehydrating the whole tree by ids and
  respecting parent-child links. Process-memory state (in-flight
  counters, inboxes, PTY children) does NOT survive; bundles' `_boot`
  respawns it. Agents with class-level `ephemeral=True` (e.g. the cli
  renderer) are never persisted — composition is per-process.
- **bundles** — pip-installable Python packages discovered via the
  `fantastic.bundles` entry-point group. Each bundle ships a
  `tools.py` with verb handlers + optional `_boot` / `on_delete`
  lifecycle hooks. Idempotent first-boot lets a bundle declaratively
  own its child agents (e.g., a host bundle's `_boot` spawns a
  backend child the first time it boots; on subsequent boots the
  child is already present from disk).
- **web** — HTTP+WS transport bundle (uvicorn). Serves each agent's
  UI at `/{agent_id}/`, proxies WS frames to/from `kernel.send`, and
  serves a tree-shape index at `/` (with ↗ visit links for HTML-
  serving agents and ⊙ reflect popups). The browser frontend brings
  its own typed WS bridge — see the TypeScript frontend kernel below.
- **python_runtime** — async Python JOB spawner. `start(code, cwd?)`
  launches `<interp> -u -c <code>` as a parallel background job, streaming
  `progress`/`job_done` events; `status` / `stop` / `interrupt` / `clear`
  by job id; each job gets an injected `kernel` connector. Per-agent venv
  resolution (record `python` / `venv` fields override the interpreter).
- **yaml_state** — durable YAML key-value memory agent; mount anywhere
  (`mode=mem|data`). A write-through `state.yaml` sidecar it owns
  directly. Reached like any other unit by id (`send(<id>, {...})`), so
  compute, inference, and memory share one calling convention.
- **terminal_backend** — PTY shell session. The browser xterm view
  (`terminal_view`) lives in the TS frontend kernel. The backend
  ports VSCode's terminal robustness: streaming **flow control** (the
  PTY reader pauses past 100K unacknowledged chars and resumes on the
  consumer's `ack` verb — backpressure so a flood can't lock up a
  tab), **incremental UTF-8 decode** (a multi-byte char split across
  an `os.read` chunk is reassembled, not shattered into `<?>` litter),
  and **serialized full-buffer writes** (large/bracketed pastes land
  whole). `paste_image` bridges a browser-clipboard image into a
  CLI running in the PTY (e.g. `claude`): the view ships the bytes,
  the backend saves a file and types its path in — mimicking a
  drag-drop, since the server can't reach the browser clipboard.
- **kernel_bridge + ssh_runner** — cross-host. `ssh_runner` uses
  subprocess SSH to start/stop a remote `fantastic` and keeps
  a local tunnel open so a canvas can iframe the remote webapp.
  `kernel_bridge` opens WS (or SSH+WS) to a peer kernel's `web_ws`
  and ships raw call frames (asymmetric — no peer bridge needed);
  local agents reach remote agents through it without merging the two
  address spaces, and `watch_remote` streams a remote agent's emits
  back onto the bridge's inbox. Weak proxy: local→local comms stay
  direct.
- **TS frontend kernel** — the browser frontend is a TypeScript
  kernel in the repo's top-level `ts/` package. It runs as a pure
  peer and federates to the host over the SAME WS bridge wire
  (`web_ws`) — bringing its own typed WS bridge, not a server-injected
  script. It is a canvas compositor + view-agents for terminal/chat/GL
  content, vendored hermetically (no CDN). Python knows
  nothing of the `ts/` package — it's served weakly through a `file`
  agent rooted at the built `ts/dist`, which serves both the bundle and
  a static `index.html` mount page over the web host's
  `/<file_id>/file/<path>` proxy. Frontend records persist back to host
  disk under `.fantastic/web/<session>/` via the frontend's
  `proxy_loader`. Recipe: [`ts/SERVE.md`](ts/SERVE.md).

Message flow:

```
                       SERVER                              BROWSER
  agent ──┐                                              ┌── view-agent
           ├── kernel.send / emit / watch ──── WS ──── ┤   (TS frontend
  agent ──┘   (text + binary frames)                    ├──  kernel, a
                                                          │   pure peer)
                                                          └── view-agent
```

## Install

```bash
uv sync                 # builds & installs all workspace bundles editable
uv sync --dev           # + pytest, xdist, etc. (for tests)
```

New bundles dropped under `bundled_agents/` or `installed_agents/`
with a `pyproject.toml` are auto-picked up on the next `uv sync`.

## Run

```bash
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default kernel)
fantastic fs_loader create_agent handler_module=web.tools port=8888    # persist web (one-shot; then `fantastic` daemonizes)
# Equivalent direct invocation: `python main.py [args]`
# main.py bootstraps: Kernel() -> fs_loader.read_tree('.fantastic') -> kernel.load(records).
# Web composition is explicit — no --port flag. The kernel blocks only
# when something keeps it alive (web agent on disk OR REPL stdin loop).
```

REPL example:

```
fantastic> add web port=8888                        # uvicorn boots
fantastic> add file
fantastic> add terminal_backend                      # PTY shell (xterm view lives in the TS frontend)
# browse http://localhost:8888/  → tree view with ↗ visit links
# the TS frontend kernel renders the canvas + its members (see ts/SERVE.md)
```

## Drive from outside

`web` is a uvicorn host that serves HTML rendering only. Verb-
invocation surfaces are sub-agents of `web`:

- **`web_ws`** — WebSocket channel at `/<host_id>/ws`. Full duplex:
  `call`, `emit`, `watch`, `state_subscribe`.
- **`web_rest`** — REST diagnostic channel at `POST /<rest_id>/<target_id>`.
  Request/reply only; curl-friendly.

Compose them per project:
```bash
fantastic fs_loader create_agent handler_module=web.tools port=8888
fantastic fs_loader create_agent handler_module=web_ws.tools parent_id=<web_id>
fantastic fs_loader create_agent handler_module=web_rest.tools parent_id=<web_id>
```

After `fantastic`:
```bash
# Rendering (always available)
curl http://localhost:8888/                            # root index — agent tree (HTML)
curl http://localhost:8888/<id>/                       # agent UI (HTML) if it ships render_html
curl http://localhost:8888/<id>/file/<path>            # file proxy for any agent answering `read`

# WS (when web_ws is mounted)
# open ws://host/<id>/ws and send
#   {"type":"call","target":"<id>","payload":{...},"id":"<corr>"}

# REST (when web_rest is mounted; <rest_id> is the agent's id)
curl -X POST -H 'content-type: application/json' \
  -d '{"type":"reflect"}' http://localhost:8888/<rest_id>/<target_id>
```

The protocol IS the API — no client library. Send
`{"type":"call","target":"kernel","payload":{"type":"reflect","readme":true,"bundles":"all"}}`
to either surface to discover the substrate (agent `tree`, the
installable-bundle catalog, and the root readme with the
transport/wire docs). Any LLM CLI dropped in cold can
bootstrap from one WS or HTTP round-trip.

**Weak binding for bridges.** `kernel_bridge` reaches a remote
kernel's `web_ws` by URL + path only — `ws://host/<peer_id>/ws`,
where `<peer_id>` is just the WS path segment (typically `fs_loader`). No
shared Python types cross the wire. (WS-only since the REST bridge
transport was dropped; the `web_rest` diagnostic surface is
unrelated and still ships.)

## Plugin system

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. The kernel
discovers bundles uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins.

Add a bundle by dropping its package under `bundled_agents/` (or
`installed_agents/`) and running `uv sync` — its entry point registers on
the next start, and you can `create_agent handler_module=<bundle>.tools`
from any agent (creates as a child of that agent).

## Tests

Two complementary layers:

- **Unit/integration via `pytest`** — fast, parallel, in-process.
  447+ tests including substrate cascade + persistence + reboot.
  ```bash
  uv run --active pytest -n auto         # ~4s parallel
  ```
- **Self-tests** — hand-written, scope-tagged markdown specs. AI
  agents (Claude Code, etc.) read them, ask required pre-flight
  questions, drive the system at the user-facing surface (CLI, HTTP,
  WS, PTY, browser), and fill summary tables. Each component owns
  one. Index + LLM protocol + scope taxonomy at **`selftest.md`**
  (root). Tell the AI things like *"perform non-web self tests"* or
  *"I'm in a canvas, do all webapp tests"* — the AI selects the
  right subset based on scope tags.

## Pre-push checks

CI gates run on PR; mirror them locally before pushing:

```bash
uvx ruff check kernel/ main.py bundled_agents/ tests/
uvx ruff format --check kernel/ main.py bundled_agents/ tests/
uv run pytest -n auto
```

## Layout

```
.                                            # project root
├── main.py                                  # bootstrap: Kernel() -> fs_loader.read_tree('.fantastic') -> kernel.load(records)
├── kernel/
│   ├── __init__.py                          # public API re-exports
│   ├── _agent.py                            # Agent (recursive) + ephemeral flag + on_delete hook
│   ├── _kernel.py                           # Kernel ctx + tree-mgmt API (create/delete/update/list/send)
│   ├── modes/                               # dispatch_argv + one-shots (reflect/call) + default (boot + REPL)
│   ├── _bundles.py                          # entry-point discovery (`fantastic.bundles`)
│   ├── _lock.py                             # serve lock (.fantastic/lock.json)
│   └── _env.py                              # .env autoloader
├── pyproject.toml                            # workspace + bundle deps
├── selftest.md                               # selftest INDEX + LLM protocol
├── conftest.py                               # pytest fixtures
├── tests/                                    # substrate-level tests
└── bundled_agents/
    ├── loader/fs_loader/                     # the ROOT agent + persistence/hydration root (owns .fantastic/)
    ├── cli/                                  # stdout renderer (ephemeral — composed when isatty)
    ├── web/                                  # HTTP+WS transport (uvicorn) + favicon
    ├── file/, scheduler/                     # filesystem + recurring tasks
    ├── python_runtime/                       # exec Python in subprocess
    ├── yaml_state/                           # durable YAML key-value memory (yaml_state.tools)
    ├── terminal/                             # PTY shell (handler_module terminal_backend.tools; xterm view lives in ts/)
    ├── ai/ai_core/                           # shared AI agent loop/state (queue/FIFO lock/menu cache, prompt assembly, agentic _run loop, verb bodies); all three AI backends bind to it
    ├── ai/ollama/ollama_backend              # local LLM (ollama) — thin binding over ai_core
    ├── ai/nvidia/nvidia_nim_backend          # NVIDIA NIM (OpenAI-compatible) — thin binding over ai_core + NIM-specific key/retry verbs
    ├── ai/anthropic/anthropic_backend        # Anthropic LLM (anthropic_backend.tools) — thin binding over ai_core
    ├── bridge/bridge_core/                   # shared kernel-bridge engine (read loop, verbs, lifecycle, transport seam); a library, not an agent
    ├── bridge/kernel_bridge/                 # cross-kernel WS bridge (asymmetric) — ws / ssh+ws transports
    ├── bridge/cloud_bridge/                  # cross-kernel bridge via a zero-trust relay — dial-out + Noise E2E (CONTRACT v1)
    ├── runner/runner_core/                   # shared runner lifecycle + Transport seam; local_runner and ssh_runner bind to it
    └── runner/{local_runner, ssh_runner}     # spawn local / remote `fantastic`
```

The browser view layer (canvas compositor + terminal/chat views) is
the TypeScript frontend kernel in the repo's top-level `ts/` package,
served weakly through generic agents — see [`ts/SERVE.md`](ts/SERVE.md).

## Universal verb

Every agent answers `{type:"reflect"}`, returning the addressed agent
uniformly: `{id, sentence, verbs:{name:doc}, …flat state}`. Root is NOT
special. Compose the reply with flags:

- `tree=all|ids|none` (default `all`) — nested distilled subtree;
  `ids` = flat descendant-id index; `none` = just this agent.
- `bundles=all|ids|none` (default `none`) — the installable-bundle
  catalog (what you can `create_agent` from); `ids` = names only.
- `readme=true` — attach the agent's
  readme.md. On the kernel/root this is the root readme: every
  transport, the wire/binary-protocol details, the `kernel` alias, the
  two-kernel model. Transport docs live there now, NOT in the reflect
  JSON — reach them with `reflect readme=true`.

The defaults give a caller enough to navigate and choose; deep details
are fetched on demand.

---

*Part of **Aisixteen Fantastic** — licensed **AGPL-3.0-or-later** ([`../LICENSE`](../LICENSE)). "Aisixteen Fantastic" and "AISIXTEEN" (USPTO reg. 7,238,635) are trademarks of AISixteen; the license covers the code only, not the marks — forks must rename. See the [root README](../README.md#license--brand).*
