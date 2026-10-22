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
import functools
import json
import os
import re
import secrets
import struct
import tempfile
import time

from exec_bridge import _monitor, proc
from file_bridge import fs
from io_bridge import decode_frame, describe as _describe, encode_frame, sendable
from io_bridge import gate_inbound, resolve_ingress, unknown_ingress_refusal
from io_bridge import loads as strict_loads
from io_bridge._base import parse_spec
from kernel._refusal import (
    MISSING_ARG,
    NOT_FOUND,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    UNHANDLED,
    bad,
    missing,
    refusal,
)

# ONE frame bound on this wire, both ends, every runtime. A corrupt length must not
# become a multi-GB allocation, and a program that means to move 20 MB means to
# STREAM it (`write_stream`), not to frame it. Over-limit is a WIRE VIOLATION: the
# relay closes the socket, and the connector refuses to write such a frame at all.
MAX_FRAME = 16 * 1024 * 1024

# job_id (the job's kernel TASK id, an int) -> {owner, proc, status,
#            started_at, exit_code, lines, last_line, line_ring, task, ctrl_sock,
#            ctrl_task}.
# status: running|stopping|done|failed|stopped. Module-global (shared across
# exec_bridge agents); every op filters by owner (the agent). Who may call a verb —
# drive or read — is the kernel's call gate alone.
_jobs: dict[int, dict] = {}

# The verbs that DRIVE one job. A running program's owner-inbox relay
# (`on_message`) forwards one of these only when its `job_id` names THAT program's
# own job: the fanout mirrors every payload sent to the leg, so otherwise a
# program started by one caller would see another caller's keystrokes, paste bytes
# and drive calls aimed at that caller's job.
DRIVE_VERBS = frozenset(
    {"write", "resize", "paste_image", "stop", "interrupt", "clear"}
)

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
    """Stream one pipe line-by-line: a `progress` event per line, the line into the
    job's ring. Nothing else keeps it — the ring is the job's only copy, so a
    chatty job never grows past it."""
    async for raw in reader:
        text = raw.decode("utf-8", errors="replace")
        job["lines"] += 1
        line = text.rstrip(chr(10) + chr(13))
        job["last_line"] = line
        _line_ring_add(job, job["lines"], stream, line)
        _monitor.feed_line(job_id, line)
        await kernel.emit(
            owner,
            {
                "type": "progress",
                "job_id": job_id,
                "n": job["lines"],
                "line": line,
                "stream": stream,
            },
        )


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
    try:
        await _pipe_body(job, job_id, owner, process, kernel)
    finally:
        _monitor.job_ended(job, job_id)


async def _pipe_body(job, job_id, owner, process, kernel):
    fault = None
    try:
        try:
            await asyncio.gather(
                _read_stream(process.stdout, job, "stdout", owner, job_id, kernel),
                _read_stream(process.stderr, job, "stderr", owner, job_id, kernel),
            )
        except asyncio.CancelledError:
            raise
        except Exception as e:  # a pump fault must never take down the loop
            fault = f"[pump error] {e}"
        code = await process.wait()
    except asyncio.CancelledError:
        job["status"] = "stopped"
        job["exit_code"] = process.returncode
        raise
    job["exit_code"] = code
    job["status"] = (
        "stopped"
        if job["status"] == "stopping"
        else ("done" if code == 0 else "failed")
    )
    done = {
        "type": "job_done",
        "job_id": job_id,
        "exit_code": code,
        "ok": code == 0,
        "lines": job["lines"],
        "last_line": job["last_line"],
    }
    if fault is not None:
        done["error"] = fault
    await kernel.emit(owner, done)


# ─── connector relay (spawner side of the control fd) ───────────


def _inbox_framer(owner_id, job_id, kernel):
    """on_message framer for job `job_id`: forward traffic on the owner agent, minus
    the job's own telemetry (its progress/job_started/job_done emits echo back via
    the owner), minus a drive verb addressed to ANOTHER job (`DRIVE_VERBS`) — and
    nothing the leg's gate refuses. The substrate's fanout mirrors every payload to
    the owner's watchers BEFORE any handler runs, so without that check a sealed leg
    whose handler refused the call would still have it delivered here."""

    def frame(payload):
        if isinstance(payload, dict) and payload.get("type") in (
            "progress",
            "output",
            "job_started",
            "job_done",
        ):
            return None
        if isinstance(payload, dict) and payload.get("type") in DRIVE_VERBS:
            jid = payload.get("job_id")
            # Dropped ONLY when it names ANOTHER job — an int, exactly as `_job_arg`
            # reads it. No `job_id` (the bulk stop/interrupt/clear), this job's id,
            # or a bool/string (no job's id) is forwarded.
            if isinstance(jid, int) and not isinstance(jid, bool) and jid != job_id:
                return None
        if _gate(owner_id, payload, kernel) is not None:
            return None
        return {"op": "inbox", "payload": payload}

    return frame


