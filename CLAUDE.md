# Fantastic Kernel — Claude Code working notes

A medium that unifies humans and AIs into a single workspace. One
Python class (`Kernel`), one primitive (`send(target_id, payload)`),
plugin-discovered agents. Every agent answers `{type:"reflect"}` —
that's the universal discovery verb.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│                              KERNEL (kernel.py)                          │
│   one class, one primitive: `send(id, payload) -> reply | None`          │
│   agent records, inboxes, watchers — that's it. No HTTP, no UI.          │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (kernel.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌────────┐      ┌────────────┐              ┌────────────────┐
   │ core   │      │ webapp     │              │ html_agent /   │
   │ cli    │      │ (uvicorn)  │              │ python_runtime │
   │ file   │      │ HTTP + WS  │              │ canvas / ...   │
   │ ...    │      │ transport  │              │  (UI bundles)  │
   └────────┘      └─────┬──────┘              └────────────────┘
                         │
                         ▼ HTTP + WS frames (text + binary)
   ┌─────────────────────────────────────────────────────────────────────┐
   │                            BROWSER                                  │
   │  iframe ↔ iframe (BroadcastChannel "fantastic" — kernel-bypass)     │
   │  fantastic_transport() injected on every served HTML                │
   └─────────────────────────────────────────────────────────────────────┘
