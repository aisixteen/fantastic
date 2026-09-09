"""exec_bridge bundle — the PROCESS-transport member of the io family.

A subprocess is OUTSIDE the trust domain: crossing kernel↔process is an IO edge, gated
exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒ every verb except
`reflect` denies with the teaching shape). Open it consciously with
`ingress_rule=allow_all_ingress`. Even open, the LAW is the ALLOWLIST: only an interpreter the
operator allowed by regex can be spawned, and that choice is not overridable per
call. The spawn primitive lives in `proc` — the ONE process surface. This agent is the
GATE + verb shape over it; it never spawns directly.

## Why this exists, and what it replaces

A per-language runtime is an entire executor — job table, connector, relay, boot
rehydration, addressable inbox — bound to ONE language, and that machinery ended up
duplicated once per language. It was also the only boundary in this kernel that was not a
sealed io edge: registering one "GRANTS ARBITRARY LOCAL CODE EXECUTION".

Here the interpreter is a FIELD. "A python agent" stops being a bundle and becomes a
record. Everything that is not language-specific — the job table, the pump, the relay,
the fd passing, the task-table integration, the mail contract — is the same code it
always was, because none of it ever knew a language.

## The wire is the contract

A spawned command MAY speak the control fd (see `proc`); one that never reads it
runs exactly like any OS process.

## Verbs

  start · status · stop · interrupt · clear · reflect · boot

Any verb this bundle does not own is MAIL for the running program — see `inbox` in
`reflect`.

## What is NOT claimed

An allowlisted interpreter is a NAMED interpreter, not a sandbox — see `proc`. The
allowlist means the operator chose which runtime and which environment, and that a
program cannot choose a different one. Stated plainly because the executor this replaces
carried a documented hole, and swapping an honest hole for a dishonest reassurance would
be the worse trade.
"""

from __future__ import annotations

import asyncio
import json
import secrets
import signal as signal_mod
import socket
import struct
import time

from exec_bridge import proc
from io_bridge import decode_frame, describe as _describe, encode_frame
from io_bridge import gate_inbound, resolve_ingress
from io_bridge._base import parse_spec
from kernel._refusal import (
    NOT_FOUND,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    bad,
    missing,
    refusal,
)

# ONE frame bound on this wire, both ends, every runtime. A corrupt length must not
# become a multi-GB allocation, and a program that means to move 20 MB means to
# STREAM it (`write_stream`), not to frame it. Over-limit is a WIRE VIOLATION: the
# relay closes the socket, and the connector refuses to write such a frame at all.
MAX_FRAME = 16 * 1024 * 1024

# job_id -> {owner, proc, status, started_at, exit_code, lines, last_line, stdout,
#            stderr, task, task_id, ctrl_sock, ctrl_task}.
# status: running|stopping|done|failed|stopped. Module-global (shared across
# exec_bridge agents); every op filters by owner.
_jobs: dict[str, dict] = {}

# The single job currently subscribed to each OWNER agent's inbox (on_message).
# owner_id -> {"job_id", "proxy", "task"}. Many jobs can run in parallel on one agent,
# but exactly ONE receives the owner's inbox mail — otherwise a durable tool re-started
# (or two `start`s) would `kernel.watch` the owner twice and process every inbound
# `send` in duplicate.
_owner_inbox: dict[str, dict] = {}


def _alive(job: dict) -> bool:
    """THE definition of a live job: its OS process has not exited.

    One definition for everything that asks — the mail path, `boot`, `stop`/`interrupt`,
    and reflect's count. The status STRING lags reality (a killed-but-unreaped job reads
    `stopping` while its process is very much alive), and `clear` is the only caller that
    legitimately wants the label, because it decides what is finished enough to forget.
    """
    return job["proc"].returncode is None


def _has_running_job(owner_id: str) -> bool:
    """True iff this agent owns a job whose subprocess hasn't exited."""
    return any(v.get("owner") == owner_id and _alive(v) for v in _jobs.values())


