# python_runtime — async Python JOB spawner

`start` runs `python -u -c <code>` as a BACKGROUND subprocess and returns
`{job_id}` at once (non-blocking); many jobs run in parallel. stdout/stderr stream
line-by-line as `progress` events on this agent's inbox, with a final `job_done`
(collected output + exit code). Poll `status` by job_id or `watch` the events;
`stop` / `interrupt` / `clear` by job_id. Jobs live in RAM (not yet persisted
across restart). The generalized, improved `execute_python` — there is NO blocking
"run-and-wait" verb.

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
