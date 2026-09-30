"""exec_bridge bundle — the PROCESS-transport member of the io family.

A subprocess is OUTSIDE the trust domain: crossing kernel↔process is an IO edge, gated
exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒ every verb except
`reflect` denies with the teaching shape). Open it consciously with
`ingress_rule=allow_all_ingress`. Even open, the LAW is the ALLOWLIST: only a command line
the operator allowed by regex can be spawned, and that choice is not overridable per
call. The spawn primitive lives in `proc` — the ONE process surface. This agent is the
GATE + verb shape over it; it never spawns directly. It knows no language: the job
table, the pump, the relay, the fd passing, the task-table integration and the mail
contract are the same for every command.

## The wire is the contract

A spawned command MAY speak the control fd (see `proc`); one that never reads it
runs exactly like any OS process.

## Verbs

  start · status · stop · interrupt · clear · reflect · boot
  PTY jobs (`start pty={cols,rows}`): write · resize · ack · output · paste_image

A terminal is a PTY JOB of this bundle — the same gate, the same `proc`, the same
task table. Every spawn parameter comes from the RECORD, which only the operator
writes: `allowed`, `cwd` (required), `env` (extras), `inherit_env` (which kernel env
vars pass through; nothing by default, `["*"]` = all), `shell` (which line of the
child's own `$SHELL` a PTY start may run without an `allowed` entry: true = bare,
"login" = `-l`). A PTY `start` with no `command` runs that `$SHELL`, read from the
child's environment — autodiscovery, never a default.

Any verb this bundle does not own is MAIL for the running program — see `inbox` in
`reflect`.

## What is NOT claimed

An allowlisted command is a NAMED command, not a sandbox — see `proc`. The allowlist
means the operator chose which command and which environment, and that a program
cannot choose a different one.
"""

from __future__ import annotations

import asyncio
import codecs
import collections
import fcntl
import json
import os
import secrets
import signal as signal_mod
import socket
import struct
import tempfile
import termios
import time

