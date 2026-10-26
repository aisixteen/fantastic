"""`await_set` — ONE wake for a SET of agents. Twin of rust `await_set.rs`.

A lead that waits for several children would otherwise wake on every report and
re-send its whole context each time. `await_set {ids, timeout_s?, label?}` answers
at once with a `wait_id`; every message of `type: "report"` that a NAMED agent
sends the caller before the set completes is HELD (not delivered as its own turn);
when every named agent has reported, or the timeout passes, the caller gets ONE
message carrying them all. `await_cancel {wait_id}` drops a wait and delivers what
it held, one by one.

The signal is a plain message — a payload whose `type` is `report`, from the named
agent to the waiting caller — so this module depends on no `report` verb. State is
a table on the kernel plus three kinds of EXECUTION ROW (`await_open`,
`await_hold`, `await_close`) in the kernel's memory store, so a wait is rebuilt on
boot (`restore`).
"""

from __future__ import annotations

import asyncio
import json
import math
import sys
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from kernel._kernel import _in_shutdown
from kernel._refusal import bad, missing

if TYPE_CHECKING:
    from kernel._kernel import Kernel

KINDS = ("await_open", "await_hold", "await_close")


@dataclass
class Wait:
    caller: str
    ids: list[str]
    label: Any = None
    deadline: float | None = None
    # reporter id -> {state, result, payload}; insertion order = the order held, a
    # second report from the same agent replaces the first IN PLACE.
    held: dict[str, dict] = field(default_factory=dict)
    timer: "asyncio.Task | None" = None

    def covered(self) -> bool:
        return all(i in self.held for i in self.ids)


def _hold(w: Wait, frm: str, state: Any, result: Any, payload: Any) -> None:
    w.held[frm] = {"state": state, "result": result, "payload": payload}


def await_set(ctx: "Kernel", caller: str | None, payload: dict) -> dict:
    verb = "await_set"
    if caller is None:
        return bad(verb, "caller", "an agent — an operator send has nobody to wake")
    raw = payload.get("ids")
    if raw is None:
        return missing(verb, "ids")
    if not isinstance(raw, list):
        return bad(verb, "ids", "a list of agent ids")
    if not raw:
        return missing(verb, "ids")
    ids: list[str] = []
    for it in raw:
        if not isinstance(it, str) or not it:
            return bad(verb, "ids", "a list of agent id strings")
        agent = ctx.root if it == "kernel" else ctx.agents.get(it)
        if agent is None:
            return ctx.unknown_agent(it)
        if agent.id == caller:
            return bad(verb, "ids", "absent — an agent cannot await itself")
        if agent.id not in ids:
            ids.append(agent.id)
    timeout = payload.get("timeout_s")
    if timeout is not None:
        if (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout)
            or timeout <= 0
        ):
            return bad(verb, "timeout_s", "a positive number of seconds")
    label = payload.get("label")
    if label is not None and not isinstance(label, str):
        return bad(verb, "label", "a string")
    deadline = time.time() + timeout if timeout is not None else None
    wait_id = ctx.mint_id()
    ctx.rows_put(
        caller,
        {
            "id": wait_id,
            "kind": "await_open",
            "wait_id": wait_id,
            "caller": caller,
            "ids": ids,
            "label": label,
            "deadline": deadline,
        },
    )
    w = Wait(caller=caller, ids=ids, label=label, deadline=deadline)
    if timeout is not None:
        w.timer = _arm(ctx, wait_id, timeout)
    ctx._await_waits[wait_id] = w
    return {"wait_id": wait_id, "label": label, "ids": ids, "timeout_s": timeout}


def _arm(ctx: "Kernel", wait_id: int, after: float) -> "asyncio.Task":
    async def run() -> None:
        await asyncio.sleep(after)
        w = ctx._await_waits.pop(wait_id, None)
        if w is not None:
            w.timer = None  # the timer IS this task: never cancel itself
            await _finish(ctx, wait_id, w, True)

    return asyncio.ensure_future(run())


def await_cancel(ctx: "Kernel", caller: str | None, payload: dict) -> dict:
    raw = payload.get("wait_id")
    if raw is None or raw == "" or isinstance(raw, bool):
        return missing("await_cancel", "wait_id")
    wait_id: int | None = None
    if isinstance(raw, int):
        wait_id = raw
    elif isinstance(raw, str):
        try:
            wait_id = int(raw.strip())
        except ValueError:
            wait_id = None
    w = ctx._await_waits.get(wait_id) if wait_id is not None else None
    if w is None or caller is None or w.caller != caller:
        return {"cancelled": False, "wait_id": raw, "released": 0}
    del ctx._await_waits[wait_id]
    if w.timer is not None:
        w.timer.cancel()
    ctx.rows_put(
        w.caller, {"kind": "await_close", "wait_id": wait_id, "how": "cancelled"}
    )
    released = len(w.held)
    if released:
        asyncio.ensure_future(_redeliver(ctx, w.caller, list(w.held.items())))
    return {"cancelled": True, "wait_id": wait_id, "released": released}


