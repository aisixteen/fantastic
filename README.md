# fantastic-kernel

A medium that unifies humans and AIs into a single workspace.
Plugin-discovered agents, one primitive (`send`), hermetic protocol.

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
- **`core`** — the userland orchestrator agent class. Lives at
  `id="core"` as the tree root. No handler_module — root is a hollow
  container; substrate handles all dispatch. Composes a stdout
  renderer child when stdin is a tty.
- **send** — the one primitive. `kernel.send(target_id, payload) →
  reply | None` (from outside any handler) or `agent.send(...)`
  (from inside) resolves the target via the flat index and
  dispatches.
- **Cascade delete** — `delete_agent` walks the subtree depth-first
  via `_children`. Each descendant runs its `on_delete` hook (kills
  PTY / drains uvicorn / closes clients + rmtrees own disk artifact)
  BEFORE detaching from `kernel.agents` and the parent's
  `_children`. Any `delete_lock` anywhere in the subtree blocks the
  entire cascade with `{locked, blocked_by, error}` — no partial
  mutations.
- **Persistence** — disk mirrors the runtime tree
  (`<root>/agents/<id>/agents/<child_id>/agent.json`). A fresh
  `Kernel()` + root construction recursively rehydrates the whole
  tree by ids, respecting parent-child links. Process-memory state
  (in-flight counters, inboxes, PTY children) does NOT survive;
  bundles' `_boot` respawns it. Agents with class-level
  `ephemeral=True` (e.g. the cli renderer) are never persisted —
  composition is per-process.
- **bundles** — pip-installable Python packages discovered via the
  `fantastic.bundles` entry-point group. Each bundle ships a
  `tools.py` with verb handlers + optional `_boot` / `on_delete`
  lifecycle hooks. Idempotent first-boot lets a bundle declaratively
  own its child agents (e.g., `terminal_webapp._boot` spawns a
  `terminal_backend` child the first time the webapp boots; on
  subsequent boots the child is already present from disk).
- **web** — HTTP+WS transport bundle (uvicorn). Serves each agent's
  UI at `/{agent_id}/`, proxies WS frames to/from `kernel.send`,
  auto-injects `fantastic_transport()` into every served HTML, and
  serves a tree-shape index at `/` (with ↗ visit links for HTML-
  serving agents and ⊙ reflect popups).
- **html_agent** — UI-as-a-record. The agent's `html_content` field
  IS the page; webapp serves it at `/<id>/` (transport auto-injected),
  duck-typed via the `render_html` verb.
