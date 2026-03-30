# Fantastic Canvas

An infinite canvas where AI agents build anything — a server/IDE where agents communicate through REST/WS tools.

## Progressive Architecture: Core → Server → AI

```
Core   — conversation loop + command parsing + ring buffer     (always)
Server — bundles + agents + REST/WS                            (on demand, singleton per folder)
AI     — brain that reads conversation, responds               (stub for now)
```

`fantastic` always starts **Core**. If bundles are added and no server is running, Core starts it alongside. If a server is already running (PID alive, singleton per folder), Core connects to it.

The conversation buffer (`{who}:{message}`) is the universal backbone. Color-coded: core=magenta, user=green, agent/bundle=cyan.

## Quick Start

```bash
# Development (uv)
uv sync                                                      # install deps + create venv
uv run fantastic add canvas                                  # add canvas bundle (once)
uv run fantastic add terminal                                # add terminal bundle (once)
uv run fantastic                                             # adaptive: Core+Server if bundles added
cd bundled_agents/canvas/web && npm run dev                  # port 3000
cd core && uv run pytest tests/ -v -x                        # core tests
cd bundled_agents/canvas/web && npx vitest run               # frontend tests

# CLI subcommands (offline use)
fantastic list                                               # show bundles + status
fantastic add <bundle>                                       # add a bundle
fantastic remove <bundle>                                    # remove a bundle
fantastic serve                                              # headless server (no input loop)

# Interactive mode (same commands work in the conversation loop)
fantastic                                                    # starts input loop
> list                                                       # show bundles
> add canvas                                                 # add canvas bundle
> log                                                        # show conversation history

# Install globally via uv
uv tool install ./core                                       # from source
uv tool install ./core[torch]                                # with PyTorch (auto: CPU on macOS, CUDA on Linux)
fantastic add canvas && fantastic --project-dir ~/my-project

# Docker
docker-compose up
# or: docker build -t fantastic-canvas . && docker run -v $(pwd):/workspace fantastic-canvas fantastic --host 0.0.0.0 --project-dir /workspace
```

## Skills

Skills are provided by bundles via their own handbook tools.

| Tool | Skill name | What It Covers |
|------|------------|----------------|
| `get_handbook` | *(none)* | Returns CLAUDE.md (overview) |
| `get_handbook_canvas` | `canvas-management` | Agent CRUD, types, content aliases, VFX |
| `get_handbook_terminal` | `terminal-control` | Read output, restart processes, send signals, scrollback, REST + WS APIs |

## Tools

All tools are discoverable via `GET /api/schema` and callable via `POST /api/call {"tool": "...", "args": {...}}`.

**Core**: `create_agent`, `list_agents`, `read_agent`, `delete_agent`, `get_state`, `execute_python`, `content_alias_file`, `content_alias_url`, `get_aliases`, `agent_call`, `launch_instance`, `stop_instance`, `list_instances`, `register_instance`, `unregister_instance`, `restart_instance`, `list_registered_instances`, `get_handbook`, `register_template`, `list_templates`, `server_logs`
**Canvas**: `move_agent`, `resize_agent`, `rename_agent`, `update_agent`, `post_output`, `refresh_agent`, `scene_vfx`, `scene_vfx_data`, `get_handbook_canvas`
**Terminal**: `terminal_output`, `terminal_restart`, `terminal_signal`, `get_handbook_terminal`
**Process (WS)**: `process_create`, `process_input`, `process_resize`, `process_enter`, `process_close`, `process_attach`

**Note:** `_DISPATCH` / `_TOOL_DISPATCH` contain ALL tools (core + bundle) — WS and REST dispatch is flat. Backend host:port is auto-assigned — always check the running server's actual URL before making requests.

## REST API

```
GET  /api/schema                        # JSON schema of all available tools
GET  /api/state                         # Full state (?scope=name to filter)
GET  /api/handbook                      # Handbook (CLAUDE.md)
POST /api/call                          # Universal tool call: {"tool": "...", "args": {...}}
POST /api/agents/{id}/resolve           # Submit + execute code
POST /api/agents/{id}/execute           # Execute raw code
GET  /api/terminal/{id}/output          # Terminal scrollback
POST /api/terminal/{id}/restart         # Restart terminal process
POST /api/terminal/{id}/signal          # Send signal: {"signal": 2}
POST /api/terminal/{id}/write           # Write to pty: {"data": "..."}
POST /api/broadcast/start               # Start broadcast mode
POST /api/broadcast/stop                # Stop broadcast mode
GET  /api/broadcast/status              # Broadcast status + viewer count
GET  /api/files                         # Project file tree
GET  /content/{alias_id}               # Serve content alias
GET  /bundles/{name}/{path}            # Serve bundle assets
```

