# exec_bridge — process edge

Spawns a program in the BACKGROUND and hands it a kernel wire. Crossing kernel↔process is
an IO edge, so this leg is gated exactly like `file_bridge`/`web_ws`: **sealed by default**,
and the interpreter it may run comes from an operator **allowlist**.

Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `reflect` · `boot`.
Any verb NOT in that list is **MAIL** for the running program (see below).

- `start(code?, args?, interpreter?, shim?, cwd?)` → `{job_id, status:'running'}` at once —
  non-blocking, many jobs in parallel. `code` falls back to the record's `code`; `args` is
  THIS call's arguments and reaches the program as `kernel.args` (they ride the process env,
  so they are capped at 256 KB — put bulk data in an agent and pass its id). There is no
  blocking run-and-wait verb: output streams as `progress` events on this agent's inbox
  (one per line, `{n, line, stream}`) with a final `job_done` (`{exit_code, ok, stdout,
  stderr}`), after a `job_started`.
- `status(job_id?)` → `{jobs:[…]}` — one job with its full stdout/stderr, or every job of
  this agent as summaries.
- `stop(job_id?)` SIGKILL · `interrupt(job_id?)` SIGINT · `clear(job_id?)` forget finished
  rows. Omit `job_id` and it applies to all of this agent's jobs.
- `boot` — idempotent; re-runs the record's `code` after a kernel restart.

Every running job is a row in the kernel TASK TABLE (`task_id` in each `status` snapshot,
listed by the universal `ps`). `cancel task_id=<id>` kills it — the OS process is SIGKILLed
before the task is aborted, so nothing is orphaned. `delete_agent` tears its jobs down too.

## Sealed by default

A freshly created leg denies every verb except `reflect` (`{reason:"unauthorized"}`).
Open it consciously:

```
update_agent <id> ingress_rule=allow_all_ingress      # open wide (local/dev)
update_agent <id> ingress_rule=password_ingress       # require group token ($FANTASTIC_GROUP_TOKEN)
```

## The allowlist is the law

Even open, this leg spawns ONLY an interpreter the operator listed on the record:

```
update_agent <id> interpreters='["~/.venvs/tools/bin/python"]'
```

`reflect` advertises the live list as `interpreters` — read it, then name one per record or
per `start` (`interpreter=<one of them>`; naming none is legal only when the list holds
exactly one). There is **no default interpreter** and no per-call override: a request for
something not on the list is refused, with the list in the message. An empty list is a legal
state — the leg then runs nothing and says so. The list is PER RECORD (nothing is inherited
from a parent), and a write that names `interpreter` must carry `interpreters` in the SAME
call — write-time validation reads the patch, not the merged record. Non-CPython
interpreters also want an `argv_template` on the record (`<interp>`/`<program>` are
substituted; the default is the CPython `["<interp>", "-u", "-c", "<program>"]`, and `-u` is
what makes output line-live).

## Your `code` is a SCRIPT

Give a record `code` + an interpreter and it becomes a durable tool:
`create_agent handler_module=exec_bridge.tools id=<name>
interpreters='["<abs path to an interpreter>"]' shim=python code='<program as one JSON
string>'` — one entry needs no `interpreter` field to pick it (`code` is ONE JSON string, so
escape newlines as `\n` and quotes as `\"`).

**The `code` is a SCRIPT: it runs top to bottom when created and again on reboot, and it
survives reboot. A bare `def` only defines and exits — put the work at module level. To
handle later sends, call `kernel.on_message(cb)` and keep running** (block at the end, e.g.
`threading.Event().wait()`; if the code falls off the end the process exits and the tool is
dead). `delete_agent` removes it. A running program has the full wire, so it can itself
`create_agent` more agents — tools compose into tools. To change a live tool: `update_agent`
its `code`, then `stop` + `boot` (no hot-swap).

## Mail — `{queued:true}`

Send a running program a turn by sending THIS agent any verb the bundle does not own; it is
delivered to the program's `on_message` and ACKed `{queued:true}`. With no live job it is an
ERROR, not a queue — a script that only defined something already exited before your turn
arrived. Results come back as whatever the program emits on this agent's id (tag `say`/`done`
with the caller's `client_id` and `watch` the id).

## The wire (`shim` is optional sugar over it)

The published contract is the WIRE. The child inherits three env vars:

- `FANTASTIC_CTRL_FD` — an already-open socket fd to the spawner.
- `FANTASTIC_SELF_ID` — the OWNER agent's id (the tool's public id, not the job id).
- `FANTASTIC_ARGS` — this call's `args`, as JSON.

Framing both directions: `[4-byte big-endian length | JSON body]`. Child → parent, the six
ops: `{op:"send", rid, target, payload}` · `{op:"emit", target?, payload}` ·
`{op:"watch", src}` · `{op:"unwatch", src}` · `{op:"onmessage"}` · `{op:"offmessage"}`.
Parent → child: `{op:"reply", rid, data}` · `{op:"event", src, payload}` ·
`{op:"inbox", payload}`. `rid` correlates a `send` with its `reply`; the parent echoes it
back untouched. **EOF on the fd means the kernel is GONE** — exit, or become an orphan
nothing can reach. Traffic goes child → spawner → kernel only: a job never dials a host and
never knows a URL.

Name `shim:'<name>'` on `start` (or on the record) and a client for that language is
prepended to your program for you — pure convenience. `reflect` lists the available names in
`shims`; `python` gives a `kernel` global:

```python
kernel.send(target, payload) -> reply      # request/reply to any agent by id
kernel.emit(payload)                       # fire-and-forget on YOUR OWN agent
kernel.emit(target, payload)               # …or on another
kernel.reflect(target="kernel")
off = kernel.watch(src, cb)                # PUSH: cb(payload) per event from src
off = kernel.on_message(cb)                # PUSH: turns on THIS agent's inbox
kernel.self_id · kernel.args               # your agent id · this call's args
```

Name no shim and your program speaks the frames itself — that is the generic path, and it is
why adding a language here is adding a blob, not a code path. So a job is a first-class
routine: read memory anywhere, call an AI, spawn another job, push to any peer — all by id.
A step written as code and a step written as an LLM call are substitutable.

## Not a sandbox

An allowlisted interpreter is a NAMED interpreter, not a sandbox. A python on the list still
has `open()`, `socket` and `subprocess`. What the allowlist buys is that the operator chose
WHICH runtime and WHICH environment (a pre-created venv with a known dependency set) instead
of the kernel guessing, and that a program cannot pick a different one. Anything stronger
needs an OS mechanism (`sandbox-exec`, a separate uid, a container) and none of it is
reachable in-process. Stated plainly rather than reassuringly: opening this leg grants local
code execution to whoever can reach it.