# ─── output pump ────────────────────────────────────────────────


async def _read_stream(reader, job, stream, owner, job_id, kernel):
    """Stream one pipe line-by-line: a `progress` event per line + collect the raw text
    (newlines preserved) for the final job_done/status output."""
    parts: list[str] = []
    async for raw in reader:
        text = raw.decode("utf-8", errors="replace")
        parts.append(text)
        job["lines"] += 1
        job["last_line"] = text.rstrip("\n")
        await kernel.emit(
            owner,
            {
                "type": "progress",
                "job_id": job_id,
                "n": job["lines"],
                "line": text.rstrip("\n"),
                "stream": stream,
            },
        )
    return "".join(parts)


async def _pump(owner, job_id, process, kernel):
    """Drain stdout+stderr concurrently, then await exit + emit job_done.

    THE PUMP IS THE JOB'S TASK — registered in the kernel task table by `_start`, so `ps`
    lists the job and `cancel` can abort it. Aborted (the SIGKILL half of cancel, after
    `on_cancel` has already killed the OS process), it finalizes the RAM row to `stopped`
    on its way out — no row is left stuck `running`."""
    job = _jobs.get(job_id)
    if job is None:
        return
    out, err = "", ""
    try:
        try:
            out, err = await asyncio.gather(
                _read_stream(process.stdout, job, "stdout", owner, job_id, kernel),
                _read_stream(process.stderr, job, "stderr", owner, job_id, kernel),
            )
        except asyncio.CancelledError:
            raise
        except Exception as e:  # a pump fault must never take down the loop
            err = f"[pump error] {e}"
        code = await process.wait()
    except asyncio.CancelledError:
        job["status"] = "stopped"
        job["exit_code"] = process.returncode
        raise
    job["exit_code"] = code
    job["stdout"], job["stderr"] = out, err
    job["status"] = (
        "stopped"
        if job["status"] == "stopping"
        else ("done" if code == 0 else "failed")
    )
    await kernel.emit(
        owner,
        {
            "type": "job_done",
            "job_id": job_id,
            "exit_code": code,
            "ok": code == 0,
            "stdout": out,
            "stderr": err,
        },
    )


# ─── connector relay (spawner side of the control fd) ───────────


def _inbox_framer(payload):
    """on_message framer: forward traffic on the owner agent, minus the job's own
    telemetry (its progress/job_started/job_done emits echo back via the owner)."""
    if isinstance(payload, dict) and payload.get("type") in (
        "progress",
        "job_started",
        "job_done",
    ):
        return None
    return {"op": "inbox", "payload": payload}


def _unframe(body: bytes) -> dict:
    """Decode one child frame: kind T = JSON; kind B = the io_bridge binary frame with
    the raw body restored at `_binary_path` (a `write_stream` payload's bytes)."""
    kind = body[:1]
    if kind == b"B":
        return decode_frame(body[1:])
    if kind == b"T":
        return json.loads(body[1:].decode("utf-8"))
    raise ValueError(f"unknown frame kind {kind!r}")