```

**No client library. The protocol IS the API.** A code agent (Claude,
LLM CLI, curl) bootstraps from a single `GET /_kernel/reflect` call.

## Run

```bash
uv sync                                      # install workspace + bundles editable
uv run python kernel.py                      # interactive REPL
uv run python kernel.py serve --port 8888    # headless: webapp on :8888
uv run python kernel.py call <id> <verb>     # one-shot RPC
uv run python kernel.py reflect [<id>]       # shorthand for reflect (default: kernel)
```

REPL example:

```
fantastic> add file
fantastic> add canvas_backend
fantastic> add canvas_webapp upstream_id=<canvas_backend_id>
fantastic> @core list_agents
```

## Bundles (current set)

| bundle | role |
|---|---|
| `core` | singleton; system verbs (list_agents, create/update/delete_agent) |
| `cli` | singleton; renders token/done/say/error events to stdout |
| `webapp` | HTTP+WS transport (uvicorn); serves `/<id>/`, `POST /<id>/call`, `WS /<id>/ws`, `GET /<id>/file/<path>` |
| `file` | filesystem-as-agent (`read`, `write`, `list`, `delete`, `rename`, `mkdir`) |
| `scheduler` | recurring tasks; persistence routed through `file_agent_id` |
| `python_runtime` | subprocess Python exec (`python -c <code>`); per-agent interrupt/stop |
| `html_agent` | UI-as-record; `html_content` stored on agent.json, served at `/<id>/` |
| `terminal/{terminal_backend, terminal_webapp}` | PTY shell + xterm UI |
| `ai/ollama/ollama_backend` | local LLM agent (ollama); per-client chat threads, FIFO lock, menu cache |
| `ai/nvidia/nvidia_nim_backend` | NVIDIA NIM LLM agent (OpenAI-compatible); api_key sidecar via `file_agent_id`; rate-limit retry; same surface as ollama_backend |
| `ai/ai_chat_webapp` | provider-agnostic chat UI; fronts any backend that answers `send`/`history`/`interrupt` |
| `canvas/{canvas_backend, canvas_webapp}` | spatial UI host; Liquid-Glass-styled DOM iframes (`get_webapp`) layered with GL views (`get_gl_view`); explicit `add_agent` membership; pure-streaming lifecycle (no polling) |
| `canvas/telemetry_pane` | live agent-vis GL view — water-floating sprites + sender→receiver neon wires + traveling pulses + last-10 messages pane; runs inside any canvas's WebGL scene |

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. `kernel.py`
discovers them uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins (drop in `installed_agents/`).

## Universal patterns

- **`reflect`** — every agent answers `{type:"reflect"}` returning
  `{id, sentence, verbs:{name:doc}, emits:{type:shape}, ...flat state}`.
  Verb signatures live in their docstrings; `reflect` derives them
  automatically. Discovery is one round-trip.
- **`render_html`** — duck-typed presentation. Any agent that returns
  `{html:str}` from `render_html` gets that body served at `/<id>/`
  with `transport.js` auto-injected. html_agent stores its body on
  the record; bundled webapps read from package resources fresh
  per-request (edit-and-refresh dev loop).
- **`get_webapp`** — duck-typed UI discovery. The canvas iframes any
  agent that answers `get_webapp` with `{url, default_width,
  default_height, title}`.
- **`reload_html`** — universal page reload. transport.js subscribes
  to it on every served page; any agent that emits `{type:"reload_html"}`
  on its own inbox triggers `location.reload()` in connected tabs.
  `set_html` and the canvas frame ⟳ button both go through it.
- **`file_agent_id`** — bundles that need persistence (ollama_backend,
  scheduler) carry an `file_agent_id` on
  their record. Failfast if unset (no implicit fallback).
- **`delete_lock: true`** on a record refuses delete. core's
  `delete_agent` returns `{error, locked:true, id}` so LLM callers
  can detect it programmatically. Clear via `update_agent`.
- **`shutdown` lifecycle hook** — symmetric to the `boot` core sends
  on `create_agent`, core sends `{type:"shutdown"}` to the agent
  before `kernel.delete` removes the record. Bundles use it to tear
  down process-memory state (PTY children, uvicorn servers,
  in-flight tasks). Opt-in: bundles that don't implement it return
  unknown-verb error which core silently ignores. Currently
  implemented by `terminal_backend` (kills the PTY child).
- **State-event `sender` + `summary`** — every `send`/`emit` state
  event carries `sender` (the dispatching agent's id, set via a
  task-local contextvar around handler dispatch; webapp's HTTP/WS
  proxy tags external traffic with the webapp's own id) and
  `summary` (a JSON-stringified, bytes-stripped, max-160-char view
  of the payload). The telemetry pane uses both to draw
  sender→receiver wires + a last-N message log.
- **Browser bus** — `fantastic_transport().bus` is a
  `BroadcastChannel("fantastic")` wrapper available on every served
  page. Same envelope as kernel send, but **bypasses the kernel
  entirely**. Use for high-frequency intra-iframe traffic (cursor,
  drag, audio frames) where round-tripping the server adds nothing.

## Self-bootstrap (for code agents)

After one `GET /_kernel/reflect`, the response carries:

- `transports.{in_process, in_prompt, cli, http, ws}` — every
  invocation form, including the actual route templates with the
  current host:port.
- `available_bundles` — every entry-point-discovered bundle (what
  you can `create_agent` from).
- `agents` — every running agent record.
- `binary_protocol` — `[4-byte BE H | JSON header | M-byte body]`
  WS frame format for byte-heavy payloads.
- `browser_bus` — the BroadcastChannel envelope shape.

Per-agent reflect carries `verbs: {name: doc-line}` so an LLM caller
can compose any `payload` from the docstring without source diving.
"If you find yourself reading kernel.py to discover a transport URL,
that's a primer regression — flag it."

## Tests

- **Unit** — `pytest -n auto` (`pytest-xdist`). 221+ tests, parallel,
  in-process. Each bundle's tests live in `bundled_agents/<bundle>/tests/`;
  kernel-level tests live in `tests/`. `conftest.py` at root exposes
  `kernel`, `seeded_kernel`, `file_agent` fixtures.
- **Selftests** — scope-tagged markdown specs. AI agents read them,
  drive the system at the user-facing surface (CLI, HTTP, WS, PTY,
  browser), and fill summary tables. Index + protocol at root
  `selftest.md`. Each bundle owns one.

## Pre-push checks

```bash
uvx ruff check kernel.py bundled_agents/ tests/
uvx ruff format --check kernel.py bundled_agents/ tests/
uv run pytest -n auto
```

## Conventions

- All async (`asyncio` throughout). `pytest-asyncio` with
  `asyncio_mode = "auto"`.
- Agent IDs: `{bundle}_{hex6}` (e.g. `ollama_backend_b04b35`).
  Singletons use the bundle name (`core`, `cli`).
- Every bundle's `tools.py` defines:
  - per-verb `async def _<name>(id, payload, kernel)` with a
    one-line docstring (auto-fed into reflect's `verbs` dict).
  - `VERBS = {"<name>": _<name>, ...}` dispatch table.
  - `async def handler(id, payload, kernel)` 4-line dispatcher.
- Records in `.fantastic/agents/<id>/agent.json` carry only metadata.
  Process-memory state (PTY child, uvicorn server, in-flight tasks)
  is per-process and visible only to the kernel that owns it —
  reflect from the live `serve` to see it, NOT via a fresh
  `kernel.py call` (which spawns a separate kernel).

## Storage policy

- Project code lives in version-controlled files in this repo or
  user dirs. **`.fantastic/` is runtime state only** — agent.json
  records, per-agent sidecars (chat_<client>.json,
  schedules.json, history.jsonl), `lock.json`, `readme.md`.
  Wipe-and-rebuild safe.
- Use the `file` agent (rooted at any path) for HTTP-served content:
  `<img src="/<file_id>/file/imgs/foo.png">` works in any html_agent.

## Path conventions

- All paths relative when invoked via `kernel.py call` (cwd = project
  dir).
- The file agent's path-safety refuses anything escaping its `root`.
- `kernel.py serve` writes `.fantastic/lock.json` with `{pid, port}`;
  a second serve in the same dir refuses with a clear error and stale
  locks (dead pid) get overwritten.

## What's NOT here (yet)

These existed in an older codebase iteration; deferred or replaced:

- openai / anthropic / integrated AI bundles (only `ollama` ships).
  Pattern: mirror `ollama_backend`. Recoverable from git history.
- `instance` bundle (SSH-tunneled connected peers). Niche.
- `register_template` / `list_templates` — replaced by per-agent
  reflect (single source of truth).
- `content_alias_file` registry — replaced by the URL convention
  `/<file_id>/file/<path>`.
- agent `memory_long.jsonl` append-only memory — replaceable by the
  `file` agent + path convention.
