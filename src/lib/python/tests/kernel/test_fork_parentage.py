"""A FORK'S WORK IS PARENTED TO THE FORK, not to the dispatch that made it.

The claim under test, stated as a failure: `exec_bridge.start` dispatches,
spawns the control relay, and returns `{job_id, status:"running"}`. That
dispatch is FINISHED — its table entry is gone. But the relay it spawned
inherited the dispatch's `_current_task`, so every send the running PROGRAM
makes afterwards is rowed with a parent that ended before the program did any
work.

Two things break when that happens. The trace lies — the row names a task that
no longer exists, which `register_task` used to paper over by dropping a dead
parent to None. And `cancel` misses it: cancel kills a task AND ITS SUBTREE, so
work that became a root is in nobody's subtree. The e2e harness reaps a capped
agent exactly that way (`e2e/helpers/ai.py` cancels its live tasks by id), so a
program that escaped the tree keeps running after the harness believes it
cleaned up.
"""

from __future__ import annotations

import asyncio

import _testkit

# A program that sends to a peer and reports the pid it was given back. The
# CONNECTOR gives it `kernel`; the send is a real gated dispatch whose row the
# test then reads.
PROGRAM = """
r = kernel.send("mailbox", {"type": "note", "text": "from the program"})
kernel.emit({"type": "say", "text": "sent"})
"""


async def _mailbox_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "a peer the program writes to", "verbs": {}}
    if verb == "note":
        _NOTES.append(payload.get("text"))
        return {"noted": True}
    return {"error": f"mailbox: unknown verb {verb!r}", "reason": "unhandled"}


_NOTES: list = []


def _install_mailbox() -> None:
    import sys
    import types

    m = types.ModuleType("mailbox_probe.tools")
    m.handler = _mailbox_handler
    sys.modules["mailbox_probe.tools"] = m


async def test_a_programs_send_is_parented_to_the_job_not_a_dead_dispatch(
    seeded_kernel,
):
    _NOTES.clear()
    _install_mailbox()
    root = seeded_kernel

    root.create(
        "mailbox_probe.tools",
        id="mailbox",
        callable_by={"runner": "*"},
    )
    root.create(id="runner", **_testkit.py_runner_meta(can_call={"mailbox": "*"}))

    start = (
        await root.send(
            [{"target": "runner", "payload": _testkit.py_start_payload(PROGRAM)}]
        )
    )[0]
    # The pid of the `start` DISPATCH itself — the parent the fork used to
    # inherit, and which is over the moment this call returns.
    start_pid = start["pid"]
    assert start["result"].get("job_id"), start["result"]

    for _ in range(200):
        if _NOTES:
            break
        await asyncio.sleep(0.02)
    assert _NOTES == ["from the program"], f"the program never sent: {_NOTES}"
    await asyncio.sleep(0.1)  # let the row settle

    # THE ROW THE PROGRAM'S SEND WROTE.
    rows = [
        r
        for r in root.ctx.rows.values()
        if r.get("agent") == "mailbox"
        if (r.get("verb") or (r.get("payload") or {}).get("type")) == "note"
    ]
    assert rows, f"no row for the program's send: {list(root.ctx.rows.values())}"
    parent = rows[0].get("parent")

    # The dispatch that forked the relay is LONG GONE...
    assert start_pid not in root.ctx.tasks, "the start dispatch is still in flight"
    # ...so it must not be what the program's work hangs from.
    assert parent != start_pid, (
        f"the program's send is parented to the settled `start` dispatch "
        f"{start_pid} — a task that ended before the program ran"
    )
    assert parent is not None, (
        "the program's send became a ROOT: cancel walks the subtree, so work "
        "outside every tree is unreachable by the caller that started it"
    )