def _unframe(body: bytes) -> dict:
    """Decode one child frame: kind T = JSON; kind B = the io_bridge binary frame with
    the raw body restored at `_binary_path` — its list of steps (a key as a string,
    a list index as an int) — e.g. a `write_stream` payload's bytes."""
    kind = body[:1]
    if kind == b"B":
        return decode_frame(body[1:])
    if kind == b"T":
        return strict_loads(body[1:].decode("utf-8"))
    raise ValueError(f"unknown frame kind {kind!r}")


async def _serve_ctrl(parent_sock, owner_id, job_id, kernel, *, ops=None, on_push=None):
    """Relay a job's wire calls into the real kernel (child → spawner → kernel).

    `ops` / `on_push` let another spawner reuse this ONE relay (py_agent): `ops` maps an extra child op to an awaitable handler of its frame,
    and `on_push` receives the `push` function once the channel is open, so the
    spawner can write its own frames (a `call`) down the same fd.

    `send` = request/reply; `emit` = one-way; `watch`/`onmessage` subscribe via
    kernel.watch into a per-sub PROXY INBOX this relay drains, pushing each event down
    the control fd. Ends when the child closes the socket (job exit).

    Wire: `[4-byte BE length | kind | rest]`, both directions — kind T a JSON
    envelope, kind B the shared io_bridge binary frame for a payload or reply that
    carries raw bytes (`read_stream`/`write_stream`), so a bytes reply reaches the
    program intact.

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
    ops = ops or {}

    def push(obj):
        # `[4-byte BE len | kind | rest]` — kind T = JSON, kind B = the io_bridge
        # binary frame (a reply carrying a `read_stream` chunk, a watched event with
        # bytes). Raw bytes ride the fd raw, never base64 — the same codec every
        # other transport uses.
        wire, binary = encode_frame(sendable(obj))
        data = (b"B" if binary else b"T") + wire
        writer.write(struct.pack(">I", len(data)) + data)

    if on_push is not None:
        on_push(push)

    def release(src, proxy, task):
        """Give a proxy back: unwatch, DROP THE INBOX ROW, cancel the drain.

        Without the `inboxes.pop` a leaked proxy keeps every later payload on `src` being copied into a Queue nobody
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
            if not isinstance(m, dict):
                continue  # so does valid JSON that is no envelope (`T[1]`)
            op = m.get("op")
            if op in ops:
                await ops[op](m)
            elif op == "send":
                # A target fault settles as a failed outcome (error in result);
                # send never raises into the relay loop.
                item = {"target": m.get("target"), "payload": m.get("payload") or {}}
                data = (await kernel.send([item]))[0]["result"]
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
                # An agent id is a string; any other `src` names nothing to watch
                # (a list would raise out of `in subs` and end the relay).
                if isinstance(src, str) and src and src not in subs:
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
                src = m.get("src")
                ent = subs.pop(src, None) if isinstance(src, str) else None
                if ent:
                    release(src, *ent)
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
                            owner_id,
                            "inbox_drain",
                            drain(proxy, _inbox_framer(owner_id, job_id, kernel)),
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
#: A pipe job's live line ring, in bytes — the same bound as the PTY ring.
LINE_RING = 256 * 1024
#: The most lines one `tail` / `grep` reply carries.
MAX_LINES = 500


def _line_ring_add(job: dict, n: int, stream: str, line: str) -> None:
    ring = job["line_ring"]
    ring.append((n, stream, line))
    job["line_ring_bytes"] += len(line.encode("utf-8", errors="replace")) + 1
    while job["line_ring_bytes"] > LINE_RING and ring:
        _, _, old = ring.popleft()
        job["line_ring_bytes"] -= len(old.encode("utf-8", errors="replace")) + 1
        job["line_ring_dropped"] += 1


def _job_lines(job: dict, stream: str) -> tuple[list[tuple[int, str, str]], int]:
    """The output a job has printed SO FAR, as (n, stream, line), oldest first, and how
    many earlier lines the ring already dropped. A PTY job has one stream: its ring,
    split into lines (a partial last line included)."""
    if job.get("pty") is not None:
        text = "".join(job["pty"]["ring"])
        return [(i + 1, "pty", ln) for i, ln in enumerate(text.splitlines())], 0
    rows = [r for r in job["line_ring"] if stream == "both" or r[1] == stream]
    return rows, job["line_ring_dropped"]


def _lines_arg(verb: str, payload: dict, default: int):
    v = payload.get("lines", default)
    if isinstance(v, bool) or not isinstance(v, int) or not 1 <= v <= MAX_LINES:
        return None, bad(verb, "lines", f"an integer 1–{MAX_LINES}")
    return v, None


def _stream_arg(verb: str, payload: dict):
    v = payload.get("stream", "both")
    if v not in ("stdout", "stderr", "both"):
        return None, bad(verb, "stream", "one of stdout, stderr, both")
    return v, None