- **python_runtime** — subprocess Python exec. `exec(code, timeout,
  cwd)` spawns `<interp> -c <code>`, captures stdout/stderr, returns
  exit code. Per-agent venv resolution (record `python` / `venv`
  fields override the kernel's interpreter).
- **terminal** — `terminal_backend` (PTY shell session) +
  `terminal_webapp` (browser xterm). The backend ports VSCode's
  terminal robustness: streaming **flow control** (the PTY reader
  pauses past 100K unacknowledged chars and resumes on the consumer's
  `ack` verb — backpressure so a flood can't lock up a tab),
  **incremental UTF-8 decode** (a multi-byte char split across an
  `os.read` chunk is reassembled, not shattered into `<?>` litter),
  and **serialized full-buffer writes** (large/bracketed pastes land
  whole). `paste_image` bridges a browser-clipboard image into a
  CLI running in the PTY (e.g. `claude`): the webapp ships the bytes,
  the backend saves a file and types its path in — mimicking a
  drag-drop, since the server can't reach the browser clipboard.
- **canvas** — `canvas_backend` + `canvas_webapp` pair.
  `canvas_webapp` is a two-layer host: a DOM layer (iframes for
  agents answering `{type:"get_webapp"}`) and a WebGL layer (Three.js
  content for agents answering `{type:"get_gl_view"}`). Each GL view
  runs in its own `THREE.Group` container — the scene-graph analogue
  of an iframe — so `gl_agent.set_gl_source` reloads one view in
  place (`gl_source_changed` → dispose group + recompile) without a
  canvas refresh. Wheel zoom is horizon-anchored (pulls toward screen
  center, DOM + GL layers locked in sync) and rAF-smoothed. Membership
  is **structural**: `canvas_backend.add_agent` spawns the new member
  as a child of the canvas via the substrate's `create_agent` — no
  separate `members` field. Cascade-delete the canvas and every
  member dies with it.
- **telemetry_pane** — a GL agent. Subscribes to the kernel state
  stream and renders each agent as a Three.js sprite with name +
  backlog dots + Tron-neon traffic blip. Real agent-to-agent traffic
  draws fading sender→recipient wires with traveling pulses.
- **kernel_bridge + ssh_runner** — cross-host. `ssh_runner` uses
  subprocess SSH to start/stop a remote `fantastic` and keeps
  a local tunnel open so a canvas can iframe the remote webapp.
  `kernel_bridge` opens WS (or SSH+WS) to a peer kernel and ships
  `forward` envelopes — local agents reach remote agents through it
  without merging the two address spaces. Weak proxy: local→local
  comms stay direct.
- **browser-only message bus** — every served page also gets
  `fantastic_transport().bus`, a `BroadcastChannel("fantastic")`
  wrapper. Same envelope as kernel send (`{type, target_id,
  source_id, ...}`) with structured-clone payloads, **bypasses the
  server entirely**. Use for high-frequency intra-browser traffic
  (audio frames, drag events) where round-tripping the server adds
  nothing.

Two-tier message flow:

```
                       SERVER                              BROWSER
  agent ──┐                                              ┌── iframe
           ├── kernel.send / emit / watch ──── WS ──── ┤
  agent ──┘   (text + binary frames)                    ├── iframe
                                                          │   ↕
                                                          │  BroadcastChannel("fantastic")
                                                          │   ↕
                                                          └── iframe
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
fantastic core create_agent handler_module=web.tools port=8888    # persist web (one-shot; then `fantastic` daemonizes)
# Equivalent direct invocation: `python main.py [args]`
# main.py composes: Kernel() + Core(kernel, argv) + core.run().
# Web composition is explicit — no --port flag. The kernel blocks only
# when something keeps it alive (web agent on disk OR REPL stdin loop).
```

REPL example:

```
fantastic> add web port=8888                        # uvicorn boots
fantastic> add file
fantastic> add canvas_webapp                        # spawns canvas_backend as child on first boot
fantastic> add terminal_webapp                      # spawns terminal_backend as child on first boot
fantastic> @<canvas_backend_id> add_agent handler_module=terminal_webapp.tools
# browse http://localhost:8888/  → tree view with ↗ visit links
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
fantastic core create_agent handler_module=web.tools port=8888
fantastic core create_agent handler_module=web_ws.tools parent_id=<web_id>
fantastic core create_agent handler_module=web_rest.tools parent_id=<web_id>
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
`{"type":"call","target":"kernel","payload":{"type":"reflect"}}`
to either surface to get the substrate primer (transports,
`available_bundles`, agent `tree`, `well_known` singletons,
`binary_protocol`, `browser_bus`). Any LLM CLI dropped in cold can
bootstrap from one WS or HTTP round-trip.

**Weak binding for bridges.** `kernel_bridge` reaches a remote
kernel's surfaces by URL + path only — `ws://host/<host>/ws` for the
WS bridge, `http://host/<rest>/` for the HTTP bridge. No shared
Python types cross the wire.

## Plugin system

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. The kernel
discovers bundles uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins.

Install a third-party bundle from anywhere `uv pip install` accepts:

```bash
# Into the kernel's own venv (sys.executable); discovered on next start.
fantastic install-bundle git+https://github.com/user/fantastic-something
fantastic install-bundle git+https://github.com/user/repo@v0.2.1      # tag
fantastic install-bundle git+https://github.com/user/repo@feat-branch # branch
fantastic install-bundle git+https://github.com/user/repo@a3f2b1c     # commit
fantastic install-bundle git+ssh://git@github.com/user/private-bundle
fantastic install-bundle some-pypi-package
fantastic install-bundle ./local/path/to/bundle

# Into a specific project's .venv (must already exist via `fantastic install <proj>`):
fantastic install-bundle git+https://... --into /path/to/project
```

After install, restart any running `fantastic`. The new bundle
shows up in `kernel.reflect → available_bundles`, and you can
`create_agent handler_module=<bundle>.tools` from any agent
(creates as a child of that agent).

## Tests

Two complementary layers:

- **Unit/integration via `pytest`** — fast, parallel, in-process.
  453+ tests including substrate cascade + persistence + reboot.
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
├── main.py                                  # 30-line composition: Kernel() + Core(kernel, argv) + core.run()
├── kernel/
│   ├── __init__.py                          # public API re-exports
│   ├── _agent.py                            # Agent (recursive) + ephemeral flag + on_delete hook
│   ├── _kernel.py                           # Kernel ctx + tree-mgmt API (create/delete/update/list/send)
│   ├── _modes.py                            # dispatch_argv + one-shots (install/reflect/call) + default (web@port + REPL)
│   ├── _bundles.py                          # entry-point discovery (`fantastic.bundles`)
│   ├── _lock.py                             # serve lock (.fantastic/lock.json)
│   └── _env.py                              # .env autoloader
├── pyproject.toml                            # workspace + bundle deps
├── selftest.md                               # selftest INDEX + LLM protocol
├── conftest.py                               # pytest fixtures
├── tests/                                    # substrate-level tests
└── bundled_agents/
    ├── core/                                 # userland orchestrator agent (root); pure Cli-decider
    ├── cli/                                  # stdout renderer (ephemeral — composed when isatty)
    ├── web/                                  # HTTP+WS transport (uvicorn) + favicon
    ├── file/, scheduler/                     # filesystem + recurring tasks
    ├── python_runtime/                       # exec Python in subprocess
    ├── terminal/{terminal_backend, terminal_webapp}
    ├── ai/ai_chat_webapp                     # provider-agnostic chat UI
    ├── ai/ollama/ollama_backend              # local LLM (ollama)
    ├── ai/nvidia/nvidia_nim_backend          # NVIDIA NIM (OpenAI-compatible)
    ├── canvas/{canvas_backend, canvas_webapp, telemetry_pane,
    │           html_agent, gl_agent}         # spatial host + inline content cells
    ├── kernel_bridge/                        # cross-kernel forward envelopes
    └── runner/{local_runner, ssh_runner}     # spawn local / remote `fantastic`
```

## Universal verb

Every agent answers `{type:"reflect"}`. Returns `{id, sentence,
verbs:{name:doc}, …flat state}`. Reflect on the kernel itself
(`send("kernel", {reflect})`) returns the substrate primer —
transports, available bundles, agent tree, plus binary protocol +
browser bus details. The only thing an external tool needs to
bootstrap.

`reflect` on root supports parameters: `depth=N` (limit recursion),
`flat=true` (flat list with parent_id), `details=true` (full
per-agent reflect inline). Defaults: full depth, nested tree,
distilled per-node summary — enough for a caller to navigate and
choose; deep details fetched on demand.
