# exec_bridge — process edge

Spawns a COMMAND in the BACKGROUND and hands it a kernel wire. Crossing kernel↔process is
an IO edge, so this leg is gated exactly like `file_bridge`/`web_ws`: **sealed by default**,
and the commands it may run come from an operator **allowlist** of regexes.

It is a GENERAL-PURPOSE command runner. A program is a FILE some process (or a `write_stream`) put on disk;
running it is `command` + `args`, exactly like anything else.

Verbs: `start` · `status` · `stop` · `interrupt` · `clear` · `reflect` · `boot`, and for a
PTY job `write` · `resize` · `ack` · `output` · `paste_image`.
Any verb NOT in that list is **MAIL** for the running program (see below).

WHERE and WITH WHAT a command runs is the RECORD's, never the call's: `cwd` (REQUIRED — a
leg without one runs nothing), `env` (the operator's extra vars), `inherit_env` (which of
the kernel's env vars pass through — none by default, `["*"]` = all; the kernel's `.env`
keys reach a child only when named). A per-call `env`/`cwd` is refused. A bare `command`
is found on the CHILD's `PATH` only — pass `PATH` through `inherit_env`/`env`, or start an
absolute path; with no `PATH` the start is refused naming it.

- `start(command, args?, pty?)` → `{job_id, status:'running', pty}` at once (`job_id` is an int) — non-blocking,
  many jobs in parallel. `command` is the program/binary; `args` is its argv (a list of
  strings). The FULL line (`command` + `args`, shell-joined) must fullmatch one of the
  record's `allowed` regexes or the start is refused, naming the patterns. There is no
  blocking run-and-wait verb: a pipe job's output streams as `progress` events on this
  agent's inbox (one per line, `{n, line, stream}`) with a final `job_done`
  (`{exit_code, ok, stdout, stderr}`), after a `job_started`.
- **A terminal is a PTY job**: `start(command, args, pty={cols, rows})` runs the command on
  a pseudo-terminal (`TERM=xterm-256color`), same gate. **Leave `command` out** and it runs
  the CHILD's own `$SHELL` — read from the environment the record passes down
  (`inherit_env`/`env`), never guessed; a child env with no `SHELL` is refused naming it.
  `job_started` and `status` report the resolved `command`. It streams `output {job_id, data}`
  chunks and ends with `job_done {job_id, exit_code, ok}`. Drive it with
  `write(job_id, data)` (keystrokes, written in full, serialized), `resize(job_id, cols,
  rows)`, `ack(job_id, chars)` (flow control — the reader pauses past 100K unacked chars,
  resumes under 5K), `output(job_id, max_bytes?)` (the 256 KiB ring — replay it when you
  re-attach) and `paste_image(job_id, data, mime)` (saves the image, types its path).
  Each is a gated verb: a viewer granted only `status`/`output`/`ack` cannot type.
  Any caller the gate admits drives any job of this agent. A running
  program's `on_message` never gets a drive verb whose `job_id` names another job —
  another job's keystrokes, pastes and drive calls never reach it; a bulk call (no
  `job_id`) does.
  Nothing respawns a terminal — `boot` starts nothing; a client that attaches and finds no
  live PTY job `start`s one.
- `status(job_id?)` → `{jobs:[…]}` — one job with its full stdout/stderr (a PTY job: its
  `cols`/`rows`), or every job of this agent as summaries.
- `stop(job_id?)` SIGKILL the job's whole session — the command and everything it
  launched (`server &`, a `nohup`, a shell's background jobs); deleting the agent or
  stopping the kernel does the same. A process that leaves the session on purpose
  (`setsid`) escapes — containment beyond that is the OS's ·
  `interrupt(job_id?)` SIGINT · `clear(job_id?)` forget finished rows. Omit `job_id` and
  it applies to every job on this agent.
- `boot` — no-op. Nothing auto-restarts: a command is started by a caller, not by the record.

Every running job IS a row in the kernel TASK TABLE: its `job_id` is that row's id (an int
from the kernel's one counter), listed by the universal `ps`. `cancel task_id=<job_id>` kills
it — the OS process is SIGKILLed before the task is aborted, so nothing is orphaned.
`delete_agent` tears its jobs down too.

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

**The child's own shell — `shell`.** A client that cannot name the shell (a browser page
has no environment) cannot write an exact `allowed` line for it either. The record's `shell`
field permits the resolved shell line itself, and nothing wider:

```
update_agent <id> inherit_env='["SHELL", "PATH"]' shell=true     # PTY start → exactly $SHELL
update_agent <id> inherit_env='["SHELL", "PATH"]' shell=login    # PTY start → exactly $SHELL -l
```

It is ONE exact line on a PTY job, not a pattern: other args, and every pipe job, still need
an `allowed` match. A shell on a terminal runs whatever is typed into it — `shell` states which
program `start` spawns, not a limit on what that program can do.

**Know what a `.*` tail grants.** `*` is allowed here like on every gate — whoever writes
it owns it. But a pattern names a COMMAND, and a command can run others:
- an interpreter with `-c`/`-e` (`python3 -c .*`, `node -e .*`), or any shell
  (`bash .*`, `zsh`), runs ANY code;
- `find … -exec`, `git -c …` / git hooks / aliases, `xargs`, `env` run ANY command.
Grant a fixed argv, or a program file, when the caller should get less than that.

## Build a durable program: a FILE plus a command

A program that persists and re-runs is a FILE you write plus a command that runs it — with
no `code` field at all:

1. **Write the program to a file** on a `file_bridge` leg, its bytes carried over a STREAM so
   nothing is escaped: `write_stream {path, bytes, truncate:true}` — `bytes` is VERBATIM, written between
   `<bytes>` and `</bytes>` exactly as it should land — real newlines, real quotes, nothing escaped. A
   program squeezed into an escaped string is lost to one stray quote; the verbatim tag is why you never
   escape it.
2. **Run it** here: `<type>start</type> <command>interp</command> <args><item>your file</item></args>`, past the `allowed`
   gate.
3. **Re-run** the SAME command later — the file is durable on disk, so it re-executes the
   same program against whatever the inputs hold THEN. That is what "a thing that keeps
   itself up to date" means.

A running program has the full wire, so it can itself `create_agent` more agents, read memory
anywhere, call an AI, or spawn more jobs — tools compose into tools.

## Mail — `{queued:true}`

Send a running program a message by sending THIS agent any verb the bundle does not own; it is
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
body]`, the header's `_binary_path` naming where the body belongs — a JSON array of
steps from the frame root, a key as a string (taken literally: `["payload", "a.b"]` is
the key `"a.b"`, never `a` → `b`), a list index as an int — so a `read_stream` chunk or
a `write_stream` payload crosses raw (never base64). Child → parent, the six
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

**Get the client instead of writing one:** send this agent `<type>connector</type>` — the
reply's `source` IS the reference wire client; paste it verbatim at the top of your
program file. A program's sends carry THIS runner's identity and grants (its `can_call`;
a target must list the runner in `callable_by`).