async def _tail(id, payload, kernel):
    """args: job_id:int (req), lines:int? (default 20, max 500), stream:str? (stdout|stderr|both, default both — a PTY job has one). The last lines a job has printed SO FAR — running or ended, pipe or PTY. Returns {job_id, status, lines:[{n, stream, line}], printed, dropped}."""
    jid, err = _job_arg("tail", payload, required=True)
    if err:
        return err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    n, err = _lines_arg("tail", payload, 20)
    if err:
        return err
    stream, err = _stream_arg("tail", payload)
    if err:
        return err
    rows, dropped = _job_lines(job, stream)
    return {
        "job_id": jid,
        "status": job["status"],
        "lines": [{"n": a, "stream": b, "line": c} for a, b, c in rows[-n:]],
        "printed": job["lines"] if job.get("pty") is None else len(rows),
        "dropped": dropped,
        "origin": f"{id} tail {{job_id: {jid}}} — the output NOW; re-send, it moves",
    }


async def _grep(id, payload, kernel):
    """args: job_id:int (req), match:str (req — a regex, searched in each line), lines:int? (most matches returned, default 50, max 500 — the LAST ones), stream:str? (stdout|stderr|both, default both). Searches what a job has printed SO FAR — running or ended, pipe or PTY; lines older than the 256 KiB ring are gone (`dropped` says how many). Returns {job_id, status, matches:[{n, stream, line}], total, scanned, dropped}."""
    jid, err = _job_arg("grep", payload, required=True)
    if err:
        return err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    pat = payload.get("match")
    if not isinstance(pat, str) or not pat:
        return bad("grep", "match", "a regex string")
    try:
        rx = re.compile(pat)
    except re.error as e:
        return bad("grep", "match", f"a valid regex ({e})")
    n, err = _lines_arg("grep", payload, 50)
    if err:
        return err
    stream, err = _stream_arg("grep", payload)
    if err:
        return err
    rows, dropped = _job_lines(job, stream)
    hits = [r for r in rows if rx.search(r[2])]
    return {
        "job_id": jid,
        "status": job["status"],
        "matches": [{"n": a, "stream": b, "line": c} for a, b, c in hits[-n:]],
        "total": len(hits),
        "scanned": len(rows),
        "dropped": dropped,
        "origin": f"{id} grep {{job_id: {jid}, match: {pat!r}}} — the output NOW; re-send, it moves",
    }


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


def _ring_add(pt: dict, text: str) -> None:
    pt["ring"].append(text)
    pt["ring_bytes"] += len(text.encode("utf-8", errors="replace"))
    while pt["ring_bytes"] > PTY_RING and pt["ring"]:
        old = pt["ring"].popleft()
        pt["ring_bytes"] -= len(old.encode("utf-8", errors="replace"))


def _pty_output(owner: str, job_id: int, kernel, data: bytes) -> None:
    """The terminal gave one chunk: out as an `output` event. EOF (`b""` — on POSIX
    also EIO, the slave closed with the child) wakes the pump; a backlog past
    HIGH_WATERMARK detaches the reader so the tty fills and the program throttles."""
    job = _jobs.get(job_id)
    if job is None:
        return
    pt = job["pty"]
    if not data:
        pt["term"].stop_reading()
        pt["eof"] = True
        pt["wake"].set()
        return
    text = pt["decoder"].decode(data)
    if not text:
        return
    _ring_add(pt, text)
    _monitor.feed_text(job_id, text)
    asyncio.ensure_future(
        kernel.emit(owner, {"type": "output", "job_id": job_id, "data": text})
    )
    pt["unacked"] += len(text)
    if pt["unacked"] > HIGH_WATERMARK and pt["reattach"]:
        # the grace belongs to the replaying viewer's stream: once nothing mirrors the
        # job any more, that stream has ended and the backlog pauses as usual
        if not kernel.has_watchers(owner):
            pt["reattach"] = False
    if pt["unacked"] > HIGH_WATERMARK and not pt["reattach"]:
        pt["term"].stop_reading()
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
    try:
        await _pty_body(job, job_id, owner, process, kernel)
    finally:
        _monitor.job_ended(job, job_id)


async def _pty_body(job, job_id, owner, process, kernel):
    pt = job["pty"]
    on_data = functools.partial(_pty_output, owner, job_id, kernel)
    try:
        try:
            while not pt["eof"]:
                pt["wake"].clear()
                if not pt["paused"]:
                    pt["term"].start_reading(on_data)
                await pt["wake"].wait()
            code = await process.wait()
        except asyncio.CancelledError:
            job["status"] = "stopped"
            job["exit_code"] = process.returncode
            raise
    finally:
        try:
            pt["term"].stop_reading()
        except (ValueError, OSError):
            pass
        try:
            pt["term"].close()
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


#: Appended to an allowlist refusal when `command` holds several words and `args`
#: is empty — the same bytes in every runtime.
COMMAND_ONE_PROGRAM = (
    " — `command` is ONE program and is quoted as one word; put its arguments "
    'in `args` (a list), e.g. command=python3 args=["-c", "print(1)"]'
)


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

# `SHELL` and `-l` are POSIX's; the OS names both (`proc.SHELL_VAR`,
# `proc.LOGIN_ARGS` — `COMSPEC` and nothing on Windows).
SHELL_VAR = proc.SHELL_VAR