async def _serve_ctrl(parent_sock, owner_id, job_id, kernel):
    """Relay a job's wire calls into the real kernel (child → spawner → kernel).

    `send` = request/reply; `emit` = one-way; `watch`/`onmessage` subscribe via
    kernel.watch into a per-sub PROXY INBOX this relay drains, pushing each event down
    the control fd. Ends when the child closes the socket (job exit).

    Wire: `[4-byte BE length | kind | rest]`, both directions — kind T a JSON
    envelope, kind B the shared io_bridge binary frame for a payload or reply that
    carries raw bytes (`read_stream`/`write_stream`). A bytes reply used to be
    unserializable here and reached the program as None (measured 2026-09-02).

    The proxy ids carry this bundle's prefix (`_xb…`): two spawner bundles in one kernel
    must not collide on a synthetic inbox id.

    A `watch`/`onmessage` the kernel REFUSES registers nothing — no proxy inbox, no
    drain task, no `subs` row — and the refusal is pushed down the fd as an `event`
    on the source. A silently-deaf subscription is the one failure the program's
    author cannot diagnose."""
    try:
        reader, writer = await asyncio.open_connection(sock=parent_sock)
    except Exception:
        try:
            parent_sock.close()
        except OSError:
            pass
        return

    subs: dict = {}  # src -> (proxy_id, drain_task)
    self_sub: list = [None]  # on_message: (proxy_id, drain_task) | None

    def push(obj):
        # `[4-byte BE len | kind | rest]` — kind T = JSON, kind B = the io_bridge
        # binary frame (a reply carrying a `read_stream` chunk, a watched event with
        # bytes). Raw bytes ride the fd raw, never base64 — the same codec every
        # other transport uses.
        wire, binary = encode_frame(obj)
        data = (b"B" if binary else b"T") + wire
        writer.write(struct.pack(">I", len(data)) + data)

    def release(src, proxy, task):
        """Give a proxy back: unwatch, DROP THE INBOX ROW, cancel the drain.

        The `inboxes.pop` is the step that used to be missing. `emit` auto-vivifies
        a row it finds, so a leaked proxy keeps every later payload on `src` being
        copied into a Queue nobody drains — one per (job, watched src), forever."""
        kernel.unwatch(src, proxy)
        kernel.ctx.inboxes.pop(proxy, None)
        task.cancel()

    async def drain(proxy_id, framer):
        q = kernel.ctx.ensure_inbox(proxy_id)
        try:
            while True:
                payload = await q.get()
                frame = framer(payload)
                if frame is not None:
                    push(frame)
                    await writer.drain()
        except asyncio.CancelledError:
            raise
        except Exception:
            pass

    try:
        while True:
            try:
                hdr = await reader.readexactly(4)
                (n,) = struct.unpack(">I", hdr)
                if n > MAX_FRAME:
                    # A WIRE VIOLATION, not a bad payload: the length prefix is the
                    # framing itself, so the stream can no longer be resynchronised
                    # and there is nothing to reply TO. Close, exactly as the ports
                    # do. The connector refuses to write such a frame in the first
                    # place, naming `write_stream`.
                    break
                body = await reader.readexactly(n)
            except (asyncio.IncompleteReadError, ConnectionError, OSError):
                break
            try:
                m = _unframe(body)
            except (UnicodeDecodeError, ValueError, KeyError, IndexError, struct.error):
                continue  # a malformed frame skips the frame, never the connection
            op = m.get("op")
            if op == "send":
                # A target fault settles as a failed outcome (error in result);
                # send never raises into the relay loop.
                data = (
                    await kernel.send(
                        [{"target": m.get("target"), "payload": m.get("payload") or {}}]
                    )
                )[0]["result"]
                push({"op": "reply", "rid": m.get("rid"), "data": data})
                await writer.drain()
            elif op == "emit":
                try:
                    # No `target` → emit on the OWNER agent (one-arg `emit(payload)`).
                    tgt = m.get("target") or owner_id
                    await kernel.emit(tgt, m.get("payload") or {})
                except Exception:
                    pass
            elif op == "watch":
                src = m.get("src")
                if src and src not in subs:
                    proxy = f"_xbw_{job_id}_{src}"
                    # THE KERNEL DECIDES. `watch` is gated exactly like a send (it
                    # IS a read of the source), so it can refuse — and a refusal
                    # discarded here left the program with a subscription that
                    # simply never fired. Tell it, and register NOTHING.
                    denial = kernel.watch(src, proxy)
                    if denial is not None:
                        push({"op": "event", "src": src, "payload": denial})
                        await writer.drain()
                        continue
                    t = asyncio.create_task(
                        drain(
                            proxy,
                            lambda p, s=src: {"op": "event", "src": s, "payload": p},
                        )
                    )
                    subs[src] = (proxy, t)
            elif op == "unwatch":
                ent = subs.pop(m.get("src"), None)
                if ent:
                    release(m.get("src"), *ent)
            elif op == "onmessage":
                if self_sub[0] is None:
                    # Exactly ONE job per owner receives the owner inbox. If another job
                    # for this owner is already subscribed (a re-started durable tool
                    # whose predecessor lingers, or two starts), SUPERSEDE it — else
                    # every inbound `send` is delivered to both and doubled.
                    prev = _owner_inbox.pop(owner_id, None)
                    if prev is not None:
                        release(owner_id, prev["proxy"], prev["task"])
                    proxy = f"_xbself_{job_id}"
                    denial = kernel.watch(owner_id, proxy)
                    if denial is not None:
                        push({"op": "event", "src": owner_id, "payload": denial})
                        await writer.drain()
                        continue
                    t = asyncio.create_task(drain(proxy, _inbox_framer))
                    self_sub[0] = (proxy, t)
                    _owner_inbox[owner_id] = {
                        "job_id": job_id,
                        "proxy": proxy,
                        "task": t,
                    }
            elif op == "offmessage":
                ent = self_sub[0]
                self_sub[0] = None
                if ent:
                    release(owner_id, *ent)
                    cur = _owner_inbox.get(owner_id)
                    if cur and cur["job_id"] == job_id:
                        _owner_inbox.pop(owner_id, None)
    finally:
        for src, ent in subs.items():
            release(src, *ent)
        if self_sub[0]:
            release(owner_id, *self_sub[0])
            # Only if it still names THIS job — a supersede must not evict its
            # replacement (the bug `c2a4297` fixed).
            cur = _owner_inbox.get(owner_id)
            if cur and cur["job_id"] == job_id:
                _owner_inbox.pop(owner_id, None)
        try:
            writer.close()
        except OSError:
            pass


