"""python_runtime — async Python JOB spawner (fully async, no blocking exec).

`start` launches `python -u -c <code>` as a BACKGROUND OS subprocess and returns a
`job_id` IMMEDIATELY — the kernel never blocks, and many jobs run in PARALLEL (each
its own process + pump task; the event loop multiplexes). A per-job pump streams the
job's stdout/stderr line-by-line as `progress` events on this agent's inbox, and a
`job_done` event (with the collected output + exit code) on exit. There is NO
synchronous "run and wait" verb: callers get results by watching events or polling
`status`. Jobs are tracked in RAM (not yet persisted across kernel restart).

Improves the prior `execute_python`: addressable parallel jobs, live progress
events, poll-or-push results, stop/interrupt by job id.

## Inside a job: the `kernel` connector
Every spawned job runs with a `kernel` object in scope (injected ahead of your
code). It mirrors the kernel surface and talks ONLY to its spawner over a private
control fd — the spawner (THIS agent) holds the live kernel and relays. The job
never dials a host, never knows a URL: same no-bypass shape as the browser iframe
connector (child -> spawner -> kernel). Surface:
    kernel.send(target, payload) -> reply     # request/reply to any agent by id
    kernel.emit(target, payload)              # fire-and-forget
    kernel.reflect(target='kernel')           # = send({type:'reflect'})
    off = kernel.watch(src, cb)               # PUSH: cb(payload) per event from src
    off = kernel.on_message(cb)               # PUSH: messages on THIS agent's inbox
So a job is a first-class routine — read memory anywhere, call an AI, spawn another
job, push to a panel — all by id, over the same protocol. (watch/on_message run
their callbacks on a background reader thread.)

Verbs:
  start     -> spawn a background job; returns {job_id, status:'running'}
  status    -> snapshot of one (full output) / all (this agent's) jobs
  stop      -> SIGKILL one (job_id) or all of this agent's running jobs
  interrupt -> SIGINT one (job_id) or all of this agent's running jobs
  clear     -> drop finished jobs from the RAM table
  reflect   -> identity + interpreter + running count + verbs/emits
  boot      -> persist sys.executable into record.python (cross-runtime determinism)
Emits (on this agent's inbox; `watch` it to receive):
  {type:'job_started', job_id}
  {type:'progress', job_id, n, line, stream}   # one per output line
  {type:'job_done', job_id, exit_code, ok, stdout, stderr}

Interpreter resolution (highest priority first):
  payload.python > payload.venv > record.python > record.venv > sys.executable
"""

from __future__ import annotations

import asyncio
import json
import os
import secrets
import signal as signal_mod
import socket
import struct
import sys
import time
from pathlib import Path

from python_runtime._connector import CONNECTOR_SRC as _CONNECTOR_SRC

# job_id -> {owner, proc, status, started_at, exit_code, lines, last_line,
#            stdout, stderr, task, ctrl_sock, ctrl_task}.
# status: running|stopping|done|failed|stopped.
# Module-global (shared across python_runtime agents); every op filters by owner.
_jobs: dict[str, dict] = {}


def _resolve_python(rec: dict, payload: dict | None = None) -> str:
    """payload.python → payload.venv → record.python → record.venv → sys.executable.
    A `venv` resolves to `<venv>/bin/python`; a path that points nowhere silently
    degrades to sys.executable (a typo never breaks every job)."""
    payload = payload or {}
    p = payload.get("python")
    if p:
        return str(Path(p).expanduser())
    pv = payload.get("venv")
    if pv:
        cand = _venv_python(pv)
        if cand:
            return cand
    rp = rec.get("python")
    if rp:
        return str(Path(rp).expanduser())
    rv = rec.get("venv")
    if rv:
        cand = _venv_python(rv)
        if cand:
            return cand
    return sys.executable


def _venv_python(venv_path: str) -> str | None:
    base = Path(venv_path).expanduser()
    for rel in ("bin/python", "bin/python3", "Scripts/python.exe", "Scripts/python"):
        cand = base / rel
        if cand.exists():
            return str(cand)
    return None


# The injected kernel connector (prepended to every job's code) lives in
# `_connector.py`, imported above as `_CONNECTOR_SRC`. It is exec'd in the JOB
# subprocess (not imported here); see that module's docstring for the wire shape.


# ─── job machinery ──────────────────────────────────────────────


async def _read_stream(reader, job, stream, owner, job_id, kernel):
    """Stream one pipe line-by-line: emit a `progress` event per line + collect
    the raw text (newlines preserved) for the final job_done/status output."""
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


