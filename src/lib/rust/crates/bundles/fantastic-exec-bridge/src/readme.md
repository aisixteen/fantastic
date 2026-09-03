# exec_bridge — process edge

Spawns a COMMAND in the BACKGROUND and hands it a kernel wire. Crossing kernel↔process is
an IO edge, so this leg is gated exactly like `file_bridge`/`web_ws`: **sealed by default**,
and the commands it may run come from an operator **allowlist** of regexes.

It is a GENERAL-PURPOSE command runner, not a code runner: there is no `code` field, no
`interpreters`, no shim. A program is a FILE some process (or a `write_stream`) put on disk;
running it is `command` + `args`, exactly like anything else.

Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `reflect` · `boot`.
Any verb NOT in that list is **MAIL** for the running program (see below).

- `start(command, args?, env?, cwd?)` → `{job_id, status:'running'}` at once — non-blocking,
  many jobs in parallel. `command` is the program/binary; `args` is its argv (a list of
  strings). The FULL line (`command` + `args`, shell-joined) must fullmatch one of the
  record's `allowed` regexes or the start is refused, naming the patterns. `env` adds
  environment vars; `cwd` overrides the record's. There is no blocking run-and-wait verb:
  output streams as `progress` events on this agent's inbox (one per line, `{n, line,
  stream}`) with a final `job_done` (`{exit_code, ok, stdout, stderr}`), after a
  `job_started`.
- `status(job_id?)` → `{jobs:[…]}` — one job with its full stdout/stderr, or every job of
  this agent as summaries.
- `stop(job_id?)` SIGKILL · `interrupt(job_id?)` SIGINT · `clear(job_id?)` forget finished
  rows. Omit `job_id` and it applies to all of this agent's jobs.
- `boot` — no-op. Nothing auto-restarts: a command is started by a caller, not by the record.

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

Even open, this leg spawns ONLY a command line matching one of the operator's `allowed`
regexes on the record:

```
update_agent <id> allowed='["/usr/bin/python3 .*", "ffmpeg .*"]'
```

`reflect` advertises the live list as `allowed` — read it, then send a `start` whose
`command` + `args` shell-join into a line that fullmatches one. There is **no default** and
no per-call override: a request that matches nothing is refused, with the patterns in the
message. An empty list is a legal state — the leg then runs nothing and says so. The list is
PER RECORD (nothing is inherited from a parent). Which interpreter/binary may run is the
operator's decision, never the caller's — that is exactly why the gate is a record field a
caller cannot edit.

## Build a durable program: a FILE plus a command

A program that persists and re-runs is a FILE you write plus a command that runs it — with
no `code` field at all:

1. **Write the program to a file** on a `file_bridge` leg, its bytes carried over a STREAM so
   nothing is escaped: `write_stream {path, bytes, truncate:true}` (or `send_binary`, which writes the
   program RAW in its last `bytes` field — nothing escaped). A program squeezed into a JSON string is lost to one stray quote.
2. **Run it** here: `start {command:'<interp>', args:['<your file>']}`, past the `allowed`
   gate.
3. **Re-run** the SAME command later — the file is durable on disk, so it re-executes the
   same program against whatever the inputs hold THEN. That is what "a thing that keeps
   itself up to date" means.

A running program has the full wire, so it can itself `create_agent` more agents, read memory
anywhere, call an AI, or spawn more jobs — tools compose into tools.

## Mail — `{queued:true}`

Send a running program a turn by sending THIS agent any verb the bundle does not own; it is
delivered to the program's `on_message` and ACKed `{queued:true}`. With no live job it is an
ERROR, not a queue — a program that exited already has no inbox. Results come back as whatever
the program emits on this agent's id (tag `say`/`done` with the caller's `client_id` and
`watch` the id).

## The wire (a program speaks it, or ignores it)

The published contract is the WIRE. The child inherits two env vars:

- `FANTASTIC_CTRL_FD` — an already-open socket fd to the spawner.
- `FANTASTIC_SELF_ID` — the OWNER agent's id (the tool's public id, not the job id).

Arguments arrive as ARGV, like any command — there is no `FANTASTIC_ARGS`.

Framing both directions: `[4-byte big-endian length | kind | rest]` — kind `T`: a JSON
envelope; kind `B`: the io_bridge binary frame `[4-byte BE H | H-byte JSON header | raw
body]`, the header's `_binary_path` naming where the body belongs, so a `read_stream`
chunk or a `write_stream` payload crosses raw (never base64). Child → parent, the six
ops: `{op:"send", rid, target, payload}` · `{op:"emit", target?, payload}` ·
`{op:"watch", src}` · `{op:"unwatch", src}` · `{op:"onmessage"}` · `{op:"offmessage"}`.
Parent → child: `{op:"reply", rid, data}` · `{op:"event", src, payload}` ·
`{op:"inbox", payload}`. `rid` correlates a `send` with its `reply`; the parent echoes it
back untouched. ONE FRAME CAPS AT 16 MiB in every runtime — over it the length prefix is a
wire violation and the relay closes the socket, so move bulk with `write_stream` chunks
(the reference client raises rather than writing such a frame). **EOF on the fd means the kernel is GONE** — exit, or become an orphan
nothing can reach. Traffic goes child → spawner → kernel only: a job never dials a host and
never knows a URL. A command that never reads the fd runs exactly like any OS process.

A program that wants a `kernel` global rather than raw frames includes the reference wire
client (`exec_bridge/_connector.py`, `CONNECTOR_SRC`) at the top of its own source — it is a
library the program bundles, not something the bridge prepends. It exposes:

```python
kernel.send(target, payload) -> reply      # request/reply to any agent by id
kernel.emit(payload)                       # fire-and-forget on YOUR OWN agent
kernel.emit(target, payload)               # …or on another
kernel.reflect(target="kernel")
off = kernel.watch(src, cb)                # PUSH: cb(payload) per event from src
off = kernel.on_message(cb)                # PUSH: turns on THIS agent's inbox
kernel.self_id                             # your agent id (arguments are ARGV)
```

So a job is a first-class routine: read memory anywhere, call an AI, spawn another job, push
to any peer — all by id. A step written as a program and a step written as an LLM call are
substitutable.

## Not a sandbox

An allowlisted command is a NAMED command, not a sandbox. A python it permits still has
`open()`, `socket` and `subprocess`. What the allowlist buys is that the operator chose WHICH
commands may run instead of the kernel guessing, and that a program cannot pick a different
one. Anything stronger needs an OS mechanism (`sandbox-exec`, a separate uid, a container)
and none of it is reachable in-process. Stated plainly rather than reassuringly: opening this
leg grants local code execution to whoever can reach it.

**Get the client instead of writing one:** send this agent `{type:'connector'}` — the
reply's `source` IS the reference wire client; paste it verbatim at the top of your
program file. A program's sends carry THIS runner's identity and grants (its `can_call`;
a target must list the runner in `callable_by`).