# ─── the command gate: a REGEX allowlist ────────────────────────


def _allowed_patterns(rec: dict) -> list[str]:
    """The record's `allowed` — regexes a command line must match to run."""
    raw = rec.get("allowed")
    if isinstance(raw, str):
        raw = [raw]
    return [str(x) for x in raw] if isinstance(raw, list) else []


def _command_permitted(rec: dict, line: str) -> str | None:
    """None when `line` matches an allowed pattern; else the refusal text.

    The FULL command line (command + args, shell-joined) is matched, anchored —
    `re.fullmatch`, so `echo.*` cannot smuggle `echo; rm -rf /` past a prefix.
    An empty allowlist runs nothing: the leg is inert until an operator states
    what it may run, the same no-defaults posture the interpreter allowlist had.
    """
    import re as _re

    pats = _allowed_patterns(rec)
    if not pats:
        return (
            "exec_bridge: no commands are allowed on this leg — an operator must "
            "set `allowed=[<regex>, ...]` naming what may run"
        )
    for pat in pats:
        try:
            # DOTALL: an argv element may legitimately contain newlines (a
            # `-c` program, a heredoc) and the shell-join preserves them, so
            # `.` must span lines or `.*` silently refuses any multi-line arg.
            if _re.fullmatch(pat, line, _re.DOTALL):
                return None
        except _re.error:
            continue  # a broken pattern permits nothing; validate_meta refuses new ones
    return f"exec_bridge: command not allowed: {line!r} — this leg permits: {pats}"


# ─── verbs ──────────────────────────────────────────────────────


