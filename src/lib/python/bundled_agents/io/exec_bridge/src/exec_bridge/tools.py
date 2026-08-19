"""exec_bridge bundle — the PROCESS-transport member of the io family.

A subprocess is OUTSIDE the trust domain: crossing kernel↔process is an IO edge, gated
exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒ every verb except
`reflect` denies with the teaching shape). Open it consciously with
`ingress_rule=allow_all`. Even open, the LAW is the ALLOWLIST: only an interpreter the
operator listed in `interpreters` can be spawned, and that choice is not overridable per
call. The spawn primitive lives in `proc` — the ONE process surface. This agent is the
GATE + verb shape over it; it never spawns directly.

## Why this exists, and what it replaces

`python_runtime` is an entire executor — job table, connector, relay, boot rehydration,
addressable inbox — bound to ONE language, and that machinery exists three times over
(python, js, lua). It is also the only boundary in this kernel that is not a sealed io
edge: its own docstring says registering it "GRANTS ARBITRARY LOCAL CODE EXECUTION".

Here the interpreter is a FIELD. "A python agent" stops being a bundle and becomes a
record. Everything that is not language-specific — the job table, the pump, the relay,
the fd passing, the task-table integration, the mail contract — is the same code it
always was, because none of it ever knew a language.

## The wire is the contract; a shim is a convenience

This bridge publishes a control fd and a frame format (see `shims`). A program may name
`shim:'python'` and have the client prepended, or name nothing and speak the wire itself.
That is the difference between generic and python-shaped: adding a language is adding a
blob to `shims.SHIMS`, not a code path here.

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

from exec_bridge import proc, shims
from io_bridge import describe as _describe
from io_bridge import gate_inbound, resolve_ingress

# Per-call `args` ride the process env, and the whole env block caps near 1 MB on
# darwin/linux — past it `execve` fails with a cryptic E2BIG. Refuse early and say why.
# Bulk data belongs in an agent; pass its id.
MAX_ARGS_BYTES = 256 * 1024

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


async def _serve_ctrl(parent_sock, owner_id, job_id, kernel):
    """Relay a job's wire calls into the real kernel (child → spawner → kernel).

    `send` = request/reply; `emit` = one-way; `watch`/`onmessage` subscribe via
    kernel.watch into a per-sub PROXY INBOX this relay drains, pushing each event down
    the control fd. Ends when the child closes the socket (job exit).

    The proxy ids carry this bundle's prefix (`_xb…`): two spawner bundles in one kernel
    must not collide on a synthetic inbox id."""
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
                    data = {"error": f"exec_bridge.connector: {e}"}
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
                    # Exactly ONE job per owner receives the owner inbox. If another job
                    # for this owner is already subscribed (a re-started durable tool
                    # whose predecessor lingers, or two starts), SUPERSEDE it — else
                    # every inbound `send` is delivered to both and doubled.
                    prev = _owner_inbox.pop(owner_id, None)
                    if prev is not None:
                        kernel.unwatch(owner_id, prev["proxy"])
                        prev["task"].cancel()
                    proxy = f"_xbself_{job_id}"
                    kernel.watch(owner_id, proxy)
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
                    proxy, t = ent
                    kernel.unwatch(owner_id, proxy)
                    t.cancel()
                    cur = _owner_inbox.get(owner_id)
                    if cur and cur["job_id"] == job_id:
                        _owner_inbox.pop(owner_id, None)
    finally:
        for src, (proxy, t) in subs.items():
            kernel.unwatch(src, proxy)
            t.cancel()
        if self_sub[0]:
            proxy, t = self_sub[0]
            kernel.unwatch(owner_id, proxy)
            t.cancel()
            # Only if it still names THIS job — a supersede must not evict its
            # replacement (the bug `c2a4297` fixed in js_runtime).
            cur = _owner_inbox.get(owner_id)
            if cur and cur["job_id"] == job_id:
                _owner_inbox.pop(owner_id, None)
        try:
            writer.close()
        except OSError:
            pass


# ─── verbs ──────────────────────────────────────────────────────


async def _start(id, payload, kernel):
    """args: code:str? (falls back to the record's `code`), args:any? (THIS CALL's
    arguments, reaches the program as `kernel.args`, default {}), interpreter:str?
    (must be allowlisted), shim:str? (prepend a wire client), cwd:str?. Spawns in the
    BACKGROUND and returns {job_id, status:'running'} at once; streams `progress` per
    output line + a final `job_done`. Many run in parallel."""
    rec = kernel.get(id) or {}
    code = payload.get("code") or rec.get("code") or ""
    if not isinstance(code, str) or not code:
        return {
            "error": "exec_bridge: code (str) required (pass it or set it on the record)"
        }
    cwd = payload.get("cwd") or rec.get("cwd") or None

    try:
        program = shims.apply(payload.get("shim") or rec.get("shim"), code)
    except KeyError as e:
        return {"error": f"exec_bridge: {e}"}

    try:
        args_json = json.dumps(payload.get("args", {}))
    except (TypeError, ValueError) as e:
        return {"error": f"exec_bridge: args must be JSON-serializable ({e})"}
    if len(args_json.encode("utf-8")) > MAX_ARGS_BYTES:
        return {
            "error": (
                f"exec_bridge: args too large ({len(args_json)} > {MAX_ARGS_BYTES}) — "
                "they ride the process env, which caps near 1 MB. Put bulk data in an "
                "agent and pass its id."
            )
        }

    job_id = secrets.token_hex(4)
    parent_sock, child_sock = socket.socketpair()
    try:
        process = await proc.spawn(
            rec,
            interpreter=payload.get("interpreter") or rec.get("interpreter"),
            program=program,
            cwd=cwd,
            env_extra={
                "FANTASTIC_CTRL_FD": str(child_sock.fileno()),
                # The OWNER AGENT id (the tool's public id), NOT the ephemeral job id —
                # so a tool can address and tag itself by the id others use.
                "FANTASTIC_SELF_ID": id,
                "FANTASTIC_ARGS": args_json,
            },
            pass_fd=child_sock.fileno(),
        )
    except proc.InterpreterRefused as e:
        parent_sock.close()
        return {"error": f"exec_bridge: {e}"}
    except Exception as e:
        parent_sock.close()
        return {"error": f"exec_bridge: spawn failed: {e}"}
    finally:
        # The parent's copy of the CHILD end must go, or EOF never fires and a parked
        # program never learns the kernel died.
        child_sock.close()

    _jobs[job_id] = {
        "owner": id,
        "proc": process,
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
    kernel.update(id, code=code)
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
    }
    if full:
        s["stdout"] = j["stdout"]
        s["stderr"] = j["stderr"]
    return s


async def _status(id, payload, kernel):
    """args: job_id:str? — one job (with full stdout/stderr), or all of this agent's
    jobs (summary). Returns {jobs:[…]}."""
    jid = payload.get("job_id")
    if jid:
        j = _jobs.get(jid)
        if j is None or j.get("owner") != id:
            return {"error": f"exec_bridge: no job {jid!r}"}
        return {"jobs": [_snap(jid, full=True)]}
    return {"jobs": [_snap(k) for k, v in _jobs.items() if v.get("owner") == id]}


async def _stop(id, payload, kernel):
    """args: job_id:str? — SIGKILL one job or ALL of this agent's running jobs.
    Returns {killed:int}."""
    jid = payload.get("job_id")
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
    """Identity + allowlist + shims + running job count + auth posture. No args."""
    rec = kernel.get(id) or {}
    running = sum(1 for v in _jobs.values() if v["owner"] == id and _alive(v))
    return {
        "id": id,
        "sentence": (
            "Process edge of the io family — sealed by default, allowlisted "
            "interpreters only. Spawns a program in the background; it gets a kernel "
            "wire over a private control fd."
        ),
        "cwd": rec.get("cwd") or "<process default>",
        # THE ALLOWLIST, advertised. Without this an authoring model has to GUESS an
        # interpreter path, and a guess has no legal answer — the same defect as an
        # unnamed executor module (measured 2026-08-05: opus invented
        # `local_runner.script` because nothing named the real one).
        "interpreters": proc.allowed(rec),
        "shims": shims.names(),
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
            "A program speaks the kernel over the inherited fd FANTASTIC_CTRL_FD: "
            "[4-byte BE length | JSON]. Name `shim:'<one of shims>'` on `start` and a "
            "client is prepended for you (python: a `kernel` global with "
            "send/emit/reflect/watch/on_message + self_id + args). Name none and speak "
            "it yourself — reflect readme=true for the frame vocabulary."
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
            "This agent's persisted `code` is a durable tool and a SCRIPT: it runs TOP "
            "TO BOTTOM, once per `start` and again on `boot`. A bare `def` only defines "
            "and exits — put the work at module level, and keep the program running if "
            "it must receive later sends. create_agent with `code` + an allowlisted "
            "`interpreter`; `delete_agent` removes it. A running program can itself "
            "create_agent more agents."
        ),
        **_describe(rec),
    }


async def _boot(id, payload, kernel):
    """Idempotent. If the record carries `code` and no job is running, RE-STARTS it —
    the program survives reboot. Returns the start error if it could not."""
    rec = kernel.get(id) or {}
    code = rec.get("code")
    if not (isinstance(code, str) and code) or _has_running_job(id):
        return None
    reply = await _start(id, {"code": code}, kernel)
    # PROPAGATE. `python_runtime._boot` discarded this, and with an allowlist that is a
    # silent-success hole: a record naming a non-allowlisted interpreter would boot,
    # fail to spawn, and still look created — indistinguishable from a program that ran
    # and did nothing. `create_agent` discards a boot REPLY too, which is why the
    # allowlist is ALSO enforced at write time by `validate_meta`; this covers the
    # reboot path, where no write happens.
    if isinstance(reply, dict) and "error" in reply:
        return reply
    return None


VERBS = {
    "start": _start,
    "status": _status,
    "stop": _stop,
    "interrupt": _interrupt,
    "clear": _clear,
    "reflect": _reflect,
    "boot": _boot,
}

UNGATED = {"reflect"}  # discovery must work on a sealed bridge


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this bundle cannot honour, at the WRITE, next to the typo.

    Called by `create_agent` AND `update_agent` before the record changes (duck-typed —
    the substrate never imports this module). Naming the legal set is the point: on
    2026-08-06 gemma-4-12B tried `ingress_rule="deny_all"`, the write-time refusal named
    the legal values, and it used `deny_inbound` on its very next turn. A refusal that
    does not say what WOULD work only teaches that something is wrong.

    Validates the MERGED state — `record` (empty at create) with `meta` applied — not the
    patch alone. Checking the patch alone made `interpreter` unenforceable in the one
    place it matters: `update_agent id=x interpreter=<already on x's allowlist>` was
    REFUSED, because `interpreters` was not in that same payload. A false refusal on a
    correct operation is worse than no check — it makes the right thing look impossible.
    """
    merged = {**record, **meta}
    if "ingress_rule" in meta:
        spec = meta["ingress_rule"]
        if getattr(resolve_ingress({"ingress_rule": spec}), "unknown", None):
            return (
                f"ingress_rule={spec!r} is not a rule this bundle implements — "
                "legal values: allow_all, deny_inbound, password"
            )
    if "shim" in meta and meta["shim"]:
        if str(meta["shim"]) not in shims.SHIMS:
            return (
                f"shim={meta['shim']!r} is unknown — this bridge serves {shims.names()}"
            )
    if "interpreters" in meta:
        raw = meta["interpreters"]
        if isinstance(raw, str):
            raw = [raw]
        if not isinstance(raw, list) or any(not str(p).strip() for p in raw):
            return (
                "interpreters must be a list of interpreter paths, e.g. "
                "interpreters=['~/.venvs/x/bin/python']"
            )
    # `interpreter` against the allowlist the record WILL have. At create the merged view
    # IS the patch, so naming an interpreter with no `interpreters` beside it is refused —
    # correctly, since that agent could never run. At update the record's existing
    # allowlist is in scope, so naming one already on it is allowed.
    if "interpreter" in merged and merged["interpreter"]:
        try:
            proc.resolve(merged, merged["interpreter"])
        except proc.InterpreterRefused as e:
            return str(e)
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