## Transport: WS-first, REST fallback

WS `/ws` is the primary transport — `{"type": "<tool_name>", ...args}` maps directly to `_DISPATCH`. HTML agents should use WS for real-time ops (create, move, resize, delete, process I/O) and fall back to `POST /api/call` for one-shot requests. For inter-agent communication, use `agent_call` to type messages into target processes. Instance lifecycle events broadcast `instances_changed` (with full instance list) to all WS clients.

### WS message protocol (frontend ↔ backend)

**Outgoing (frontend → backend):** `create_agent`, `delete_agent`, `move_agent`, `resize_agent`, `process_create`, `process_input`, `process_resize`, `process_close` — all use `agent_id` field.
**Incoming (backend → frontend):** `agent_created`, `agent_moved`, `agent_resized`, `agent_updated`, `agent_deleted`, `agent_output`, `agent_refresh`, `process_output`, `process_created`, `process_closed`, `process_started` — all use `agent_id` field.

## Best Practices

- **Never inline base64 in `post_output`** — payloads over 512KB crash the canvas. Use `content_alias_file(file_path)` to get a `/content/{id}` URL, reference it in HTML via `window.parent.location.origin + alias_path`.
- **Large assets** (images, plots, data): save to project dir → `content_alias_file` → URL in HTML. Lightweight HTML, assets served via HTTP.

## Architecture

- **Agent types**: `terminal`, `html`
- **`delete_lock`**: boolean property on any agent's agent.json; `delete_agent` refuses deletion when true
- **Tool dispatch as component router**: all agent-to-agent communication goes through tools (`agent_call`, `post_output`, etc.)
- **Code execution**: Python subprocess (stateless, one-shot)
- **Broadcast mode**: readonly WS streaming to remote viewers (`/ws/broadcast?token=...`)
- **Remote instances**: `launch_instance` with `ssh_host` + `remote_cmd` (e.g. `"uv run fantastic"` or `"fantastic"` if installed globally). The `remote_cmd` prefix derives the remote Python for port-finding. If a server is already running on the remote (detected via `.fantastic/config.json` PID check over SSH), `launch_instance` reuses it by setting up a tunnel only (`-N` flag) instead of launching a new process.

## Project Structure

```
fantastic_canvas/
├── CLAUDE.md, fantastic.md, .env, .python-version
├── scripts/                                # Build & test scripts
│   ├── build-core.sh                       # Build pip package with bundled frontend
│   └── test-core.sh                        # Run core tests
├── skills/                                 # Core skill docs (3 files)
├── bundled_agents/                         # Agent templates (terminal, canvas)
│   ├── canvas/
│   │   ├── template.json
│   │   ├── tools.py
│   │   ├── default_vfx.js
│   │   ├── skills/canvas-management.md
│   │   └── web/                            # Frontend (React + Vite)
│   │       ├── index.html
│   │       ├── package.json
│   │       ├── vite.config.ts              # @bundles alias, proxy config
│   │       ├── tsconfig.json               # @bundles path alias
│   │       └── src/
│   │           ├── main.tsx                # imports @bundles/terminal/plugin
│   │           ├── App.tsx
│   │           ├── types.ts                # CanvasAgent, WSMessage
│   │           ├── styles.css
│   │           ├── hooks/useWebSocket.ts
│   │           ├── components/
│   │           │   ├── Canvas.tsx           # Pan/zoom + agent WS protocol
│   │           │   ├── AgentShape.tsx       # Agent UI (dispatches by type)
│   │           │   └── base/
│   │           │       ├── HtmlAgentBody.tsx
│   │           │       └── index.ts
│   │           ├── plugins/
│   │           │   ├── registry.ts
│   │           │   └── types.ts
│   │           └── test/
│   └── terminal/
│       ├── template.json
│       ├── tools.py
│       ├── plugin.ts                       # Canvas plugin (imported by main.tsx)
│       ├── source.py
│       ├── bridge.ts
│       ├── index.html
│       ├── dist/
│       └── skills/terminal-control.md
├── docs/                                   # Architecture & analysis docs
├── .fantastic/                             # Persistent agent state
│   ├── fantastic.md
│   ├── config.json                         # Server config (port, PID)
│   ├── registry.json                       # Server registry
│   ├── aliases.json                        # Content alias registry
│   ├── instances.json                      # Instance tracking
│   └── agents/
│       ├── {canvas_agent_id}/              # Canvas (real agent, bundle="canvas")
│       │   ├── agent.json                  # {id, bundle: "canvas", ...}
│       │   ├── layout.json                 # Layout positions
│       │   └── canvasbg.js                 # Background VFX
│       └── {agent_id}/                     # Per agent
│           ├── agent.json                  # identity, type, metadata
│           ├── source.py                   # last executed code
│           ├── output.html                 # HTML output
│           └── terminal.log                # scrollback (terminal-type only)
├── core/                                   # Backend package
│   ├── pyproject.toml                      # Package config + pytest config
│   ├── _paths.py                           # Asset path resolver (dev vs pip-installed)
│   ├── _bundled/                           # Bundled assets (gitignored, built by scripts/)
│   ├── cli.py                              # CLI entry point (adaptive: Core/Server/connect)
│   ├── conversation.py                     # Ring buffer + color formatting
│   ├── input_loop.py                       # Interactive conversation loop
│   ├── agent.py                            # @autorun decorator + AST discovery
│   ├── agent_store.py                      # Persistent .fantastic/ store
│   ├── bundles.py                          # Bundle store (bundled_agents/)
│   ├── engine.py                           # Core orchestration
│   ├── code_runner.py                      # Subprocess-based Python executor
│   ├── process_runner.py                   # PTY terminal management
│   ├── dispatch.py                         # ToolResult + dispatch
│   ├── instance_backend.py                 # Local/SSH instance launcher
│   ├── tools/                              # Tool dispatch (REST + WS)
│   │   ├── _agents.py                      # Agent CRUD, execution, output
│   │   ├── _bundles.py                     # Bundle management (add/remove/list)
│   │   ├── _content.py                     # Aliases, file ops
│   │   ├── _conversation.py               # Conversation log/say
│   │   ├── _terminal.py                    # Terminal control, agent_call
│   │   ├── _registry.py                    # VFX, handbook, templates
│   │   ├── _instances.py                   # Instance lifecycle
│   │   ├── _instance_tracking.py           # Instance tracking helpers
│   │   ├── _server_log.py                  # Server log buffer
│   │   └── _ws_handlers.py                 # WS-only dispatch handlers
│   ├── server/                             # FastAPI server
│   │   ├── __init__.py                     # App setup, routes, broadcast
│   │   ├── _lifespan.py                    # Startup/shutdown
│   │   ├── _rest.py                        # REST endpoints
│   │   ├── _ws.py                          # WebSocket handler
│   │   ├── _broadcast_mode.py              # Broadcast viewer mode
│   │   └── _state.py                       # Shared server state
│   └── tests/                              # Backend tests (pytest)
```

