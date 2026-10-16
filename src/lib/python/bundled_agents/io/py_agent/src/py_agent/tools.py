"""py_agent — the BASE a folder bundle with a program extends.

A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent` names its
program (`command`, run with cwd = the folder) and its verbs (`verbs`: verb → doc,
the one truth `docs` and `reflect bundles=all` read). Each instance
(`create_agent handler_module=<name>`) is a FULL agent: the kernel starts the
program when the instance boots and kills it when the instance is deleted or the
kernel stops; callers reach it only through verbs — there are no process verbs.

THE WIRE is the exec_bridge connector channel (`FANTASTIC_CTRL_FD`, frames
`[4-byte BE length | T json | B io_bridge binary]`, raw bytes never base64), the one
relay `exec_bridge._serve_ctrl` with two more ops:

  - child → kernel `{op:"hello", verbs:[…]}` — FIRST: the verbs it serves; they must
    equal `bundle.yaml` `verbs`, else the instance stops, saying so;
  - kernel → child `{op:"call", rid, payload}` — a verb call;
  - child → kernel `{op:"result", rid, result}` — its answer (a refusal is data).

plus the connector's own `send`/`reply`, `emit`, `watch`, `onmessage`. A call that
arrives before `hello` waits for it (the caller's timeout bounds it).

Nothing respawns: a program that exits settles every in-flight call with its exit,
emits `status phase=stopped`, and is back only by `reload_bundle` (the root's verb).
SECURITY: the program runs with the user's OS rights — its own subprocesses, files
and network never pass a gate.
"""

from __future__ import annotations

import asyncio
from types import ModuleType

from exec_bridge import proc
from exec_bridge.tools import _child_env, _serve_ctrl
from kernel._refusal import (
    MISSING_ARG,
    TRANSPORT_CLOSED,
    TRANSPORT_ERROR,
    UNHANDLED,
    refusal,
)

NAME = "py_agent"
SECURITY = (
    "a py_agent program runs with the user's OS rights — its own subprocesses, "
    "files and network never pass a gate"
)
#: Chars of the program's stderr kept for its instance's reflect.
STDERR_TAIL = 2000
#: Verbs the kernel and this base answer; a bundle cannot declare them.
RESERVED = ("reflect", "boot", "shutdown")

# instance id → its running program
_RUN: dict[str, dict] = {}


# ─── the base itself ────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """No args. → {id, sentence}."""
    return {
        "id": id,
        "sentence": "The base a folder bundle with a program extends — create an "
        "agent of the bundle, not of py_agent.",
    }


VERBS = {"reflect": _reflect}
NOTES = (
    "A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent`: "
    "`sentence`, `command` (argv, cwd = the folder), `verbs` (verb → doc), optional "
    "`emits`, `notes`, `record` (defaults under each instance). " + SECURITY + "."
)


def validate_meta(meta: dict, record: dict) -> str | None:
    """py_agent is a base: an agent is created of a bundle folder that extends it."""
    return (
        "py_agent is a base — create an agent of a bundle folder whose bundle.yaml "
        "says `base: py_agent` (handler_module = the folder name)"
    )


async def handler(id: str, payload: dict, kernel) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return refusal(
            UNHANDLED,
            f"py_agent: no verb {payload.get('type')!r}",
            verb=payload.get("type"),
        )
    return await fn(id, payload, kernel)


# ─── a folder bundle that extends it ────────────────────────────────