async def _start(id, payload, kernel):
    """args: command:str (req), args:list[str]? (argv, default []), env:dict? (extra
    env vars), cwd:str?. The FULL line (command + args) must fullmatch one of the
    record's `allowed` regexes or the start is refused naming the patterns.
    Spawns in the BACKGROUND and returns {job_id, status:'running'} at once;
    streams `progress` per output line + a final `job_done`. Many run in parallel.

    A GENERAL-PURPOSE runner: it runs COMMANDS. It is not a code runner and not
    an interpreter runner — no `code`, no `interpreters`, no shims. A program is
    a file some process (or stream) put on disk; running it is `command` +
    `args`, exactly like anything else.
    """
    import shlex

    rec = kernel.get(id) or {}
    # Every refusal on this verb carries a CODE. Sibling argument errors used to
    # split — `args`/`env` answered `bad_arg` while `command`/`code`/the policy wall
    # answered a bare `{error}` — so a model could not tell a typo from a policy
    # refusal without reading the prose.
    if payload.get("code") is not None:
        return bad("start", "code", "absent — run a command instead")
    command = payload.get("command")
    if not isinstance(command, str) or not command.strip():
        return missing("start", "command")
    raw_args = payload.get("args", [])
    if not isinstance(raw_args, list) or any(
        not isinstance(a, (str, int, float)) for a in raw_args
    ):
        return bad("start", "args", "a list of strings")
    argv = [command, *[str(a) for a in raw_args]]
    line = shlex.join(argv)
    if why := _command_permitted(rec, line):
        # An AUTHORIZATION denial, not a bad argument: the command line is
        # well-formed and the LEG refused it. Same code the ingress gate answers,
        # so one branch covers "the operator has not opened this".
        return refusal(
            UNAUTHORIZED,
            why,
            hint="the leg's `allowed` policy decides; an operator may widen it",
        )
    cwd = payload.get("cwd") or rec.get("cwd") or None

    env_extra_user = payload.get("env") or {}
    if not isinstance(env_extra_user, dict) or any(
        not isinstance(k, str) or not isinstance(v, (str, int, float))
        for k, v in env_extra_user.items()
    ):
        return bad("start", "env", "a dict of str -> str")

    job_id = secrets.token_hex(4)
    parent_sock, child_sock = socket.socketpair()
    try:
        process = await proc.spawn_argv(
            argv,
            cwd=cwd,
            env_extra={
                **{str(k): str(v) for k, v in env_extra_user.items()},
                "FANTASTIC_CTRL_FD": str(child_sock.fileno()),
                # The OWNER AGENT id (the tool's public id), NOT the ephemeral job id —
                # so a tool can address and tag itself by the id others use.
                "FANTASTIC_SELF_ID": id,
            },
            pass_fd=child_sock.fileno(),
        )
    except Exception as e:
        parent_sock.close()
        return refusal(TRANSPORT_ERROR, f"exec_bridge: spawn failed: {e}")
    finally:
        # The parent's copy of the CHILD end must go, or EOF never fires and a parked
        # program never learns the kernel died.
        child_sock.close()

    _jobs[job_id] = {
        "owner": id,
        "proc": process,
        # The exact invocation, so `status` can report WHAT ran — a caller that
        # wants to re-run a job re-issues these verbatim rather than reconstructing
        # a command line by guesswork.
        "command": command,
        "args": [str(a) for a in raw_args],
        "cwd": cwd,
        "status": "running",
        "started_at": time.time(),
        "exit_code": None,
        "lines": 0,
        "last_line": "",
        "stdout": "",
        "stderr": "",
        "task": None,
        "task_id": None,
        "ctrl_sock": parent_sock,
        "ctrl_task": None,
    }
    _jobs[job_id]["ctrl_task"] = asyncio.create_task(
        _serve_ctrl(parent_sock, id, job_id, kernel)
    )
    task = asyncio.create_task(_pump(id, job_id, process, kernel))
    _jobs[job_id]["task"] = task
    _jobs[job_id]["task_id"] = kernel.ctx.spawn_task(id, "start", task)
    await kernel.emit(id, {"type": "job_started", "job_id": job_id})
    return {"job_id": job_id, "status": "running"}