def _shell_tail(rec: dict) -> list[str] | None:
    """The argv the record's `shell` permits AFTER `$SHELL`, or None when the
    record permits no shell line. `True` is compared by identity so a stray
    `1` never reads as `true`."""
    v = rec.get("shell")
    if v is True:
        return []
    if v == "login":
        return list(proc.LOGIN_ARGS)
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
    # What the OS itself needs in every process (`proc.OS_ENV`: nothing on POSIX,
    # SYSTEMROOT on Windows) comes down from the kernel's env even when
    # `inherit_env` leaves it out; the record's `env` still has the last word.
    env.update(
        {n: os.environ[n] for n in proc.OS_ENV if n in os.environ and n not in env}
    )
    env.update({str(k): str(v) for k, v in (rec.get("env") or {}).items()})
    return env


async def _start(id, payload, kernel):
    """args: command:str (req — the executable ALONE, e.g. `grep`; on a PTY start, absent = the child's $SHELL, and with no `args` the record's whole shell line: `$SHELL -l` under shell="login"), args:list[str]? (its arguments, one item each — `<args><item>-rn</item><item>x</item></args>`), pty:{cols,rows}? (run on a terminal). The FULL line (command + args) must fullmatch one of the `allowed` regexes, or be the shell line the record's `shell` permits.
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
    # (`LD_PRELOAD`, `PATH`, …) or `cwd` would change what an allowlisted line
    # actually runs.
    for gone in ("env", "cwd"):
        if payload.get(gone) is not None:
            return bad(
                "start",
                gone,
                f"absent — `{gone}` is the record's (update_agent {id} {gone}=…)",
            )
    # A WAKE-UP is a monitor's, never the start's: a `notify` here is refused,
    # never dropped silently with the caller waiting for a message nothing sends.
    for gone in ("notify", "match", "every"):
        if payload.get(gone) is not None:
            return bad(
                "start",
                gone,
                "absent — `start` sends no one anything; to be told what the job "
                "prints (or when it ends), `monitor` it: job_id, notify, match, every",
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
        # …and with no `args` either, it is the SHELL LINE the record permits:
        # `$SHELL` under `shell: true`, its login line under `shell: "login"` (`-l`;
        # nothing on Windows) — a client asks for "the shell" without knowing
        # which OS, or which flag, makes it a login shell.
        if not raw_args:
            raw_args = _shell_tail(rec) or []
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
        if not raw_args and len(command.split()) > 1:
            # A whole line in `command` is quoted as ONE word, so no pattern
            # over `<program> <args>` can ever match it — say where the words go.
            why += COMMAND_ONE_PROGRAM
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

    parent_sock, child_sock = proc.ctrl_pair()
    env["FANTASTIC_CTRL_FD"] = str(child_sock.fileno())
    # The OWNER AGENT id (the tool's public id), NOT the ephemeral job id — so a
    # tool can address and tag itself by the id others use.
    env["FANTASTIC_SELF_ID"] = id
    term = None
    try:
        if dims is None:
            process = await proc.spawn_argv(
                argv, cwd=cwd, env=env, pass_fd=child_sock.fileno()
            )
        else:
            term = proc.open_pty(*dims)
            env["TERM"] = "xterm-256color"
            process = await proc.spawn_pty(
                argv, cwd=cwd, env=env, pass_fd=child_sock.fileno(), term=term
            )
    except proc.NoPath:
        parent_sock.close()
        if term is not None:
            term.close()
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
        if term is not None:
            term.close()
        return refusal(TRANSPORT_ERROR, f"exec_bridge: spawn failed: {e}")
    finally:
        # The parent's copy of the CHILD end must go, or EOF never fires and a parked
        # program never learns the kernel died. (The PTY's child side went with the
        # spawn: the terminal sees EOF only once the job's last copy is closed.)
        child_sock.close()

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
        # THE LIVE LINE RING: every line a pipe job prints, as
        # (n, stream, line), the last LINE_RING bytes — what `tail`/`grep` read,
        # running or ended. The job's only copy of its output.
        "line_ring": collections.deque(),
        "line_ring_bytes": 0,
        "line_ring_dropped": 0,
        "task": None,
        "ctrl_sock": parent_sock,
        "ctrl_task": None,
        "pty": None
        if dims is None
        else {
            "term": term,
            "cols": dims[0],
            "rows": dims[1],
            "ring": collections.deque(),
            "ring_bytes": 0,
            "decoder": codecs.getincrementaldecoder("utf-8")(errors="replace"),
            "unacked": 0,
            "paused": False,
            # a viewer's replay opened a re-attach grace: until its first ack or its
            # stream's end, the backlog does not pause the reader
            "reattach": False,
            "eof": False,
            "wake": asyncio.Event(),
            "write_lock": asyncio.Lock(),
            "paste_dir": None,
        },
    }
    # THE PUMP IS THE JOB'S TASK, and the job's id IS that task's id — minted
    # by the kernel's one counter when the pump is forked. One number names the
    # job everywhere: `status`/`stop`/`write` take it as `job_id`, `ps` lists it,
    # `cancel task_id=` kills it. (A random hex string would be coerced by the
    # tag grammar into an int or an infinite float, e.g. `04203581` or `458e5912`,
    # and the job could not be addressed again.)
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
        s["cwd"] = j.get("cwd")
    return s


async def _status(id, payload, kernel):
    """args: job_id:int? — one job (with its `cwd`; read its output with `tail`/`grep`), or all of this agent's jobs (summary). A job's id is its kernel task id: `ps` lists it, `cancel task_id=` kills it. wait_s:float? (0–120) with job_id — returns as soon as the job is no longer running, or when the wait ends; use it instead of polling. Returns {jobs:[…]}."""
    jid, err = _job_arg("status", payload, required=False)
    if err:
        return err
    w = payload.get("wait_s")
    if w is not None:
        if jid is None:
            return bad("status", "wait_s", "a job_id to wait for")
        if isinstance(w, bool) or not isinstance(w, (int, float)) or w < 0 or w > 120:
            return bad("status", "wait_s", "a number of seconds, 0 to 120")
    if jid is not None:
        j = _jobs.get(jid)
        if j is None or j.get("owner") != id:
            return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
        if w is not None:
            deadline = time.monotonic() + w
            while _alive(j) or j["status"] in ("running", "stopping"):
                if time.monotonic() >= deadline:
                    break
                await asyncio.sleep(0.2)
        return {
            "jobs": [_snap(jid, full=True)],
            "origin": f"{id} status {{job_id: {jid}}} — a live job; re-send for its output NOW, which may differ",
        }
    return {
        "jobs": [_snap(k) for k, v in _jobs.items() if v.get("owner") == id],
        "origin": f"{id} status — the job table NOW; re-send, it changes",
    }


async def _run(id, payload, kernel):
    """args: command:str (req), args?, wait_s:float? (default 60, max 600), tail:int? (default 200) — start, wait, and return the output in ONE reply: {job_id, status, exit_code, lines}. Use run for short commands; a job still running at wait_s comes back running (continue with status / tail). Read files with files.read, not run."""
    import asyncio

    wait_s = payload.get("wait_s")
    wait_s = 60 if wait_s is None else wait_s
    if (
        isinstance(wait_s, bool)
        or not isinstance(wait_s, (int, float))
        or not 0 <= wait_s <= 600
    ):
        return bad("run", "wait_s", "a number of seconds, 0 to 600")
    tail = payload.get("tail")
    tail = 200 if tail is None else tail
    if isinstance(tail, bool) or not isinstance(tail, int) or not 1 <= tail <= 2000:
        return bad("run", "tail", "a line count, 1 to 2000")
    rest = {k: v for k, v in payload.items() if k not in ("wait_s", "tail")}
    r = await _start(id, rest, kernel)
    if "reason" in r:
        return r
    jid = r["job_id"]
    job = _jobs[jid]
    deadline = time.monotonic() + wait_s
    while _alive(job) and time.monotonic() < deadline:
        await asyncio.sleep(0.2)
    out = await _tail(id, {"job_id": jid, "lines": min(tail, MAX_LINES)}, kernel)
    if "reason" in out:
        return out
    alive = _alive(job)
    return {
        "job_id": jid,
        "status": "running" if alive else job["status"],
        "exit_code": job["exit_code"],
        "lines": out["lines"],
    }


async def _monitor_verb(id, payload, kernel):
    """args: job_id:int (req — a RUNNING job of this agent), notify:str (req — the agent told), match:str? (a regex; absent = every line), every:number? (seconds — at most one delivery per window; required without `match`), tail:int? (newest lines per delivery, default 100), label:str?. Watches the job's output AS IT ARRIVES and SENDS what matches to `notify` as a turn — it polls nothing. Returns {monitor_id, job_id, notify, match, every, tail, label}; kernel `cancel` (or `unmonitor`) ends it, and it ends with its job.

    Each delivery is a call made by THIS agent — its `can_call` must name `notify`,
    and `notify`'s `callable_by` this agent — headed
    `[monitor <id> · job <id> · from your call <pid> · "<label>"]` with the matched
    lines below. The first match goes at once; with `every`, later ones are gathered
    and delivered at most once per window, and a timer runs only while something
    waits. When the job ends, one last delivery headed `job ended` carries what
    still waited (the header alone when nothing did). A monitor is a kernel task:
    `ps` lists it, `monitors` lists this agent's."""
    from kernel import current_origin, current_task

    jid, err = _job_arg("monitor", payload, required=True)
    if err:
        return err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    # `monitors_closed`: the pump already told this job's monitors it ended — its
    # process may still be unreaped (a cancel), but a monitor filed now would never end
    if not _alive(job) or job.get("monitors_closed"):
        return bad("monitor", "job_id", "a RUNNING job — this one has exited")
    notify = payload.get("notify")
    if notify is None:
        return missing("monitor", "notify")
    if not isinstance(notify, str) or not notify:
        return bad("monitor", "notify", "an agent id")
    if kernel.get(notify) is None:
        return refusal(
            NOT_FOUND, f"monitor: no agent {notify!r} to notify", arg="notify"
        )
    pattern = payload.get("match")
    match = None
    if pattern is not None:
        if not isinstance(pattern, str) or not pattern:
            return bad("monitor", "match", "a regex string")
        try:
            match = _monitor.compile_match(pattern)
        except re.error as e:
            return bad("monitor", "match", f"a valid regex ({e})")
    every = payload.get("every")
    if every is not None and (
        isinstance(every, bool) or not isinstance(every, (int, float)) or every <= 0
    ):
        return bad("monitor", "every", "a positive number of seconds")
    if match is None and every is None:
        return refusal(
            MISSING_ARG,
            "monitor: every required — without `match` every line counts, so name the "
            "window (`every`, seconds) its deliveries are gathered in",
            arg="every",
        )
    tail = payload.get("tail", _monitor.DEFAULT_TAIL)
    if isinstance(tail, bool) or not isinstance(tail, int) or tail < 1:
        return bad("monitor", "tail", "an int >= 1 (lines per delivery)")
    label = payload.get("label")
    if label is not None and not isinstance(label, str):
        return bad("monitor", "label", "a string")
    m = {
        "id": None,
        "owner": id,
        "job_id": jid,
        "notify": notify,
        "match": match,
        "every": every,
        "tail": tail,
        "label": label or None,
        "caller": current_origin(),
        "call": current_task(),
        "queue": asyncio.Queue(),
        "carry": "",
        "delivered": 0,
        "pending": 0,
    }
    mid = kernel.ctx.spawn_task(id, "monitor", _monitor.run(m, kernel))
    m["id"] = mid
    _monitor._MONITORS[mid] = m
    return {
        "monitor_id": mid,
        "job_id": jid,
        "notify": notify,
        "match": pattern,
        "every": every,
        "tail": tail,
        "label": m["label"],
    }


