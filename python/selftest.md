# Fantastic selftests

Every component owns a hand-written, executable test spec. AI agents
(Claude Code, etc.) read these files, ask required pre-flight questions,
drive the system at the user-facing surface, and fill summary tables.

Complement to `pytest` — pytest covers in-process unit logic; selftests
cover the user surface (CLI, HTTP, WS, PTY, browser).

## For Claude Code / other AI agents

1. Read this index completely.
2. Parse user intent → set of scopes to **INCLUDE / EXCLUDE**.
3. From the index table, pick files whose scopes are all allowed.
4. For each selected file, in order:
   - Read it end-to-end.
   - If `requires:` lists pre-flight items, **ASK USER** for missing info
     (LLM provider, running serve, etc.) **BEFORE** wiping any state.
   - Run tests in numerical order.
   - Capture pass/fail + notes per test.
5. After all files: emit ONE aggregate report — rows (file, test#),
   columns (name, pass/fail, notes). List skipped files with reason.

Strict rules:

- Never silently fake a skip.
- Never run destructive ops without user confirmation if state exists.
- If a regression signal in a test triggers, **STOP** that file's
  remaining tests and flag it.
- Do not invent expected output — ask if the spec is unclear.
- A correctly-deployed `fantastic` answers `kernel.reflect` over WS
  with every URL/transport/bundle/agent you need to issue your first
  send. Open `ws://host/<any-agent>/ws` and send `{"type":"call",
  "target":"kernel","payload":{"type":"reflect"},"id":"1"}`. If you
  find yourself reading `kernel/` source or `web/app.py` to figure
  out a transport URL — that's a primer regression. Stop and flag it.

## Stateful bundles need a running `serve`

Some bundles hold state in process-memory that doesn't survive
separate `fantastic call …` invocations:

- `terminal_backend` — PTY child process; dies with the kernel.
- `ollama_backend` — cached HTTP client + in-flight `_run` tasks.
- `web` — uvicorn instance.

Their selftests start a single `fantastic` and drive it over the
WS proxy (`ws://localhost:$PORT/<id>/ws`). Each selftest's pre-flight
defines a shell `call()` helper that wraps a one-shot WS round-trip
in inline Python. Don't try to use the `call` subcommand for these —
you'll get false failures (PTY reports `running:false`, provider
state lost between calls, etc.) because the short-runner spawns a
fresh kernel that can't see the daemon's process-memory state.

Canonical WS `call()` helper for selftest pre-flight (paste verbatim
after `PORT=...` is set):

```bash
call() {
  TARGET="$1" PAYLOAD="$2" PORT="$PORT" uv run --active python - <<'PY'
import asyncio, json, os, websockets
target = os.environ["TARGET"]; payload = json.loads(os.environ["PAYLOAD"])
port = os.environ["PORT"]
async def main():
    async with websockets.connect(f"ws://localhost:{port}/{target}/ws") as ws:
        await ws.send(json.dumps({"type":"call","target":target,"payload":payload,"id":"1"}))
        while True:
            m = json.loads(await ws.recv())
            if m.get("id") == "1" and m.get("type") in ("reply","error"):
                print(json.dumps(m.get("data"))); return
asyncio.run(main())
PY
}
```

Usage: `call <id> '{"type":"<verb>", ...}'` — prints the reply (or
error) data as JSON on stdout. Same I/O shape as the old curl helper.

**Stateless bundles** (`cli`, `file`, `scheduler`, `canvas_backend`)
keep state on disk only and run fine via `fantastic call`. Admin
verbs (`create_agent` / `delete_agent` / `update_agent` /
`list_agents`) are baked into the `Agent` class itself — every
agent answers them natively for its own children, so there's no
`core` *bundle* to drive selftests against; the root agent (id
`"core"`) IS what core was.

## Test-runner pitfalls (LLM agents read this)

- **Pipe curl directly into `python -m json.tool | grep -F …`**, like the
  specs do. Do NOT do `out=$(curl …)` then `echo "$out" | …` — zsh/bash
  `echo` may interpret JSON escape sequences (`\r`, `\n`) inside the
  captured string, corrupting strict JSON parsers downstream. The
  server's JSON is correctly escaped; the corruption happens during
  shell variable round-tripping.
- **`timeout` is not installed on macOS by default.** Use
  `curl --max-time <s>` to bound a slow request instead.
- **After `fantastic` reports `kernel up`, sleep ~0.4s** before
  hitting routes that depend on freshly booted singletons (rare race
  between print and route binding under cold caches).

## Scope taxonomy

