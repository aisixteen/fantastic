# fantastic-kernel

A medium that unifies humans and AIs into a single workspace.
Plugin-discovered agents, one primitive (`send`), hermetic protocol.

## Concept

- **kernel** — one Python class, in-process. Owns agent records, inboxes,
  watchers. Single primitive: `send(target_id, payload) → reply | None`.
- **agents** — anything addressable. Pip-installable Python packages
  (one per bundle), discovered via the `fantastic.bundles` entry-point group.
  Each agent answers `{type:"reflect"}` returning a flat self-description.
- **webapp** — transport bundle. Runs uvicorn, serves each agent's UI at
  `/{agent_id}/`, proxies WebSocket frames to/from `kernel.send`. Auto-injects
  `fantastic_transport()` into every served HTML.
- **html_agent** — UI-as-a-record. The agent's `html_content` field
  IS the page; webapp serves it at `/<id>/` (transport auto-injected),
  duck-typed via the `render_html` verb. A coding agent can spawn a
  full UI cell in one `create_agent` call. Pair with `python_runtime`
  for backend logic and a `file` agent (via `/<id>/file/<path>`) for
  static assets.
- **python_runtime** — subprocess Python exec. `exec(code, timeout, cwd)`
  spawns `python -c <code>`, captures stdout/stderr, returns exit code.
  Stateless per call; per-agent `interrupt`/`stop`.
- **canvas** — `canvas_backend` + `canvas_webapp` pair. The webapp is
  a two-layer host: a DOM layer (iframes for agents answering
  `{type:"get_webapp"}`) and a WebGL layer (Three.js content for
  agents answering `{type:"get_gl_view"}`). An agent answering BOTH
  gets BOTH presentations. Membership is explicit (`add_agent`).
  THREE.js loads from esm.sh at runtime. Iframe chrome is styled
  Apple-style Liquid Glass (translucent + backdrop-blur + specular).
  Lifecycle is purely streamed (no polling) — `agent_updated`,
  `agent_deleted`, `members_updated` events drive refresh; stale
  members self-heal on probe.
- **telemetry_pane** — a GL agent. Subscribes to the kernel state
  stream and renders each agent as a Three.js sprite with name +
  backlog dots + Tron-neon traffic blip. Real agent-to-agent traffic
  draws fading sender→recipient wires with traveling pulses. Sprites
  drift on a slow water wobble. A right-side pane shows the last 10
  messages with kind, sender→target, and a trimmed payload summary.
  Plug into any canvas via `add_agent`.
- **kernel_bridge + ssh_runner** — cross-host. `ssh_runner` uses
  subprocess SSH to start/stop a remote `fantastic serve` and keeps
  a local tunnel open so a canvas can iframe the remote webapp.
  `kernel_bridge` opens WS (or SSH+WS) to a peer kernel and ships
  `forward` envelopes — local agents reach remote agents through
  it without merging the two address spaces. Weak proxy: local→local
  comms stay direct.
- **webapps** — UI bundles (`*_webapp`) that hold an `upstream_id`
  pointing at a backend they front. Pure browser code; no compute.
  Duck-typed via `get_webapp` — any agent that returns `{url, ...}` is
  treated as a UI by the canvas.
- **browser-only message bus** — every page also gets
  `fantastic_transport().bus`, a `BroadcastChannel("fantastic")` wrapper.
  Same envelope as kernel (`{type, target_id, source_id, ...}`) with
  structured-clone payloads — bytes/objects/strings native, **bypasses the
  kernel entirely**. Use for high-frequency intra-browser traffic (audio
  frames, drag events) where round-tripping the server adds nothing.

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

New bundles dropped under `bundled_agents/` or `installed_agents/` with a
`pyproject.toml` are auto-picked up on the next `uv sync`.

## Run

```bash
uv run python kernel.py                       # interactive REPL (default)
uv run python kernel.py serve --port 8888     # headless: webapp on :8888
uv run python kernel.py call <id> <verb> [k=v ...]   # one-shot RPC
uv run python kernel.py reflect [<id>]        # shorthand for reflect
```

