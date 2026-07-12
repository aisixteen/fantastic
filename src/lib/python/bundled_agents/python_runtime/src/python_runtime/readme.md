# python_runtime — async Python JOB spawner

`start` runs `python -u -c <code>` as a BACKGROUND subprocess and returns
`{job_id}` at once (non-blocking); many jobs run in parallel. stdout/stderr stream
line-by-line as `progress` events on this agent's inbox, with a final `job_done`
(collected output + exit code). Poll `status` by job_id or `watch` the events;
`stop` / `interrupt` / `clear` by job_id. A transient `start` job lives in RAM, but
the agent's `code` field IS a durable tool: it persists ON the record, `boot`
re-runs it after a restart, and `delete_agent` tears it down. The generalized,
improved `execute_python` — there is NO blocking "run-and-wait" verb.

Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `reflect` · `boot`.

## Your code gets a `kernel` connector

Every spawned job runs with a `kernel` object injected ahead of your code. It
mirrors the kernel surface and talks ONLY to its spawner (this agent) over a
private control fd — the spawner holds the live kernel and relays. Same no-bypass
shape every out-of-process connector follows (child → spawner → kernel): the job
never dials a host and never knows a URL.

    kernel.send(target, payload) -> reply      # request/reply to any agent by id
    kernel.emit(target, payload)               # fire-and-forget
    kernel.reflect(target="kernel")
    off = kernel.watch(src, cb)                # PUSH: cb(payload) per event from src
    off = kernel.on_message(cb)                # PUSH: messages on THIS job's inbox

(`watch` / `on_message` run callbacks on a background reader thread.) So a job is a
first-class routine — read memory anywhere, call an AI, spawn another job, push to
any peer agent — all by id, over the same protocol. A step written as code and a
step written as an LLM call become substitutable.

## Authored tool — self-extension

Give a record a `code` field and it becomes a durable tool:
`create_agent handler_module=python_runtime.tools id=<name> code='<program as one
JSON string>'`. The `code` is stored on the record, auto-runs, and `boot` re-runs it
after a restart; `delete_agent` removes it. **Escaping:** `code` is ONE JSON string,
so escape newlines as `\n` and quotes as `\"` inside it.

The script is **push**: register an `on_message` callback, then KEEP THE PROCESS
ALIVE (a durable tool must block — if the code falls off the end the process exits
and the tool is dead). The callback runs on its own thread, so it may freely
`kernel.send`/`emit` (blocking on replies). Handle ONLY `send` turns: ALL inbound
traffic — including your own emitted `say`/`done` and `reflect`/`boot` — reaches the
callback, so filter by type. Emit on YOUR OWN agent with the ONE-ARG
`kernel.emit(payload)` (there is no `kernel.say`); `kernel.self_id` is your agent id.

    import threading
    def on_turn(msg):                        # msg = the inbound payload
        if msg.get("type") != "send":        # ignore your own say/done + reflect/boot
            return
        client = msg.get("client_id")
        kernel.emit({"type": "say",  "text": msg.get("text"), "client_id": client})
        kernel.emit({"type": "done", "client_id": client})       # end of this turn
    kernel.on_message(on_turn)
    threading.Event().wait()                 # stay resident to keep receiving turns

Tag every `say` / `done` with the caller's `client_id` so its result routes back to
the right listener. A running script has the full `kernel` connector, so it can
itself `create_agent` more agents — tools compose into tools. To change a running
tool's `code`, `update_agent` it then `stop` + `boot` (persisted code re-runs on
boot; a live job does not hot-swap).

## Meta-possibility — any routine orchestrates the whole substrate

Because every routine reaches every agent by id through its connector — a host
python job here, any other out-of-process routine (host or peer) over there — from
EITHER kernel you can: read memory from anywhere (`send(<state>, {read})`), run an
inference turn
(`send(<ai>, {send, system_prompt, text})`), and/or spawn a compute job
(`send(<py>, {start, code})`) — regardless of which kernel owns the target. Memory,
inference, and compute are interchangeable units you wire from anywhere.

## Cross-runtime interpreter determinism

On first `boot`, when neither `record.python` nor `record.venv` is set, this
bundle persists `sys.executable` into `record.python` via `kernel.update`. The
interpreter is then pinned on disk — opening the same workdir under a different
runtime reads the persisted path and dispatches to the same interpreter. Idempotent
(subsequent boots are no-ops when `python` is set); explicit `python` / `venv`
overrides on the record always take precedence.