def _snap(job_id: str, full: bool = False) -> dict:
    j = _jobs[job_id]
    s = {
        "job_id": job_id,
        # The job's row in the kernel TASK TABLE — `cancel task_id=…` kills it (and
        # reaps the OS process); `ps` lists it.
        "task_id": j["task_id"],
        "status": j["status"],
        "exit_code": j["exit_code"],
        "started_at": j["started_at"],
        "lines": j["lines"],
        "last_line": j["last_line"],
        # WHAT ran — so a caller can re-issue this job verbatim.
        "command": j.get("command"),
        "args": j.get("args"),
    }
    if full:
        s["stdout"] = j["stdout"]
        s["stderr"] = j["stderr"]
        s["cwd"] = j.get("cwd")
    return s


async def _status(id, payload, kernel):
    """args: job_id:str? — one job (with full stdout/stderr), or all of this agent's
    jobs (summary). Returns {jobs:[…]}."""
    jid = payload.get("job_id")
    if jid:
        j = _jobs.get(jid)
        if j is None or j.get("owner") != id:
            return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
        return {"jobs": [_snap(jid, full=True)]}
    return {"jobs": [_snap(k) for k, v in _jobs.items() if v.get("owner") == id]}


def _no_job(id: str, jid) -> dict | None:
    """The `not_found` refusal for a named job this agent does not own, or None.

    A job_id naming nothing used to answer `{killed:0}` / `{interrupted:0}` — the
    same reply as "it had already exited", so a typo read as success."""
    if jid and (jid not in _jobs or _jobs[jid].get("owner") != id):
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    return None


async def _stop(id, payload, kernel):
    """args: job_id:str? — SIGKILL one job or ALL of this agent's running jobs.
    Returns {killed:int}."""
    jid = payload.get("job_id")
    if gone := _no_job(id, jid):
        return gone
    n = 0
    for k, v in _jobs.items():
        if v.get("owner") != id or (jid and k != jid):
            continue
        if _alive(v):
            v["status"] = "stopping"
            try:
                v["proc"].kill()
            except (ProcessLookupError, OSError):
                pass
            n += 1
    return {"killed": n}


async def _interrupt(id, payload, kernel):
    """args: job_id:str? — SIGINT one job or ALL of this agent's running jobs.
    Returns {interrupted:int}."""
    jid = payload.get("job_id")
    if gone := _no_job(id, jid):
        return gone
    n = 0
    for k, v in _jobs.items():
        if v.get("owner") != id or (jid and k != jid):
            continue
        if _alive(v):
            try:
                v["proc"].send_signal(signal_mod.SIGINT)
            except (ProcessLookupError, OSError):
                pass
            n += 1
    return {"interrupted": n}


async def _clear(id, payload, kernel):
    """args: job_id:str? — drop finished jobs (one, or all non-running) from the RAM
    table. Returns {cleared:int}."""
    jid = payload.get("job_id")
    # The one caller that WANTS the status label, not process liveness: it is deciding
    # what is finished enough to forget.
    targets = [
        k
        for k, v in _jobs.items()
        if v.get("owner") == id
        and (k == jid if jid else v["status"] not in ("running", "stopping"))
    ]
    for k in targets:
        _jobs.pop(k, None)
    return {"cleared": len(targets)}