REPL example:

```
fantastic> add webapp                                       # uvicorn boots
fantastic> add file
fantastic> add ollama_backend file_agent_id=<file_id>
fantastic> add ai_chat_webapp upstream_id=<ollama_backend_id>     # or any LLM backend
fantastic> add canvas_backend
fantastic> add canvas_webapp upstream_id=<canvas_backend_id>
# browse http://localhost:8888/<canvas_webapp_id>/  → all webapps in one view
```

## Drive from outside

After `kernel.py serve`, the entire kernel is reachable over HTTP+WS:

```bash
curl http://localhost:8888/_kernel/reflect             # substrate primer
curl http://localhost:8888/_agents                     # list_agents
curl -X POST http://localhost:8888/<id>/call -H 'content-type: application/json' -d '{...}'
# WS: ws://host/<id>/ws  — frames per the protocol shown in /_kernel/reflect
```

The protocol IS the API — no client library. `/_kernel/reflect` returns
the substrate primer with `transports` (in-process / in-prompt / cli /
http / ws), `available_bundles` (entry-point inventory), running
`agents`, `well_known` singletons, plus `binary_protocol` and
`browser_bus`. Any LLM CLI dropped in cold can bootstrap from one
reflect call.

## Plugin system

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. The kernel
discovers bundles uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party plugins.

## Tests

Two complementary layers:

- **Unit/integration via `pytest`** — fast, parallel, in-process. 373+ tests.
  ```bash
  uv run --active pytest -n auto         # ~3s parallel
  ```
- **Self-tests** — hand-written, scope-tagged markdown specs. AI agents
  (Claude Code, etc.) read them, ask required pre-flight questions, drive
  the system at the user-facing surface (CLI, HTTP, WS, PTY, browser),
  and fill summary tables. Each component owns one. Index + LLM protocol
  + scope taxonomy at **`selftest.md`** (root). Tell the AI things like
  *"perform non-web self tests"* or *"I'm in a canvas, do all webapp
  tests"* — the AI selects the right subset based on scope tags.

## Pre-push checks

CI gates run on PR; mirror them locally before pushing:

```bash
uvx ruff check kernel.py bundled_agents/ tests/
uvx ruff format --check kernel.py bundled_agents/ tests/
uv run pytest -n auto
```

## Layout

```
.                                            # project root
├── kernel.py                                 # Kernel + REPL + serve/call/reflect
├── pyproject.toml                            # workspace + bundle deps
├── selftest.md                               # selftest INDEX + LLM protocol
├── conftest.py                               # pytest fixtures
├── tests/                                    # kernel-level tests
├── installed_agents/                         # third-party drop-ins (empty)
└── bundled_agents/
    ├── core/                                 # system verbs (singleton)
    ├── cli/                                  # terminal renderer (singleton)
    ├── webapp/                               # HTTP+WS transport (uvicorn)
    ├── file/, scheduler/                     # filesystem + recurring tasks
    ├── python_runtime/                       # exec Python in subprocess
    ├── html_agent/                           # UI-as-record (html_content per instance)
    ├── terminal/{terminal_backend, terminal_webapp}
    ├── ai/ai_chat_webapp                     # provider-agnostic chat UI
    ├── ai/ollama/ollama_backend              # local LLM (ollama)
    ├── ai/nvidia/nvidia_nim_backend          # NVIDIA NIM (OpenAI-compatible)
    ├── canvas/{canvas_backend, canvas_webapp, telemetry_pane}
    ├── kernel_bridge/                      # cross-kernel forward envelopes
    └── ssh_runner/                         # remote serve lifecycle over ssh
```

## Universal verb

Every agent answers `{type:"reflect"}`. Returns `{id, sentence, verbs:{name:doc}, …flat state}`.
Reflect on the kernel itself (`send("kernel", {reflect})`) returns the
substrate primer — transports, available bundles, running agents, plus
binary protocol + browser bus details. The only thing an external tool
needs to bootstrap.