async def _monitors(id, payload, kernel):
    """no args — this agent's monitors: {monitors:[{monitor_id, job_id, notify, match, every, tail, label, delivered, pending, status, error}]}. `status` is running, or dead once `notify` REFUSED a delivery (`error` says why; nothing more is sent) — `unmonitor` clears it."""
    return {"monitors": _monitor.listing(id)}


async def _unmonitor(id, payload, kernel):
    """args: monitor_id:int (req). End one of this agent's monitors (kernel `cancel` on its id does the same). Returns {unmonitored:true, monitor_id}."""
    mid = payload.get("monitor_id")
    if mid is None:
        return missing("unmonitor", "monitor_id")
    if isinstance(mid, bool) or not isinstance(mid, int):
        return bad("unmonitor", "monitor_id", "an int — the id `monitor` returned")
    m = _monitor._MONITORS.get(mid)
    if m is None or m["owner"] != id:
        return refusal(NOT_FOUND, f"exec_bridge: no monitor {mid!r}", monitor_id=mid)
    if m.get("error"):  # dead: its task is over, only the record is left
        _monitor._MONITORS.pop(mid, None)
        return {"unmonitored": True, "monitor_id": mid}
    await kernel.ctx.cancel_task(mid)
    return {"unmonitored": True, "monitor_id": mid}


