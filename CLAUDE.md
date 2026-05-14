# Fantastic Kernel — Claude Code working notes

A medium that unifies humans and AIs into a single workspace. Recursive
`Agent` class + a `Kernel` shared-context object, one primitive
(`send(target_id, payload)`), plugin-discovered bundles. Every agent
answers `{type:"reflect"}` — the universal discovery verb.

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│         SUBSTRATE  (kernel/_agent.py + kernel/_kernel.py)                │
│   Agent  — recursive node; .send / .emit / .create / .delete             │
│   Kernel — tree-wide ctx (flat agents index, state subs, bundle cache)   │
│   System verbs (create/delete/update/list_agents) baked into Agent.      │
└────────────────────────┬─────────────────────────────────────────────────┘
                         │  agent ⇌ agent ⇌ agent (agent.send)
       ┌─────────────────┼─────────────────────────────┐
       ▼                 ▼                             ▼
   ┌────────┐      ┌────────────┐              ┌────────────────┐
   │ core   │      │ web        │              │ html_agent /   │
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
LLM CLI) bootstraps from a single WS `kernel.reflect` round-trip
(open `ws://host/<any-agent>/ws`, send a `call` frame with
`target:"kernel", payload:{type:"reflect"}`).

## Run

```bash
uv sync                                              # install workspace + bundles editable
fantastic                                            # boot all + REPL (tty) + daemon (if web is persisted)
fantastic <id> <verb> [k=v ...]                      # one-shot RPC
fantastic reflect [<id>]                             # shorthand: <id> reflect (default: kernel)
fantastic core create_agent handler_module=web.tools port=8888    # persist web record (first time)
```

`main.py` composes the substrate: `Kernel() → Core(kernel, argv) →
core.run()`. CLI dispatch lives in `kernel/_modes.py`:
  - one-shot: `<id> <verb> [k=v]` / `reflect [<id>]` / `install` /
    `install-bundle`
  - long-running default: boots every persisted agent. If a `web`
    agent is among them, acquires lock + blocks (uvicorn lives via
    its asyncio task). If stdin is a tty, runs the REPL stdin loop.
    Composing neither → exit silently.

Web composition is **explicit** — no `--port` flag. To make `fantastic`
serve HTTP, persist a web agent first (one-shot create_agent or
REPL `add web port=N`). Next invocation boots it as a daemon.