async def _pump(owner, job_id, proc, kernel):
    """Drain stdout+stderr concurrently, then await exit + emit job_done."""
    job = _jobs.get(job_id)
    if job is None:
        return
    out, err = "", ""
    try:
        out, err = await asyncio.gather(
            _read_stream(proc.stdout, job, "stdout", owner, job_id, kernel),
            _read_stream(proc.stderr, job, "stderr", owner, job_id, kernel),
        )
    except asyncio.CancelledError:
        raise
    except Exception as e:  # a pump fault must never take down the loop
        err = f"[pump error] {e}"
    code = await proc.wait()
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


async def _serve_ctrl(parent_sock, owner_id, job_id, kernel):
    """Relay a job's connector calls into the real kernel (child -> spawner ->
    kernel). send = request/reply; emit = one-way; watch/on_message subscribe via
    kernel.watch into a per-sub proxy inbox the spawner drains, PUSHING each event
    down the control fd. Ends when the child closes the socket (job exit)."""
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
        try:
            data = json.dumps(obj).encode("utf-8")
        except (TypeError, ValueError):
            data = json.dumps(
                {"op": obj.get("op"), "rid": obj.get("rid"), "error": "unserializable"}
            ).encode("utf-8")
        writer.write(struct.pack(">I", len(data)) + data)

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
                body = await reader.readexactly(n)
            except (asyncio.IncompleteReadError, ConnectionError, OSError):
                break
            try:
                m = json.loads(body.decode("utf-8"))
            except (UnicodeDecodeError, ValueError):
                continue
            op = m.get("op")
            if op == "send":
                try:
                    data = await kernel.send(m.get("target"), m.get("payload") or {})
                except Exception as e:  # never let a target fault kill the relay
                    data = {"error": f"python_runtime.connector: {e}"}
                push({"op": "reply", "rid": m.get("rid"), "data": data})
                await writer.drain()
            elif op == "emit":
                try:
                    await kernel.emit(m.get("target"), m.get("payload") or {})
                except Exception:
                    pass
            elif op == "watch":
                src = m.get("src")
                if src and src not in subs:
                    proxy = f"_pyw_{job_id}_{src}"
                    kernel.watch(src, proxy)
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
                    proxy, t = ent
                    kernel.unwatch(m.get("src"), proxy)
                    t.cancel()
            elif op == "onmessage":
                if self_sub[0] is None:
                    proxy = f"_pyself_{job_id}"
                    kernel.watch(owner_id, proxy)
                    t = asyncio.create_task(drain(proxy, _inbox_framer))
                    self_sub[0] = (proxy, t)
            elif op == "offmessage":
                ent = self_sub[0]
                self_sub[0] = None
                if ent:
                    proxy, t = ent
                    kernel.unwatch(owner_id, proxy)
                    t.cancel()
    finally:
        for src, (proxy, t) in subs.items():
            kernel.unwatch(src, proxy)
            t.cancel()
        if self_sub[0]:
            proxy, t = self_sub[0]
            kernel.unwatch(owner_id, proxy)
            t.cancel()
        try:
            writer.close()
        except OSError:
            pass


# ─── verbs ──────────────────────────────────────────────────────


async def _start(id, payload, kernel):
    """args: code:str (req), cwd:str?, python:str?, venv:str?. Spawns `python -u -c code`
    in the BACKGROUND (non-blocking) and returns {job_id, status:'running'} at once;
    streams stdout/stderr as `progress` events + a final `job_done`. Many run in parallel.
    The job gets a `kernel` connector (send/emit/reflect/watch/on_message) over a private
    control fd relayed by this agent — read memory, call agents, spawn jobs by id."""
    code = payload.get("code", "")
    if not isinstance(code, str) or not code:
        return {"error": "python_runtime: code (str) required"}
    rec = kernel.get(id) or {}
    cwd = payload.get("cwd") or rec.get("cwd") or None
    interp = _resolve_python(rec, payload)
    job_id = secrets.token_hex(4)

    parent_sock, child_sock = socket.socketpair()
    try:
        env = dict(os.environ)
        env["FANTASTIC_CTRL_FD"] = str(child_sock.fileno())
        env["FANTASTIC_SELF_ID"] = f"_pyjob_{job_id}"
        full_code = _CONNECTOR_SRC + "\n" + code
        proc = await asyncio.create_subprocess_exec(
            interp,
            "-u",
            "-c",
            full_code,
            cwd=cwd if (isinstance(cwd, str) and cwd) else None,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
            pass_fds=(child_sock.fileno(),),
        )
    except Exception as e:
        parent_sock.close()
        return {"error": f"python_runtime: spawn failed: {e}"}
    finally:
        child_sock.close()

    _jobs[job_id] = {
        "owner": id,
        "proc": proc,
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
    }
    _jobs[job_id]["ctrl_task"] = asyncio.create_task(
        _serve_ctrl(parent_sock, id, job_id, kernel)
    )
    _jobs[job_id]["task"] = asyncio.create_task(_pump(id, job_id, proc, kernel))
    await kernel.emit(id, {"type": "job_started", "job_id": job_id})
    return {"job_id": job_id, "status": "running"}