def folder_bundle(name: str, folder: str, spec: dict) -> ModuleType | str:
    """The module a folder bundle runs as, or the refusal text naming the field.
    `folder` is the bundle folder's absolute path, `spec` its parsed bundle.yaml."""
    command = spec.get("command")
    if not (
        isinstance(command, list)
        and command
        and all(isinstance(a, str) and a for a in command)
    ):
        return (
            "command: required — the program's argv as a list of strings, "
            "e.g. [python3, agent.py] (run with cwd = the bundle folder)"
        )
    verbs = spec.get("verbs")
    if not (
        isinstance(verbs, dict)
        and verbs
        and all(
            isinstance(k, str) and k and isinstance(v, str) for k, v in verbs.items()
        )
    ):
        return (
            "verbs: required — a map of verb → its doc (the first sentence is the "
            "signature), e.g. {search: 'args: q:str (req). Returns {hits}.'}"
        )
    taken = [v for v in verbs if v in RESERVED]
    if taken:
        return f"verbs: {', '.join(taken)} — answered by the kernel; name the program's verbs otherwise"
    record = spec.get("record") or {}
    if not isinstance(record, dict):
        return "record: must be a mapping of defaults for each instance's record"
    mod = ModuleType(name)
    mod.BASE = NAME
    mod.FOLDER = folder
    mod.SENTENCE = spec["sentence"]
    mod.VERBS = dict(verbs)
    mod.EMITS = spec.get("emits")
    notes = spec.get("notes")
    mod.NOTES = f"{notes}\n\n{SECURITY}." if notes else f"{SECURITY}."

    async def _handler(id: str, payload: dict, kernel) -> dict | None:
        verb = payload.get("type")
        if verb == "reflect":
            return _instance_reflect(id, name, folder, spec)
        if verb == "boot":
            return await _start(id, name, folder, spec, kernel)
        if verb == "shutdown":
            # a reload (`reload_bundle`: shutdown, then boot) or a stop
            await _stop(id, f"{id}: {payload.get('reason') or 'the kernel stopped'}")
            return None
        if verb in verbs:
            return await _call(id, payload)
        return refusal(
            UNHANDLED,
            f"{name}: no verb {verb!r}",
            hint=f"its verbs: {', '.join(verbs)}",
            verb=verb,
        )

    async def _on_delete(agent) -> None:
        await _stop(agent.id, f"{agent.id}: deleted")

    async def _on_cancel(id, task_id, kernel) -> None:
        # the program's own task (relay / wait) cancelled — by `cancel` or the
        # kernel's stop: the real process goes with it
        run = _RUN.get(id)
        if run is not None and task_id in run["task_ids"]:
            await _stop(id, f"{id}: cancelled")

    mod.handler = _handler
    mod.on_delete = _on_delete
    mod.on_cancel = _on_cancel
    return mod


def _instance_reflect(id: str, name: str, folder: str, spec: dict) -> dict:
    run = _RUN.get(id)
    out = {
        "id": id,
        "sentence": spec["sentence"],
        "base": NAME,
        "bundle": name,
        "folder": folder,
        "program": run["state"] if run else "not started",
        "security": SECURITY,
    }
    if run and run.get("code") is not None:
        out["exit_code"] = run["code"]
    if run and run.get("error"):
        out["program_error"] = run["error"]
    if run and run.get("stderr"):
        out["stderr_tail"] = run["stderr"]
    return out