from exec_bridge import proc
from file_bridge import fs
from io_bridge import decode_frame, describe as _describe, encode_frame
from io_bridge import gate_inbound, resolve_ingress
from io_bridge._base import parse_spec
from kernel._refusal import (
    MISSING_ARG,
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

# job_id (the job's kernel TASK id, an int) -> {owner, proc, status, started_at,
#            exit_code, lines, last_line, stdout, stderr, task, ctrl_sock, ctrl_task}.
# status: running|stopping|done|failed|stopped. Module-global (shared across
# exec_bridge agents); every op filters by owner.
_jobs: dict[int, dict] = {}

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


async def _pump(owner, ready, process, kernel):
    """Drain stdout+stderr concurrently, then await exit + emit job_done.

    THE PUMP IS THE JOB'S TASK — registered in the kernel task table by `_start`, so `ps`
    lists the job and `cancel` can abort it. Aborted (the SIGKILL half of cancel, after
    `on_cancel` has already killed the OS process), it finalizes the RAM row to `stopped`
    on its way out — no row is left stuck `running`. `ready` resolves to the job's id
    (this task's own id) once `_start` has filed the row."""
    job_id = await ready
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
        "output",
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

        The `inboxes.pop` is the step that used to be missing: a leaked proxy
        keeps every later payload on `src` being copied into a Queue nobody
        drains — one per (job, watched src), forever."""
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
                    # A program emits on its OWNER agent — the one it runs as.
                    await kernel.emit(owner_id, m.get("payload") or {})
                except Exception:
                    pass
            elif op == "watch":
                src = m.get("src")
                if src and src not in subs:
                    proxy = f"_xbw_{job_id}_{src}"
                    # REGISTER THE READER FIRST. `watch` refuses a watcher
                    # with no inbox, and the drain task below is what reads
                    # this one — so claim the queue before asking to be
                    # watched, not after.
                    kernel.ctx.ensure_inbox(proxy)
                    # THE KERNEL DECIDES. `watch` is gated exactly like a send (it
                    # IS a read of the source), so it can refuse — and a refusal
                    # discarded here left the program with a subscription that
                    # simply never fired. Tell it, and register NOTHING.
                    denial = kernel.watch(src, proxy)
                    if denial is not None:
                        # REGISTER NOTHING on a refusal — the queue we claimed a
                        # line ago has no watcher feeding it and no drain task
                        # reading it, so it would sit in `ctx.inboxes` forever.
                        kernel.ctx.inboxes.pop(proxy, None)
                        push({"op": "event", "src": src, "payload": denial})
                        await writer.drain()
                        continue
                    # REGISTERED: a drain lives as long as the subscription.
                    t = kernel.ctx.tasks[
                        kernel.ctx.spawn_task(
                            owner_id,
                            "watch_drain",
                            drain(
                                proxy,
                                lambda p, s=src: {
                                    "op": "event",
                                    "src": s,
                                    "payload": p,
                                },
                            ),
                        )
                    ]["_task"]
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
                    kernel.ctx.ensure_inbox(proxy)
                    denial = kernel.watch(owner_id, proxy)
                    if denial is not None:
                        kernel.ctx.inboxes.pop(proxy, None)
                        push({"op": "event", "src": owner_id, "payload": denial})
                        await writer.drain()
                        continue
                    t = kernel.ctx.tasks[
                        kernel.ctx.spawn_task(
                            owner_id, "inbox_drain", drain(proxy, _inbox_framer)
                        )
                    ]["_task"]
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


# ─── PTY jobs — a terminal is a job ─────────────────────────────
# terminal_backend's robustness, ported onto the job table: incremental UTF-8
# decode (a multi-byte char split across reads is not `<?>` litter), writes in FULL
# and serialized per job (a bracketed paste is never truncated or interleaved), a
# bounded scrollback RING for re-attach, and VSCode's flow control (the reader pauses
# past HIGH_WATERMARK unacked chars; a streamer's `ack` resumes it under LOW).

PTY_RING = 256 * 1024
HIGH_WATERMARK = 100_000
LOW_WATERMARK = 5_000
# Image paste — the formats a CLI like Claude Code accepts, and its size cap. A
# browser xterm cannot hand a server-side program the browser clipboard, so the
# image is saved and its path typed (a file drag-drop).
_IMAGE_EXT = {
    "image/png": "png",
    "image/jpeg": "jpg",
    "image/gif": "gif",
    "image/webp": "webp",
}
MAX_PASTE_IMAGE = 5 * 1024 * 1024


def _set_winsize(fd: int, cols: int, rows: int) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def _ring_add(pt: dict, text: str) -> None:
    pt["ring"].append(text)
    pt["ring_bytes"] += len(text.encode("utf-8", errors="replace"))
    while pt["ring_bytes"] > PTY_RING and pt["ring"]:
        old = pt["ring"].popleft()
        pt["ring_bytes"] -= len(old.encode("utf-8", errors="replace"))


def _pty_readable(owner: str, job_id: int, kernel) -> None:
    """The master fd is readable: one chunk out as an `output` event. EOF (or EIO —
    the slave closed with the child) wakes the pump; a backlog past HIGH_WATERMARK
    detaches the reader so the tty fills and the program throttles."""
    job = _jobs.get(job_id)
    if job is None:
        return
    pt = job["pty"]
    try:
        data = os.read(pt["fd"], 4096)
    except BlockingIOError:
        return
    except OSError:
        data = b""
    if not data:
        asyncio.get_running_loop().remove_reader(pt["fd"])
        pt["eof"] = True
        pt["wake"].set()
        return
    text = pt["decoder"].decode(data)
    if not text:
        return
    _ring_add(pt, text)
    asyncio.ensure_future(
        kernel.emit(owner, {"type": "output", "job_id": job_id, "data": text})
    )
    pt["unacked"] += len(text)
    if pt["unacked"] > HIGH_WATERMARK:
        asyncio.get_running_loop().remove_reader(pt["fd"])
        pt["paused"] = True
        pt["wake"].set()


async def _pty_pump(owner, ready, process, kernel):
    """THE JOB'S TASK for a PTY job. It owns the reader — attaching it HERE, never in
    a verb's dispatch, keeps every `output` emit in the job's own context — re-attaches
    it when an `ack` drains the backlog, and ends at EOF with `job_done`. `ready`
    resolves to the job's id (this task's own id) once `_start` has filed the row."""
    job_id = await ready
    job = _jobs.get(job_id)
    if job is None:
        return
    pt = job["pty"]
    loop = asyncio.get_running_loop()
    try:
        try:
            while not pt["eof"]:
                pt["wake"].clear()
                if not pt["paused"]:
                    loop.add_reader(pt["fd"], _pty_readable, owner, job_id, kernel)
                await pt["wake"].wait()
            code = await process.wait()
        except asyncio.CancelledError:
            job["status"] = "stopped"
            job["exit_code"] = process.returncode
            raise
    finally:
        try:
            loop.remove_reader(pt["fd"])
        except (ValueError, OSError):
            pass
        try:
            os.close(pt["fd"])
        except OSError:
            pass
        if pt.get("paste_dir"):
            try:
                fs.remove(pt["paste_dir"], "", recursive=True, external=True)
            except (OSError, ValueError):
                pass
    job["exit_code"] = code
    job["status"] = (
        "stopped"
        if job["status"] == "stopping"
        else ("done" if code == 0 else "failed")
    )
    await kernel.emit(
        owner,
        {"type": "job_done", "job_id": job_id, "exit_code": code, "ok": code == 0},
    )


async def _write_all(fd: int, buf: bytes) -> int:
    """Write the FULL buffer to the non-blocking master fd. `os.write` short-writes
    and raises `BlockingIOError` when the tty buffer is full; one unchecked write
    drops the tail — a bracketed paste then loses its end marker and the shell
    sticks in paste mode. Loop until every byte lands."""
    loop = asyncio.get_running_loop()
    written = 0
    while written < len(buf):
        try:
            written += os.write(fd, buf[written:])
        except BlockingIOError:
            ev = asyncio.Event()
            loop.add_writer(fd, ev.set)
            try:
                await ev.wait()
            finally:
                loop.remove_writer(fd)
    return written


def _job_arg(verb: str, payload: dict, *, required: bool):
    """The `job_id` argument: `(id, None)`, `(None, None)` when absent and optional,
    or `(None, refusal)`.

    A job's id IS its kernel TASK id — an int. Anything else is refused as a bad
    argument naming that, never looked up: a string like "42" is not coerced, so a
    caller that stringified an id learns so here instead of reading `no job`."""
    jid = payload.get("job_id")
    if jid is None:
        return None, (missing(verb, "job_id") if required else None)
    if isinstance(jid, bool) or not isinstance(jid, int):
        return None, bad(
            verb,
            "job_id",
            "an int — the job's task id (from `start`, `status` or `ps`)",
        )
    return jid, None


def _pty_job(verb: str, id: str, payload: dict):
    """`(job, None)` for a live PTY job this agent owns, or `(None, refusal)`."""
    jid, err = _job_arg(verb, payload, required=True)
    if err:
        return None, err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return None, refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    if job.get("pty") is None:
        return None, bad(verb, "job_id", "a PTY job (started with `pty`)")
    if not _alive(job) or job["pty"]["eof"]:
        return None, refusal(
            NOT_FOUND, f"exec_bridge: job {jid!r} has ended", job_id=jid
        )
    return job, None


def _dims(verb: str, raw) -> tuple[tuple[int, int] | None, dict | None]:
    """`((cols, rows), None)` or `(None, refusal)` — both required, positive."""
    if not isinstance(raw, dict):
        return None, bad(verb, "pty", "an object {cols, rows}")
    cols, rows = raw.get("cols"), raw.get("rows")
    for name, v in (("cols", cols), ("rows", rows)):
        if isinstance(v, bool) or not isinstance(v, int) or v < 1:
            return None, bad(verb, name, "a positive integer")
    return (cols, rows), None


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
    what it may run.
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


# ─── the child's own shell ──────────────────────────────────────
#
# A PTY `start` that names no `command` runs the CHILD's `$SHELL`: a client with
# no environment of its own (a browser page) cannot name the shell, but the
# record can — `inherit_env`/`env` decide what the child's environment holds,
# and `SHELL` is read from THAT, never from the kernel's own env unless it was
# passed down. One read, no chain: a child env without `SHELL` is refused loud.
#
# The ALLOWLIST still decides what may run. A record writer who does not know
# the shell's path cannot write an exact `allowed` line for it, so the record's
# `shell` field permits the resolved shell line itself — and nothing wider:
#
#   shell: true     → exactly [$SHELL]        (the bare shell)
#   shell: "login"  → exactly [$SHELL, "-l"]  (a login shell)
#
# It is a LINE, not a pattern: any other argv, and every pipe job, still has to
# fullmatch `allowed`. Who may set `shell` is who may write the record — the
# same authority that writes `allowed`.

SHELL_VAR = "SHELL"


def _shell_tail(rec: dict) -> list[str] | None:
    """The argv the record's `shell` permits AFTER `$SHELL`, or None when the
    record permits no shell line. `True` is compared by identity so a stray
    `1` never reads as `true`."""
    v = rec.get("shell")
    if v is True:
        return []
    if v == "login":
        return ["-l"]
    return None


def _shell_permits(rec: dict, env: dict, argv: list[str], pty: bool) -> bool:
    """True when `argv` is exactly the shell line the record's `shell` permits:
    a PTY job whose argv is the child's own `$SHELL` plus the tail its `shell`
    value names. Judged on the RESOLVED argv, so it holds whether the caller
    named the shell or left `command` out."""
    tail = _shell_tail(rec)
    shell = env.get(SHELL_VAR)
    return pty and tail is not None and bool(shell) and argv == [shell, *tail]


# ─── verbs ──────────────────────────────────────────────────────


def _child_env(rec: dict) -> dict:
    """The child's WHOLE environment, from the record: the kernel env vars named in
    `inherit_env` (nothing by default; `["*"]` = every one — the operator's wildcard),
    then the operator's `env` extras. Nothing else leaks in — the kernel's `.env` keys
    reach a child only when the operator names them."""
    names = rec.get("inherit_env") or []
    if "*" in names:
        env = dict(os.environ)
    else:
        env = {n: os.environ[n] for n in names if n in os.environ}
    env.update({str(k): str(v) for k, v in (rec.get("env") or {}).items()})
    return env


async def _start(id, payload, kernel):
    """args: command:str (req — on a PTY start, absent = the child's $SHELL), args:list[str]? (argv, default []), pty:{cols,rows}? (run on a terminal). The FULL line (command + args) must fullmatch one of the `allowed` regexes, or be the shell line the record's `shell` permits.
    The FULL line (command + args) must fullmatch one of the
    record's `allowed` regexes or the start is refused naming the patterns.
    A PTY start with no `command` runs the CHILD's own `$SHELL` (read from the
    environment `env` + `inherit_env` give the child; none → refused naming
    SHELL), and the record's `shell` (true → bare, "login" → `-l`) permits that
    exact line without an `allowed` entry. A pipe job always names its command.
    Everything else comes from the RECORD, never the call: `cwd` (required),
    `env` + `inherit_env` (the child's environment). Spawns in the BACKGROUND and
    returns {job_id, status:'running', pty} at once. A pipe job streams `progress`
    per output line + a final `job_done`; a PTY job streams `output` chunks and is
    driven with write/resize/ack/output/paste_image. Many run in parallel.

    A GENERAL-PURPOSE runner: it runs COMMANDS. A program is a file some process
    (or stream) put on disk; running it is `command` + `args`, exactly like
    anything else.
    """
    import shlex

    rec = kernel.get(id) or {}
    # Every refusal on this verb carries a CODE, so a model can tell a typo from a
    # policy refusal without reading the prose.
    # WHERE and WITH WHAT a command runs are the OPERATOR's: a per-call `env`
    # (`LD_PRELOAD`, `PATH`, …) or `cwd` changed what an allowlisted line actually
    # ran (audit 2026-09-28).
    for gone in ("env", "cwd"):
        if payload.get(gone) is not None:
            return bad(
                "start",
                gone,
                f"absent — `{gone}` is the record's (update_agent {id} {gone}=…)",
            )
    raw_args = payload.get("args", [])
    if not isinstance(raw_args, list) or any(
        not isinstance(a, (str, int, float)) for a in raw_args
    ):
        return bad("start", "args", "a list of strings")
    dims = None
    if payload.get("pty") is not None:
        dims, err = _dims("start", payload["pty"])
        if err:
            return err
    # The child's WHOLE environment, from the record — built before the command
    # is resolved, because an absent `command` on a PTY start is read from it.
    env = _child_env(rec)
    command = payload.get("command")
    if command is None and dims is not None:
        # AUTODISCOVERY, not a default: the shell is whatever the record passed
        # down to the child as SHELL. No chain (zsh → bash → sh) behind it — a
        # child env without one is refused, naming where it comes from.
        command = env.get(SHELL_VAR)
        if not command:
            return refusal(
                MISSING_ARG,
                "exec_bridge: start named no command and the child's environment "
                f"has no {SHELL_VAR} to run — a PTY start without `command` runs "
                "the shell the record passes down, never a guessed one",
                hint=f'update_agent {id} inherit_env=["{SHELL_VAR}"] '
                f'(or env={{"{SHELL_VAR}": …}}), or name `command`',
                arg=SHELL_VAR,
            )
    if not isinstance(command, str) or not command.strip():
        # A pipe job always names its command; so does a PTY start that passed
        # one explicitly (an empty string is a typo, not "use the shell").
        return missing("start", "command")
    argv = [command, *[str(a) for a in raw_args]]
    line = shlex.join(argv)
    # Two ways a line is permitted, both written on the RECORD: the shell line
    # its `shell` field names, or a fullmatch against `allowed`. Checked in that
    # order only because the first is an exact comparison; neither widens the
    # other.
    if not _shell_permits(rec, env, argv, dims is not None) and (
        why := _command_permitted(rec, line)
    ):
        # An AUTHORIZATION denial, not a bad argument: the command line is
        # well-formed and the LEG refused it. Same code the ingress gate answers,
        # so one branch covers "the operator has not opened this".
        if _shell_tail(rec) is not None:
            # Say what `shell` DOES permit, so a caller that passed extra args
            # (or a pipe job) learns why the shell permission did not cover it.
            # The value is JSON (`true`/`"login"`), the wire's own spelling, so
            # every runtime prints the same bytes.
            why += (
                f" (`shell={json.dumps(rec.get('shell'))}` permits only the PTY line "
                f"{shlex.join([env.get(SHELL_VAR) or '$' + SHELL_VAR, *_shell_tail(rec)])!r})"
            )
        return refusal(
            UNAUTHORIZED,
            why,
            hint="the leg's `allowed` policy decides; an operator may widen it",
        )
    cwd = rec.get("cwd")
    if not isinstance(cwd, str) or not cwd:
        return refusal(
            MISSING_ARG,
            "exec_bridge: this leg names no `cwd` — where a command runs is the "
            "operator's decision, never guessed",
            hint=f"update_agent {id} cwd=<dir>",
            arg="cwd",
        )

    parent_sock, child_sock = socket.socketpair()
    env["FANTASTIC_CTRL_FD"] = str(child_sock.fileno())
    # The OWNER AGENT id (the tool's public id), NOT the ephemeral job id — so a
    # tool can address and tag itself by the id others use.
    env["FANTASTIC_SELF_ID"] = id
    master = slave = None
    try:
        if dims is None:
            process = await proc.spawn_argv(
                argv, cwd=cwd, env=env, pass_fd=child_sock.fileno()
            )
        else:
            master, slave = os.openpty()
            _set_winsize(master, *dims)
            env["TERM"] = "xterm-256color"
            process = await proc.spawn_pty(
                argv, cwd=cwd, env=env, pass_fd=child_sock.fileno(), slave=slave
            )
            os.set_blocking(master, False)
    except proc.NoPath:
        parent_sock.close()
        if master is not None:
            os.close(master)
        return refusal(
            MISSING_ARG,
            f"exec_bridge: {command!r} is a bare command and this leg gives the "
            "child no PATH to find it in — the kernel's own PATH is not the child's",
            hint=f'update_agent {id} inherit_env=["PATH"] (or env={{"PATH": …}}), '
            "or start an absolute path",
            arg="PATH",
        )
    except Exception as e:
        parent_sock.close()
        if master is not None:
            os.close(master)
        return refusal(TRANSPORT_ERROR, f"exec_bridge: spawn failed: {e}")
    finally:
        # The parent's copy of the CHILD end must go, or EOF never fires and a parked
        # program never learns the kernel died. Same for the PTY slave: the master
        # sees EOF only once every slave copy is closed.
        child_sock.close()
        if slave is not None:
            os.close(slave)

    job = {
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
        "ctrl_sock": parent_sock,
        "ctrl_task": None,
        "pty": None
        if dims is None
        else {
            "fd": master,
            "cols": dims[0],
            "rows": dims[1],
            "ring": collections.deque(),
            "ring_bytes": 0,
            "decoder": codecs.getincrementaldecoder("utf-8")(errors="replace"),
            "unacked": 0,
            "paused": False,
            "eof": False,
            "wake": asyncio.Event(),
            "write_lock": asyncio.Lock(),
            "paste_dir": None,
        },
    }
    # THE PUMP IS THE JOB'S TASK, and the job's id IS that task's id — minted
    # by the kernel's one counter when the pump is forked. One number names the
    # job everywhere: `status`/`stop`/`write` take it as `job_id`, `ps` lists it,
    # `cancel task_id=` kills it. (It used to be a random 8-hex string, which a
    # model's `<job_id>04203581</job_id>` or `458e5912` coerced into an int or
    # an infinite float — 3.7% of jobs could not be addressed again.)
    #
    # The id exists only once the fork does, so the pump learns it through
    # `ready`, resolved the moment the job's row is in `_jobs` — never assuming
    # the fork has not started yet.
    ready: asyncio.Future[int] = asyncio.get_running_loop().create_future()
    pump = (
        _pump(id, ready, process, kernel)
        if dims is None
        else _pty_pump(id, ready, process, kernel)
    )
    job_id = kernel.ctx.spawn_task(id, "start", pump)
    job["task"] = kernel.ctx.tasks[job_id]["_task"]
    _jobs[job_id] = job
    ready.set_result(job_id)
    # The relay is the job's too: a program's `kernel.send` must be rowed under
    # the job, not under the `start` dispatch that settles the moment we return.
    job["ctrl_task"] = kernel.ctx.tasks[
        kernel.ctx.spawn_task(id, "relay", _serve_ctrl(parent_sock, id, job_id, kernel))
    ]["_task"]
    # The RESOLVED invocation rides the announcement: a client that let the
    # kernel pick the shell learns which one ran, and can re-issue it verbatim.
    await kernel.emit(
        id,
        {
            "type": "job_started",
            "job_id": job_id,
            "command": command,
            "args": [str(a) for a in raw_args],
        },
    )
    return {"job_id": job_id, "status": "running", "pty": dims is not None}


def _snap(job_id: int, full: bool = False) -> dict:
    j = _jobs[job_id]
    s = {
        "job_id": job_id,
        "status": j["status"],
        "exit_code": j["exit_code"],
        "started_at": j["started_at"],
        "lines": j["lines"],
        "last_line": j["last_line"],
        # WHAT ran — so a caller can re-issue this job verbatim.
        "command": j.get("command"),
        "args": j.get("args"),
        "pty": j.get("pty") is not None,
    }
    if j.get("pty") is not None:
        s["cols"], s["rows"] = j["pty"]["cols"], j["pty"]["rows"]
    if full:
        if j.get("pty") is None:
            s["stdout"] = j["stdout"]
            s["stderr"] = j["stderr"]
        s["cwd"] = j.get("cwd")
    return s


async def _status(id, payload, kernel):
    """args: job_id:int? — one job (with full stdout/stderr), or all of this agent's jobs (summary). A job's id is its kernel task id: `ps` lists it, `cancel task_id=` kills it. Returns {jobs:[…]}."""
    jid, err = _job_arg("status", payload, required=False)
    if err:
        return err
    if jid is not None:
        j = _jobs.get(jid)
        if j is None or j.get("owner") != id:
            return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
        return {
            "jobs": [_snap(jid, full=True)],
            "origin": f"{id} status {{job_id: {jid}}} — a live job; re-send for its output NOW, which may differ",
        }
    return {
        "jobs": [_snap(k) for k, v in _jobs.items() if v.get("owner") == id],
        "origin": f"{id} status — the job table NOW; re-send, it changes",
    }


def _no_job(id: str, jid) -> dict | None:
    """The `not_found` refusal for a named job this agent does not own, or None.

    A job_id naming nothing used to answer `{killed:0}` / `{interrupted:0}` — the
    same reply as "it had already exited", so a typo read as success."""
    if jid is not None and (jid not in _jobs or _jobs[jid].get("owner") != id):
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    return None


async def _stop(id, payload, kernel):
    """args: job_id:int? — SIGKILL one job (its whole session: the command and everything it launched) or ALL of this agent's running jobs.
    Returns {killed:int}."""
    jid, err = _job_arg("stop", payload, required=False)
    if err:
        return err
    if gone := _no_job(id, jid):
        return gone
    n = 0
    for k, v in _jobs.items():
        if v.get("owner") != id or (jid is not None and k != jid):
            continue
        if _alive(v):
            v["status"] = "stopping"
            _kill(v)
            n += 1
    return {"killed": n}


async def _interrupt(id, payload, kernel):
    """args: job_id:int? — SIGINT one job or ALL of this agent's running jobs.
    Returns {interrupted:int}."""
    jid, err = _job_arg("interrupt", payload, required=False)
    if err:
        return err
    if gone := _no_job(id, jid):
        return gone
    n = 0
    for k, v in _jobs.items():
        if v.get("owner") != id or (jid is not None and k != jid):
            continue
        if _alive(v):
            try:
                v["proc"].send_signal(signal_mod.SIGINT)
            except (ProcessLookupError, OSError):
                pass
            n += 1
    return {"interrupted": n}


async def _clear(id, payload, kernel):
    """args: job_id:int? — drop finished jobs (one, or all non-running) from the RAM
    table. Returns {cleared:int}."""
    jid, err = _job_arg("clear", payload, required=False)
    if err:
        return err
    # The one caller that WANTS the status label, not process liveness: it is deciding
    # what is finished enough to forget.
    targets = [
        k
        for k, v in _jobs.items()
        if v.get("owner") == id
        and (
            k == jid if jid is not None else v["status"] not in ("running", "stopping")
        )
    ]
    for k in targets:
        _jobs.pop(k, None)
    return {"cleared": len(targets)}


async def _write(id, payload, kernel):
    """args: job_id:int (req), data:str (req). Keystrokes/paste into a PTY job, written IN FULL and serialized per job (a bracketed paste is never truncated or interleaved). Returns {written:int}."""
    job, err = _pty_job("write", id, payload)
    if err:
        return err
    data = payload.get("data")
    if not isinstance(data, str):
        return bad("write", "data", "a string")
    pt = job["pty"]
    try:
        async with pt["write_lock"]:
            written = await _write_all(pt["fd"], data.encode("utf-8"))
    except OSError as e:
        return refusal(TRANSPORT_ERROR, f"exec_bridge: write failed: {e}")
    return {"written": written}


async def _resize(id, payload, kernel):
    """args: job_id:int (req), cols:int (req), rows:int (req). Sets a PTY job's window size (the tty sends SIGWINCH, so a TUI redraws). Returns {resized:true, cols, rows}."""
    job, err = _pty_job("resize", id, payload)
    if err:
        return err
    dims, err = _dims("resize", payload)
    if err:
        return err
    pt = job["pty"]
    pt["cols"], pt["rows"] = dims
    try:
        _set_winsize(pt["fd"], *dims)
    except OSError as e:
        return refusal(TRANSPORT_ERROR, f"exec_bridge: resize failed: {e}")
    return {"resized": True, "cols": dims[0], "rows": dims[1]}


async def _ack(id, payload, kernel):
    """args: job_id:int (req), chars:int (req). Flow control from a streaming consumer (an xterm acks each `output` chunk after parsing it): the reader pauses past 100K unacked chars and resumes under 5K. Returns {unacked:int, paused:bool}."""
    job, err = _pty_job("ack", id, payload)
    if err:
        return err
    chars = payload.get("chars")
    if isinstance(chars, bool) or not isinstance(chars, int) or chars < 0:
        return bad("ack", "chars", "a non-negative integer")
    pt = job["pty"]
    pt["unacked"] = max(0, pt["unacked"] - chars)
    if pt["paused"] and pt["unacked"] < LOW_WATERMARK:
        pt["paused"] = False
        pt["wake"].set()  # the pump re-attaches the reader in the job's own context
    return {"unacked": pt["unacked"], "paused": pt["paused"]}


async def _output(id, payload, kernel):
    """args: job_id:int (req), max_bytes:int? (default the whole 256 KiB ring). The PTY job's recent output — what a client re-attaching replays before the live `output` events. Returns {job_id, output:str}."""
    jid, err = _job_arg("output", payload, required=True)
    if err:
        return err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    if job.get("pty") is None:
        return bad("output", "job_id", "a PTY job (a pipe job's output is in `status`)")
    text = "".join(job["pty"]["ring"])
    mb = payload.get("max_bytes")
    if mb is not None:
        if isinstance(mb, bool) or not isinstance(mb, int) or mb < 0:
            return bad("output", "max_bytes", "a non-negative integer")
        raw = text.encode("utf-8")
        text = (
            raw[len(raw) - mb :].decode("utf-8", errors="ignore")
            if mb < len(raw)
            else text
        )
    return {
        "job_id": jid,
        "output": text,
        "origin": f"{id} output {{job_id: {jid}}} — the ring NOW; re-send, it moves",
    }


async def _paste_image(id, payload, kernel):
    """args: job_id:int (req), data:bytes (req), mime:str (req — image/png|jpeg|gif|webp). Saves a pasted image to a per-job scratch file and types its path into the PTY (a drag-drop — a program like claude in a browser xterm cannot reach the browser clipboard). Returns {path:str, bytes:int}."""
    job, err = _pty_job("paste_image", id, payload)
    if err:
        return err
    data = payload.get("data")
    if not isinstance(data, (bytes, bytearray)):
        return bad("paste_image", "data", "raw bytes")
    if len(data) > MAX_PASTE_IMAGE:
        return bad("paste_image", "data", f"at most {MAX_PASTE_IMAGE} bytes")
    mime = payload.get("mime")
    if not isinstance(mime, str) or not mime:
        return missing("paste_image", "mime")
    ext = _IMAGE_EXT.get(mime.lower())
    if ext is None:
        return bad("paste_image", "mime", f"one of {sorted(_IMAGE_EXT)}")
    pt = job["pty"]
    if not pt["paste_dir"]:
        pt["paste_dir"] = tempfile.mkdtemp(prefix=f"fantastic_paste_{id}_")
    name = f"paste_{secrets.token_hex(4)}.{ext}"
    # The scratch dir is a system temp dir (outside cwd by design) — written through
    # fs's external surface, the one disk module.
    try:
        fs.write_bytes(pt["paste_dir"], name, bytes(data), atomic=False, external=True)
        path = os.path.join(pt["paste_dir"], name)
        async with pt["write_lock"]:
            # the path at the cursor with a trailing space, no newline — pasting
            # must not submit
            await _write_all(pt["fd"], (path + " ").encode("utf-8"))
    except (OSError, ValueError) as e:
        return refusal(TRANSPORT_ERROR, f"exec_bridge: paste_image failed: {e}")
    return {"path": path, "bytes": len(data)}


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
        "cwd": rec.get("cwd"),
        # THE GATE, advertised: a caller must see what it may run, where, and with
        # which of the kernel's env vars.
        "allowed": _allowed_patterns(rec),
        "inherit_env": rec.get("inherit_env") or [],
        # The shell line the record permits without an `allowed` entry:
        # false (none), true ($SHELL bare) or "login" ($SHELL -l).
        "shell": rec.get("shell") or False,
        "running": running,
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "job_started": "{type:'job_started', job_id, command, args} — the RESOLVED invocation (a PTY start without `command` names the shell it ran)",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout?, stderr?} — a PTY job carries no stdout/stderr split",
            "output": "{type:'output', job_id, data} — a PTY job's output chunk (UTF-8)",
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
            "error — a script that only defines something exits before its first message "
            "arrives."
        ),
        "tasks": (
            "Every running job IS a row in the kernel TASK TABLE: its `job_id` is that "
            "row's id (an int), listed by the universal `ps`. `cancel task_id=<job_id>` "
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
    """No-op. Nothing auto-restarts: a command is started by a caller, not by the record.
    The `code`-field auto-run died with the code field."""
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
    "write": _write,
    "resize": _resize,
    "ack": _ack,
    "output": _output,
    "paste_image": _paste_image,
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
    if "cwd" in meta and meta["cwd"] is not None:
        if not isinstance(meta["cwd"], str) or not meta["cwd"]:
            return "cwd must be a non-empty directory path"
    if "env" in meta and meta["env"] is not None:
        env = meta["env"]
        if not isinstance(env, dict) or any(
            not isinstance(k, str) or not isinstance(v, (str, int, float))
            for k, v in env.items()
        ):
            return "env must be a map of NAME -> value"
    if "inherit_env" in meta and meta["inherit_env"] is not None:
        names = meta["inherit_env"]
        if not isinstance(names, list) or any(
            not isinstance(n, str) or not n for n in names
        ):
            return 'inherit_env must be a list of env var names (["*"] = all)'
    if "shell" in meta and meta["shell"] is not None:
        # `bool` first: a bare `1` equals True in Python and must not read as
        # permission. The legal set is closed — each value names ONE exact line.
        v = meta["shell"]
        if not (isinstance(v, bool) or v == "login"):
            return (
                f"shell={v!r} is not a shell permission — legal values: true "
                '(the child\'s $SHELL, bare), "login" ($SHELL -l), false (none)'
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


def _kill(job: dict) -> None:
    """SIGKILL one job: its whole SESSION — the command and everything it launched
    (a pipe job's `server &`, a PTY shell's background jobs). Every job leads its
    own session (`proc`), so this never reaches the kernel."""
    try:
        proc.kill_session(job["proc"].pid)
    except (ProcessLookupError, OSError):
        pass


def _teardown(job: dict) -> None:
    """Free the REAL resources of ONE job: SIGKILL the OS subprocess (a PTY job's
    whole session) + cancel its relay. The single teardown both lifecycle hooks
    use. Idempotent."""
    if _alive(job):
        job["status"] = "stopping"
        _kill(job)
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
    for k, v in _jobs.items():
        # A job's id IS its task id.
        if v.get("owner") == id and k == task_id:
            _teardown(v)


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