async def _reflect(id, payload, kernel):
    """Identity + running job count + auth posture. No args."""
    rec = kernel.get(id) or {}
    running = sum(1 for v in _jobs.values() if v["owner"] == id and _alive(v))
    return {
        "id": id,
        "sentence": (
            "Process edge of the io family — a general-purpose COMMAND runner, "
            "sealed by default, gated by the `allowed` regexes. Spawns commands in "
            "the background; a child may speak the kernel over a private control fd."
        ),
        "cwd": rec.get("cwd") or "<process default>",
        # THE GATE, advertised: a caller must see what it may run.
        "allowed": _allowed_patterns(rec),
        "running": running,
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "job_started": "{type:'job_started', job_id}",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout, stderr}",
        },
        "wire": (
            "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, "
            "a BIDIRECTIONAL socket (one end of a socketpair): frame is "
            "[4-byte BE length | kind | rest], both directions — kind T: a JSON "
            "envelope; kind B: [4-byte BE H | H-byte JSON header | raw body], the "
            "header's `_binary_path` naming where the body belongs (a stream chunk "
            "rides raw, never base64). To CALL another agent: "
            "write {op:'send', rid:<your id>, target, payload} and the reply COMES "
            "BACK on the SAME fd as {op:'reply', rid:<same id>, data} — the fd is "
            "not one-way. It is not inline, though: read frames in a loop and skip "
            "any whose rid isn't yours (event/inbox frames can arrive first) until "
            "you see your reply. ops: send/emit/reflect/watch/on_message. A command "
            "that never reads the fd runs exactly like any OS process."
        ),
        "inbox": (
            "Any verb NOT listed in `verbs` is MAIL for the running program, delivered "
            "to its on_message and ACKed {queued:true}. With no live job it is an "
            "error — a script that only defines something exits before its first turn "
            "arrives."
        ),
        "tasks": (
            "Every running job holds a row in the kernel TASK TABLE (`task_id` in each "
            "`status` snapshot; listed by the universal `ps`). `cancel task_id=<id>` "
            "kills it — the OS subprocess is SIGKILLed by this bundle's `on_cancel` "
            "hook before the kernel aborts the task, so nothing is orphaned."
        ),
        "authoring": (
            "A general-purpose command runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by "
            "the `allowed` regexes on the full command line."
        ),
        **_describe(rec),
    }


async def _boot(id, payload, kernel):
    """No-op. Nothing auto-restarts: a command is started by a caller, not by the
    record — the `code`-field auto-run died with the code field."""
    return None


async def _connector(id, payload, kernel):
    """no args — the reference wire client SOURCE (python): paste `source` verbatim at the top of your program and it gets a `kernel` global (send/emit/watch/on_message); returns {source, howto}."""
    from ._connector import CONNECTOR_SRC

    return {
        "source": CONNECTOR_SRC,
        "howto": (
            "Write `source` verbatim as the first lines of your program file, then "
            "your code below it. It opens FANTASTIC_CTRL_FD (a socket, length-prefixed "
            "frames; a payload carrying a bytes value rides the binary channel raw: "
            "`write_stream` puts its chunk in `bytes`, `read_stream` passes `bytes: b''` "
            "and gets its chunk back raw in the reply's `bytes`) and "
            "exposes `kernel.send(target, payload) -> reply` (raises if the relay could "
            "not deliver), "
            "`kernel.emit(payload)`, `kernel.watch(src, cb)`, `kernel.on_message(cb)`, "
            "`kernel.self_id`. Arguments arrive as ARGV, like any command — there is "
            "no `kernel.args`. One frame caps at 16 MiB; move more than that with "
            "`write_stream` chunks. Your sends carry THIS runner's identity "
            "and grants (its `can_call`; a target must list this runner in "
            "`callable_by`). Do not rewrite the wire by hand."
        ),
    }


VERBS = {
    "start": _start,
    "status": _status,
    "stop": _stop,
    "interrupt": _interrupt,
    "clear": _clear,
    "connector": _connector,
    "reflect": _reflect,
    "boot": _boot,
}