def _snap(job_id: str, full: bool = False) -> dict:
    j = _jobs[job_id]
    s = {
        "job_id": job_id,
        "status": j["status"],
        "exit_code": j["exit_code"],
        "started_at": j["started_at"],
        "lines": j["lines"],
        "last_line": j["last_line"],
    }
    if full:
        s["stdout"] = j["stdout"]
        s["stderr"] = j["stderr"]
    return s


async def _status(id, payload, kernel):
    """args: job_id:str? — one job (with full stdout/stderr), or all of this agent's
    jobs (summary). Returns {jobs:[{job_id,status,exit_code,started_at,lines,last_line,...}]}."""
    jid = payload.get("job_id")
    if jid:
        j = _jobs.get(jid)
        if not j or j["owner"] != id:
            return {"error": f"python_runtime: no job {jid!r}"}
        return {"jobs": [_snap(jid, full=True)]}
    return {"jobs": [_snap(k) for k, v in _jobs.items() if v["owner"] == id]}


async def _stop(id, payload, kernel):
    """args: job_id:str? — SIGKILL one job (job_id) or ALL of this agent's running
    jobs. Returns {killed:int}."""
    jid = payload.get("job_id")
    n = 0
    for k, j in list(_jobs.items()):
        if j["owner"] != id or (jid and k != jid):
            continue
        if j["proc"].returncode is None:
            j["status"] = "stopping"
            try:
                j["proc"].kill()
                n += 1
            except (ProcessLookupError, OSError):
                pass
    return {"killed": n}


async def _interrupt(id, payload, kernel):
    """args: job_id:str? — SIGINT one job (job_id) or ALL of this agent's running
    jobs. Returns {interrupted:int}."""
    jid = payload.get("job_id")
    n = 0
    for k, j in list(_jobs.items()):
        if j["owner"] != id or (jid and k != jid):
            continue
        if j["proc"].returncode is None:
            try:
                j["proc"].send_signal(signal_mod.SIGINT)
                n += 1
            except (ProcessLookupError, OSError):
                pass
    return {"interrupted": n}


async def _clear(id, payload, kernel):
    """args: job_id:str? — drop finished jobs (one, or all non-running) from the RAM
    table. Returns {cleared:int}."""
    jid = payload.get("job_id")
    targets = [
        k
        for k, v in _jobs.items()
        if v["owner"] == id
        and v["status"] not in ("running", "stopping")
        and (not jid or k == jid)
    ]
    for k in targets:
        _jobs.pop(k, None)
    return {"cleared": len(targets)}


async def _reflect(id, payload, kernel):
    """Identity + cwd + interpreter + running job count + verbs/emits. No args."""
    rec = kernel.get(id) or {}
    running = sum(
        1 for v in _jobs.values() if v["owner"] == id and v["status"] == "running"
    )
    return {
        "id": id,
        "sentence": (
            "Async Python job spawner — start/status/stop, parallel, live progress. "
            "Jobs get a `kernel` connector (send/emit/reflect/watch/on_message)."
        ),
        "cwd": rec.get("cwd") or "<process default>",
        "python": _resolve_python(rec),
        "venv": rec.get("venv"),
        "running": running,
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "job_started": "{type:'job_started', job_id}",
            "progress": "{type:'progress', job_id, n, line, stream} — one per output line",
            "job_done": "{type:'job_done', job_id, exit_code, ok, stdout, stderr}",
        },
        "connector": (
            "Spawned code has a `kernel` global over a private control fd this agent "
            "relays: kernel.send/emit/reflect (pull) + kernel.watch/on_message (push)."
        ),
    }


async def _boot(id, payload, kernel):
    """Idempotent. Persists sys.executable into record.python when neither python
    nor venv is set, so cross-runtime opens read a deterministic interpreter."""
    rec = kernel.get(id) or {}
    if not rec.get("python") and not rec.get("venv"):
        kernel.update(id, python=sys.executable)
    return None


async def on_delete(agent) -> None:
    """Cascade teardown: SIGKILL this agent's still-running jobs + cancel their
    connector relays (PROCESS state)."""
    oid = agent.id
    for v in _jobs.values():
        if v.get("owner") != oid:
            continue
        if v["proc"].returncode is None:
            try:
                v["proc"].kill()
            except (ProcessLookupError, OSError):
                pass
        t = v.get("ctrl_task")
        if t is not None:
            t.cancel()


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "start": _start,
    "status": _status,
    "stop": _stop,
    "interrupt": _interrupt,
    "clear": _clear,
    "reflect": _reflect,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"python_runtime: unknown type {t!r}"}
    return await fn(id, payload, kernel)