`Core` decides whether to wire the stdout renderer (Cli) — ephemeral,
never persisted.

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
| `core` | userland orchestrator agent at the root (`id="core"`); no handler_module (substrate handles dispatch). Composes the stdout renderer (Cli) when `stdin.isatty()`. System verbs (list_agents, create/update/delete_agent) are native to Agent class. |
| `cli` | singleton child of root; renders token/done/say/error events to stdout |
| `web` | uvicorn HTTP host. Serves rendering only — `/` (root index from `templates/index.html`), `/<id>/` (agent's `render_html`), `/<id>/file/<path>` (read-verb file proxy), `transport.js`, favicon. Call surfaces (WS, REST) live in sibling sub-agents and mount via the duck-typed `get_routes` verb. |
| `web_ws` | WebSocket verb-invocation surface. Child of a `web` agent. Mounts `/<host_id>/ws` on its parent web's FastAPI app. Opt-in: `create_agent handler_module=web_ws.tools parent_id=<web>`. |
| `web_rest` | REST diagnostic surface. Child of a `web` agent. Mounts `POST /<self_id>/<target_id>` body=payload → kernel.send → JSON reply. Multiple instances coexist with different ids. Opt-in. |
| `file` | filesystem-as-agent (`read`, `write`, `list`, `delete`, `rename`, `mkdir`) |
| `scheduler` | recurring tasks; persistence routed through `file_agent_id` |
| `python_runtime` | subprocess Python exec (`python -c <code>`); per-agent interrupt/stop |
| `html_agent` | UI-as-record; `html_content` stored on agent.json, served at `/<id>/` |
| `terminal/{terminal_backend, terminal_webapp}` | PTY shell + xterm UI. VSCode-ported terminal robustness: streaming flow control (reader pauses past 100K unacked chars, resumes on the `ack` verb), incremental UTF-8 decode (no split-char `<?>` litter across `os.read` chunks), serialized full-buffer writes (bracketed-paste-safe), image-paste bridge (browser-clipboard image → `paste_image` → file saved + path typed into the PTY for a CLI like `claude`). |
| `ai/ollama/ollama_backend` | local LLM agent (ollama); per-client chat threads, FIFO lock, menu cache |
| `ai/nvidia/nvidia_nim_backend` | NVIDIA NIM LLM agent (OpenAI-compatible); api_key sidecar via `file_agent_id`; rate-limit retry; same surface as ollama_backend |
| `ai/ai_chat_webapp` | provider-agnostic chat UI; fronts any backend that answers `send`/`history`/`interrupt` |
| `canvas/{canvas_backend, canvas_webapp}` | spatial UI host; Liquid-Glass-styled DOM iframes (`get_webapp`) layered with GL views (`get_gl_view`); explicit `add_agent` membership; pure-streaming lifecycle (no polling). Each GL view runs in its own `THREE.Group` container — live `set_gl_source` reload in place (`gl_source_changed`), no canvas refresh. Wheel zoom is horizon-anchored (pulls toward screen center, 2D + GL locked in sync) and smoothed (rAF-lerped toward a `targetZ`). |
| `canvas/telemetry_pane` | live agent-vis GL view — water-floating sprites + sender→receiver neon wires + traveling pulses + last-10 messages pane; runs inside any canvas's WebGL scene |
| `kernel_bridge` | cross-kernel comms — pairs of bridge agents exchange `forward` envelopes over memory / WS / SSH+WS / HTTP. WS targets the remote's `web_ws` surface (full duplex); HTTP targets `web_rest` (request/reply only). All transports are **weak binding** — addressed by URL + path only; no shared Python types with the remote kernel. Weak proxy: local→local stays direct. |
| `ssh_runner` | remote `fantastic` lifecycle over SSH — start/stop/restart/status + local SSH tunnel for canvas iframing. Pure subprocess ssh; composes with `kernel_bridge` for messaging |

Each bundle is a real Python package with its own `pyproject.toml`,
declaring `[project.entry-points."fantastic.bundles"]`. `fantastic`
discovers them uniformly via `importlib.metadata.entry_points` —
works for in-tree workspace members AND `pip install` third-party
plugins (drop in `installed_agents/`).

Install a third-party bundle straight from a git URL (or PyPI, or
a local path — anything `uv pip install` accepts):

```bash
fantastic install-bundle git+https://github.com/user/fantastic-bundle
fantastic install-bundle git+https://github.com/user/repo@v0.2.1   # pin tag
fantastic install-bundle git+https://github.com/user/repo --into /path/to/project
```

Default target is the kernel's own venv (sys.executable); `--into
<project>` installs into that project's `.venv` instead. Restart
any running `fantastic` after install — entry points are
scanned at process start.

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
- **`on_delete` cascade hook** — substrate calls `await
  agent.on_delete()` depth-first during cascade-delete BEFORE
  detaching the record from `kernel.agents` and the parent's
  `_children`. Default implementation: if the agent's `handler_module`
  exposes `async def on_delete(agent)`, invoke it; then rmtree the
  agent's directory (unless `ephemeral=True`). Bundles port their
  teardown logic into this function — `terminal_backend.on_delete`
  closes the PTY, `web.on_delete` drains uvicorn,
  `kernel_bridge.on_delete` cancels the read loop + tunnel, runner
  bundles call their own `_stop`.
- **`ephemeral` class flag** — `class Cli(Agent): ephemeral = True`
  means the agent never persists to disk (no agent.json, no agents/
  dir). Composition is per-process; reboots compose afresh based on
  mode. Use for stateless renderers / debuggers / dispatchers.
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

Open `ws://host/<any-agent>/ws` and send
`{"type":"call","target":"kernel","payload":{"type":"reflect"},"id":"1"}`.
The reply carries:

- `transports.{in_process, in_prompt, cli, ws}` — every invocation
  form, including the actual WS URL template with the current
  host:port.
- `available_bundles` — every entry-point-discovered bundle (what
  you can `create_agent` from).
- `agents` — every running agent record.
- `binary_protocol` — `[4-byte BE H | JSON header | M-byte body]`
  WS frame format for byte-heavy payloads.
- `browser_bus` — the BroadcastChannel envelope shape.

Per-agent reflect carries `verbs: {name: doc-line}` so an LLM caller
can compose any `payload` from the docstring without source diving.
"If you find yourself reading kernel/ to discover a transport URL,
that's a primer regression — flag it."

## Tests

- **Unit** — `pytest -n auto` (`pytest-xdist`). 480+ tests, parallel,
  in-process. Each bundle's tests live in `bundled_agents/<bundle>/tests/`;
  kernel-level tests live in `tests/`. `conftest.py` at root exposes
  `kernel`, `seeded_kernel`, `file_agent` fixtures.
- **Selftests** — scope-tagged markdown specs. AI agents read them,
  drive the system at the user-facing surface (CLI, HTTP, WS, PTY,
  browser), and fill summary tables. Index + protocol at root
  `selftest.md`. Each bundle owns one.

## Pre-push checks

```bash
uvx ruff check kernel/ main.py bundled_agents/ tests/
uvx ruff format --check kernel/ main.py bundled_agents/ tests/
uv run pytest -n auto
```

## Commits & pushes — ASK FIRST

**Do NOT `git commit` or `git push` unless the user explicitly asks
for it.** No "I'll just commit this since the work is done" — wait
for the word.

OK to do without asking: edit files, run `pytest`, run `ruff`,
create a branch when the user has named it, show diffs (`git diff
--cached`, `git status`). Anything that touches refs or origin
needs explicit consent: commit, amend, push, force-push, branch
delete, tag, rebase, reset --hard.

If you think work should be committed, _say so_ and wait. The cost
of asking once is low; the cost of an unwanted commit (lost diff
review, force-push surprise, polluted history) is high.

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
  `fantastic call` (which spawns a separate kernel).

## Storage policy

- Project code lives in version-controlled files in this repo or
  user dirs. **`.fantastic/` is runtime state only** — agent.json
  records, per-agent sidecars (chat_<client>.json,
  schedules.json, history.jsonl), `lock.json`, `readme.md`.
  Wipe-and-rebuild safe.
- Use the `file` agent (rooted at any path) for HTTP-served content:
  `<img src="/<file_id>/file/imgs/foo.png">` works in any html_agent.

## Path conventions

- All paths relative when invoked via `fantastic call` (cwd = project
  dir).
- The file agent's path-safety refuses anything escaping its `root`.
- `fantastic` writes `.fantastic/lock.json` with `{pid, port}`;
  a second serve in the same dir refuses with a clear error and stale
  locks (dead pid) get overwritten.

## What's NOT here (yet)

These existed in an older codebase iteration; deferred or replaced:

- openai / anthropic / integrated AI bundles (only `ollama` ships).
  Pattern: mirror `ollama_backend`. Recoverable from git history.
- `register_template` / `list_templates` — replaced by per-agent
  reflect (single source of truth).
- `content_alias_file` registry — replaced by the URL convention
  `/<file_id>/file/<path>`.
- agent `memory_long.jsonl` append-only memory — replaceable by the
  `file` agent + path convention.