async def _redeliver(ctx: "Kernel", caller: str, held: list) -> None:
    """Deliver each held report to the caller as ITS OWN message, from the agent
    that sent it, in the order held. The wait is gone, so the hook lets them by."""
    from kernel._agent import _current_sender

    for frm, h in held:
        token = _current_sender.set(frm)
        try:
            await ctx.send([{"target": caller, "payload": h["payload"]}])
        finally:
            _current_sender.reset(token)


def hold_report(
    ctx: "Kernel", target: str, origin: str | None, payload: dict
) -> dict | None:
    """THE HOOK in `_dispatch`: is this `report` held for an open wait? A reply =
    held (the reporter is answered at once, never blocked on the lead's turn);
    None = dispatch it normally."""
    if payload.get("type") != "report" or origin is None:
        return None
    found = next(
        (
            (wid, w)
            for wid, w in ctx._await_waits.items()
            if w.caller == target and origin in w.ids
        ),
        None,
    )
    if found is None:
        return None
    wait_id, w = found
    state, result = payload.get("state"), payload.get("result")
    _hold(w, origin, state, result, payload)
    ctx.rows_put(
        target,
        {
            "kind": "await_hold",
            "wait_id": wait_id,
            "from": origin,
            "state": state,
            "result": result,
            "payload": payload,
        },
    )
    if w.covered():
        # removal is the atomic claim: the timer and the last report cannot both win
        if ctx._await_waits.pop(wait_id, None) is not None:
            if w.timer is not None:
                w.timer.cancel()
                w.timer = None
            asyncio.ensure_future(_finish(ctx, wait_id, w, False))
    return {"held": True, "wait_id": wait_id}


async def _finish(ctx: "Kernel", wait_id: int, w: Wait, timed_out: bool) -> None:
    """Close a claimed wait and deliver its ONE combined message."""
    # A stopping kernel delivers nothing; the open row stays, so the wait is
    # rebuilt on the next boot.
    if ctx.draining and not _in_shutdown.get():
        return
    done = [
        {"id": i, "state": w.held[i]["state"], "result": w.held[i]["result"]}
        for i in w.ids
        if i in w.held
    ]
    miss = [i for i in w.ids if i not in w.held]
    body = {
        "wait_id": wait_id,
        "label": w.label,
        "done": done,
        "missing": miss,
        "timed_out": timed_out,
    }
    ctx.rows_put(
        w.caller,
        {
            "kind": "await_close",
            "wait_id": wait_id,
            "how": "timeout" if timed_out else "done",
        },
    )
    agent = ctx.agents.get(w.caller)
    if agent is None:
        return  # the lead left the tree: nobody to wake
    # `text` is what an AI lead reads; the structured fields ride beside it.
    msg = {"type": "send", "text": json.dumps(body, separators=(",", ":")), **body}
    await ctx.bookkeeping(agent, msg)


async def restore(ctx: "Kernel") -> list[tuple[int, bool]]:
    """Rebuild every wait the store holds an `await_open` for and no `await_close`.
    Called by `Kernel.start` once the store is wired; returns the waits already due
    (every id held, or the deadline passed) for `resume`, which runs AFTER the
    agents booted. A store read error is logged, never fatal."""
    rows: list[dict] = []
    try:
        rows = await ctx._store_read({"kinds": list(KINDS)})
    except Exception as e:  # noqa: BLE001
        print(f"  [kernel] await_set restore failed: {e}", file=sys.stderr)
    rows = rows + [r for r in ctx.rows.values() if r.get("kind") in KINDS]
    by_id = {r["id"]: r for r in rows if isinstance(r.get("id"), int)}
    waits: dict[int, Wait] = {}
    closed: list[int] = []
    for rid in sorted(by_id):
        r = by_id[rid]
        kind, wid = r.get("kind"), r.get("wait_id")
        if not isinstance(wid, int):
            continue
        if kind == "await_open":
            if isinstance(r.get("caller"), str):
                waits[wid] = Wait(
                    caller=r["caller"],
                    ids=[i for i in (r.get("ids") or []) if isinstance(i, str)],
                    label=r.get("label"),
                    deadline=r.get("deadline"),
                )
        elif kind == "await_hold":
            w, frm = waits.get(wid), r.get("from")
            if w is not None and isinstance(frm, str):
                _hold(w, frm, r.get("state"), r.get("result"), r.get("payload"))
        elif kind == "await_close":
            closed.append(wid)
    for wid in closed:
        waits.pop(wid, None)
    due: list[tuple[int, bool]] = []
    now = time.time()
    for wid, w in waits.items():
        if w.covered():
            due.append((wid, False))
        elif w.deadline is not None and w.deadline <= now:
            due.append((wid, True))
        elif w.deadline is not None:
            w.timer = _arm(ctx, wid, w.deadline - now)
        ctx._await_waits[wid] = w
    return due


def resume(ctx: "Kernel", due: list[tuple[int, bool]]) -> None:
    """Complete the waits `restore` found due; background, so the start is not held."""
    for wid, timed_out in due:

        async def run(wid: int = wid, timed_out: bool = timed_out) -> None:
            w = ctx._await_waits.pop(wid, None)
            if w is not None:
                await _finish(ctx, wid, w, timed_out)

        asyncio.ensure_future(run())