| tag | meaning |
|---|---|
| `kernel` | in-process Agent tree only; no HTTP, no PTY |
| `cli` | drives REPL via stdin |
| `subprocess` | uses `fantastic call/reflect/serve` |
| `http` | needs running webapp (uvicorn) |
| `ws` | exercises WebSocket proxy |
| `web` | superset of http+ws (any browser-touching server flow) |
| `webapp` | a UI bundle (terminal_webapp / ai_chat_webapp / canvas_webapp) |
| `pty` | requires real PTY |
| `ai` | needs live LLM provider |
| `persistence` | exercises file-agent-routed I/O |
| `binary` | bytes through WS binary protocol |
| `bus` | browser BroadcastChannel; requires actual browser |
| `cascade` | exercises substrate cascade-delete + lock semantics |

## Index

| file | scopes | description |
|---|---|---|
| `bundled_agents/core/selftest.md` | kernel, cli | system verbs (list/create/update/delete/reflect) + REPL parsing |
| `bundled_agents/cli/selftest.md` | cli | renderer verbs (token/done/say/error) |
| `bundled_agents/web/host/selftest.md` | http, web, binary | uvicorn rendering host — index, file proxy, transport.js, favicon, lock |
| `bundled_agents/web/web_ws/selftest.md` | http, ws, web, web_ws | WS verb-invocation surface — mounts `/<host_id>/ws` on parent web |
| `bundled_agents/web/web_rest/selftest.md` | http, web, web_rest | REST diagnostic surface — `POST /<rest_id>/<target_id>` body=payload |
| `bundled_agents/scheduler/selftest.md` | kernel, persistence, time | schedule/tick/fire, history.jsonl, file_agent_id failfast |
| `bundled_agents/file/selftest.md` | kernel, persistence | read/write/list/delete/rename/mkdir, path safety |
| `bundled_agents/terminal/terminal_backend/selftest.md` | kernel, pty | PTY spawn, shell done-token, timeout recovery |
| `bundled_agents/terminal/terminal_webapp/selftest.md` | webapp, web | get_webapp + xterm UI in browser |
| `bundled_agents/ai/ollama/ollama_backend/selftest.md` | kernel, ai, persistence | reflect-driven assembly, native tool-calls, multi-step loop |
| `bundled_agents/ai/nvidia/nvidia_nim_backend/selftest.md` | kernel, ai, persistence, http | NVIDIA NIM (OpenAI-compatible); api_key sidecar; rate-limit retry; live single-shot |
| `bundled_agents/ai/ai_chat_webapp/selftest.md` | webapp, web | provider-agnostic chat UI (fronts ollama_backend, nvidia_nim_backend, etc.) |
| `bundled_agents/canvas/canvas_backend/selftest.md` | kernel | dual-verb add_agent (get_webapp / get_gl_view); explicit membership |
| `bundled_agents/canvas/canvas_webapp/selftest.md` | webapp, web, bus | two-layer host (DOM iframe + GL view); per-agent dispatch on probe |
| `bundled_agents/canvas/telemetry_pane/selftest.md` | webapp, web | live agent-vis GL view; subscribes to kernel state stream |
| `bundled_agents/canvas/gl_agent/selftest.md` | kernel, http, web | GL-view-as-record agent; mirror of html_agent for inline `gl_source` |
| `bundled_agents/canvas/html_agent/selftest.md` | kernel, http, web | UI-as-record agent; render_html duck type; cross-agent calls from iframe |
| `bundled_agents/kernel_bridge/selftest.md` | kernel, ws, ssh | cross-kernel forward envelopes; memory + WS + SSH+WS transports |
| `bundled_agents/ssh_runner/selftest.md` | kernel, ssh | remote `fantastic` lifecycle; SSH tunnel for canvas iframing |
| `bundled_agents/python_runtime/selftest.md` | kernel | subprocess Python exec; timeout / interrupt / cwd |

## Selection examples

| user says | filter | files run |
|---|---|---|
| "all tests" | (all) | every file |
| "non-web tests" / "no browser" | EXCLUDE {http, ws, web, webapp, bus} | core, cli, scheduler, file, terminal_backend, ollama_backend, canvas_backend |
| "in canvas, run webapp tests" | INCLUDE {web, webapp, bus} | webapp, terminal_webapp, ai_chat_webapp, canvas_webapp, telemetry_pane |
| "kernel only" | INCLUDE {kernel}, EXCLUDE {pty, ai, web} | core, cli, scheduler, file, canvas_backend |
| "I have ollama running" | + ai | adds ollama_backend AI tests |
| "no PTY" | EXCLUDE {pty} | drops terminal_backend |
| "binary protocol" | INCLUDE {binary} | webapp's binary subset |

## Aggregate report format

```
# Selftest report — <date>, branch <git rev>

provider used: <ollama@localhost / anthropic / openai / none>
files run: <N>   files skipped: <N>

| file | test# | name | pass | notes |
|---|---|---|---|---|
| core/selftest.md | 1 | list_agents | ✓ | |
| ...

skipped:
- ollama_backend/selftest.md — no AI provider configured
- terminal_backend/selftest.md — user excluded `pty`
```
