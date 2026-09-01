"""A run-to-completion facade over an `exec_bridge` runner.

The seam it closes: `exec_bridge.start` is deliberately async — it spawns in the
BACKGROUND and returns `{status:'running'}` at once; the result arrives out of
band as a `job_done` event or via `status` polling. A weak model treats that
immediate `running` as terminal, fires the test suite as its LAST action, never
polls, and ends before the (slow-to-compile) suite goes red.

This agent adds NO capability the model lacks — it only COLLAPSES the
start→poll→read dance into one blocking `run` call, exactly like Aider's own
polyglot harness runs tests for the model. The command gate stays on the
runner: `run` forwards `start` to `target_id` (the exec_bridge agent), which
enforces its own `allowed` allow-list and `cwd`. Composition only, zero kernel
code.
"""

from __future__ import annotations

import asyncio

from kernel import current_task  # noqa: F401  (kept: parity with sibling bundles)
from kernel._refusal import UNHANDLED, refusal

#: exec_bridge job states that are NOT terminal (see exec_bridge/tools.py:66).
_LIVE = {"running", "stopping"}
_TAIL_LINES = 40


def _runner(id: str, payload: dict, kernel) -> str | None:
    """The exec_bridge id this call drives: `on` wins, else the record's
    `target_id`. None means unwired — `run` fails loud on it."""
    on = payload.get("on")
    if isinstance(on, str) and on:
        return on
    rec = kernel.get(id) or {}
    tid = rec.get("target_id")
    return tid if isinstance(tid, str) and tid else None


async def _send(kernel, target: str, payload: dict):
    return (await kernel.send([{"target": target, "payload": payload}]))[0].get("result")


def _tail(text: str, n: int = _TAIL_LINES) -> str:
    return "\n".join((text or "").splitlines()[-n:])


async def _run(id, payload, kernel):
    """args: command:str (req), args:list[str]?, on:str?, timeout:float=600, poll:float=1.5 —
    forwards `start` to the runner, BLOCKS until the job exits, returns
    {ok, exit_code, status, tail, lines, job_id}. One call = run-to-completion."""
    runner = _runner(id, payload, kernel)
    if not runner:
        return refusal(
            "missing",
            "sync_run: no runner — set `target_id` on this agent or pass `on`",
            arg="target_id",
        )
    timeout = float(payload.get("timeout", 600) or 600)
    poll = max(0.25, float(payload.get("poll", 1.5) or 1.5))

    start = {"type": "start", "command": payload.get("command")}
    if "args" in payload:
        start["args"] = payload["args"]
    r = await _send(kernel, runner, start)
    if not isinstance(r, dict) or r.get("error"):
        return r if isinstance(r, dict) else {"error": str(r)}
    job_id = r.get("job_id")

    waited = 0.0
    while True:
        st = await _send(kernel, runner, {"type": "status", "job_id": job_id})
        if not isinstance(st, dict) or st.get("error"):
            return st if isinstance(st, dict) else {"error": str(st)}
        jobs = st.get("jobs") or []
        job = jobs[0] if jobs else {}
        state = job.get("status")
        if state not in _LIVE:
            out = (job.get("stdout") or "") + (job.get("stderr") or "")
            code = job.get("exit_code")
            return {
                "job_id": job_id,
                "status": state,
                "exit_code": code,
                "ok": code == 0,
                "lines": job.get("lines"),
                "tail": _tail(out),
            }
        if waited >= timeout:
            await _send(kernel, runner, {"type": "stop", "job_id": job_id})
            return {
                "job_id": job_id,
                "status": "timeout",
                "exit_code": None,
                "ok": False,
                "tail": _tail(job.get("last_line") or ""),
            }
        await asyncio.sleep(poll)
        waited += poll


async def _reflect(id, payload, kernel):
    """Identity + the one verb + wired runner. No args."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "kind": "sync_run",
        "runner": rec.get("target_id"),
        "verbs": {n: ((f.__doc__ or "").strip().splitlines() or [""])[0] for n, f in VERBS.items()},
        "howto": (
            "Call `run` with the exact test command to run the suite to completion "
            "in ONE blocking call — it returns {ok, exit_code, tail}. No polling; "
            "read `tail` for failures, fix the stub, call `run` again until ok."
        ),
    }


async def _boot(id, payload, kernel):
    return await _reflect(id, payload, kernel)


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "run": _run,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"sync_run: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)