def _no_job(verb: str, id: str, jid) -> dict | None:
    """The `not_found` refusal for a named job this agent does not own, or None.

    A job_id naming nothing is refused rather than answered `{killed:0}` /
    `{interrupted:0}` — the same reply as "it had already exited", so a typo would
    read as success."""
    if jid is None:
        return None
    if jid not in _jobs or _jobs[jid].get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    return None


async def _stop(id, payload, kernel):
    """args: job_id:int? — SIGKILL one job (its whole session: the command and everything it launched) or ALL running jobs on this agent.
    Returns {killed:int}."""
    jid, err = _job_arg("stop", payload, required=False)
    if err:
        return err
    if gone := _no_job("stop", id, jid):
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
    """args: job_id:int? — SIGINT one job or ALL running jobs on this agent.
    Returns {interrupted:int}."""
    jid, err = _job_arg("interrupt", payload, required=False)
    if err:
        return err
    if gone := _no_job("interrupt", id, jid):
        return gone
    n = 0
    for k, v in _jobs.items():
        if v.get("owner") != id or (jid is not None and k != jid):
            continue
        if _alive(v):
            try:
                proc.interrupt(v["proc"], v["pty"]["term"] if v.get("pty") else None)
            except (ProcessLookupError, OSError):
                pass
            n += 1
    return {"interrupted": n}