UNGATED = {"reflect", "connector"}  # discovery must work on a sealed bridge


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this bundle cannot honour, at the WRITE, next to the typo.

    Validates the MERGED state — `record` (empty at create) with `meta` applied —
    and names the legal set in every refusal.
    """
    merged = {**record, **meta}
    del merged  # kept for symmetry with other bundles; nothing merged-scoped left
    if "ingress_rule" in meta:
        spec = meta["ingress_rule"]
        if getattr(resolve_ingress({"ingress_rule": spec}), "unknown", None):
            return (
                f"ingress_rule={spec!r} is not a rule this bundle implements — "
                "legal values: allow_all_ingress, deny_ingress"
            )
    # `password_ingress` is meaningless on an in-process leg: the token is read
    # off a WIRE ENVELOPE and the process edge has none — no frame ever presents
    # one, so the rule could only refuse, or (the ports, until 2026-09-02) be
    # satisfied by a secret persisted ON THE RECORD. Refused at the write.
    # Same rule as file_bridge's `_password_refusal`.
    for field, stems in (
        ("ingress_rule", ("password_ingress",)),
        ("auth", ("password",)),
    ):
        if field in meta:
            name, _ = parse_spec(meta[field])
            if name in stems:
                return (
                    f"{field}={meta[field]!r} is not applicable to the process edge: "
                    "a leg with no wire envelope can never present a token — "
                    "legal values: allow_all_ingress, deny_ingress"
                )
    for gone in ("code", "shim", "interpreter", "interpreters", "argv_template"):
        if gone in meta and meta[gone] is not None:
            return (
                f"exec_bridge: `{gone}` is not a field — this is a general-purpose "
                "COMMAND runner; gate it with `allowed=[<regex>, ...]`"
            )
    if "allowed" in meta and meta["allowed"] is not None:
        import re as _re

        raw = meta["allowed"]
        if isinstance(raw, str):
            raw = [raw]
        if not isinstance(raw, list) or not raw:
            return "allowed must be a non-empty list of regexes, e.g. <allowed><item>ls( .*)?</item></allowed>"
        for pat in raw:
            try:
                _re.compile(str(pat))
            except _re.error as e:
                return f"allowed contains a broken regex {pat!r}: {e}"
    return None


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        # Addressable inbox: any verb this bundle doesn't own is delivered to the running
        # program. The delivery is the substrate's OWN dispatch fanout (`_dispatch`
        # mirrors every payload to the agent's watchers — the on_message proxy is one),
        # so we do NOT re-emit here; that would double-deliver. We only ACK.
        if _has_running_job(id):
            return {"queued": True}
        return {
            "error": f"exec_bridge: unknown type {t!r} (no running job to receive it)"
        }
    if t not in UNGATED:
        # AUTH GATE — the process edge, gated like any io_bridge leg (sealed by default).
        # `kind="spawn"` because that is what is being authorized; no rule inspects
        # `Action.kind`, so a new kind costs nothing and the audit trail stays honest.
        decision = gate_inbound(
            resolve_ingress(kernel.get(id) or {}),
            "spawn",
            {"target": id, "payload": payload},
        )
        if not decision.allowed:
            out = {"error": decision.reason, "reason": "unauthorized"}
            if decision.hint:
                out["hint"] = decision.hint
            if decision.see:
                out["see"] = decision.see
            return out
    return await fn(id, payload, kernel)


def _teardown(job: dict) -> None:
    """Free the REAL resources of ONE job: SIGKILL the OS subprocess + cancel its relay.
    The single teardown both lifecycle hooks use. Idempotent."""
    if _alive(job):
        job["status"] = "stopping"
        try:
            job["proc"].kill()
        except (ProcessLookupError, OSError):
            pass
    t = job.get("ctrl_task")
    if t is not None:
        t.cancel()


async def on_delete(agent) -> None:
    """Cascade teardown: SIGKILL this agent's still-running jobs + cancel their relays."""
    for v in _jobs.values():
        if v.get("owner") == agent.id:
            _teardown(v)


async def on_cancel(id, task_id, kernel) -> None:
    """Cancel hook (substrate-invoked): the task being cancelled is a JOB of ours —
    SIGKILL its OS subprocess BEFORE the kernel aborts the pump task.

    This is the whole reason `cancel` has a hook. Aborting the pump's asyncio.Task only
    unwinds coroutines; the child would keep running, unreachable and unowned."""
    for v in _jobs.values():
        if v.get("owner") == id and v.get("task_id") == task_id:
            _teardown(v)