async def _start(id: str, name: str, folder: str, spec: dict, kernel) -> dict | None:
    """Spawn the instance's program (idempotent while it runs)."""
    run = _RUN.get(id)
    if run is not None and run["state"] in ("starting", "running"):
        return None
    record = {**(spec.get("record") or {}), **(kernel.get(id) or {})}
    env = _child_env(record)
    parent, child = proc.ctrl_pair()
    env["FANTASTIC_CTRL_FD"] = str(child.fileno())
    env["FANTASTIC_SELF_ID"] = id
    try:
        process = await proc.spawn_argv(
            list(spec["command"]), cwd=folder, env=env, pass_fd=child.fileno()
        )
    except proc.NoPath:
        parent.close()
        return refusal(
            MISSING_ARG,
            f"{name}: {spec['command'][0]!r} is a bare command and the program gets no "
            "PATH to find it in — the kernel's own PATH is not the child's",
            hint="bundle.yaml record: {inherit_env: [PATH]} (or an absolute command)",
            arg="PATH",
        )
    except Exception as e:  # noqa: BLE001 — the spawn's own failure is the answer
        parent.close()
        return refusal(TRANSPORT_ERROR, f"{name}: spawn failed: {e}")
    finally:
        # the parent's copy of the child end must go, or EOF never reaches either side
        child.close()

    run = {
        "proc": process,
        "push": None,
        "pending": {},
        "hello": asyncio.Event(),
        "rid": 0,
        "state": "starting",
        "code": None,
        "error": None,
        "tasks": [],
        "task_ids": set(),
        "stderr": "",
    }
    _RUN[id] = run

    def _on_push(push) -> None:
        run["push"] = push

    async def _hello(m: dict) -> None:
        served = m.get("verbs")
        declared = set(spec["verbs"])
        if not isinstance(served, list) or set(served) != declared:
            got = sorted(served) if isinstance(served, list) else served
            await _stop(
                id,
                f"{name}: the program serves {got} but bundle.yaml declares "
                f"{sorted(declared)} — they must match",
            )
            return
        run["state"] = "running"
        run["hello"].set()
        await kernel.emit(id, {"type": "status", "phase": "running"})

    async def _result(m: dict) -> None:
        fut = run["pending"].pop(m.get("rid"), None)
        if fut is not None and not fut.done():
            fut.set_result(m.get("result"))

    async def _wait() -> None:
        code = await process.wait()
        if run["state"] != "stopped":
            run["code"] = code
            await _stop(id, f"{id}: its program exited (code {code})", exited=True)
            await kernel.emit(
                id,
                {
                    "type": "status",
                    "phase": "stopped",
                    "reason": "exited",
                    "code": code,
                },
            )

    relay = _serve_ctrl(
        parent,
        id,
        kernel.ctx.mint_id(),
        kernel,
        ops={"hello": _hello, "result": _result},
        on_push=_on_push,
    )

    async def _drain() -> None:
        # stdout/stderr are PIPES: unread, a chatty program blocks once one fills.
        # stdout is the program's own; stderr's tail is kept for reflect (a crash's
        # traceback is the one thing that says why it died).
        async def out() -> None:
            while await process.stdout.read(65536):
                pass

        async def err() -> None:
            while chunk := await process.stderr.read(65536):
                run["stderr"] = (run["stderr"] + chunk.decode("utf-8", "replace"))[
                    -STDERR_TAIL:
                ]

        await asyncio.gather(out(), err())

    for verb, coro in (("relay", relay), ("program", _wait()), ("output", _drain())):
        tid = kernel.ctx.spawn_task(id, verb, coro)
        run["task_ids"].add(tid)
        run["tasks"].append(kernel.ctx.tasks[tid]["_task"])
    return None


async def _call(id: str, payload: dict):
    run = _RUN.get(id)
    if run is None or run["state"] == "stopped":
        why = (run or {}).get("error") or f"{id}: its program is not running"
        return refusal(
            TRANSPORT_CLOSED,
            why,
            hint="fix the bundle, then the root's reload_bundle restarts it",
        )
    await run["hello"].wait()
    if run["state"] != "running":
        return refusal(
            TRANSPORT_CLOSED, run.get("error") or f"{id}: its program stopped"
        )
    run["rid"] += 1
    rid = str(run["rid"])
    fut = asyncio.get_running_loop().create_future()
    run["pending"][rid] = fut
    run["push"]({"op": "call", "rid": rid, "payload": payload})
    try:
        return await fut
    finally:
        run["pending"].pop(rid, None)


async def _stop(id: str, why: str, *, exited: bool = False) -> None:
    """Stop the program (kill its session unless it already exited), settle every
    in-flight call with `why`, release the waiters."""
    run = _RUN.get(id)
    if run is None or run["state"] == "stopped":
        return
    run["state"] = "stopped"
    run["error"] = why  # why it stopped — said to every later call and in reflect
    if not exited and run["proc"].returncode is None:
        proc.kill_session(run["proc"].pid)
    for fut in list(run["pending"].values()):
        if not fut.done():
            fut.set_result(refusal(TRANSPORT_CLOSED, why))
    run["pending"].clear()
    run["hello"].set()  # a call waiting for hello now sees `stopped`
    current = asyncio.current_task()
    for t in run["tasks"]:
        if t is not current and not t.done():
            t.cancel()