async def _clear(id, payload, kernel):
    """args: job_id:int? — drop finished jobs (one, or all non-running) from the RAM
    table. A named job still running or stopping is refused — `stop` it first.
    Returns {cleared:int}."""
    jid, err = _job_arg("clear", payload, required=False)
    if err:
        return err
    if jid is not None and jid in _jobs and _jobs[jid].get("owner") == id:
        # A NAMED job follows the bulk form's rule: a row still running or
        # stopping is the only handle stop, cancel, delete and shutdown kill its
        # process through — forgotten, it would outlive them all.
        if (status := _jobs[jid]["status"]) in ("running", "stopping"):
            return bad(
                "clear",
                "job_id",
                f"a finished job (job {jid} is {status} — `stop` it, then clear)",
            )
    # The one caller that WANTS the status label, not process liveness: it is deciding
    # what is finished enough to forget.
    targets = [
        k
        for k, v in _jobs.items()
        if v.get("owner") == id
        and (jid is None or k == jid)
        and v["status"] not in ("running", "stopping")
    ]
    for k in targets:
        _jobs.pop(k, None)
    return {"cleared": len(targets)}


async def _write(id, payload, kernel):
    """args: job_id:int (req), data:str or bytes:bytes (exactly one). Keystrokes/paste into a PTY job, written IN FULL and serialized per job (a bracketed paste is never truncated or interleaved). `bytes` is written VERBATIM — a trailing newline (Enter) survives; a `<call>` strips a scalar value like `data`, so send keys that end in Enter as `bytes`. Returns {written:int}."""
    job, err = _pty_job("write", id, payload)
    if err:
        return err
    data, raw = payload.get("data"), payload.get("bytes")
    if (data is None) == (raw is None):
        return bad(
            "write", "data", "exactly one of data (a string) or bytes (verbatim)"
        )
    if raw is not None:
        if isinstance(raw, str):
            raw = raw.encode("utf-8")
        if not isinstance(raw, (bytes, bytearray)):
            return bad("write", "bytes", "bytes, or a string written verbatim")
        buf = bytes(raw)
    elif not isinstance(data, str):
        return bad("write", "data", "a string")
    else:
        buf = data.encode("utf-8")
    pt = job["pty"]
    try:
        async with pt["write_lock"]:
            written = await pt["term"].write(buf)
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
        pt["term"].resize(*dims)
    except OSError as e:
        return refusal(TRANSPORT_ERROR, f"exec_bridge: resize failed: {e}")
    return {"resized": True, "cols": dims[0], "rows": dims[1]}


async def _ack(id, payload, kernel):
    """args: job_id:int (req), chars:int (req). Flow control from a streaming consumer (an xterm acks each `output` chunk after parsing it): the reader pauses past 100K unacked chars and resumes under 5K. After an `output` replay the reader does not pause for the backlog until this viewer's first ack or its stream ends. Returns {unacked:int, paused:bool}."""
    job, err = _pty_job("ack", id, payload)
    if err:
        return err
    chars = payload.get("chars")
    if isinstance(chars, bool) or not isinstance(chars, int) or chars < 0:
        return bad("ack", "chars", "a non-negative integer")
    pt = job["pty"]
    # the re-attach grace ends at the viewer's first ack after its replay
    pt["reattach"] = False
    pt["unacked"] = max(0, pt["unacked"] - chars)
    if pt["paused"] and pt["unacked"] < LOW_WATERMARK:
        pt["paused"] = False
        pt["wake"].set()  # the pump re-attaches the reader in the job's own context
    return {"unacked": pt["unacked"], "paused": pt["paused"]}


async def _output(id, payload, kernel):
    """args: job_id:int (req), max_bytes:int? (default the whole 256 KiB ring). The PTY job's recent output — what a client re-attaching replays before the live `output` events; it clears the unacked backlog (flow control restarts at 0 for this viewer) and opens a re-attach grace: the reader does not pause for the backlog until this viewer's first `ack` or its stream ends. Returns {job_id, output:str}."""
    jid, err = _job_arg("output", payload, required=True)
    if err:
        return err
    job = _jobs.get(jid)
    if job is None or job.get("owner") != id:
        return refusal(NOT_FOUND, f"exec_bridge: no job {jid!r}", job_id=jid)
    if job.get("pty") is None:
        return bad("output", "job_id", "a PTY job (a pipe job's output is in `status`)")
    pt = job["pty"]
    text = "".join(pt["ring"])
    # The replay hands this viewer everything so far: what a gone viewer streamed
    # but never acked is owed to no one, so the backlog clears and the reader resumes.
    pt["unacked"] = 0
    pt["reattach"] = True
    if pt["paused"]:
        pt["paused"] = False
        pt["wake"].set()  # the pump re-attaches the reader in the job's own context
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
            await pt["term"].write((path + " ").encode("utf-8"))
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
            "exposes `kernel.send(target, payload) -> reply` (raises if "
            "the relay could not deliver), "
            "`kernel.emit(payload)`, `kernel.watch(src, cb)`, `kernel.on_message(cb)`, "
            "`kernel.self_id`. Arguments arrive as ARGV, like any command — there is "
            "no `kernel.args`. One frame caps at 16 MiB; move more than that with "
            "`write_stream` chunks. Your sends carry THIS runner's identity "
            "and grants (its `can_call`; a target must list this runner in "
            "`callable_by`). Do not rewrite the wire by hand."
        ),
    }


