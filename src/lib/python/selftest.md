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

- **io legs SEAL by default — a denial is expected, not a failure.** A freshly-created
  `web_ws` / `web_rest` / `file_bridge` denies every inbound frame/verb
  (`{reason:"unauthorized"}`, REST `403`) until opened with `ingress_rule=allow_all_ingress`
  (or `password`). Selftest pre-flights open the leg they exercise; if you compose your
  own, open it too. Reading a seal as a test failure is the misread to avoid.
- Never silently fake a skip.
- Never run destructive ops without user confirmation if state exists.
- If a regression signal in a test triggers, **STOP** that file's
  remaining tests and flag it.
- Do not invent expected output — ask if the spec is unclear.
- A correctly-deployed `fantastic` hands you the whole substrate in one
  call: `reflect readme=true` returns the addressed agent's identity +
  the root readme (every transport URL / bundle / agent you need to
  issue your first send). Over WS: open `ws://host/<any-agent>/ws` and
  send `{"type":"call","target":"kernel","payload":{"type":"reflect",
  "readme":true,"bundles":"all"},"id":"1"}`. The transport/wire prose
  lives in that readme now, not in the reflect JSON. If you find
  yourself reading `kernel/` source or `web/app.py` to figure out a
  transport URL — that's a regression. Stop and flag it.

## Stateful bundles need a running `serve`

Some bundles hold state in process-memory that doesn't survive
separate `fantastic <id> <verb> …` invocations:

- `exec_bridge` — its jobs (pipe or PTY) are child processes; they die with the kernel.
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

**Stateless bundles** (`cli`, `file_bridge`, `scheduler`) keep state on disk
only and run fine via `fantastic <id> <verb>`. Admin
verbs (`create_agent` / `delete_agent` / `update_agent` /
`list_agents`) are baked into the `Agent` class itself — every
agent answers them natively for its own children, so there's no
`kernel_state` *bundle* to drive selftests against; the root agent (id
`"kernel_state"`) IS what kernel_state was.

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
| `subprocess` | uses `fantastic <id> <verb>` / `reflect` / `serve` |
| `http` | needs running webapp (uvicorn) |
| `ws` | exercises WebSocket proxy |
| `web` | superset of http+ws (any browser-touching server flow) |
| `webapp` | a TS frontend view (canvas compositor / terminal_view / ai_view / GL host — in `ts/`) |
| `pty` | requires real PTY |
| `ai` | needs live LLM provider |
| `persistence` | exercises file_bridge-routed I/O |
| `binary` | bytes through WS binary protocol |
| `cascade` | exercises substrate cascade-delete + lock semantics |

## Index

| file | scopes | description |
|---|---|---|
| `bundled_agents/kernel_state/selftest.md` | kernel, cli | system verbs (list/create/update/delete/reflect) + REPL parsing |
| `bundled_agents/cli/selftest.md` | cli | renderer verbs (token/done/say/error) |
| `bundled_agents/web/host/selftest.md` | http, web, binary | uvicorn rendering host — index, file proxy, favicon, lock |
| `bundled_agents/io/web_ws/selftest.md` | http, ws, web, web_ws | WS verb-invocation surface — mounts `/<host_id>/ws` on parent web |
| `bundled_agents/io/web_rest/selftest.md` | http, web, web_rest | REST diagnostic surface — `POST /<rest_id>/<target_id>` body=payload |
| `bundled_agents/scheduler/selftest.md` | kernel, persistence, time | schedule/tick/fire, schedule_events.yaml persisted through the loader (no store ⇒ fails loud) |
| `bundled_agents/agentic_resources/selftest.md` | kernel, roles | role cards, spawn, per-role and total limits (off by default: `FANTASTIC_AGENTIC_RESOURCES=1`) |
| `bundled_agents/io/file_bridge/selftest.md` | kernel, persistence | sealed-by-default fs edge — read/write/list/delete/rename/mkdir, running-dir clamp, path safety |
| `bundled_agents/ai/ollama/ollama_backend/selftest.md` | kernel, ai, persistence | reflect-driven assembly, native tool-calls, multi-step loop |
| `bundled_agents/io/ws_bridge/selftest.md` | kernel, ws | cross-kernel WS bridge — asymmetric raw call frames to remote `web_ws` (no peer bridge); memory + WS transports; streaming via `watch_remote` (subscribed `src` only) |
| `bundled_agents/io/exec_bridge/selftest.md` | kernel, pty | sealed-by-default command runner — regex `allowed` allowlist (refusal names the patterns), record-owned cwd/env/inherit_env, start/status/stop/interrupt/clear/boot, PTY jobs (write/resize/ack/output/paste_image), control wire, mail to a live program, task-table row + cancel |

The root selftest is the `kernel_state` one (id `"kernel_state"`) — the
persistence/hydration root that owns `.fantastic/`. View/content
selftests belong to the apps on the TS kernel library, not here.

## Selection examples

| user says | filter | files run |
|---|---|---|
| "all tests" | (all) | every file |
| "non-web tests" / "no browser" | EXCLUDE {http, ws, web, webapp, bus} | kernel_state, cli, scheduler, file, exec_bridge, ollama_backend |
| "in canvas, run view tests" | INCLUDE {web, webapp, bus} | (none here — the TS view-agents live in `ts/`) |
| "kernel only" | INCLUDE {kernel}, EXCLUDE {pty, ai, web} | kernel_state, cli, scheduler, file |
| "I have ollama running" | + ai | adds ollama_backend AI tests |
| "no PTY" | EXCLUDE {pty} | drops exec_bridge |
| "binary protocol" | INCLUDE {binary} | web/host's binary subset |

## Aggregate report format

```
# Selftest report — <date>, branch <git rev>

provider used: <ollama@localhost / vllm / openai / none>
files run: <N>   files skipped: <N>

| file | test# | name | pass | notes |
|---|---|---|---|---|
| kernel_state/selftest.md | 1 | list_agents | ✓ | |
| ...

skipped:
- ollama_backend/selftest.md — no AI provider configured
- io/exec_bridge/selftest.md — user excluded `pty`
```
