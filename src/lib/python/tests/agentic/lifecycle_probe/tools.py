"""A dispatch that STAYS in flight, plus the bundle half of cancellation.

It exists because no shipped bundle offers a cancellable in-flight dispatch
without a real OS resource: `scheduler`'s tick is a bare `create_task` outside
the task table, and `exec_bridge` would have to name an interpreter path in the
manifest. Inventing a kernel mechanism to make a test expressible is worse than
a test bundle, so this contributes only the two halves the substrate already
calls — a handler that blocks, and the optional `on_cancel` hook.

It is deliberately NOT used for the error taxonomy: a probe that authors its own
error strings would be measuring itself. Those probes all go to `file_bridge`.
"""

from __future__ import annotations

import asyncio

STARTED: dict[str, asyncio.Event] = {}  # agent id → "my slow handler is running"
PAST_THE_AWAIT: list[str] = []  # the step AFTER the await — the C14 negative
CANCEL_HOOK: list[tuple[str, str, bool]] = []  # (agent, task_id, handler alive)

VERBS: dict[str, str] = {}


def reset() -> None:
    """Drop everything recorded so far.

    Module globals are process-wide and every test hydrates the SAME ids, so a
    stale `asyncio.Event` from a previous test would belong to a closed loop and
    never fire again."""
    STARTED.clear()
    PAST_THE_AWAIT.clear()
    CANCEL_HOOK.clear()


async def handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {
            "id": id,
            "sentence": "a probe agent whose `slow` verb stays in flight",
            "verbs": {"slow": "no args. Blocks until cancelled."},
        }
    if verb == "slow":
        STARTED.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
        # Two observables in one line: the handler HAS a next step, and a
        # cancelled dispatch must never reach it.
        PAST_THE_AWAIT.append(id)
        return {"finished": True}
    return {"error": f"lifecycle_probe: unknown type {verb!r}"}


async def on_cancel(id, task_id, kernel):
    """The SIGTERM half. Records whether the asyncio task was STILL ALIVE when
    the hook ran — that is the whole contract: a bundle releases its real
    resources while its handler still exists, not after the abort."""
    entry = kernel.ctx.tasks.get(task_id)
    CANCEL_HOOK.append((id, task_id, entry is not None and not entry["_task"].done()))