## Environment

- **uv**: project manager + virtualenv (`uv sync` to set up)
- **Python**: 3.11+ (pinned in `.python-version`)
- **Backend**: FastAPI, uvicorn, httpx
- **Frontend**: React 18, Vite 6, TypeScript
- **Ports**: Backend (auto), Frontend dev 3000
- **Docker**: python:3.11-slim + Node.js 20

## LLM Automation Restriction

Running LLM CLI agents (e.g. `claude`, `gemini`) in terminals is **restricted by subscription**. If you are not sure that a running agent or a command you are about to invoke is under an active subscription — **ask the user first**. Automated agents running on API keys or a Fantastic subscription are allowed. Do not spawn or automate LLM CLI agents without confirming authorization.

## Agent Storage Policy

**Agents are ephemeral runners, not code containers.** All source code MUST live in the project directory (version controlled), never inside `.fantastic/`.

- **DO**: store .py files in the project (e.g. `steps/01_load.py`, `src/train.py`)
- **DO**: use `execute_python("exec(open('steps/01_load.py').read())", agent_id)` to run external files
- **DO NOT**: pass large code blocks directly to `execute_python` — read from project files instead
- **DO NOT**: treat `source.py` in `.fantastic/agents/{id}/` as a code store — it's an auto-generated log, not source of truth
- `.fantastic/` should contain only: `agent.json` (metadata), `output.html` (ephemeral render), and lightweight config

The pattern is: **project owns code, agents own execution and output.** If you are unsure where something should be stored — **ask the user before proceeding**.

## File Paths — Always Relative

Terminals start with `cwd` set to the project directory. **Always use relative paths** — never `~/...` or absolute paths.

- **DO**: `open("notebooks/config.yaml")`, `os.path.join("keys", "creds.json")`
- **DO NOT**: `os.path.expanduser("~/Projects/my-project/notebooks/config.yaml")`
- **Why**: Absolute paths break in Docker (`-v $(pwd):/workspace`) where `~` = `/workspace`, not your home dir. Relative paths work everywhere — local, pip-installed, and containerized.

## Conventions

- Agent IDs: `{type}_{hex6}` format (e.g. `terminal_a3f2b1`)
- Default agent type: `terminal`
- All async (`asyncio` throughout)
- Tests: `pytest-asyncio` with `asyncio_mode = "auto"`
- `.fantastic/` excluded from file listings