#: What a job emits on this agent's inbox — served by `docs` (tier detailed).
EMITS = {
    "job_started": "{type:'job_started', job_id, command, args} — the RESOLVED invocation (a PTY start without `command` names the shell it ran)",
    "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
    "job_done": "{type:'job_done', job_id, exit_code, ok, lines?, last_line?, error?} — a pipe job's output is read with `tail`/`grep`; a PTY job carries exit_code and ok only",
    "output": "{type:'output', job_id, data} — a PTY job's output chunk (UTF-8)",
    "monitor_dead": "{type:'monitor_dead', monitor_id, job_id, notify, reason, error} — `notify` REFUSED a monitor's delivery (`reason` unauthorized|unknown_agent); the monitor ended, nothing more is sent",
}

#: How a running program talks back — served by `docs` (tier detailed).
NOTES = {
    "wire": (
        "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD, a "
        "BIDIRECTIONAL socket: frame is [4-byte BE length | kind | rest], both "
        "directions — kind T: a JSON envelope; kind B: [4-byte BE H | H-byte JSON "
        "header | raw body], the header's `_binary_path` naming where the body "
        "belongs as steps from the frame root — a key as a string (taken literally: "
        "['payload', 'a.b'] is the key 'a.b'), a list index as an int (a stream "
        "chunk rides raw, never base64). To CALL another agent: write {op:'send', "
        "rid:<your id>, target, payload}; the reply COMES BACK on the SAME fd as "
        "{op:'reply', rid:<same id>, data}. Read frames in a loop and skip any whose "
        "rid isn't yours (event/inbox frames can arrive first). ops: send/emit/"
        "reflect/watch/on_message. A command that never reads the fd runs like any "
        "OS process. The `connector` verb returns a ready client."
    ),
    "inbox": (
        "Any verb this bundle does not answer is MAIL for the running program, "
        "delivered to its on_message and ACKed {queued:true}. With no live job it is "
        "refused — a script that only defines something exits before its first "
        "message arrives."
    ),
    "tasks": (
        "Every running job IS a row in the kernel TASK TABLE: its `job_id` is that "
        "row's id, listed by `ps`. `cancel task_id=<job_id>` kills it — the OS "
        "process is SIGKILLed by this bundle's `on_cancel` hook first, so nothing is "
        "orphaned."
    ),
}

VERBS = {
    "start": _start,
    "run": _run,
    "status": _status,
    "stop": _stop,
    "interrupt": _interrupt,
    "clear": _clear,
    "write": _write,
    "resize": _resize,
    "ack": _ack,
    "output": _output,
    "tail": _tail,
    "grep": _grep,
    "paste_image": _paste_image,
    "connector": _connector,
    "monitor": _monitor_verb,
    "monitors": _monitors,
    "unmonitor": _unmonitor,
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
    if bad := unknown_ingress_refusal(meta, ("allow_all_ingress", "deny_ingress")):
        return bad
    # `password_ingress` is meaningless on an in-process leg: the token is read
    # off a WIRE ENVELOPE and the process edge has none — no frame ever presents
    # one, so the rule could only refuse, or be satisfied by a secret persisted
    # ON THE RECORD. Refused at the write.
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


def _gate(id: str, payload, kernel) -> dict | None:
    """THE LEG'S GATE on one payload — the process edge, gated like any io_bridge
    leg (sealed by default), off the LIVE record: None when its `ingress_rule`
    admits it, else the refusal. `kind="spawn"` because that is what is being
    authorized; no rule inspects `Action.kind`, so a new kind costs nothing and
    the audit trail stays honest. The handler asks it for every verb but the
    UNGATED ones, mail included; the inbox relay asks it for every payload."""
    decision = gate_inbound(
        resolve_ingress(kernel.get(id) or {}),
        "spawn",
        {"target": id, "payload": payload},
    )
    if decision.allowed:
        return None
    out = {"error": decision.reason, "reason": "unauthorized"}
    if decision.hint:
        out["hint"] = decision.hint
    if decision.see:
        out["see"] = decision.see
    return out


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    # AUTH GATE — every verb but `reflect`/`connector`, MAIL INCLUDED: a sealed leg
    # delivers nothing to its program (the relay's framer refuses the same
    # payloads, since the fanout reaches it before this handler runs).
    if t not in UNGATED and (out := _gate(id, payload, kernel)):
        return out
    fn = VERBS.get(t)
    if fn is None:
        # Addressable inbox: any verb this bundle doesn't own is delivered to the running
        # program. The delivery is the substrate's OWN dispatch fanout (`_dispatch`
        # mirrors every payload to the agent's watchers — the on_message proxy is one),
        # so we do NOT re-emit here; that would double-deliver. We only ACK.
        if _has_running_job(id):
            return {"queued": True}
        return refusal(
            UNHANDLED,
            f"exec_bridge: unknown type {t!r} (no running job to receive it)",
            verb=t,
        )
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
